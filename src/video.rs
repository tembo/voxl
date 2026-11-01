//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{Encoder, EncoderConfig, Planes};
use crate::hub::wait_any;
use crate::hub::{Client, Hub, Inflight, Out};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct VideoConfig {
    pub display: Option<String>,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    pub max_kbps: u32,
    pub paintover_crf: f32,
    pub paintover_burst: u32,
    pub paintover_delay_ms: u64,
    pub min_kbps: u32,
    pub coalesce: Duration,
    pub stripes: u32,
    pub adaptive_threads: bool,
    /// Viewers that can't keep up get a stream of their own (see `Tiering`).
    pub slow_tier: bool,
}

/// The main stream's tier and the one for viewers that can't keep up with it.
const MAIN: u8 = 0;
const SLOW: u8 = 1;

struct Job {
    /// The whole current screen. Every tier that encodes this capture holds the same
    /// buffer and hands it back when done.
    buf: Arc<I420>,
    force_idr: bool,
    paintover: bool,
    /// Last pass of a paint-over burst.
    paintover_final: bool,
    /// Rows that changed in this capture.
    band: Option<Band>,
    /// Per-macroblock change map for this capture (None: analyse everything).
    mb_dirty: Option<Arc<Vec<u8>>>,
    captured_at: Instant,
    marker_ms: i64,
}

fn union(a: Option<Band>, b: Band) -> Band {
    match a {
        None => b,
        Some(a) => Band {
            y0: a.y0.min(b.y0),
            y1: a.y1.max(b.y1),
        },
    }
}

/// Runs the pipeline. With a `Screen` (dynamic resolution), a client's size request
/// ends the capture loop: the encoders drain, the screen is resized and the pipeline
/// is rebuilt at the new size, and clients get the new geometry and a keyframe.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<crate::screen::Screen>,
) -> Result<(), String> {
    if let Some(s) = &screen {
        *hub.resize_max.lock() = Some(s.max());
    }
    let mut cap = open(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut tiering = Tiering::new(cfg.slow_tier);
            loop {
                if let Err(e) = capture_loop(&mut cap, &cfg, &hub, &mut tiering) {
                    log::error!("capture loop ended: {e}");
                    std::process::exit(1);
                }
                drop(cap);
                let req = hub.resize_req.lock().take();
                if let (Some(s), Some((w, h))) = (screen.as_mut(), req) {
                    if let Err(e) = s.resize(w, h) {
                        log::warn!("resize to {w}x{h} failed: {e}");
                    }
                }
                cap = match open(&cfg, &hub) {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("video restart failed: {e}");
                        std::process::exit(1);
                    }
                };
            }
        })
        .unwrap();
    Ok(())
}

/// Opens the capture for the current screen size.
fn open(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<Capturer, String> {
    let cap = Capturer::new(cfg.display.as_deref())?;
    let (w, h) = (cap.width, cap.height);
    hub.width.store(w, Ordering::Relaxed);
    hub.height.store(h, Ordering::Relaxed);
    hub.activity.reset(w, h);
    log::info!(
        "capturing {}x{} @ {} fps, preset {} threads {}",
        w,
        h,
        cfg.fps,
        cfg.preset,
        cfg.threads
    );
    *hub.stripes.lock() = stripe_layout(h, cfg.stripes);
    // New geometry (and a keyframe) for clients already connected; no-op at startup.
    hub.restart_clients();
    Ok(cap)
}

/// The encoders for one tier: a stripe each.
fn open_stripes(cfg: &VideoConfig, w: u32, layout: &[(u32, u32)]) -> Result<Vec<Stripe>, String> {
    let mut stripes = Vec::new();
    for &(y, sh) in layout {
        let fixed = if cfg.adaptive_threads {
            1
        } else {
            (cfg.threads / layout.len() as u32).max(1)
        };
        let ecfg = EncoderConfig {
            width: w,
            height: sh,
            fps: cfg.fps,
            preset: cfg.preset.clone(),
            threads: fixed,
            crf: cfg.crf,
            max_kbps: cfg.max_kbps,
        };
        let max_threads = if cfg.adaptive_threads {
            (cfg.threads / layout.len() as u32).max(1)
        } else {
            1
        };
        stripes.push(Stripe {
            y: y as usize,
            h: sh as usize,
            enc: Encoder::new(&ecfg)?,
            cfg: ecfg,
            max_threads,
            applied_kbps: 0,
            applied_crf: -1.0,
        });
    }
    Ok(stripes)
}

/// Whether these viewers have room for another frame.
fn flow_ok(clients: &[Arc<Client>], interval_ms: f64) -> bool {
    for c in clients {
        let (n, age) = c.backlog();
        if n == 0 {
            continue;
        }
        let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
            0 => 40,
            r => r,
        } as f64;
        // Enough frames in flight to cover one RTT plus decode, but never more than ~rtt+120ms of queue.
        let max_n = (((rtt + 40.0) / interval_ms).ceil() as usize + 2).clamp(3, 40);
        let max_age = rtt as u128 + 120;
        if age > 4000 {
            // Stalled client (hidden tab, dead socket): stop letting it gate everyone.
            log::warn!(
                "client {} stalled ({} frames, {} ms); resyncing",
                c.id,
                n,
                age
            );
            c.inflight.lock().clear();
            c.needs_keyframe.store(true, Ordering::Relaxed);
            c.started.store(false, Ordering::Relaxed);
            continue;
        }
        if n >= max_n || age > max_age {
            return false;
        }
    }
    true
}

/// Whether a viewer's acks show frames queueing up on the way to it.
fn is_congested(c: &Client) -> bool {
    let q = c.delay.lock().queue_ms;
    let (_, age) = c.backlog();
    let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
        0 => 40,
        r => r,
    } as f64;
    q > 30.0 || age as f64 > rtt + 80.0
}

/// Delay-based bitrate adaptation: back off toward the measured delivery rate when
/// acks show queueing, probe upward slowly when the path is clear.
struct RateCtl {
    target: f64,
    max: f64,
    min: f64,
    last_eval: Instant,
    last_dec: Instant,
    last_inc: Instant,
    /// Ramp multiplicatively until the first sign of congestion (like TCP slow start),
    /// so a session neither starts by flooding a slow link nor crawls up on a fast one.
    slow_start: bool,
    /// Delivery rate observed at the last congestion: the path's likely capacity.
    ceiling: f64,
    ceiling_at: Instant,
}

/// Starting point for a new session's bitrate cap (kbit/s).
const START_KBPS: f64 = 8000.0;

