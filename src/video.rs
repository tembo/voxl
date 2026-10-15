//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::{convert_band, I420};
use crate::encoder::{Encoder, EncoderConfig, Planes};
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
    pub crf: f32,
    pub max_kbps: u32,
    pub paintover_crf: f32,
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
        Some(a) => Band { y0: a.y0.min(b.y0), y1: a.y1.max(b.y1) },
    }
}

pub fn spawn(cfg: VideoConfig, hub: Arc<Hub>) -> Result<(), String> {
    let mut cap = Capturer::new(cfg.display.as_deref())?;
    let (w, h) = (cap.width, cap.height);
    hub.width.store(w, Ordering::Relaxed);
    hub.height.store(h, Ordering::Relaxed);
    log::info!("capturing {}x{} @ {} fps, preset {} threads {}", w, h, cfg.fps, cfg.preset, cfg.threads);

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
    let (ret_tx, ret_rx) = mpsc::channel::<(usize, Box<I420>)>();
    let layout = stripe_layout(h, cfg.stripes);
    let mut stripes = Vec::new();
    for &(y, sh) in &layout {
        let fixed = if cfg.adaptive_threads { 1 } else { (cfg.threads / layout.len() as u32).max(1) };
        let ecfg = EncoderConfig { width: w, height: sh, fps: cfg.fps, preset: cfg.preset.clone(), threads: fixed, crf: cfg.crf, max_kbps: cfg.max_kbps };
        let max_threads = if cfg.adaptive_threads { (cfg.threads / layout.len() as u32).max(1) } else { 1 };
        stripes.push(Stripe { y: y as usize, h: sh as usize, enc: Encoder::new(&ecfg)?, cfg: ecfg, max_threads, touched: false, applied_kbps: 0, applied_crf: -1.0 });
    }
    *hub.stripes.lock() = layout;
    let hub2 = hub.clone();
    let (crf, po_crf) = (cfg.crf, cfg.paintover_crf);
    std::thread::Builder::new().name("encode".into()).spawn(move || encode_loop(stripes, job_rx, ret_tx, hub2, crf, po_crf)).unwrap();

    let hub3 = hub.clone();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(&mut cap, cfg, hub3, job_tx, ret_rx) {
                log::error!("capture loop ended: {e}");
                std::process::exit(1);
            }
        })
        .unwrap();
    Ok(())
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
            log::warn!("client {} stalled ({} frames, {} ms); resyncing", c.id, n, age);
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
}

/// Starting point for a new session's bitrate cap (kbit/s).
const START_KBPS: f64 = 8000.0;

impl RateCtl {
    fn new(max: f64, min: f64) -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        RateCtl { target: START_KBPS.min(max), max, min, last_eval: past, last_dec: past, last_inc: past, slow_start: true, ceiling: f64::MAX }
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
            if rate.is_finite() {
                self.ceiling = rate;
                t = t.min(rate * 0.85);
            }
            self.target = t.max(self.min);
            self.last_dec = now;
            self.slow_start = false;
            log::debug!("rate: congested, target {:.0} kbps (delivered {:.0})", self.target, rate);
        } else if self.slow_start && !congested && clear && self.last_inc.elapsed() >= Duration::from_millis(250) {
            // Only probe upward once the content actually presses against the cap.
            if delivered >= self.target * 0.5 {
                self.target = (self.target * 1.5).min(self.max);
                self.last_inc = now;
                log::debug!("rate: slow start, target {:.0} kbps (delivered {:.0})", self.target, delivered);
            }
        } else if !congested && clear && self.last_dec.elapsed() >= Duration::from_secs(1) {
            // Recover quickly to just under the known capacity, then probe past it gently
            // so a busy link isn't driven into the queue every couple of seconds.
            if self.target < self.ceiling * 0.9 && self.last_inc.elapsed() >= Duration::from_millis(200) {
                self.target = (self.target * 1.06 + 100.0).min(self.ceiling * 0.9).min(self.max);
                self.last_inc = now;
            } else if self.target >= self.ceiling * 0.9 && self.last_inc.elapsed() >= Duration::from_millis(1000) {
                self.target = (self.target * 1.02).min(self.max);
                self.last_inc = now;
            }
        }
        hub.target_kbps.store(self.target as u32, Ordering::Relaxed);
    }
}

fn wait_fds(fds: &[i32], timeout_ms: i32) {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    unsafe {
        libc::poll(p.as_mut_ptr(), p.len() as _, timeout_ms);
    }
}

