//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{Encoder, EncoderConfig, Planes};
use crate::hub::wait_any;
use crate::hub::{Hub, Inflight, Out};
use bytes::Bytes;
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
    pub qp: f32,
    pub max_kbps: u32,
    pub paintover_qp: f32,
    pub paintover_burst: u32,
    pub paintover_delay_ms: u64,
    pub min_kbps: u32,
    pub coalesce: Duration,
    pub stripes: u32,
    pub adaptive_threads: bool,
}

struct Job {
    buf: Box<I420>,
    idx: usize,
    force_idr: bool,
    paintover: bool,
    /// Last pass of a paint-over burst.
    paintover_final: bool,
    /// Rows that changed in this capture.
    band: Option<Band>,
    /// Macroblocks that changed in this capture (None: analyse everything). They are
    /// always encoded.
    mb_dirty: Option<Arc<Vec<u8>>>,
    /// Macroblocks to sharpen (paint-over), encoded only where the frame's QP makes them
    /// sharper; the encoder hands back any it leaves out (`Hub::repaint`).
    refine: Option<Arc<Vec<u8>>>,
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

/// Part of the screen a buffer needs converting: rows y0..y1, columns x0..x1.
#[derive(Clone, Copy)]
struct Area {
    y0: usize,
    y1: usize,
    x0: usize,
    x1: usize,
}

impl Area {
    fn full(w: usize, h: usize) -> Self {
        Area {
            y0: 0,
            y1: h,
            x0: 0,
            x1: w,
        }
    }

    fn grow(a: Option<Area>, b: Area) -> Area {
        match a {
            None => b,
            Some(a) => Area {
                y0: a.y0.min(b.y0),
                y1: a.y1.max(b.y1),
                x0: a.x0.min(b.x0),
                x1: a.x1.max(b.x1),
            },
        }
    }
}

/// Columns spanned by the changed macroblocks of `map` (`cols` per row).
fn dirty_cols(map: &[u8], cols: usize, width: usize) -> Option<(usize, usize)> {
    let (mut c0, mut c1) = (cols, 0);
    for row in map.chunks_exact(cols) {
        if let Some(a) = row.iter().position(|&d| d != 0) {
            c0 = c0.min(a);
            c1 = c1.max(cols - row.iter().rev().position(|&d| d != 0).unwrap());
        }
    }
    (c1 > c0).then(|| (c0 * 16, (c1 * 16).min(width)))
}

/// One capture/encode setup for the current screen size.
struct Pipeline {
    cap: Capturer,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<(usize, Box<I420>)>,
    encode: std::thread::JoinHandle<()>,
}

/// Runs the pipeline. With a `Screen` (dynamic resolution), a client's size request
/// ends the capture loop: the encoder drains, the screen is resized and the pipeline
/// is rebuilt at the new size, and clients get the new geometry and a keyframe.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<crate::screen::Screen>,
) -> Result<(), String> {
    if let Some(s) = &screen {
        *hub.resize_max.lock() = Some(s.max());
    }
    let mut p = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let Pipeline {
                mut cap,
                job_tx,
                ret_rx,
                encode,
            } = p;
            if let Err(e) = capture_loop(&mut cap, cfg.clone(), hub.clone(), job_tx, ret_rx) {
                #[cfg(target_os = "linux")]
                let resized = cap.geometry_changed();
                #[cfg(not(target_os = "linux"))]
                let resized = false;
                if resized {
                    log::info!("capture geometry changed: {e}");
                } else {
                    log::error!("capture loop ended: {e}");
                    std::process::exit(1);
                }
            }
            drop(cap);
            let _ = encode.join();
            let req = hub.resize_req.lock().take();
            if let (Some(s), Some((w, h))) = (screen.as_mut(), req) {
                if let Err(e) = s.resize(w, h) {
                    log::warn!("resize to {w}x{h} failed: {e}");
                }
            }
            p = match build(&cfg, &hub) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("video restart failed: {e}");
                    std::process::exit(1);
                }
            };
        })
        .unwrap();
    Ok(())
}

fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<Pipeline, String> {
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

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
    let (ret_tx, ret_rx) = mpsc::channel::<(usize, Box<I420>)>();
    let layout = stripe_layout(h, cfg.stripes);
    let mut stripes = Vec::new();
    for &(y, sh) in &layout {
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
            rc: QpCtl::new(),
        });
    }
    *hub.stripes.lock() = layout;
    let hub2 = hub.clone();
    let (qp, po_qp, max_kbps) = (cfg.qp, cfg.paintover_qp, cfg.max_kbps);
    let encode = std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || encode_loop(stripes, job_rx, ret_tx, hub2, qp, po_qp, max_kbps))
        .unwrap();
    // New geometry (and a keyframe) for clients already connected; no-op at startup.
    hub.restart_clients();
    Ok(Pipeline {
        cap,
        job_tx,
        ret_rx,
        encode,
    })
}