impl RateCtl {
    fn new(max: f64, min: f64) -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        RateCtl {
            target: START_KBPS.min(max),
            max,
            min,
            last_eval: past,
            last_dec: past,
            last_inc: past,
            slow_start: true,
            ceiling: f64::MAX,
            ceiling_at: past,
        }
    }

    /// A controller for viewers whose link was measured carrying `kbps`: it starts just
    /// under that instead of probing up from scratch.
    fn seeded(max: f64, min: f64, kbps: f64) -> Self {
        let mut rc = RateCtl::new(max, min);
        if kbps > 0.0 {
            // Not clamp: a ceiling set below the default floor (--max-kbps=1000) is allowed.
            rc.target = (kbps * 0.85).max(min).min(max);
            rc.ceiling = kbps;
            rc.ceiling_at = Instant::now();
            rc.slow_start = false;
        }
        rc
    }

    /// Forgets the capacity it measured and ramps up again: the viewer that set it has
    /// moved to the slow tier.
    fn reprobe(&mut self) {
        self.ceiling = f64::MAX;
        self.slow_start = true;
    }

    fn update(&mut self, clients: &[Arc<Client>]) {
        if self.last_eval.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_eval = Instant::now();
        let mut congested = false;
        let mut clear = true;
        let mut rate = f64::MAX;
        let mut delivered = 0f64;
        for c in clients {
            if !c.started.load(Ordering::Relaxed) {
                continue;
            }
            let q = c.delay.lock().queue_ms;
            if is_congested(c) {
                congested = true;
                let r = c.delivered.lock().rate_bps / 1000.0;
                if r > 0.0 {
                    rate = rate.min(r);
                }
            }
            if q > 15.0 {
                clear = false;
            }
            delivered = delivered.max(c.delivered.lock().rate_bps / 1000.0);
        }
        let now = Instant::now();
        if congested && self.last_dec.elapsed() >= Duration::from_millis(300) {
            let mut t = self.target * 0.85;
            // Delivery rate only says something about capacity when we were actually
            // pushing near the target; congestion from one big frame on an idle screen
            // would otherwise pin the ceiling at a few kbps.
            if rate.is_finite() && rate >= self.target * 0.5 {
                self.ceiling = rate;
                self.ceiling_at = now;
                t = t.min(rate * 0.85);
            }
            self.target = t.max(self.min);
            self.last_dec = now;
            self.slow_start = false;
            log::debug!(
                "rate: congested, target {:.0} kbps (delivered {:.0})",
                self.target,
                rate
            );
        } else if self.slow_start
            && !congested
            && clear
            && self.last_inc.elapsed() >= Duration::from_millis(250)
        {
            // Only probe upward once the content actually presses against the cap.
            if delivered >= self.target * 0.5 {
                self.target = (self.target * 1.5).min(self.max);
                self.last_inc = now;
                log::debug!(
                    "rate: slow start, target {:.0} kbps (delivered {:.0})",
                    self.target,
                    delivered
                );
            }
        } else if !congested
            && clear
            && self.last_dec.elapsed() >= Duration::from_secs(1)
            && delivered >= self.target * 0.5
        {
            // Only raise the cap while the content actually uses it: an idle screen says
            // nothing about the path, and an inflated cap overshoots the next burst.
            // Paths change; forget a capacity estimate that hasn't been confirmed lately.
            if self.ceiling_at.elapsed() >= Duration::from_secs(15) {
                self.ceiling = f64::MAX;
            }
            // Recover quickly to just under the known capacity, then probe past it gently
            // so a busy link isn't driven into the queue every couple of seconds.
            if self.target < self.ceiling * 0.9
                && self.last_inc.elapsed() >= Duration::from_millis(200)
            {
                self.target = (self.target * 1.06 + 100.0)
                    .min(self.ceiling * 0.9)
                    .min(self.max);
                self.last_inc = now;
            } else if self.target >= self.ceiling * 0.9
                && self.last_inc.elapsed() >= Duration::from_millis(1000)
            {
                self.target = (self.target * 1.02).min(self.max);
                self.last_inc = now;
            }
        }
    }
}

/// Regional sharpening. Blocks encoded while they changed carry motion-level quality;
/// once a block has been still for a while it is re-encoded at paint-over quality,
/// even while other parts of the screen keep moving (a playing video would otherwise
/// keep the frame-level paint-over from ever running).
struct Refiner {
    epoch: Instant,
    changed_ms: Vec<u32>,
    pending: Vec<bool>,
    last_refine: Instant,
    cols: usize,
}

const REFINE_STILL_MS: u32 = 300;
const REFINE_EVERY: Duration = Duration::from_millis(250);

impl Refiner {
    fn new(blocks: usize, cols: usize) -> Self {
        Refiner {
            epoch: Instant::now(),
            changed_ms: vec![0; blocks],
            pending: vec![false; blocks],
            last_refine: Instant::now(),
            cols,
        }
    }
    fn now(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }
    fn mark(&mut self, dirty: &[u8]) {
        let now = self.now();
        for (i, &d) in dirty.iter().enumerate() {
            if d != 0 {
                self.pending[i] = true;
                self.changed_ms[i] = now;
            }
        }
    }
    fn mark_all(&mut self) {
        let now = self.now();
        self.pending.fill(true);
        self.changed_ms.fill(now);
    }
    fn clear_all(&mut self) {
        self.pending.fill(false);
    }
    /// Every block not yet sharpened (1 = encode) and the row band they span.
    fn pending_map(&self) -> Option<(Vec<u8>, Band)> {
        let (mut r0, mut r1) = (usize::MAX, 0);
        let map: Vec<u8> = self
            .pending
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                if p {
                    r0 = r0.min(i / self.cols);
                    r1 = r1.max(i / self.cols + 1);
                }
                p as u8
            })
            .collect();
        (r1 > r0).then(|| {
            (
                map,
                Band {
                    y0: (r0 * 16) as u32,
                    y1: (r1 * 16) as u32,
                },
            )
        })
    }
    /// Blocks due for sharpening (1 = refine) and the row band they span.
    fn due(&mut self) -> Option<(Vec<u8>, Band)> {
        if self.last_refine.elapsed() < REFINE_EVERY {
            return None;
        }
        let now = self.now();
        let mut map = vec![0u8; self.pending.len()];
        let (mut r0, mut r1, mut any) = (usize::MAX, 0, false);
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i] && now.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS {
                *m = 1;
                self.pending[i] = false;
                let row = i / self.cols;
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
                any = true;
            }
        }
        if !any {
            return None;
        }
        self.last_refine = Instant::now();
        Some((
            map,
            Band {
                y0: (r0 * 16) as u32,
                y1: (r1 * 16) as u32,
            },
        ))
    }
}

/// Notes the damage the capturer has taken for /wait, whether or not anyone is watching.
fn note_changes(cap: &mut Capturer, hub: &Hub) {
    if !cap.changes.is_empty() {
        hub.activity.note(&cap.changes);
        cap.changes.clear();
    }
}

/// One encoded stream and its state. Tier 0 is the main stream; tier 1 exists while
/// some viewer can't keep up with it (see `Tiering`). Capture and conversion are
/// shared: a tier whose viewers are still catching up skips captures and collects
/// what they changed until it can send again.
struct Tier {
    job_tx: mpsc::SyncSender<Job>,
    encode: std::thread::JoinHandle<()>,
    rc: RateCtl,
    refiner: Refiner,
    paintover_left: u32,
    last_motion: Instant,
    /// Rows and macroblocks changed since this tier last encoded.
    band: Option<Band>,
    dirty: Vec<u8>,
    /// Had viewers at the last capture.
    active: bool,
}