fn capture_loop(cap: &mut Capturer, cfg: VideoConfig, hub: Arc<Hub>, job_tx: mpsc::SyncSender<Job>, ret_rx: mpsc::Receiver<(usize, Box<I420>)>) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let mut pool: Vec<Option<Box<I420>>> = vec![Some(Box::new(I420::new(w, h))), Some(Box::new(I420::new(w, h)))];
    let mut pending: [Option<Band>; 2] = [Some(cap.full()), Some(cap.full())];
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
    let x_fd = cap.fd();
    let debug_marker = std::env::var("SRS_DEBUG_MARKER").is_ok();

    loop {
        cap.pump()?;
        hub.drain_wake();
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
            wait_fds(&[x_fd, hub.wake_r], 500);
            continue;
        }
        rc.update(&hub, &clients);
        let want_key = clients.iter().any(|c| c.needs_keyframe.load(Ordering::Relaxed));
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pending = [Some(cap.full()), Some(cap.full())];
            cap.damaged = true;
            full_next = true;
        }
        let paintover_due = paintover_left > 0 && last_motion.elapsed() >= Duration::from_millis(cfg.paintover_delay_ms);
        if !cap.damaged && !want_key && !paintover_due {
            let timeout = if paintover_left == 0 { 100 } else { cfg.paintover_delay_ms.saturating_sub(last_motion.elapsed().as_millis() as u64) as i32 + 1 };
            wait_fds(&[x_fd, hub.wake_r], timeout);
            continue;
        }
        let since = last_capture.elapsed();
        if since < min_interval {
            let rem = min_interval - since;
            wait_fds(&[hub.wake_r], (rem.as_micros() as i32 + 999) / 1000);
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
            wait_fds(&[x_fd, hub.wake_r], 4);
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
        hub.stats.damage_wait_us.fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mut band = None;
        let mut mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        if cap.damaged {
            band = cap.take_damage()?;
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let t1 = Instant::now();
        if debug_marker {
            log::info!("cap dt {:.1} ms band {:?} grab {} us", last_capture.elapsed().as_secs_f64() * 1000.0, band, (t1 - t0).as_micros());
        }
        let marker_ms = if debug_marker { marker_probe(cap.pixels(), cap.stride, "grab") } else { -1 };
        let is_motion = band.is_some();
        if !is_motion && !want_key && !paintover_due {
            // Damage was empty after all.
            continue;
        }
        let idx = next_idx;
        let mut buf = pool[idx].take().unwrap();
        let todo = match (pending[idx], band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(b) = band {
            pending[1 - idx] = Some(union(pending[1 - idx], b));
        }
        pending[idx] = None;
        if let Some(t) = todo {
            convert_band(cap.pixels(), cap.stride, &mut buf, t.y0 as usize, t.y1 as usize, cfg.convert_threads);
        }
        let t2 = Instant::now();
        hub.stats.grab_us.fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats.convert_us.fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        let paintover = if is_motion {
            last_motion = Instant::now();
            paintover_left = cfg.paintover_burst;
            false
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
        }
        last_capture = t0;
        next_idx = 1 - idx;
        if job_tx.send(Job { buf, idx, force_idr: want_key, paintover, paintover_final: paintover && paintover_left == 0, mb_dirty: if want_key || paintover { None } else { mb_dirty.take().map(Arc::new) }, band, captured_at: origin, marker_ms }).is_err() {
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
    /// Changed since its last paint-over pass.
    touched: bool,
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

/// x264's sliced threading costs ~3x the CPU of one thread on desktop content, so a
/// stripe encodes single-threaded and only fans out while sustained heavy motion
/// would otherwise blow the frame budget.
struct ThreadGovernor {
    threads: u32,
    ewma_ms: f64,
    heavy: u32,
    light_since: Option<Instant>,
    budget_ms: f64,
}

impl ThreadGovernor {
    fn want(&mut self, enc_ms: f64, max: u32) -> Option<u32> {
        self.ewma_ms = self.ewma_ms * 0.8 + enc_ms * 0.2;
        if self.threads == 1 {
            // Only when one thread can no longer hold the frame rate.
            self.heavy = if self.ewma_ms > self.budget_ms * 0.9 { self.heavy + 1 } else { 0 };
            (self.heavy >= 30).then_some(max)
        } else if self.ewma_ms < self.budget_ms * 0.45 {
            // Threaded encode is ~2x faster, so this means one thread would fit again.
            let since = *self.light_since.get_or_insert_with(Instant::now);
            (since.elapsed() >= Duration::from_secs(2)).then_some(1)
        } else {
            self.light_since = None;
            None
        }
    }
}

fn stripe_worker(i: usize, mut s: Stripe, rx: mpsc::Receiver<StripeJob>, done: mpsc::Sender<StripeResult>, debug: bool) {
    let mut gov = ThreadGovernor { threads: 1, ewma_ms: 0.0, heavy: 0, light_since: None, budget_ms: 1000.0 / s.cfg.fps as f64 };
    while let Ok(job) = rx.recv() {
        let buf = unsafe { &*job.buf.0 };
        let moved = s.applied_kbps == 0 || (job.cap as f64 - s.applied_kbps as f64).abs() > s.applied_kbps as f64 * 0.04;
        if job.crf != s.applied_crf || moved {
            s.enc.set_rate(job.crf, job.cap);
            s.applied_crf = job.crf;
            s.applied_kbps = job.cap;
        }
        let suv = buf.stride_uv();
        let planes = Planes { y: &buf.y[s.y * buf.width..], u: &buf.u[(s.y / 2) * suv..], v: &buf.v[(s.y / 2) * suv..], stride_y: buf.width, stride_uv: suv };
        let mut out = Vec::with_capacity(64 << 10);
        out.resize(HDR, 0);
        static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        let _g = std::env::var("SRS_SERIAL").is_ok().then(|| SERIAL.lock());
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let rows = s.h.div_ceil(16);
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols].iter().map(|&x| (x == 0) as u8).collect::<Vec<u8>>()
        });
        let te = Instant::now();
        let r = s.enc.encode(&planes, job.ts, job.force_idr, None, info.as_deref_mut(), &mut out);
        if debug {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            let cpu_now = ts.tv_sec as i64 * 1_000_000 + ts.tv_nsec as i64 / 1000;
            log::info!("stripe y{} encode {} us wall, thread cpu total {} us, {} bytes", s.y, te.elapsed().as_micros(), cpu_now, out.len());
        }
        if s.max_threads > 1 {
            if let Some(t) = gov.want(te.elapsed().as_secs_f64() * 1000.0, s.max_threads) {
                match Encoder::new(&EncoderConfig { threads: t, ..s.cfg.clone() }) {
                    Ok(e) => {
                        log::info!("stripe {i}: encoder now {t} thread(s) (encode {:.1} ms)", gov.ewma_ms);
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

fn encode_loop(stripes_in: Vec<Stripe>, rx: mpsc::Receiver<Job>, ret: mpsc::Sender<(usize, Box<I420>)>, hub: Arc<Hub>, crf: f32, po_crf: f32) {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes_in.len();
    let debug_stripes = std::env::var("SRS_DEBUG_STRIPES").is_ok();
    // Geometry and paint-over bookkeeping stay here; encoders move to their workers.
    let mut stripes: Vec<(usize, usize, bool)> = stripes_in.iter().map(|s| (s.y, s.h, false)).collect();
    let (done_tx, done_rx) = mpsc::channel::<StripeResult>();
    let workers: Vec<(mpsc::Sender<StripeJob>, std::thread::JoinHandle<()>)> = stripes_in
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let (tx, rx) = mpsc::channel();
            let done = done_tx.clone();
            let h = std::thread::Builder::new().name(format!("enc-{i}")).spawn(move || stripe_worker(i, s, rx, done, debug_stripes)).unwrap();
            (tx, h)
        })
        .collect();
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        let selected: Vec<bool> = stripes
            .iter()
            .map(|&(y, h, touched)| job.force_idr || (job.paintover && touched) || job.band.is_some_and(|b| (b.y0 as usize) < y + h && (b.y1 as usize) > y))
            .collect();
        if !selected.iter().any(|&x| x) {
            let _ = ret.send((job.idx, job.buf));
            continue;
        }
        let target = hub.target_kbps.load(Ordering::Relaxed);
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
                let cap = if job.paintover { share.saturating_mul(2) } else { share };
                let _ = workers[i].0.send(StripeJob { buf: buf_ptr, ts, force_idr: job.force_idr, crf: want_crf, cap, mb_dirty: job.mb_dirty.clone(), mb_cols: job.buf.width.div_ceil(16) });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        for _ in 0..pending {
            if let Ok((i, r)) = done_rx.recv() {
                results[i] = r;
            }
        }
        let _ = ret.send((job.idx, job.buf));
        for (y, h, touched) in stripes.iter_mut() {
            if job.paintover_final {
                *touched = false;
            }
            if job.band.is_some_and(|b| (b.y0 as usize) < *y + *h && (b.y1 as usize) > *y) {
                *touched = true;
            }
        }
        let parts: Vec<(usize, bool, Vec<u8>)> = results.into_iter().enumerate().filter_map(|(i, r)| r.map(|(k, o)| (i, k, o))).collect();
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
        frame_id = frame_id.wrapping_add(1);
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let mut pipe = (job.captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        if job.marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            pipe = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u32;
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
                q.push_back(Inflight { id: frame_id, sent: now, bytes: total });
            }
            for m in &msgs {
                c.send(Out::Bin(m.clone()));
            }
        }
        hub.stats.send_lat_us.fetch_add(job.captured_at.elapsed().as_micros() as u64, Ordering::Relaxed);
        if job.marker_ms >= 0 {
            log::info!("marker sent age {} ms (encode {} us)", marker_age(job.marker_ms), enc_us);
        }
    }
}

/// Debug: decode the bench loadgen marker at (300,150) and log its age.
fn marker_probe(buf: &[u8], stride: usize, what: &str) -> i64 {
    let mut v: u64 = 0;
    let (mx, my) = std::env::var("SRS_DEBUG_MARKER")
        .ok()
        .and_then(|s| s.split_once(',').and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))))
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
    let now = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64) & 0xffffff;
    log::info!("marker {} frame {} age {} ms", what, v >> 24, now - ms);
    ms
}

fn marker_age(ms: i64) -> i64 {
    let now = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64) & 0xffffff;
    now - ms
}
