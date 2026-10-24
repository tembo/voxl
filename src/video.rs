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
                log::error!("capture loop ended: {e}");
                std::process::exit(1);
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
        let capacity = clients
            .iter()
            .filter(|c| c.started.load(Ordering::Relaxed))
            .filter_map(|c| c.capacity_kbps())
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
        hub.drain_wake();
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage
            // stays pending, so the stream still sends what changed. Only the pixels
            // are copied here (the framebuffer is live); the PNG is made off this thread.
            cap.grab(cap.full())?;
            let shot = Arc::new(crate::record::Snapshot {
                width: w,
                height: h,
                bgrx: cap.snapshot(),
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
        // A refinement frame encodes what moved plus what is due; everything else stays skipped.
        let mb_map = if want_key {
            None
        } else if frame_paintover {
            paint_map.map(|(m, b)| {
                band = Some(match band {
                    Some(x) => union(Some(x), b),
                    None => b,
                });
                Arc::new(m)
            })
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(match band {
                Some(b) => union(Some(b), rband),
                None => rband,
            });
            let mut m = mb_dirty.take().unwrap_or_else(|| vec![1; rmap.len()]);
            for (d, r) in m.iter_mut().zip(rmap) {
                *d |= r;
            }
            hub.stats.refines.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(m))
        } else {
            mb_dirty.take().map(Arc::new)
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
    /// QP added to what frames ask for while the average rate is over --max-kbps.
    bias: f64,
    /// Bits sent since `window`, to compare the average rate with --max-kbps.
    sent: f64,
    window: Instant,
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
            cost: [c(20.0), c(8.0), c(30.0)],
            motion_qp: crate::encoder::MIN_QP,
            motion_bytes: 0.0,
            repair: 0.0,
            roomy: false,
            bias: 0.0,
            sent: 0.0,
            window: Instant::now(),
        }
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
        max_kbps: u32,
        roomy: bool,
    ) -> f64 {
        let rate = cap_kbps as f64 * 1000.0;
        self.size = rate / fps.max(1) as f64 * buf_frames;
        self.roomy = roomy;
        // It refills at the cap (a VBV's maximum rate), but no faster than the link was
        // measured to carry (after a scene change, frames would otherwise queue up on
        // a slow link; until a measurement, at half the cap) or than --max-kbps allows.
        let refill = if link_kbps > 0 {
            rate.min(link_kbps as f64 * 900.0)
        } else {
            rate * 0.5
        };
        // Bursts above --max-kbps are fine; a sustained rate isn't: past it, frames are
        // asked for a coarser QP, a step every half second, until it holds.
        let el = self.window.elapsed().as_secs_f64();
        if el >= 0.5 {
            let avg = self.sent / el;
            let max = max_kbps as f64 * 1000.0;
            if avg > max {
                self.bias = (self.bias + 1.0).min(20.0);
            } else if avg < max * 0.85 {
                self.bias = (self.bias - 1.0).max(0.0);
            }
            self.sent = 0.0;
            self.window = Instant::now();
        }
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

    /// Predicted bytes of a frame of `kind` at `qp` encoding `mbs` macroblocks.
    fn predict(&self, kind: Kind, qp: i32, mbs: usize) -> f64 {
        let c = &self.cost[kind as usize];
        mbs.max(1) as f64 * c.avg.max(c.peak) * size_at(qp as f64)
    }

    /// QP for a frame of `kind` that wants `want` and encodes `mbs` macroblocks: the
    /// first from `want` up whose predicted size fits `budget` (51 if none does).
    /// Motion QP rises to that at once but comes down in steps: a frame sharper than
    /// its reference also re-encodes the detail the reference lost, at a cost per QP
    /// learned from earlier steps, so it comes down as far as that cost fits the
    /// budget (as a VBV lets the quantizer down).
    fn qp(&self, kind: Kind, want: f64, mbs: usize, budget: f64) -> i32 {
        let want = ((want + self.bias).round() as i32).clamp(crate::encoder::MIN_QP, 51);
        let fit = (want..=51)
            .find(|&q| self.predict(kind, q, mbs) <= budget)
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
        (self.motion_qp - step).max(fit)
    }

    /// Learn from an encoded frame (sent or not) that wanted QP `want`.
    fn learn(&mut self, kind: Kind, qp: i32, want: f64, mbs: usize, bytes: usize, slices: u32) {
        // A motion frame that came down in QP was partly a repair (see `qp`): it says
        // what coming down costs, not what steady motion does.
        if !(kind == Kind::Motion && self.learn_repair(qp, bytes)) {
            // Slice headers and the like cost the same whatever changed.
            let payload = bytes.saturating_sub(8 * slices as usize) as f64;
            let per_mb = payload / mbs.max(1) as f64 / size_at(qp as f64);
            let c = &mut self.cost[kind as usize];
            c.avg = c.avg * 0.6 + per_mb * 0.4;
            c.peak = per_mb.max(c.peak * 0.85);
            // Only frames at (about) the quality asked for: costs at coarse QPs don't
            // carry over to fine ones.
            if qp as f64 <= want.round() + 2.0 {
                c.steady = if c.steady > 0.0 {
                    c.steady * 0.8 + per_mb * 0.2
                } else {
                    per_mb
                };
            }
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
    kind: Kind,
    /// QP the frame asks for (motion frames go lower when little changed).
    qp: f32,
    cap: u32,
    /// The link shows no queueing (`Hub::link_clear`).
    clear: bool,
    /// This stripe's share of the link's measured capacity (kbit/s), 0 if unknown.
    link_kbps: u32,
    /// This stripe's share of `--max-kbps`.
    max_kbps: u32,
    mb_dirty: Option<Arc<Vec<u8>>>,
    mb_cols: usize,
}

/// Stripe index, its output (keyframe or not) if it produced any, the QP used, and a
/// frame to send before the output (standing in for an overshoot that was re-encoded).
type StripeResult = (usize, Option<(bool, Vec<u8>)>, i32, Option<Vec<u8>>);

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
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let rows = s.h.div_ceil(16);
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols]
                .iter()
                .map(|&x| (x == 0) as u8)
                .collect::<Vec<u8>>()
        });
        let mbs = if job.force_idr {
            mbs_total
        } else {
            info.as_ref()
                .map_or(mbs_total, |m| m.iter().filter(|&&c| c == 0).count())
        };
        let (cap, buf_frames, want) = match job.kind {
            // A keyframe is what a new viewer sees first; don't starve it to the
            // steady-state frame budget (up to ~8 frames' worth at 4x the cap).
            // Everything after refers back to it: 3 QP sharper, as x264's I/P ratio.
            Kind::Key => (
                job.cap.saturating_mul(4).max(16_000),
                8.0,
                job.qp as f64 - 3.0,
            ),
            // Small updates (typing, a spinner) are cheap: encode them sharper, as
            // CRF would, by up to 6 QP for a sliver of the screen.
            Kind::Motion => (
                job.cap,
                1.25,
                job.qp as f64
                    + (mbs.max(1) as f64 * 4.0 / mbs_total as f64)
                        .log2()
                        .clamp(-6.0, 0.0),
            ),
            Kind::Refine => (job.cap, 1.25, job.qp as f64),
        };
        let fresh = job.kind == Kind::Key;
        // What a frame may take with the buffer full.
        let nominal = cap as f64 * 1000.0 / s.cfg.fps.max(1) as f64 * buf_frames * 0.85 / 8.0;
        let roomy = s.rc.is_roomy(want, mbs, nominal, cap, job.link_kbps);
        let budget = s.rc.budget(
            cap,
            buf_frames,
            s.cfg.fps,
            fresh,
            job.clear,
            job.link_kbps,
            job.max_kbps,
            roomy,
        );
        let mut qp = s.rc.qp(job.kind, want, mbs, budget);
        let mut filler = None;
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
            let kind = if key { Kind::Key } else { job.kind };
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
                nominal * 3.0
            } else {
                budget * 1.5
            };
            let over = kind != Kind::Key
                && qp < 51
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
                .then(|| s.rc.qp(Kind::Key, want - 3.0, mbs_total, key_budget))
                .filter(|&q| s.rc.predict(Kind::Key, q, mbs_total) <= key_budget);
            if let Some(f) = fill {
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
                // a step down in QP that cost more than expected.
                if kind == Kind::Motion {
                    s.rc.learn_repair(qp, bytes);
                }
                qp = q;
                s.rc.note(kind, qp, out.len() - HDR);
                let mut v = vec![0u8; HDR];
                v.extend_from_slice(&f);
                filler = Some(v);
            } else if let Some(q) = key_qp {
                out.truncate(HDR);
                qp = q;
                r = s.enc.encode(&planes, job.ts, true, qp, None, &mut out);
                if r.is_ok() {
                    // Learn from the keyframe only: the frame it replaced says nothing
                    // about the next one.
                    s.rc.learn(
                        Kind::Key,
                        qp,
                        want - 3.0,
                        mbs_total,
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
                s.rc.learn(kind, qp, want, mbs, bytes, s.cfg.threads);
            }
            s.rc.spend(out.len() - HDR, fresh);
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
        if done.send((i, res, qp, filler)).is_err() {
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
        let kind = if job.force_idr {
            Kind::Key
        } else if job.paintover {
            Kind::Refine
        } else {
            Kind::Motion
        };
        let want_qp = if job.paintover { po_qp } else { qp };
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
                    kind,
                    qp: want_qp,
                    cap,
                    clear: hub.link_clear.load(Ordering::Relaxed),
                    link_kbps: hub.link_kbps.load(Ordering::Relaxed) / n as u32,
                    max_kbps: max_kbps / n as u32,
                    mb_dirty: job.mb_dirty.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        let mut fillers = Vec::new();
        for _ in 0..pending {
            if let Ok((i, r, qp, f)) = done_rx.recv() {
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
        let _ = ret.send((job.idx, job.buf));
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