/// What a tier sends from the current capture (`Tier::plan`).
struct Plan {
    want_key: bool,
    paintover_due: bool,
    band: Option<Band>,
    dirty: Vec<u8>,
    /// Blocks a paint-over sharpens, and the rows they span.
    paint_map: Option<(Vec<u8>, Band)>,
}

impl Tier {
    /// Starts a tier's encoder thread. The encoders open on that thread, so a tier that
    /// starts mid-stream doesn't hold up the capture loop.
    fn start(
        id: u8,
        cfg: &VideoConfig,
        hub: &Arc<Hub>,
        (w, h): (usize, usize),
        rc: RateCtl,
        ret: mpsc::Sender<Arc<I420>>,
    ) -> Tier {
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let layout = hub.stripes.lock().clone();
        let (cfg2, hub2) = (cfg.clone(), hub.clone());
        let encode = std::thread::Builder::new()
            .name(if id == MAIN { "encode" } else { "encode-slow" }.into())
            .spawn(move || {
                let stripes = match open_stripes(&cfg2, w as u32, &layout) {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("encoder: {e}");
                        std::process::exit(1);
                    }
                };
                encode_loop(id, stripes, job_rx, ret, hub2, cfg2.crf, cfg2.paintover_crf)
            })
            .unwrap();
        let blocks = w.div_ceil(16) * h.div_ceil(16);
        Tier {
            job_tx,
            encode,
            rc,
            refiner: Refiner::new(blocks, w.div_ceil(16)),
            paintover_left: 0,
            last_motion: Instant::now(),
            band: None,
            dirty: vec![0; blocks],
            active: false,
        }
    }

    /// Stops the encoder once it has sent what it has.
    fn stop(self) {
        drop(self.job_tx);
        let _ = self.encode.join();
    }

    fn paintover_due(&self, cfg: &VideoConfig) -> bool {
        self.paintover_left > 0
            && self.last_motion.elapsed() >= Duration::from_millis(cfg.paintover_delay_ms)
    }

    /// Notes what a capture changed, for this tier's next frame.
    fn note(&mut self, band: Option<Band>, dirty: &[u8]) {
        if let Some(b) = band {
            self.band = Some(union(self.band, b));
        }
        for (d, &m) in self.dirty.iter_mut().zip(dirty) {
            *d |= m;
        }
    }

    /// What this tier sends from the current capture, if anything: what changed since it
    /// last encoded, a keyframe, or a paint-over.
    fn plan(&mut self, cfg: &VideoConfig, want_key: bool) -> Option<Plan> {
        let band = self.band.take();
        let n = self.dirty.len();
        let dirty = std::mem::replace(&mut self.dirty, vec![0; n]);
        self.refiner.mark(&dirty);
        let paintover_due = self.paintover_due(cfg);
        let is_motion = band.is_some();
        if !is_motion && !want_key && !paintover_due {
            return None;
        }
        // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
        let paint_map = if paintover_due && !is_motion && !want_key {
            match self.refiner.pending_map() {
                Some(m) => Some(m),
                None => {
                    self.paintover_left = 0;
                    return None;
                }
            }
        } else {
            None
        };
        Some(Plan {
            want_key,
            paintover_due,
            band,
            dirty,
            paint_map,
        })
    }

    /// The frame `plan` decided on, from `buf`: the capture just converted.
    fn job(
        &mut self,
        plan: Plan,
        cfg: &VideoConfig,
        hub: &Hub,
        buf: Arc<I420>,
        captured_at: Instant,
        marker_ms: i64,
    ) -> Job {
        let Plan {
            want_key,
            paintover_due,
            mut band,
            dirty,
            paint_map,
        } = plan;
        let mut refine = None;
        let paintover = if band.is_some() {
            self.last_motion = Instant::now();
            self.paintover_left = cfg.paintover_burst;
            // Piggyback sharpening of blocks that have gone still on this frame.
            refine = if want_key { None } else { self.refiner.due() };
            refine.is_some()
        } else if paintover_due {
            self.paintover_left -= 1;
            hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        };
        if want_key {
            // A keyframe is squeezed into the rate cap; sharpen all of it once still.
            self.refiner.mark_all();
        }
        let frame_paintover = paintover && refine.is_none();
        if frame_paintover && self.paintover_left == 0 {
            self.refiner.clear_all();
        }
        // A refinement frame encodes what moved plus what is due; everything else stays skipped.
        let mb_map = if want_key {
            None
        } else if frame_paintover {
            paint_map.map(|(m, b)| {
                band = Some(union(band, b));
                Arc::new(m)
            })
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(union(band, rband));
            let mut m = dirty;
            for (d, r) in m.iter_mut().zip(rmap) {
                *d |= r;
            }
            hub.stats.refines.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(m))
        } else {
            Some(Arc::new(dirty))
        };
        Job {
            buf,
            force_idr: want_key,
            paintover,
            paintover_final: frame_paintover && self.paintover_left == 0,
            mb_dirty: mb_map,
            band,
            captured_at,
            marker_ms,
        }
    }
}

/// Moves viewers between the main stream and the slow tier's.
///
/// A viewer whose acks keep showing congestion while another viewer of the main stream
/// keeps up gets the slow tier's stream instead, at a bitrate its own link carries, so
/// it no longer holds back everyone else's bitrate and frame rate (or a recording's).
/// Congestion is remembered for a while: once the stream has slowed down to suit a
/// viewer, it rarely congests again, and a faster viewer joining later shouldn't have to
/// wait for it to. A viewer moves back once it has gone without congestion for a while
/// and its link has carried about as much as the main stream's viewers get. The slow
/// tier's encoder only runs while it has viewers.
struct Tiering {
    enabled: bool,
    last_eval: Instant,
    viewers: HashMap<u64, Standing>,
}

#[derive(Default)]
struct Standing {
    /// Decaying count of evaluations that found this viewer congested on the main stream.
    strain: f64,
    /// Evaluations it has been watched for.
    seen: u32,
    /// Last evaluation that found it congested.
    congested_at: Option<Instant>,
    /// Times it has moved to the slow tier.
    moves: u32,
}

/// Evaluations are 100 ms apart, and `Standing::strain` halves in about 14 s.
const STRAIN_DECAY: f64 = 0.995;
/// Strain that moves a viewer to the slow tier, if another viewer of the main stream has
/// less than `STRAIN_CLEAR` and has been watched for `SEEN_MIN` evaluations.
const STRAIN_MOVE: f64 = 3.0;
const STRAIN_CLEAR: f64 = 1.0;
const SEEN_MIN: u32 = 10;
/// How long a slow-tier viewer has to go without congestion before it rejoins the main
/// stream, doubling (up to 16x) each time it has moved so a borderline link settles
/// instead of flapping...
const REJOIN_AFTER: Duration = Duration::from_secs(10);
/// ...and how much of what the main stream's viewers get it has to have received.
const REJOIN_RATIO: f64 = 0.8;