fn flow_ok(hub: &Hub, interval_ms: f64) -> bool {
    for c in hub.video_clients() {
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

    fn update(&mut self, hub: &Hub, clients: &[Arc<crate::hub::Client>]) {
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
            let (_, age) = c.backlog();
            let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
                0 => 40,
                r => r,
            } as f64;
            if q > 30.0 || age as f64 > rtt + 80.0 {
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
        // Never aim above what the link was measured to carry (with room to spare): an
        // uncongested link only says the content didn't need more, and bursts after a
        // scene change would otherwise queue up behind a target the link can't take.
        // Links change, though, and frames held small by an old estimate never get large
        // enough to measure it again: one that holds the rate down is forgotten once
        // stale (rate control goes back to its cautious unmeasured mode and probes). One
        // above --max-kbps holds nothing down, and if that link slows, the frames it
        // allows measure it.
        let capacity = clients
            .iter()
            .filter(|c| c.started.load(Ordering::Relaxed))
            .filter_map(|c| c.capacity_kbps())
            .filter(|&(kbps, fresh)| fresh || kbps * 0.8 >= self.max)
            .map(|(kbps, _)| kbps)
            .fold(f64::MAX, f64::min);
        self.target = self.target.min(capacity * 0.8).max(self.min);
        hub.link_kbps.store(
            if capacity < f64::MAX {
                capacity as u32
            } else {
                0
            },
            Ordering::Relaxed,
        );
        hub.target_kbps.store(self.target as u32, Ordering::Relaxed);
        hub.link_clear.store(clear && !congested, Ordering::Relaxed);
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

fn capture_loop(
    cap: &mut Capturer,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<(usize, Box<I420>)>,
) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let mut pool: Vec<Option<Box<I420>>> = vec![
        Some(Box::new(I420::new(w, h))),
        Some(Box::new(I420::new(w, h))),
    ];
    // What each buffer is missing: the changes since it was last converted.
    let mut pending: [Option<Area>; 2] = [Some(Area::full(w, h)), Some(Area::full(w, h))];
    let mut next_idx = 0usize;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow slight early capture so source jitter doesn't alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
    let mut last_motion = Instant::now();
    let mut paintover_left = 0u32;
    let mut rc = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
    hub.target_kbps.store(rc.target as u32, Ordering::Relaxed);
    let mut had_clients = false;
    let mut full_next = true;
    let mut refiner = Refiner::new(w.div_ceil(16) * h.div_ceil(16), w.div_ceil(16));
    let debug_marker = std::env::var("VOXL_DEBUG_MARKER").is_ok();

    loop {
        if hub.resize_req.lock().is_some() || cap.needs_rebuild() {
            return Ok(());
        }
        cap.pump()?;
        if cap.needs_rebuild() {
            return Ok(());
        }
        note_changes(cap, &hub);
        hub.drain_wake();
        // Sharpening the encoder left out (the rate didn't allow it yet): mark it again,
        // and have a paint-over follow even if nothing moves.
        let repaint = std::mem::take(&mut *hub.repaint.lock());
        if repaint.len() == refiner.pending.len() {
            refiner.mark(&repaint);
            paintover_left = paintover_left.max(1);
            last_motion = Instant::now();
        }
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage
            // stays pending, so the stream still sends what changed. Only the pixels
            // are copied here (the framebuffer is live); the PNG is made off this thread.
            // The capturer takes the damage for exactly these pixels with them, so the
            // screenshot's seq covers what it shows and nothing drawn after it.
            let bgrx = cap.snapshot()?;
            note_changes(cap, &hub);
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
        while let Ok((i, b)) = ret_rx.try_recv() {
            pool[i] = Some(b);
        }
        let clients = hub.video_clients();
        if clients.is_empty() {
            if had_clients {
                rc = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
                hub.target_kbps.store(rc.target as u32, Ordering::Relaxed);
            }
            had_clients = false;
            wait_any(&[cap.fd(), hub.waker.waitable()], 500);
            continue;
        }
        rc.update(&hub, &clients);
        let want_key = clients
            .iter()
            .any(|c| c.needs_keyframe.load(Ordering::Relaxed));
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pending = [Some(Area::full(w, h)), Some(Area::full(w, h))];
            cap.damaged = true;
            full_next = true;
        }
        let paintover_due = paintover_left > 0
            && last_motion.elapsed() >= Duration::from_millis(cfg.paintover_delay_ms);
        if !cap.damaged && !want_key && !paintover_due {
            let timeout = if paintover_left == 0 {
                100
            } else {
                cfg.paintover_delay_ms
                    .saturating_sub(last_motion.elapsed().as_millis() as u64) as i32
                    + 1
            };
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
        if !want_key && !flow_ok(&hub, interval_ms) {
            hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
            wait_any(&[cap.fd(), hub.waker.waitable()], 4);
            continue;
        }
        // Need a free buffer (the encoder may still hold the other).
        if pool[next_idx].is_none() {
            match ret_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((i, b)) => pool[i] = Some(b),
                Err(_) => continue,
            }
            if pool[next_idx].is_none() {
                continue;
            }
        }

        let t0 = Instant::now();
        let origin = cap.damaged_at.unwrap_or(t0);
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mut band = None;
        let mut mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        // Columns to convert: the changed macroblocks' (the screen's without a map).
        let cols = mb_dirty
            .as_ref()
            .and_then(|m| dirty_cols(m, w.div_ceil(16), w))
            .unwrap_or((0, w));
        match &mb_dirty {
            Some(m) => refiner.mark(m),
            None => refiner.mark_all(),
        }
        if hub.resharpen.swap(false, Ordering::Relaxed) {
            refiner.mark_all();
        }
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
        let is_motion = band.is_some();
        if !is_motion && !want_key && !paintover_due {
            // Damage was empty after all.
            continue;
        }
        // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
        let paint_map = if paintover_due && !is_motion && !want_key {
            match refiner.pending_map() {
                Some(m) => Some(m),
                None => {
                    paintover_left = 0;
                    continue;
                }
            }
        } else {
            None
        };
        let idx = next_idx;
        let mut buf = pool[idx].take().unwrap();
        let changed = band.map(|b| Area {
            y0: b.y0 as usize,
            y1: b.y1 as usize,
            x0: cols.0,
            x1: cols.1,
        });
        let todo = match (pending[idx], changed) {
            (p, Some(c)) => Some(Area::grow(p, c)),
            (p, None) => p,
        };
        if let Some(c) = changed {
            pending[1 - idx] = Some(Area::grow(pending[1 - idx], c));
        }
        pending[idx] = None;
        if let Some(t) = todo {
            cap.convert(&mut buf, (t.y0, t.y1), (t.x0, t.x1), cfg.convert_threads);
        }
        let t2 = Instant::now();
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        let mut refine = None;
        let paintover = if is_motion {
            last_motion = Instant::now();
            paintover_left = cfg.paintover_burst;
            // Piggyback sharpening of blocks that have gone still on this frame.
            refine = if want_key { None } else { refiner.due() };
            refine.is_some()
        } else if paintover_due {
            paintover_left -= 1;
            hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        };
        if want_key {
            for c in &clients {
                c.needs_keyframe.store(false, Ordering::Relaxed);
            }
            // A keyframe is squeezed into the rate cap; sharpen all of it once still.
            refiner.mark_all();
        }
        let frame_paintover = paintover && refine.is_none();
        if frame_paintover && paintover_left == 0 {
            refiner.clear_all();
        }
        // A refinement frame encodes what moved plus what is due; everything else stays
        // skipped. What moved and what is due travel apart: a frame that moved is always
        // sent, the sharpening only where it helps.
        let (mb_map, refine_map) = if want_key {
            (None, None)
        } else if frame_paintover {
            let refine = paint_map.map(|(m, b)| {
                band = Some(match band {
                    Some(x) => union(Some(x), b),
                    None => b,
                });
                Arc::new(m)
            });
            let nothing_moved = refine.as_ref().map(|m| Arc::new(vec![0; m.len()]));
            (nothing_moved, refine)
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(match band {
                Some(b) => union(Some(b), rband),
                None => rband,
            });
            hub.stats.refines.fetch_add(1, Ordering::Relaxed);
            // Without a change map everything is encoded anyway.
            let moved = mb_dirty.take().map(Arc::new);
            let refine = moved.as_ref().map(|_| Arc::new(rmap));
            (moved, refine)
        } else {
            (mb_dirty.take().map(Arc::new), None)
        };
        last_capture = t0;
        next_idx = 1 - idx;
        if job_tx
            .send(Job {
                buf,
                idx,
                force_idr: want_key,
                paintover,
                paintover_final: frame_paintover && paintover_left == 0,
                mb_dirty: mb_map,
                refine: refine_map,
                band,
                captured_at: origin,
                marker_ms,
            })
            .is_err()
        {
            return Err("encoder gone".into());
        }
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
    rc: QpCtl,
}

/// Frame size relative to QP 22 at QPs from 16 to 51, measured on desktop text
/// (keyframes and motion alike). It falls much slower than the textbook halving every
/// 6 QP: sharp edges keep their coefficients until the quantizer is very coarse.
const SIZE_AT_QP: [(f64, f64); 7] = [
    (16.0, 1.3),
    (22.0, 1.0),
    (28.0, 0.77),
    (34.0, 0.59),
    (40.0, 0.44),
    (45.0, 0.30),
    (51.0, 0.14),
];

/// Size of a frame at `qp` relative to QP 22 (log-linear between measurements).
fn size_at(qp: f64) -> f64 {
    let t = &SIZE_AT_QP;
    let i = t
        .iter()
        .rposition(|&(q, _)| q <= qp)
        .unwrap_or(0)
        .min(t.len() - 2);
    let ((q0, s0), (q1, s1)) = (t[i], t[i + 1]);
    let f = (qp - q0) / (q1 - q0);
    (s0.ln() * (1.0 - f) + s1.ln() * f).exp()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Motion,
    /// Paint-over: re-encoding blocks that went still at sharper quality.
    Refine,
    Key,
}

/// Bytes per encoded macroblock at QP 22 for one kind of frame.
struct Cost {
    avg: f64,
    /// Recent maximum, decaying: content can turn from cheap to expensive in one frame.
    peak: f64,
    /// Average of frames at about the QP asked for (0 until measured): what the
    /// content costs when not repairing a coarser reference.
    steady: f64,
}

/// Frame QPs under the bitrate cap. x264 runs at constant QP (its CRF and VBV need a
/// lowres lookahead that costs as much as the encode itself), so the VBV lives here:
/// frames draw on a buffer refilled at the cap, and each frame gets the QP it asks for,
/// raised until its predicted size fits. Sizes are predicted from the macroblocks being
/// encoded and the bytes per macroblock of recent frames of the same kind, scaled
/// across QPs by `size_at`.
struct QpCtl {
    /// Bits in the buffer; negative after an overshoot, until the cap repays it.
    fill: f64,
    /// Buffer size (bits).
    size: f64,
    last: Instant,
    cost: [Cost; 3],
    /// QP of the last motion frame (or keyframe): the quality of the reference.
    motion_qp: i32,
    /// Size of the last motion frame (bytes).
    motion_bytes: f64,
    /// Bytes a motion frame costs per QP it comes down (repairing its reference), 0
    /// until measured.
    repair: f64,
    /// Plenty of room (`is_roomy`) when the last budget was set.
    roomy: bool,
    /// QP added to what frames ask for while the stream is over --max-kbps (`MaxRate`).
    bias: f64,
    /// QP each macroblock of the stripe was last coded at, row-major (51 until known).
    coded: Vec<u8>,
    /// QP of the frame x264 refers to. x264 only skips a block marked unchanged outright
    /// when the reference is the frame just before and was coded at that QP or coarser
    /// (`effective_qp`, the frame's QP for every block): a sharper frame re-analyses
    /// every block and re-codes those coded coarser than it (encoder/analyse.c).
    ref_qp: i32,
    /// The frame before was dropped (`invalidate_last`), so the next one refers two
    /// frames back and gets no outright skips either.
    hints_lost: bool,
    /// Motion QP doesn't step below this (since `floor_at`): a step down to just under
    /// it overshot. Re-coding what's still coarse can cost far more than the moving
    /// blocks predict, so the step isn't retried every frame, only now and then.
    floor: i32,
    floor_at: Instant,
    /// Bits sharpening may spend (`sharpen_budget`), apart from the buffer motion frames
    /// draw on: sharpening still pixels never starves what moves.
    sharpen_fill: f64,
    sharpen_last: Instant,
}

/// How long a step down that overshot isn't tried again (`QpCtl::floor`).
const FLOOR_HOLD: Duration = Duration::from_secs(2);

/// Share of the link sharpening refills at (`QpCtl::sharpen_budget`): the rest stays
/// with what moves. After a detailed window is crushed on a 10 Mbps link, 10% of the
/// link sharpens it within seconds while a small animation keeps its frame rate and
/// latency close to main's (bench/stale.sh); 20% and up made the animation choppy.
const SHARPEN_SHARE: f64 = 0.1;

/// Most a frame may add for sharpening, in frame intervals of the link: the frames behind
/// it wait at most this long. Fewer means smaller steps, which detailed content (a photo)
/// can't take one QP at a time: at 2, such a window stayed at QP 37 on 10 Mbps.
const SHARPEN_FRAMES: f64 = 4.0;

/// Unchanged blocks counted by the QP they were coded at (`QpCtl::coarse`).
type Coarse = [u32; 52];

/// Fold a measured cost per block (at QP 22) into `c`; `steady`: the frame was at about
/// the QP it asked for.
fn learn_cost(c: &mut Cost, per_mb: f64, steady: bool) {
    c.avg = c.avg * 0.6 + per_mb * 0.4;
    c.peak = per_mb.max(c.peak * 0.85);
    if steady {
        c.steady = if c.steady > 0.0 {
            c.steady * 0.8 + per_mb * 0.2
        } else {
            per_mb
        };
    }
}

impl QpCtl {
    fn new() -> Self {
        // Priors until frames are measured, on the dense side for desktop text.
        let c = |b: f64| Cost {
            avg: b,
            peak: 0.0,
            steady: 0.0,
        };
        QpCtl {
            fill: 0.0,
            size: 0.0,
            last: Instant::now(),
            // Detail (Refine) is per block of detail added (`QpCtl::detail`).
            cost: [c(20.0), c(40.0), c(30.0)],
            motion_qp: crate::encoder::MIN_QP,
            motion_bytes: 0.0,
            repair: 0.0,
            roomy: false,
            bias: 0.0,
            coded: Vec::new(),
            ref_qp: 51,
            hints_lost: false,
            floor: 0,
            floor_at: Instant::now(),
            sharpen_fill: 0.0,
            sharpen_last: Instant::now(),
        }
    }

    /// Bytes sharpening may add to a frame now, and at most: its own budget, refilled at
    /// SHARPEN_SHARE of the link (of half the cap until the link is measured) and holding
    /// at most SHARPEN_FRAMES frame intervals of it, so a frame that sharpens delays the
    /// ones behind it by that much at worst.
    fn sharpen_budget(&mut self, cap_kbps: u32, link_kbps: u32, fps: u32) -> (f64, f64) {
        let rate = if link_kbps > 0 {
            link_kbps as f64 * 1000.0
        } else {
            cap_kbps as f64 * 500.0
        };
        let most = rate * SHARPEN_FRAMES / fps.max(1) as f64;
        let refill = rate * SHARPEN_SHARE * self.sharpen_last.elapsed().as_secs_f64();
        self.sharpen_fill = (self.sharpen_fill + refill).min(most);
        self.sharpen_last = Instant::now();
        (self.sharpen_fill / 8.0, most / 8.0)
    }

    /// The unchanged blocks of a frame (`unchanged[i] != 0`), counted by the QP they were
    /// coded at; none when every block is encoded anyway.
    fn coarse(&self, unchanged: Option<&[u8]>) -> Coarse {
        let mut h = [0; 52];
        if let Some(u) = unchanged {
            for (&u, &c) in u.iter().zip(&self.coded) {
                if u != 0 {
                    h[c.min(51) as usize] += 1;
                }
            }
        }
        h
    }

    /// The coarsest QP among the blocks `of` marks (`of[i] != 0`): no frame at that QP
    /// or coarser makes them any sharper.
    fn coarsest(&self, of: &[u8]) -> i32 {
        of.iter()
            .zip(&self.coded)
            .filter(|(&o, _)| o != 0)
            .map(|(_, &c)| c as i32)
            .max()
            .unwrap_or(0)
    }

    /// Record a kept frame at `qp`: what it encoded (every block for a keyframe or
    /// without a map, else those `unchanged` doesn't mark) is now at `qp`, and so are
    /// the unchanged blocks it re-coded. `rescan`: it referred two frames back (a
    /// re-encode), so it re-coded like a frame sharper than its reference.
    fn kept(&mut self, qp: i32, key: bool, unchanged: Option<&[u8]>, rescan: bool) {
        let q = qp.clamp(0, 51) as u8;
        let recode = rescan || qp < self.ref_qp || self.hints_lost;
        match unchanged {
            Some(u) if !key => {
                for (c, &u) in self.coded.iter_mut().zip(u) {
                    if u == 0 || (recode && *c > q) {
                        *c = q;
                    }
                }
            }
            _ => {
                self.coded.fill(q);
                self.floor = 0;
            }
        }
        self.ref_qp = qp;
        self.hints_lost = false;
    }

    /// Bytes the next frame may take: what the buffer holds after refilling at
    /// `cap_kbps`, the buffer being `buf_frames` frames at the cap. A `fresh` buffer
    /// (a keyframe for a new viewer) starts full. On a `clear` link a frame always gets
    /// a quarter of the buffer, even in debt after an overshoot: crushing frames to
    /// repay it there only lowers the quality every later frame starts from. Where
    /// acks show queueing, the debt holds the rate down.
    #[allow(clippy::too_many_arguments)]
    fn budget(
        &mut self,
        cap_kbps: u32,
        buf_frames: f64,
        fps: u32,
        fresh: bool,
        clear: bool,
        link_kbps: u32,
        roomy: bool,
    ) -> f64 {
        let rate = cap_kbps as f64 * 1000.0;
        self.size = rate / fps.max(1) as f64 * buf_frames;
        self.roomy = roomy;
        // It refills at the cap (a VBV's maximum rate), but no faster than the link was
        // measured to carry (after a scene change, frames would otherwise queue up on
        // a slow link; until a measurement, at half the cap).
        let refill = if link_kbps > 0 {
            rate.min(link_kbps as f64 * 900.0)
        } else {
            rate * 0.5
        };
        self.fill = if fresh {
            self.size
        } else {
            (self.fill + refill * self.last.elapsed().as_secs_f64()).min(self.size)
        };
        self.last = Instant::now();
        let fill = if clear {
            self.fill.max(self.size * 0.25)
        } else {
            self.fill
        };
        // Aim below it: predictions miss.
        (fill * 0.85 / 8.0).max(1.0)
    }

    /// Plenty of room: a link measured at more than twice the cap, and a `budget` four
    /// times what steady motion at `want` costs (a fast link, not just a low cap).
    fn is_roomy(&self, want: f64, mbs: usize, budget: f64, cap_kbps: u32, link_kbps: u32) -> bool {
        let steady = self.cost[Kind::Motion as usize].steady;
        link_kbps as f64 >= cap_kbps as f64 * 2.0
            && steady > 0.0
            && mbs.max(1) as f64 * steady * size_at(want) * 4.0 <= budget
    }

    /// Detail a frame at `qp` adds to blocks already coded: the ones it sharpens
    /// (`refine`) and, below the reference's QP or after a dropped frame, the unchanged
    /// ones it re-codes (`coarse`), both counted by the QP they were coded at. Bringing
    /// a block from QP `c` to `qp` only adds the detail in between, so it counts as
    /// `size_at(qp) - size_at(c)` of a block coded afresh: a step of one QP over a whole
    /// window is cheap, going from 51 to 16 isn't.
    fn detail(&self, qp: i32, refine: &Coarse, coarse: &Coarse) -> f64 {
        let at = size_at(qp as f64);
        let recode = qp < self.ref_qp || self.hints_lost;
        ((qp.clamp(0, 51) + 1) as usize..52)
            .map(|c| {
                let n = refine[c] + if recode { coarse[c] } else { 0 };
                n as f64 * (at - size_at(c as f64)).max(0.0)
            })
            .sum()
    }

    /// Predicted bytes of a frame at `qp`: `moved` blocks of new content, at what
    /// `kind` costs per block (a keyframe its own, anything else motion's), plus the
    /// detail it adds to blocks already coded (`detail`), at what detail costs.
    fn predict(&self, kind: Kind, qp: i32, moved: usize, refine: &Coarse, coarse: &Coarse) -> f64 {
        let fresh = if kind == Kind::Key {
            Kind::Key
        } else {
            Kind::Motion
        };
        let c = &self.cost[fresh as usize];
        let d = &self.cost[Kind::Refine as usize];
        moved as f64 * c.avg.max(c.peak) * size_at(qp as f64)
            + self.detail(qp, refine, coarse) * d.avg.max(d.peak)
    }

    /// QP for a frame of `kind` that wants `want`, with `moved` blocks of new content
    /// and the `refine` blocks to sharpen: the first from `want` up whose predicted size
    /// fits `budget` (51 if none does), counting what a sharper frame re-codes
    /// (`predict`), so a step down that would re-code a crushed window waits for room.
    /// Motion QP rises to that at once but comes down in steps: a frame sharper than
    /// its reference also re-encodes the detail the reference lost, at a cost per QP
    /// learned from earlier steps, so it comes down as far as that cost fits the
    /// budget (as a VBV lets the quantizer down).
    fn qp(
        &self,
        kind: Kind,
        want: f64,
        moved: usize,
        refine: &Coarse,
        budget: f64,
        coarse: &Coarse,
    ) -> i32 {
        let want = ((want + self.bias).round() as i32).clamp(crate::encoder::MIN_QP, 51);
        let fit = (want..=51)
            .find(|&q| self.predict(kind, q, moved, refine, coarse) <= budget)
            .unwrap_or(51);
        if kind != Kind::Motion || fit >= self.motion_qp {
            return fit;
        }
        // Several steps only with plenty of room (a fast link recovering from a scene
        // change): elsewhere the cost of a step grows faster as QP comes down than the
        // steps before can tell. One at a time while frames leave half the budget
        // unused.
        let used = self.motion_bytes / budget;
        let step = if self.fill < self.size * 0.5 || used >= 0.5 {
            0
        } else if self.roomy && self.repair > 0.0 {
            ((budget * 0.85 / self.repair) as i32).clamp(1, 6)
        } else {
            1
        };
        let floor = if self.floor_at.elapsed() < FLOOR_HOLD {
            self.floor
        } else {
            0
        };
        (self.motion_qp - step).max(fit).max(floor)
    }

    /// Learn from an encoded frame (sent or not) that wanted QP `want`, with `moved`
    /// blocks of new content and the `refine` and `coarse` blocks `predict` counted.
    #[allow(clippy::too_many_arguments)]
    fn learn(
        &mut self,
        kind: Kind,
        qp: i32,
        want: f64,
        moved: usize,
        refine: &Coarse,
        coarse: &Coarse,
        bytes: usize,
        slices: u32,
    ) {
        // Slice headers and the like cost the same whatever changed.
        let payload = bytes.saturating_sub(8 * slices as usize) as f64;
        let fresh = if kind == Kind::Key {
            Kind::Key
        } else {
            Kind::Motion
        };
        let detail = self.detail(qp, refine, coarse);
        let new_content = moved as f64 * size_at(qp as f64);
        // A motion frame that came down in QP was partly a repair (see `qp`).
        let stepped = kind == Kind::Motion && self.learn_repair(qp, bytes);
        if detail > 0.0 && detail >= new_content * 0.25 {
            // Mostly detail added to blocks already coded: what detail costs, after what
            // the new content is expected to take.
            let c = &self.cost[fresh as usize];
            let rest = (payload - new_content * c.avg).max(payload * 0.25);
            learn_cost(&mut self.cost[Kind::Refine as usize], rest / detail, false);
        } else if !stepped && moved > 0 {
            // Only frames at (about) the quality asked for: costs at coarse QPs don't
            // carry over to fine ones.
            let steady = qp as f64 <= want.round() + 2.0;
            learn_cost(
                &mut self.cost[fresh as usize],
                payload / new_content,
                steady,
            );
        }
        self.note(kind, qp, bytes);
    }

    /// Learn the cost of coming down in QP from a motion frame at `qp` that did
    /// (whether or not it was sent); false if it didn't come down.
    fn learn_repair(&mut self, qp: i32, bytes: usize) -> bool {
        let down = self.motion_qp - qp;
        if down <= 0 {
            return false;
        }
        let per_qp = bytes as f64 / down as f64;
        self.repair = if self.repair > 0.0 {
            self.repair * 0.5 + per_qp * 0.5
        } else {
            per_qp
        };
        true
    }

    /// Remember the quality and size of the latest reference frame.
    fn note(&mut self, kind: Kind, qp: i32, bytes: usize) {
        if kind != Kind::Refine {
            // A keyframe is the new reference for everything.
            self.motion_qp = qp;
            self.motion_bytes = bytes as f64;
        }
    }

    /// Charge a frame that sharpened: its own budget first, the rest to the buffer.
    fn spend_sharpened(&mut self, bytes: usize) {
        let bits = bytes as f64 * 8.0;
        let own = bits.min(self.sharpen_fill.max(0.0));
        self.sharpen_fill -= own;
        self.fill -= bits - own;
    }

    /// Charge a sent frame to the buffer.
    fn spend(&mut self, bytes: usize, fresh: bool) {
        // A new viewer's keyframe had a buffer of its own; the stream after it starts
        // from a full one (clamped to its size by the next `budget`), as under x264's
        // VBV, rather than crushing the first frames a viewer sees.
        self.fill = if fresh {
            self.size
        } else {
            self.fill - bytes as f64 * 8.0
        };
    }
}

/// Bursts above --max-kbps are fine; a sustained rate isn't: past it, frames are asked
/// for a coarser QP, a step every half second, until it holds. Over the whole stream,
/// so one busy stripe may use what idle ones leave.
struct MaxRate {
    max_bps: f64,
    /// Bits sent since `window`.
    sent: f64,
    window: Instant,
    bias: f64,
}

impl MaxRate {
    fn new(max_kbps: u32) -> Self {
        MaxRate {
            max_bps: max_kbps as f64 * 1000.0,
            sent: 0.0,
            window: Instant::now(),
            bias: 0.0,
        }
    }

    /// QP to add to what frames ask for.
    fn bias(&mut self) -> f64 {
        let el = self.window.elapsed().as_secs_f64();
        if el >= 0.5 {
            let avg = self.sent / el;
            if avg > self.max_bps {
                self.bias = (self.bias + 1.0).min(20.0);
            } else if avg < self.max_bps * 0.85 {
                self.bias = (self.bias - 1.0).max(0.0);
            }
            self.sent = 0.0;
            self.window = Instant::now();
        }
        self.bias
    }

    fn sent(&mut self, bytes: usize) {
        self.sent += bytes as f64 * 8.0;
    }
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
    /// QP the frame asks for (motion frames go lower when little changed).
    qp: f32,
    /// QP sharpening asks for (--paintover-qp).
    po_qp: f32,
    cap: u32,
    /// `cap` is twice the stripe's share, as for any job that sharpens.
    doubled: bool,
    /// The link shows no queueing (`Hub::link_clear`).
    clear: bool,
    /// This stripe's share of the link's measured capacity (kbit/s), 0 if unknown.
    link_kbps: u32,
    /// QP the stream adds to what frames ask for (`MaxRate`).
    bias: f64,
    mb_dirty: Option<Arc<Vec<u8>>>,
    /// Macroblocks to sharpen, where the frame's QP allows it (`Job::refine`).
    refine: Option<Arc<Vec<u8>>>,
    mb_cols: usize,
}

/// Stripe index, its output (keyframe or not) if it produced any, the QP used, a frame
/// to send before the output (standing in for an overshoot that was re-encoded), and
/// whether sharpening the job asked for was left out, to be marked again.
type StripeResult = (usize, Option<(bool, Vec<u8>)>, i32, Option<Vec<u8>>, bool);

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
    let mbs_total = s.h.div_ceil(16) * s.cfg.width.div_ceil(16) as usize;
    s.rc.coded = vec![51; mbs_total];
    while let Ok(job) = rx.recv() {
        let buf = unsafe { &*job.buf.0 };
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
        // This stripe's rows of a screen-wide macroblock map.
        let rows_of = |d: &Arc<Vec<u8>>| {
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + s.h.div_ceil(16) * job.mb_cols].to_vec()
        };
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let moved: Option<Vec<u8>> = job
            .mb_dirty
            .as_ref()
            .map(|d| rows_of(d).iter().map(|&x| (x == 0) as u8).collect());
        // Blocks to sharpen that didn't move (those that did are encoded anyway).
        let sharpen: Option<Vec<u8>> = match (&job.refine, &moved) {
            (Some(r), Some(m)) => Some(
                rows_of(r)
                    .iter()
                    .zip(m)
                    .map(|(&r, &u)| (r != 0 && u != 0) as u8)
                    .collect::<Vec<u8>>(),
            ),
            _ => None,
        }
        .filter(|v| v.iter().any(|&x| x != 0));
        let moved_mbs = match &moved {
            Some(m) if !job.force_idr => m.iter().filter(|&&c| c == 0).count(),
            _ => mbs_total,
        };
        // Small updates (typing, a spinner) are cheap: encode them sharper, as CRF
        // would, by up to 6 QP for a sliver of the screen.
        let motion_want = job.qp as f64
            + (moved_mbs.max(1) as f64 * 4.0 / mbs_total as f64)
                .log2()
                .clamp(-6.0, 0.0);
        // A keyframe is what a new viewer sees first; don't starve it to the steady-state
        // frame budget (up to ~8 frames' worth at 4x the cap). Everything after refers
        // back to it: 3 QP sharper, as x264's I/P ratio.
        let (cap, buf_frames) = if job.force_idr {
            (job.cap.saturating_mul(4).max(16_000), 8.0)
        } else {
            (job.cap, 1.25)
        };
        let fresh = job.force_idr;
        // What a frame may take with the buffer full.
        let nominal = cap as f64 * 1000.0 / s.cfg.fps.max(1) as f64 * buf_frames * 0.85 / 8.0;
        let roomy =
            s.rc.is_roomy(motion_want, moved_mbs, nominal, cap, job.link_kbps);
        let budget = s.rc.budget(
            cap,
            buf_frames,
            s.cfg.fps,
            fresh,
            job.clear,
            job.link_kbps,
            roomy,
        );
        s.rc.bias = job.bias;
        // Sharpening re-codes everything still coarse in one frame (x264 refers to the
        // frame's QP for every block), so after a scene change crushed to the budget it
        // never fits a frame's share. It gets its own budget on top (`sharpen_budget`),
        // so it goes in steps the link carries beside what moves.
        let base_cap = if job.doubled { cap / 2 } else { cap };
        let (sharpen_now, sharpen_most) = s.rc.sharpen_budget(base_cap, job.link_kbps, s.cfg.fps);
        let sharpen_budget = budget + sharpen_now;
        // What a frame that sharpens could take at best: a whole frame's share and a full
        // sharpening budget.
        let sharpen_allowance = nominal + sharpen_most;
        // Sharpening only goes in where the frame's QP (counting the blocks a sharper
        // frame re-codes) is finer than what those blocks have; otherwise it's left out
        // and handed back, if a full buffer could ever afford it.
        let sharpen_at = |rc: &QpCtl, budget: f64| {
            let sh = sharpen.as_deref()?;
            let unchanged: Vec<u8> = moved
                .as_deref()?
                .iter()
                .zip(sh)
                .map(|(&u, &x)| (u != 0 && x == 0) as u8)
                .collect();
            let coarse = rc.coarse(Some(&unchanged));
            let refine = rc.coarse(Some(sh));
            let q = rc.qp(
                Kind::Refine,
                job.po_qp as f64,
                moved_mbs,
                &refine,
                budget,
                &coarse,
            );
            (q < rc.coarsest(sh)).then_some((q, unchanged, refine, coarse))
        };
        let worth_retry = |rc: &QpCtl| sharpen_at(rc, sharpen_allowance).is_some();
        let with_sharpen = if job.force_idr {
            None
        } else {
            sharpen_at(&s.rc, sharpen_budget)
        };
        let mut left_out = sharpen.is_some() && with_sharpen.is_none() && worth_retry(&s.rc);
        let none: Coarse = [0; 52];
        // The frame: its kind and wanted QP, blocks of new content, blocks it sharpens and
        // unchanged blocks (each by coded QP), the map x264 gets, and its QP.
        let (kind, want, new_mbs, refine, coarse, mut info, mut qp) = if job.force_idr {
            let want = job.qp as f64 - 3.0;
            let q = s.rc.qp(Kind::Key, want, mbs_total, &none, budget, &none);
            (Kind::Key, want, mbs_total, none, none, moved.clone(), q)
        } else if let Some((q, unchanged, refine, coarse)) = with_sharpen {
            let want = job.po_qp as f64;
            (
                Kind::Refine,
                want,
                moved_mbs,
                refine,
                coarse,
                Some(unchanged),
                q,
            )
        } else {
            let coarse = s.rc.coarse(moved.as_deref());
            let q =
                s.rc.qp(Kind::Motion, motion_want, moved_mbs, &none, budget, &coarse);
            let want = motion_want;
            (
                Kind::Motion,
                want,
                moved_mbs,
                none,
                coarse,
                moved.clone(),
                q,
            )
        };
        // Blocks the frame encodes (for the thread governor's cost model and logs).
        let mbs = new_mbs + refine.iter().sum::<u32>() as usize;
        let budget = if kind == Kind::Refine {
            sharpen_budget
        } else {
            budget
        };
        if kind == Kind::Motion && moved_mbs == 0 {
            // Nothing in the stripe moved and its sharpening has to wait: no frame.
            if done.send((i, None, qp, None, left_out)).is_err() {
                return;
            }
            continue;
        }
        let mut filler = None;
        // A paint-over replaced by its stand-in: its encode isn't what viewers get.
        let mut dropped = false;
        // The map x264 gets (it reads it during the encode): kept to record what was coded.
        let mut coded_map = info.clone();
        // A re-encode refers two frames back, so x264 re-codes like a sharper frame.
        let mut rescan = false;
        let te = Instant::now();
        let mut r = s.enc.encode(
            &planes,
            job.ts,
            job.force_idr,
            qp,
            info.as_deref_mut(),
            &mut out,
        );
        if let Ok(key) = r {
            // A reopened encoder (adaptive threads) starts with an IDR of its own.
            let kind = if key { Kind::Key } else { kind };
            let bytes = out.len() - HDR;
            if debug {
                log::info!("stripe y{} qp {qp} (wants {want:.1}) {mbs} mbs {bytes} bytes (budget {budget:.0})", s.y);
            }
            // The content outran the model (a scene change: nothing on screen predicts
            // it, or a frame repairing a coarser reference) and this frame would hold the
            // link well past its turn. Have x264 forget it and encode the picture again
            // from the frame before, at the QP its actual size says fits (the same
            // picture, so a good estimate), as a VBV would have; static areas stay as
            // they were. In its place goes a frame with every macroblock skipped, so
            // frame numbers stay continuous.
            // On a link measured far faster than the cap (the cap is policy), a frame a
            // few times its budget costs no time on the wire: only re-encode when far
            // over, and spare the encode time.
            let limit = if job.link_kbps >= cap.saturating_mul(2) {
                (nominal * 3.0).max(budget * 1.5)
            } else {
                budget * 1.5
            };
            // A re-encode costs a whole second encode (tens of milliseconds at 2560x1600)
            // before the frame can go out. On a link measured to carry this frame within
            // a frame interval, sending it is quicker; --max-kbps still holds the average
            // rate (`bias`).
            let wire_ms = bytes as f64 * 8.0 / job.link_kbps.max(1) as f64;
            let fast_link = job.link_kbps > 0 && wire_ms <= 1000.0 / s.cfg.fps.max(1) as f64;
            let over = kind != Kind::Key
                && qp < 51
                && !fast_link
                && bytes as f64 > limit
                && bytes as f64 > nominal * 0.5;
            let fill = over
                .then(|| s.enc.skip_frame(&out[HDR..]))
                .flatten()
                .filter(|_| s.enc.invalidate_last());
            // Without that (streams the stand-in can't handle), a keyframe doesn't
            // depend on the frame either: if one within a few frames' budget comes out
            // much smaller, send the picture as that. The encode loop has the whole
            // screen sharpened afterwards.
            let key_budget = (bytes as f64 * 0.5).min(nominal * 4.0);
            let key_qp = (over && kind == Kind::Motion && fill.is_none())
                .then(|| {
                    s.rc.qp(Kind::Key, want - 3.0, mbs_total, &none, key_budget, &none)
                })
                .filter(|&q| s.rc.predict(Kind::Key, q, mbs_total, &none, &none) <= key_budget);
            if let (Some(f), Kind::Refine, 0) = (&fill, kind, moved_mbs) {
                // A paint-over that overshot is dropped, not re-encoded: the picture on
                // screen is right already, only less sharp, and a coarser re-encode
                // would refine little at the price of a second full encode. The
                // stand-in takes its place, its real cost is learned, and its blocks go
                // back to be sharpened once a QP that helps fits.
                s.rc.learn(
                    kind,
                    qp,
                    want,
                    new_mbs,
                    &refine,
                    &coarse,
                    bytes,
                    s.cfg.threads,
                );
                if debug {
                    log::info!(
                        "stripe y{} paint-over overshot {bytes} bytes at qp {qp}: dropped",
                        s.y
                    );
                }
                out.truncate(HDR);
                out.extend_from_slice(f);
                dropped = true;
                // x264 dropped its reference: the next frame refers two frames back.
                s.rc.hints_lost = true;
                left_out = worth_retry(&s.rc);
            } else if let (Some(f), Kind::Refine) = (&fill, kind) {
                // A frame that moved and carried sharpening that overshot: what moved
                // goes out again on its own (it's never dropped), the sharpening goes
                // back to wait for a QP that fits, and its real cost is learned.
                s.rc.learn(
                    kind,
                    qp,
                    want,
                    new_mbs,
                    &refine,
                    &coarse,
                    bytes,
                    s.cfg.threads,
                );
                // The re-encode refers two frames back: no outright skips.
                s.rc.hints_lost = true;
                let coarse = s.rc.coarse(moved.as_deref());
                let q =
                    s.rc.qp(Kind::Motion, motion_want, moved_mbs, &none, budget, &coarse);
                info = moved.clone();
                coded_map = moved.clone();
                out.truncate(HDR);
                r = s
                    .enc
                    .encode(&planes, job.ts, false, q, info.as_deref_mut(), &mut out);
                if debug {
                    log::info!(
                        "stripe y{} overshot {bytes} bytes at qp {qp} with sharpening: what moved again at qp {q}, {} bytes",
                        s.y,
                        out.len() - HDR
                    );
                }
                qp = q;
                rescan = true;
                s.rc.note(Kind::Motion, qp, out.len() - HDR);
                left_out = worth_retry(&s.rc);
                let mut v = vec![0u8; HDR];
                v.extend_from_slice(f);
                filler = Some(v);
            } else if let Some(f) = fill {
                // A step down in QP that cost too much goes back to where it came
                // from (the cost was the repair, which a coarser QP doesn't have);
                // anything else, or far over, to the QP its size says fits.
                let fits = (qp + 1..=51)
                    .find(|&q| bytes as f64 * size_at(q as f64) / size_at(qp as f64) <= budget)
                    .unwrap_or(51);
                let q = if kind == Kind::Motion && qp < s.rc.motion_qp {
                    if bytes as f64 <= budget * 4.0 {
                        s.rc.motion_qp
                    } else {
                        fits.max(s.rc.motion_qp)
                    }
                } else {
                    fits
                };
                out.truncate(HDR);
                r = s
                    .enc
                    .encode(&planes, job.ts, false, q, info.as_deref_mut(), &mut out);
                if debug {
                    log::info!(
                        "stripe y{} overshot {bytes} bytes at qp {qp}: again at qp {q}, {} bytes",
                        s.y,
                        out.len() - HDR
                    );
                }
                // A one-off: it says nothing about the frames after it, unless it was
                // a step down in QP that cost more than expected; that step isn't tried
                // again for a while.
                if kind == Kind::Motion && s.rc.learn_repair(qp, bytes) {
                    s.rc.floor = qp + 1;
                    s.rc.floor_at = Instant::now();
                }
                qp = q;
                rescan = true;
                s.rc.note(kind, qp, out.len() - HDR);
                let mut v = vec![0u8; HDR];
                v.extend_from_slice(&f);
                filler = Some(v);
            } else if let Some(q) = key_qp {
                out.truncate(HDR);
                qp = q;
                coded_map = None;
                r = s.enc.encode(&planes, job.ts, true, qp, None, &mut out);
                if r.is_ok() {
                    // Learn from the keyframe only: the frame it replaced says nothing
                    // about the next one.
                    s.rc.learn(
                        Kind::Key,
                        qp,
                        want - 3.0,
                        mbs_total,
                        &none,
                        &none,
                        out.len() - HDR,
                        s.cfg.threads,
                    );
                }
                if debug {
                    log::info!(
                        "stripe y{} overshot {bytes} bytes: keyframe at qp {qp}, {} bytes",
                        s.y,
                        out.len() - HDR
                    );
                }
            } else {
                s.rc.learn(
                    kind,
                    qp,
                    want,
                    new_mbs,
                    &refine,
                    &coarse,
                    bytes,
                    s.cfg.threads,
                );
            }
            if kind == Kind::Refine && !dropped && !rescan {
                s.rc.spend_sharpened(out.len() - HDR);
            } else {
                s.rc.spend(out.len() - HDR, fresh);
            }
            if let (Ok(key), false) = (&r, dropped) {
                let key = *key;
                s.enc.count_psnr();
                s.rc.kept(qp, key, coded_map.as_deref(), rescan);
                // Sharpening that went in short of what it asks for goes back for another
                // round, as long as a full allowance could make it sharper still.
                if kind == Kind::Refine && !rescan && (qp as f32) > job.po_qp + 0.5 {
                    left_out = worth_retry(&s.rc);
                }
            }
        }
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
            if let Some(t) = gov.want(
                te.elapsed().as_secs_f64() * 1000.0,
                mbs as f64,
                s.max_threads,
            ) {
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
                        s.cfg.threads = t;
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
        if done.send((i, res, qp, filler, left_out)).is_err() {
            return;
        }
    }
}

fn encode_loop(
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::Sender<(usize, Box<I420>)>,
    hub: Arc<Hub>,
    qp: f32,
    po_qp: f32,
    max_kbps: u32,
) {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes_in.len();
    let debug_stripes = std::env::var("VOXL_DEBUG_STRIPES").is_ok();
    let mut max_rate = MaxRate::new(max_kbps);
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
            let h = std::thread::Builder::new()
                .name(format!("enc-{i}"))
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
            let _ = ret.send((job.idx, job.buf));
            continue;
        }
        let target = hub.target_kbps.load(Ordering::Relaxed);
        // Each stripe may use up to twice its area share of the budget; the delay-based
        // controller reins the total in if several stripes move at once.
        let share = ((target as u64 * 2) / n as u64).max(target as u64 / n as u64 + 1) as u32;
        let ts = job.captured_at.duration_since(epoch).as_millis() as i64;
        let bias = max_rate.bias();
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
                    qp,
                    po_qp,
                    cap,
                    doubled: job.paintover,
                    clear: hub.link_clear.load(Ordering::Relaxed),
                    link_kbps: hub.link_kbps.load(Ordering::Relaxed) / n as u32,
                    bias,
                    mb_dirty: job.mb_dirty.clone(),
                    refine: job.refine.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        let mut fillers = Vec::new();
        let mut undone = Vec::new();
        for _ in 0..pending {
            if let Ok((i, r, qp, f, left_out)) = done_rx.recv() {
                if left_out {
                    undone.push(i);
                }
                if r.is_some() {
                    hub.stats.qp_total.fetch_add(qp as u64, Ordering::Relaxed);
                    hub.stats.qp_frames.fetch_add(1, Ordering::Relaxed);
                }
                results[i] = r;
                if let Some(f) = f {
                    fillers.push((i, false, f));
                }
            }
        }
        let cols = job.buf.width.div_ceil(16);
        let _ = ret.send((job.idx, job.buf));
        if let (Some(refine), false) = (&job.refine, undone.is_empty()) {
            // Sharpening a stripe left out goes back to the capture loop, its rows only.
            let mut repaint = hub.repaint.lock();
            if repaint.len() != refine.len() {
                *repaint = vec![0; refine.len()];
            }
            for &i in &undone {
                let (y, h, _) = stripes[i];
                let rows = (y / 16 * cols)..((y + h).div_ceil(16) * cols).min(refine.len());
                for (r, &m) in repaint[rows.clone()].iter_mut().zip(&refine[rows]) {
                    *r |= m;
                }
            }
            drop(repaint);
            hub.wake();
        }
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
        if !job.force_idr && parts.iter().any(|p| p.1) {
            // A stripe sent a keyframe of its own, most likely a scene change crushed to
            // the budget, static areas included: sharpen everything once it's still.
            hub.resharpen.store(true, Ordering::Relaxed);
        }
        let enc_us = t0.elapsed().as_micros() as u64;
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        max_rate.sent(total + fillers.iter().map(|p| p.2.len()).sum::<usize>());
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let mut pipe = (job.captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        if job.marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            pipe = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32;
        }
        let ys: Vec<usize> = stripes.iter().map(|s| s.0).collect();
        if !fillers.is_empty() {
            // Stand-ins for re-encoded overshoots: frames of their own, ahead of the
            // re-encodes that refer past them.
            hub.stats.bytes.fetch_add(
                fillers.iter().map(|p| p.2.len() as u64).sum(),
                Ordering::Relaxed,
            );
            send_frame(&hub, &mut frame_id, fillers, &ys, false, false, pipe);
        }
        send_frame(
            &hub,
            &mut frame_id,
            parts,
            &ys,
            all_key,
            job.paintover,
            pipe,
        );
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

/// Sends one frame's stripe outputs `parts` (index, keyframe, output with HDR bytes
/// reserved) to every viewer, as frame `frame_id + 1`.
fn send_frame(
    hub: &Hub,
    frame_id: &mut u32,
    parts: Vec<(usize, bool, Vec<u8>)>,
    ys: &[usize],
    all_key: bool,
    paintover: bool,
    pipe: u32,
) {
    *frame_id = frame_id.wrapping_add(1);
    let id = *frame_id;
    let total: usize = parts.iter().map(|p| p.2.len()).sum();
    let nparts = parts.len() as u8;
    let msgs: Vec<Bytes> = parts
        .into_iter()
        .map(|(i, key, mut out)| {
            // type, flags, frame id, pipe, stripe, stripe count, parts in frame, reserved, y
            out[0] = 0x01;
            out[1] = (key as u8) | if paintover { 2 } else { 0 };
            out[2..6].copy_from_slice(&id.to_be_bytes());
            out[6..10].copy_from_slice(&pipe.to_be_bytes());
            out[10] = i as u8;
            out[11] = ys.len() as u8;
            out[12] = nparts;
            out[13] = 0;
            out[14..16].copy_from_slice(&(ys[i] as u16).to_be_bytes());
            Bytes::from(out)
        })
        .collect();
    let now = Instant::now();
    for c in hub.video_clients() {
        if !c.started.load(Ordering::Relaxed) {
            if !all_key {
                continue;
            }
            c.started.store(true, Ordering::Relaxed);
            c.inflight.lock().clear();
        }
        {
            let mut q = c.inflight.lock();
            if q.len() > 600 {
                log::warn!("client {} hopelessly behind; dropping", c.id);
                drop(q);
                c.send(Out::Text("{\"t\":\"close\"}".into()));
                continue;
            }
            q.push_back(Inflight {
                id,
                sent: now,
                bytes: total,
            });
        }
        for m in &msgs {
            c.send(Out::Bin(m.clone()));
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

    #[test]
    fn max_rate_is_stream_wide() {
        // One stripe of four sending 2 Mbps under --max-kbps=6000: nothing to correct.
        let mut m = MaxRate::new(6000);
        m.sent(125_000);
        m.window -= Duration::from_millis(500);
        assert_eq!(m.bias(), 0.0);
        // The stream at 8 Mbps: a step coarser.
        m.sent(500_000);
        m.window -= Duration::from_millis(500);
        assert_eq!(m.bias(), 1.0);
    }

    /// A rate controller for `blocks` macroblocks, all coded at `qp` by a keyframe.
    fn coded_at(blocks: usize, qp: i32) -> QpCtl {
        let mut rc = QpCtl::new();
        rc.coded = vec![51; blocks];
        rc.kept(qp, true, None, false);
        rc.note(Kind::Key, qp, 0);
        rc
    }

    #[test]
    fn sharper_frames_recode_what_is_coarser() {
        let mut rc = coded_at(10, 40);
        // Block 0 moved, the other nine didn't.
        let mut unchanged = vec![1u8; 10];
        unchanged[0] = 0;
        let coarse = rc.coarse(Some(&unchanged));
        assert_eq!(coarse[40], 9);
        // At the reference's QP or coarser x264 skips them outright; sharper, it re-codes
        // every one coded coarser, adding only the detail between the two QPs.
        let none = [0; 52];
        assert_eq!(rc.detail(40, &none, &coarse), 0.0);
        assert_eq!(rc.detail(45, &none, &coarse), 0.0);
        let step = 9.0 * (size_at(39.0) - size_at(40.0));
        assert!((rc.detail(39, &none, &coarse) - step).abs() < 1e-9);
        // Blocks to sharpen count whatever the reference: 9 blocks from 40 to 39.
        assert!(
            (rc.detail(45, &coarse, &none)).abs() < 1e-9,
            "45 isn't sharper than 40"
        );
        assert!((rc.detail(39, &coarse, &none) - step).abs() < 1e-9);
        // Right after a dropped frame there are no outright skips at any QP.
        rc.hints_lost = true;
        assert!((rc.detail(39, &none, &coarse) - step).abs() < 1e-9);
        assert_eq!(
            rc.detail(41, &none, &coarse),
            0.0,
            "none is coarser than 41"
        );
        rc.hints_lost = false;
        // A coarser frame codes what moved; a sharper one also re-codes the rest.
        rc.kept(45, false, Some(&unchanged), false);
        assert_eq!(rc.coded[0], 45);
        assert_eq!(rc.coded[1], 40);
        rc.kept(35, false, Some(&unchanged), false);
        assert!(rc.coded.iter().all(|&c| c == 35));
        assert_eq!(rc.coarsest(&[1, 1, 0, 0, 0, 0, 0, 0, 0, 0]), 35);
    }

    #[test]
    fn no_step_down_that_recodes_a_crushed_window() {
        // A scene change crushed 1000 blocks to QP 51; a 10-block animation keeps moving.
        let mut rc = coded_at(1010, 51);
        rc.size = 100_000.0;
        rc.fill = rc.size;
        rc.motion_bytes = 100.0;
        let mut unchanged = vec![1u8; 1010];
        unchanged[..10].fill(0);
        let crushed = rc.coarse(Some(&unchanged));
        // Stepping down would re-code the whole window (about 870 bytes of detail here):
        // it doesn't fit, so QP stays.
        assert_eq!(rc.qp(Kind::Motion, 20.0, 10, &[0; 52], 500.0, &crushed), 51);
        // With nothing coarse to re-code, the same frame steps down.
        assert_eq!(rc.qp(Kind::Motion, 20.0, 10, &[0; 52], 500.0, &[0; 52]), 50);
    }

    #[test]
    fn a_step_down_that_overshot_waits() {
        let mut rc = coded_at(10, 45);
        rc.size = 100_000.0;
        rc.fill = rc.size;
        rc.motion_bytes = 100.0;
        rc.floor = 45;
        rc.floor_at = Instant::now();
        assert_eq!(
            rc.qp(Kind::Motion, 20.0, 10, &[0; 52], 50_000.0, &[0; 52]),
            45
        );
        // After FLOOR_HOLD it's tried again.
        rc.floor_at = Instant::now() - FLOOR_HOLD;
        assert_eq!(
            rc.qp(Kind::Motion, 20.0, 10, &[0; 52], 50_000.0, &[0; 52]),
            44
        );
    }
}