impl Tiering {
    fn new(enabled: bool) -> Self {
        Tiering {
            enabled,
            last_eval: Instant::now() - Duration::from_secs(1),
            viewers: HashMap::new(),
        }
    }

    /// Moves viewers between tiers, and opens and closes the slow tier for them.
    /// `start_slow` starts the slow tier's encoder with that rate controller.
    fn update(
        &mut self,
        cfg: &VideoConfig,
        clients: &[Arc<Client>],
        tiers: &mut [Option<Tier>; 2],
        start_slow: &dyn Fn(RateCtl) -> Tier,
    ) {
        let tier = |c: &Client| c.tier.load(Ordering::Relaxed);
        let started = |c: &Client| c.started.load(Ordering::Relaxed);
        let kbps = |c: &Client| c.delivered.lock().rate_bps / 1000.0;
        let seeded = |kbps| RateCtl::seeded(cfg.max_kbps as f64, cfg.min_kbps as f64, kbps);
        let main_has = clients.iter().any(|c| tier(c) == MAIN);
        // A slow-tier viewer without a slow tier: back from a restart, or from having its
        // video off while the tier closed.
        if tiers[SLOW as usize].is_none() {
            let slow: Vec<_> = clients.iter().filter(|c| tier(c) == SLOW).collect();
            if !slow.is_empty() {
                if self.enabled && main_has {
                    let k = slow.iter().map(|c| kbps(c)).fold(f64::MAX, f64::min);
                    tiers[SLOW as usize] = Some(start_slow(seeded(k)));
                } else {
                    for c in slow {
                        c.move_to(MAIN);
                    }
                }
            }
        }
        if self.last_eval.elapsed() < Duration::from_millis(100) {
            return;
        }
        let now = Instant::now();
        self.last_eval = now;
        self.viewers
            .retain(|id, _| clients.iter().any(|c| c.id == *id));
        let mut congested = Vec::with_capacity(clients.len());
        let mut strain = Vec::with_capacity(clients.len());
        for c in clients {
            let s = self.viewers.entry(c.id).or_default();
            let busy = started(c) && is_congested(c);
            if started(c) && tier(c) == MAIN {
                s.strain = s.strain * STRAIN_DECAY + busy as u8 as f64;
                s.seen = s.seen.saturating_add(1);
            }
            if busy {
                s.congested_at = Some(now);
            }
            congested.push(busy);
            strain.push((s.strain, s.seen));
        }
        // Main-stream viewers that have kept up so far.
        let keeps_up: Vec<bool> = clients
            .iter()
            .enumerate()
            .map(|(j, d)| {
                tier(d) == MAIN
                    && started(d)
                    && !congested[j]
                    && strain[j].0 < STRAIN_CLEAR
                    && strain[j].1 >= SEEN_MIN
            })
            .collect();
        let mut moved = Vec::new();
        for (i, c) in clients.iter().enumerate() {
            if !self.enabled || tier(c) != MAIN || !started(c) || strain[i].0 < STRAIN_MOVE {
                continue;
            }
            if (0..clients.len()).any(|j| j != i && keeps_up[j]) {
                let s = self.viewers.get_mut(&c.id).unwrap();
                s.strain = 0.0;
                s.moves += 1;
                // The wait to rejoin starts now.
                s.congested_at = Some(now);
                moved.push(c);
            }
        }
        if !moved.is_empty() {
            for c in &moved {
                log::info!(
                    "client {} can't keep up ({:.0} kbps); moving it to the slow tier",
                    c.id,
                    kbps(c)
                );
                c.move_to(SLOW);
            }
            if let Some(main) = tiers[MAIN as usize].as_mut() {
                main.rc.reprobe();
            }
            if tiers[SLOW as usize].is_none() {
                let k = moved.iter().map(|c| kbps(c)).fold(f64::MAX, f64::min);
                tiers[SLOW as usize] = Some(start_slow(seeded(k)));
            }
        }
        if tiers[SLOW as usize].is_none() {
            return;
        }
        let slow_clients: Vec<_> = clients.iter().filter(|c| tier(c) == SLOW).collect();
        let main_has = clients.iter().any(|c| tier(c) == MAIN);
        if slow_clients.is_empty() {
            // Nobody left on it.
            tiers[SLOW as usize] = None;
        } else if !main_has {
            // Its viewers are the only ones left: the main stream is theirs again, at their bitrate.
            let slow = tiers[SLOW as usize].take().unwrap();
            if let Some(main) = tiers[MAIN as usize].as_mut() {
                main.rc = slow.rc;
            }
            for c in slow_clients {
                log::info!("client {} is the main stream's only viewer now", c.id);
                c.move_to(MAIN);
            }
        } else {
            // What the main stream carries (as of the last frames acked). Below the
            // bitrate floor every link keeps up, which says nothing.
            let main_kbps = clients
                .iter()
                .filter(|c| tier(c) == MAIN && started(c))
                .map(|c| kbps(c))
                .fold(0.0, f64::max);
            for c in slow_clients {
                let s = self.viewers.entry(c.id).or_default();
                let hold = REJOIN_AFTER * (1 << s.moves.saturating_sub(1).min(4));
                let calm = s.congested_at.is_none_or(|t| now - t >= hold);
                let paced = main_kbps >= cfg.min_kbps as f64 && kbps(c) >= main_kbps * REJOIN_RATIO;
                if calm && paced && started(c) {
                    log::info!(
                        "client {} keeps up again; moving it to the main stream",
                        c.id
                    );
                    s.strain = 0.0;
                    c.move_to(MAIN);
                }
            }
        }
    }
}

fn capture_loop(
    cap: &mut Capturer,
    cfg: &VideoConfig,
    hub: &Arc<Hub>,
    tiering: &mut Tiering,
) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    // Two buffers: one is converted while the encoders read the other.
    let mut pool = [Arc::new(I420::new(w, h)), Arc::new(I420::new(w, h))];
    let mut pending: [Option<Band>; 2] = [Some(cap.full()), Some(cap.full())];
    let mut next_idx = 0usize;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow slight early capture so source jitter doesn't alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
    let (ret_tx, ret_rx) = mpsc::channel::<Arc<I420>>();
    let new_rc = || RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
    let start_slow = |rc| Tier::start(SLOW, cfg, hub, (w, h), rc, ret_tx.clone());
    let mut tiers = [
        Some(Tier::start(
            MAIN,
            cfg,
            hub,
            (w, h),
            new_rc(),
            ret_tx.clone(),
        )),
        None,
    ];
    let mut had_clients = false;
    let debug_marker = std::env::var("VOXL_DEBUG_MARKER").is_ok();

    loop {
        if hub.resize_req.lock().is_some() || cap.needs_rebuild() {
            // Viewers get the new geometry once the encoders have sent their last frames.
            for t in tiers.into_iter().flatten() {
                t.stop();
            }
            return Ok(());
        }
        cap.pump()?;
        note_changes(cap, hub);
        hub.drain_wake();
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage
            // stays pending, so the stream still sends what changed. Only the pixels
            // are copied here (the framebuffer is live); the PNG is made off this thread.
            // The capturer takes the damage for exactly these pixels with them, so the
            // screenshot's seq covers what it shows and nothing drawn after it.
            let bgrx = cap.snapshot()?;
            note_changes(cap, hub);
            let shot = Arc::new(crate::record::Snapshot {
                width: w,
                height: h,
                bgrx,
                seq: hub.activity.seq(),
            });
            for tx in snaps {
                let _ = tx.send(shot.clone());
            }
        }
        // Encoders hand buffers back when done; a buffer is free once both have.
        while ret_rx.try_recv().is_ok() {}
        let clients = hub.video_clients();
        if clients.is_empty() {
            if had_clients {
                tiers[SLOW as usize] = None;
                hub.target_kbps[SLOW as usize].store(0, Ordering::Relaxed);
                if let Some(t) = tiers[MAIN as usize].as_mut() {
                    t.rc = new_rc();
                    hub.target_kbps[MAIN as usize].store(t.rc.target as u32, Ordering::Relaxed);
                }
            }
            had_clients = false;
            wait_any(&[cap.fd(), hub.waker.waitable()], 500);
            continue;
        }
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pending = [Some(cap.full()), Some(cap.full())];
            cap.damaged = true;
        }
        tiering.update(cfg, &clients, &mut tiers, &start_slow);
        let mut by_tier: [Vec<Arc<Client>>; 2] = Default::default();
        for c in clients {
            let i = (c.tier.load(Ordering::Relaxed) == SLOW) as usize;
            by_tier[i].push(c);
        }
        let mut want_key = [false; 2];
        let mut work = [false; 2];
        for (i, slot) in tiers.iter_mut().enumerate() {
            let Some(t) = slot else {
                hub.target_kbps[i].store(0, Ordering::Relaxed);
                continue;
            };
            let cs = &by_tier[i];
            if t.active && cs.is_empty() {
                t.band = None;
                t.dirty.fill(0);
            }
            t.active = !cs.is_empty();
            if t.active {
                t.rc.update(cs);
                want_key[i] = cs.iter().any(|c| c.needs_keyframe.load(Ordering::Relaxed));
                work[i] = cap.damaged || want_key[i] || t.band.is_some() || t.paintover_due(cfg);
            }
            hub.target_kbps[i].store(t.rc.target as u32, Ordering::Relaxed);
        }
        if !work.contains(&true) {
            let timeout = tiers
                .iter()
                .flatten()
                .filter(|t| t.active && t.paintover_left > 0)
                .map(|t| {
                    cfg.paintover_delay_ms
                        .saturating_sub(t.last_motion.elapsed().as_millis() as u64)
                        as i32
                        + 1
                })
                .min()
                .unwrap_or(100);
            wait_any(&[cap.fd(), hub.waker.waitable()], timeout);
            continue;
        }
        let since = last_capture.elapsed();
        if since < min_interval {
            let rem = min_interval - since;
            wait_any(
                &[hub.waker.waitable()],
                (rem.as_micros() as i32 + 999) / 1000,
            );
            continue;
        }
        // Apps present a frame as a burst of puts; give the burst a moment to land so
        // the capture holds one whole frame instead of its first tiles.
        if let Some(t) = cap.damaged_at {
            let el = t.elapsed();
            if el < cfg.coalesce {
                let rem = cfg.coalesce - el;
                std::thread::sleep(rem);
                continue;
            }
        }
        // Each tier sends only when its own viewers have room for another frame.
        let go: [bool; 2] =
            std::array::from_fn(|i| work[i] && (want_key[i] || flow_ok(&by_tier[i], interval_ms)));
        if !go.contains(&true) {
            hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
            wait_any(&[cap.fd(), hub.waker.waitable()], 4);
            continue;
        }
        // Need a free buffer (an encoder may still hold the other).
        if Arc::get_mut(&mut pool[next_idx]).is_none() {
            if ret_rx.recv_timeout(Duration::from_millis(50)).is_err() {
                continue;
            }
            if Arc::get_mut(&mut pool[next_idx]).is_none() {
                continue;
            }
        }

        let t0 = Instant::now();
        let origin = cap.damaged_at.unwrap_or(t0);
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mb_dirty = cap.take_mb_map();
        let mut band = None;
        if cap.damaged {
            band = cap.take_damage()?;
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let t1 = Instant::now();
        if debug_marker {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                last_capture.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = match cap.bgrx() {
            Some((px, stride)) if debug_marker => marker_probe(px, stride, "grab"),
            _ => -1,
        };
        // What changed goes to every tier: now, or once its viewers have caught up.
        for t in tiers.iter_mut().flatten().filter(|t| t.active) {
            t.note(band, &mb_dirty);
        }
        let mut plans: [Option<Plan>; 2] = [None, None];
        for (i, slot) in tiers.iter_mut().enumerate() {
            if let (true, Some(t)) = (go[i], slot) {
                plans[i] = t.plan(cfg, want_key[i]);
            }
        }
        if plans.iter().all(Option::is_none) {
            // Damage was empty after all.
            continue;
        }
        let idx = next_idx;
        let todo = match (pending[idx], band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(b) = band {
            pending[1 - idx] = Some(union(pending[1 - idx], b));
        }
        pending[idx] = None;
        if let Some(t) = todo {
            let buf = Arc::get_mut(&mut pool[idx]).expect("buffer in use");
            cap.convert(buf, t.y0 as usize, t.y1 as usize, cfg.convert_threads);
        }
        let t2 = Instant::now();
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        for (i, slot) in tiers.iter_mut().enumerate() {
            let (Some(t), Some(plan)) = (slot, plans[i].take()) else {
                continue;
            };
            if plan.want_key {
                for c in &by_tier[i] {
                    c.needs_keyframe.store(false, Ordering::Relaxed);
                }
            }
            let job = t.job(plan, cfg, hub, pool[idx].clone(), origin, marker_ms);
            if t.job_tx.send(job).is_err() {
                return Err("encoder gone".into());
            }
        }
        last_capture = t0;
        next_idx = 1 - idx;
    }
}

/// Split the screen into horizontal stripes, each an independent H.264 stream.
/// x264 pays ~9 ms of CPU per 2560x1600 frame even when nothing changed, and sliced
/// threading triples that; independent single-threaded stripes are encoded in
/// parallel (low latency) and only when they changed (low CPU).
fn stripe_layout(height: u32, n: u32) -> Vec<(u32, u32)> {
    let n = n.clamp(1, 16);
    let each = (height.div_ceil(n) + 15) & !15;
    let mut v = Vec::new();
    let mut y = 0;
    while y < height {
        let h = each.min(height - y);
        v.push((y, h));
        y += h;
    }
    v
}

struct Stripe {
    y: usize,
    h: usize,
    enc: Encoder,
    cfg: EncoderConfig,
    /// Sliced threads to fan out to under sustained heavy motion (1 = never).
    max_threads: u32,
    applied_kbps: u32,
    applied_crf: f32,
}

const HDR: usize = 16;

struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SendPtr<T> {}

struct StripeJob {
    /// Valid until the worker replies: the encode loop waits for every stripe.
    buf: SendPtr<I420>,
    ts: i64,
    force_idr: bool,
    crf: f32,
    cap: u32,
    mb_dirty: Option<Arc<Vec<u8>>>,
    mb_cols: usize,
}

type StripeResult = (usize, Option<(bool, Vec<u8>)>);

/// x264's sliced threading costs ~3x the CPU of one thread on desktop content (and on
/// small updates is even slower), so a stripe encodes single-threaded and only fans out
/// while sustained heavy motion would otherwise blow the frame budget.
struct ThreadGovernor {
    threads: u32,
    ewma_ms: f64,
    heavy: u32,
    light_since: Option<Instant>,
    budget_ms: f64,
    /// One-thread encode time as a function of changed macroblocks, learned from
    /// single-threaded frames, to tell when one thread would fit again.
    fit: CostFit,
    /// Predicted one-thread time while threaded.
    predicted_ms: f64,
}

/// Exponentially weighted least squares fit of encode ms = a + b * changed MBs.
#[derive(Default)]
struct CostFit {
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl CostFit {
    fn add(&mut self, x: f64, y: f64) {
        let k = 0.995;
        self.w = self.w * k + 1.0;
        self.x = self.x * k + x;
        self.y = self.y * k + y;
        self.xx = self.xx * k + x * x;
        self.xy = self.xy * k + x * y;
    }
    fn predict(&self, x: f64) -> Option<f64> {
        if self.w < 30.0 {
            return None;
        }
        let var = self.xx * self.w - self.x * self.x;
        if var <= f64::EPSILON * self.w * self.w {
            return Some(self.y / self.w);
        }
        let b = (self.xy * self.w - self.x * self.y) / var;
        let a = (self.y - b * self.x) / self.w;
        Some(a + b * x)
    }
}

impl ThreadGovernor {
    fn want(&mut self, enc_ms: f64, changed_mbs: f64, max: u32) -> Option<u32> {
        self.ewma_ms = self.ewma_ms * 0.8 + enc_ms * 0.2;
        if self.threads == 1 {
            self.fit.add(changed_mbs, enc_ms);
            // Before one thread runs out of frame budget: encode times vary from frame to
            // frame, and at ~90% of the budget on average enough frames miss it to drop
            // the frame rate.
            self.heavy = if self.ewma_ms > self.budget_ms * 0.65 {
                self.heavy + 1
            } else {
                0
            };
            if self.heavy >= 20 {
                self.predicted_ms = self.ewma_ms;
                return Some(max);
            }
            return None;
        }
        // Without enough one-thread history, assume one thread is no faster than these.
        let one = self.fit.predict(changed_mbs).unwrap_or(enc_ms);
        self.predicted_ms = self.predicted_ms * 0.8 + one * 0.2;
        if self.predicted_ms < self.budget_ms * 0.5 {
            // One thread would fit with room to spare (switching reopens the encoder,
            // which costs a keyframe, so not on a brief lull).
            let since = *self.light_since.get_or_insert_with(Instant::now);
            (since.elapsed() >= Duration::from_secs(2)).then_some(1)
        } else {
            self.light_since = None;
            None
        }
    }
}

fn stripe_worker(
    i: usize,
    mut s: Stripe,
    rx: mpsc::Receiver<StripeJob>,
    done: mpsc::Sender<StripeResult>,
    debug: bool,
) {
    let mut gov = ThreadGovernor {
        threads: 1,
        ewma_ms: 0.0,
        heavy: 0,
        light_since: None,
        budget_ms: 1000.0 / s.cfg.fps as f64,
        fit: CostFit::default(),
        predicted_ms: 0.0,
    };
    while let Ok(job) = rx.recv() {
        let buf = unsafe { &*job.buf.0 };
        if job.force_idr {
            // A keyframe is what a new viewer sees first; don't starve it to the
            // steady-state frame budget (up to ~8 frames' worth at 4x the cap).
            let boost = job.cap.saturating_mul(4).max(16_000);
            s.enc
                .set_rate_buf(job.crf, boost, boost / s.cfg.fps.max(1) * 8);
            s.applied_kbps = 0;
        } else {
            let moved = s.applied_kbps == 0
                || (job.cap as f64 - s.applied_kbps as f64).abs() > s.applied_kbps as f64 * 0.04;
            if job.crf != s.applied_crf || moved {
                s.enc.set_rate(job.crf, job.cap);
                s.applied_crf = job.crf;
                s.applied_kbps = job.cap;
            }
        }
        let suv = buf.stride_uv();
        let planes = Planes {
            y: &buf.y[s.y * buf.width..],
            u: &buf.u[(s.y / 2) * suv..],
            v: &buf.v[(s.y / 2) * suv..],
            stride_y: buf.width,
            stride_uv: suv,
        };
        let mut out = Vec::with_capacity(64 << 10);
        out.resize(HDR, 0);
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let rows = s.h.div_ceil(16);
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols]
                .iter()
                .map(|&x| (x == 0) as u8)
                .collect::<Vec<u8>>()
        });
        let te = Instant::now();
        let r = s.enc.encode(
            &planes,
            job.ts,
            job.force_idr,
            None,
            info.as_deref_mut(),
            &mut out,
        );
        if debug {
            let cpu_now = thread_cpu_us();
            log::info!(
                "stripe y{} encode {} us wall, thread cpu total {} us, {} bytes",
                s.y,
                te.elapsed().as_micros(),
                cpu_now,
                out.len()
            );
        }
        if s.max_threads > 1 {
            // Changed macroblocks in this stripe (all of them without a damage map).
            let changed = info
                .as_ref()
                .map_or((s.h.div_ceil(16) * job.mb_cols) as f64, |m| {
                    m.iter().filter(|&&c| c == 0).count() as f64
                });
            if let Some(t) = gov.want(te.elapsed().as_secs_f64() * 1000.0, changed, s.max_threads) {
                match Encoder::new(&EncoderConfig {
                    threads: t,
                    ..s.cfg.clone()
                }) {
                    Ok(e) => {
                        log::info!(
                            "stripe {i}: encoder now {t} thread(s) (encode {:.1} ms)",
                            gov.ewma_ms
                        );
                        // A fresh encoder opens with an IDR on its next frame.
                        s.enc = e;
                        s.applied_kbps = 0;
                        gov.threads = t;
                        gov.heavy = 0;
                        gov.light_since = None;
                    }
                    Err(e) => log::error!("encoder reopen failed: {e}"),
                }
            }
        }
        let res = match r {
            Ok(key) if out.len() > HDR => Some((key, out)),
            Ok(_) => None,
            Err(e) => {
                log::error!("{e}");
                None
            }
        };
        if done.send((i, res)).is_err() {
            return;
        }
    }
}

fn encode_loop(
    tier: u8,
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::Sender<Arc<I420>>,
    hub: Arc<Hub>,
    crf: f32,
    po_crf: f32,
) {
    let epoch = Instant::now();
    let n = stripes_in.len();
    let debug_stripes = std::env::var("VOXL_DEBUG_STRIPES").is_ok();
    // Geometry and paint-over bookkeeping stay here; encoders move to their workers.
    let mut stripes: Vec<(usize, usize, bool)> =
        stripes_in.iter().map(|s| (s.y, s.h, false)).collect();
    let (done_tx, done_rx) = mpsc::channel::<StripeResult>();
    let workers: Vec<(mpsc::Sender<StripeJob>, std::thread::JoinHandle<()>)> = stripes_in
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let (tx, rx) = mpsc::channel();
            let done = done_tx.clone();
            let name = if tier == MAIN {
                format!("enc-{i}")
            } else {
                format!("enc-slow-{i}")
            };
            let h = std::thread::Builder::new()
                .name(name)
                .spawn(move || stripe_worker(i, s, rx, done, debug_stripes))
                .unwrap();
            (tx, h)
        })
        .collect();
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        let selected: Vec<bool> = stripes
            .iter()
            .map(|&(y, h, touched)| {
                job.force_idr
                    || (job.paintover && touched)
                    || job
                        .band
                        .is_some_and(|b| (b.y0 as usize) < y + h && (b.y1 as usize) > y)
            })
            .collect();
        if !selected.iter().any(|&x| x) {
            let _ = ret.send(job.buf);
            continue;
        }
        let target = hub.target_kbps[tier as usize].load(Ordering::Relaxed);
        let want_crf = if job.paintover { po_crf } else { crf };
        // Each stripe may use up to twice its area share of the budget; the delay-based
        // controller reins the total in if several stripes move at once.
        let share = ((target as u64 * 2) / n as u64).max(target as u64 / n as u64 + 1) as u32;
        let ts = job.captured_at.duration_since(epoch).as_millis() as i64;
        // Persistent per-stripe workers keep each encoder's state warm in one core's cache.
        let buf_ptr = SendPtr(&*job.buf as *const I420);
        let mut pending = 0;
        for (i, &on) in selected.iter().enumerate() {
            if on {
                let cap = if job.paintover {
                    share.saturating_mul(2)
                } else {
                    share
                };
                let _ = workers[i].0.send(StripeJob {
                    buf: buf_ptr,
                    ts,
                    force_idr: job.force_idr,
                    crf: want_crf,
                    cap,
                    mb_dirty: job.mb_dirty.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        for _ in 0..pending {
            if let Ok((i, r)) = done_rx.recv() {
                results[i] = r;
            }
        }
        let _ = ret.send(job.buf);
        for (y, h, touched) in stripes.iter_mut() {
            if job.paintover_final {
                *touched = false;
            }
            if job
                .band
                .is_some_and(|b| (b.y0 as usize) < *y + *h && (b.y1 as usize) > *y)
            {
                *touched = true;
            }
        }
        let parts: Vec<(usize, bool, Vec<u8>)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|(k, o)| (i, k, o)))
            .collect();
        if parts.is_empty() {
            continue;
        }
        let enc_us = t0.elapsed().as_micros() as u64;
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        let frame_id = hub.frame_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let mut pipe = (job.captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        if job.marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            pipe = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32;
        }
        let nparts = parts.len() as u8;
        let msgs: Vec<Bytes> = parts
            .into_iter()
            .map(|(i, key, mut out)| {
                // type, flags, frame id, pipe, stripe, stripe count, parts in frame, reserved, y
                out[0] = 0x01;
                out[1] = (key as u8) | if job.paintover { 2 } else { 0 };
                out[2..6].copy_from_slice(&frame_id.to_be_bytes());
                out[6..10].copy_from_slice(&pipe.to_be_bytes());
                out[10] = i as u8;
                out[11] = n as u8;
                out[12] = nparts;
                out[13] = 0;
                out[14..16].copy_from_slice(&(stripes[i].0 as u16).to_be_bytes());
                Bytes::from(out)
            })
            .collect();
        let now = Instant::now();
        for c in hub.video_clients() {
            // Checked and sent under the viewer's lock, so once `Client::move_to` takes
            // it to another tier, nothing more from this one reaches it.
            let mut q = c.inflight.lock();
            if c.tier.load(Ordering::Relaxed) != tier {
                continue;
            }
            if !c.started.load(Ordering::Relaxed) {
                if !all_key {
                    continue;
                }
                c.started.store(true, Ordering::Relaxed);
                q.clear();
            }
            if q.len() > 600 {
                log::warn!("client {} hopelessly behind; dropping", c.id);
                drop(q);
                c.send(Out::Text("{\"t\":\"close\"}".into()));
                continue;
            }
            q.push_back(Inflight {
                id: frame_id,
                sent: now,
                bytes: total,
            });
            for m in &msgs {
                c.send(Out::Bin(m.clone()));
            }
        }
        hub.stats.send_lat_us.fetch_add(
            job.captured_at.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        if job.marker_ms >= 0 {
            log::info!(
                "marker sent age {} ms (encode {} us)",
                marker_age(job.marker_ms),
                enc_us
            );
        }
    }
}

/// Debug: decode the bench loadgen marker at (300,150) and log its age.
/// CPU time this thread has used, in microseconds.
#[cfg(unix)]
fn thread_cpu_us() -> i64 {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(|t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
        .unwrap_or(0)
}

#[cfg(windows)]
fn thread_cpu_us() -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut c, mut e, mut k, mut u) = Default::default();
    let ft = |f: FILETIME| ((f.dwHighDateTime as i64) << 32 | f.dwLowDateTime as i64) / 10;
    match unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } {
        Ok(()) => ft(k) + ft(u),
        Err(_) => 0,
    }
}

fn marker_probe(buf: &[u8], stride: usize, what: &str) -> i64 {
    let mut v: u64 = 0;
    let (mx, my) = std::env::var("VOXL_DEBUG_MARKER")
        .ok()
        .and_then(|s| {
            s.split_once(',')
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        })
        .unwrap_or((300, 150));
    for b in 0..40 {
        let x = mx + (b % 20) * 24 + 12;
        let y = my + (b / 20) * 24 + 12;
        let i = y * stride + x * 4;
        if buf[i + 2] > 128 {
            v |= 1 << b;
        }
    }
    let ms = (v & 0xffffff) as i64;
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    log::info!("marker {} frame {} age {} ms", what, v >> 24, now - ms);
    ms
}

fn marker_age(ms: i64) -> i64 {
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    now - ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VideoConfig {
        VideoConfig {
            display: None,
            fps: 60,
            preset: "ultrafast".into(),
            threads: 1,
            convert_threads: 1,
            crf: 24.0,
            max_kbps: 30000,
            paintover_crf: 16.0,
            paintover_burst: 3,
            paintover_delay_ms: 150,
            min_kbps: 2000,
            coalesce: Duration::ZERO,
            stripes: 1,
            adaptive_threads: false,
            slow_tier: true,
        }
    }

    /// A tier without an encoder: these tests only move viewers between tiers.
    fn tier(rc: RateCtl) -> Tier {
        let (job_tx, _) = mpsc::sync_channel(1);
        Tier {
            job_tx,
            encode: std::thread::spawn(|| {}),
            rc,
            refiner: Refiner::new(1, 1),
            paintover_left: 0,
            last_motion: Instant::now(),
            band: None,
            dirty: vec![0],
            active: false,
        }
    }

    /// A viewer that has its keyframe, with this much queueing and throughput.
    fn viewer(hub: &Hub, queue_ms: f64, kbps: f64) -> Arc<Client> {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let c = hub.add_client(tx);
        c.started.store(true, Ordering::Relaxed);
        set(&c, queue_ms, kbps);
        c
    }

    fn set(c: &Client, queue_ms: f64, kbps: f64) {
        c.delay.lock().queue_ms = queue_ms;
        c.delivered.lock().rate_bps = kbps * 1000.0;
    }

    /// Runs `n` evaluations (100 ms apart, as far as `Tiering` can tell).
    fn evaluate(t: &mut Tiering, clients: &[Arc<Client>], tiers: &mut [Option<Tier>; 2], n: u32) {
        let cfg = cfg();
        for _ in 0..n {
            t.last_eval = Instant::now() - Duration::from_secs(1);
            t.update(&cfg, clients, tiers, &tier);
        }
    }

    fn main_tiers() -> [Option<Tier>; 2] {
        [Some(tier(RateCtl::new(30000.0, 2000.0))), None]
    }

    #[test]
    fn moves_a_viewer_that_holds_the_others_back() {
        let hub = Hub::new();
        let slow = viewer(&hub, 120.0, 3000.0);
        let fast = viewer(&hub, 0.0, 3000.0);
        let clients = [slow.clone(), fast.clone()];
        let mut tiering = Tiering::new(true);
        let mut tiers = main_tiers();
        // Not before the fast viewer has been watched for a while.
        evaluate(&mut tiering, &clients, &mut tiers, SEEN_MIN - 1);
        assert_eq!(slow.tier.load(Ordering::Relaxed), MAIN);
        evaluate(&mut tiering, &clients, &mut tiers, 1);
        assert_eq!(slow.tier.load(Ordering::Relaxed), SLOW);
        assert_eq!(fast.tier.load(Ordering::Relaxed), MAIN);
        // It starts again with a keyframe, at about what its link carried.
        assert!(slow.needs_keyframe.load(Ordering::Relaxed));
        let slow_rc = &tiers[SLOW as usize].as_ref().unwrap().rc;
        assert!((slow_rc.target - 2550.0).abs() < 1.0);
        // The main stream ramps up again rather than staying at the slow viewer's rate.
        assert!(tiers[MAIN as usize].as_ref().unwrap().rc.slow_start);
    }

    #[test]
    fn seeds_within_a_ceiling_below_the_floor() {
        let rc = RateCtl::seeded(1000.0, 2000.0, 3000.0);
        assert_eq!(rc.target, 1000.0);
    }

    #[test]
    fn keeps_viewers_together_unless_another_keeps_up() {
        let hub = Hub::new();
        // Alone, a congested viewer has nobody to make room for.
        let alone = [viewer(&hub, 120.0, 3000.0)];
        let mut tiering = Tiering::new(true);
        let mut tiers = main_tiers();
        evaluate(&mut tiering, &alone, &mut tiers, 50);
        assert_eq!(alone[0].tier.load(Ordering::Relaxed), MAIN);
        assert!(tiers[SLOW as usize].is_none());
        // Both congested: the link (or the content) is the limit for both.
        let both = [viewer(&hub, 120.0, 3000.0), viewer(&hub, 80.0, 3000.0)];
        evaluate(&mut tiering, &both, &mut tiers, 50);
        assert!(both.iter().all(|c| c.tier.load(Ordering::Relaxed) == MAIN));
        // Turned off, nobody moves.
        let mixed = [viewer(&hub, 120.0, 3000.0), viewer(&hub, 0.0, 3000.0)];
        let mut off = Tiering::new(false);
        evaluate(&mut off, &mixed, &mut tiers, 50);
        assert!(mixed.iter().all(|c| c.tier.load(Ordering::Relaxed) == MAIN));
        assert!(tiers[SLOW as usize].is_none());
    }

    #[test]
    fn rejoins_once_its_link_keeps_pace() {
        let hub = Hub::new();
        let slow = viewer(&hub, 120.0, 3000.0);
        let fast = viewer(&hub, 0.0, 7000.0);
        let clients = [slow.clone(), fast.clone()];
        let mut tiering = Tiering::new(true);
        let mut tiers = main_tiers();
        evaluate(&mut tiering, &clients, &mut tiers, SEEN_MIN);
        assert_eq!(slow.tier.load(Ordering::Relaxed), SLOW);
        slow.started.store(true, Ordering::Relaxed);
        // No congestion, but it gets far less than the main stream carries.
        set(&slow, 0.0, 3000.0);
        let calm_since = Instant::now() - REJOIN_AFTER - Duration::from_secs(1);
        tiering.viewers.get_mut(&slow.id).unwrap().congested_at = Some(calm_since);
        evaluate(&mut tiering, &clients, &mut tiers, 1);
        assert_eq!(slow.tier.load(Ordering::Relaxed), SLOW);
        // Keeping pace, but not for long enough yet.
        set(&slow, 0.0, 6500.0);
        tiering.viewers.get_mut(&slow.id).unwrap().congested_at = Some(Instant::now());
        evaluate(&mut tiering, &clients, &mut tiers, 1);
        assert_eq!(slow.tier.load(Ordering::Relaxed), SLOW);
        tiering.viewers.get_mut(&slow.id).unwrap().congested_at = Some(calm_since);
        evaluate(&mut tiering, &clients, &mut tiers, 1);
        assert_eq!(slow.tier.load(Ordering::Relaxed), MAIN);
        // With nobody left on it, the slow tier closes.
        evaluate(&mut tiering, &clients, &mut tiers, 1);
        assert!(tiers[SLOW as usize].is_none());
    }

    #[test]
    fn slow_viewers_get_the_main_stream_when_the_others_leave() {
        let hub = Hub::new();
        let slow = viewer(&hub, 120.0, 3000.0);
        let fast = viewer(&hub, 0.0, 7000.0);
        let mut tiering = Tiering::new(true);
        let mut tiers = main_tiers();
        evaluate(&mut tiering, &[slow.clone(), fast], &mut tiers, SEEN_MIN);
        assert_eq!(slow.tier.load(Ordering::Relaxed), SLOW);
        slow.started.store(true, Ordering::Relaxed);
        evaluate(&mut tiering, std::slice::from_ref(&slow), &mut tiers, 1);
        assert_eq!(slow.tier.load(Ordering::Relaxed), MAIN);
        assert!(tiers[SLOW as usize].is_none());
        // At the bitrate its own link was found to carry.
        assert!((tiers[MAIN as usize].as_ref().unwrap().rc.target - 2550.0).abs() < 1.0);
    }
}
