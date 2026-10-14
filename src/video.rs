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
}

struct Job {
    buf: Box<I420>,
    idx: usize,
    force_idr: bool,
    paintover: bool,
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
    let enc_cfg = EncoderConfig { width: w, height: h, fps: cfg.fps, preset: cfg.preset.clone(), threads: cfg.threads, crf: cfg.crf, max_kbps: cfg.max_kbps };
    let mut encoder = Encoder::new(&enc_cfg)?;
    let hub2 = hub.clone();
    let (crf, po_crf) = (cfg.crf, cfg.paintover_crf);
    std::thread::Builder::new().name("encode".into()).spawn(move || encode_loop(&mut encoder, job_rx, ret_tx, hub2, crf, po_crf)).unwrap();

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
}

impl RateCtl {
    fn update(&mut self, hub: &Hub, clients: &[Arc<crate::hub::Client>]) {
        if self.last_eval.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_eval = Instant::now();
        let mut congested = false;
        let mut clear = true;
        let mut rate = f64::MAX;
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
            if q > 40.0 || age as f64 > rtt + 90.0 {
                congested = true;
                let r = c.delivered.lock().rate_bps / 1000.0;
                if r > 0.0 {
                    rate = rate.min(r);
                }
            }
            if q > 15.0 {
                clear = false;
            }
        }
        let now = Instant::now();
        if congested && self.last_dec.elapsed() >= Duration::from_millis(300) {
            let mut t = self.target * 0.85;
            if rate.is_finite() {
                t = t.min(rate * 0.9);
            }
            self.target = t.max(self.min);
            self.last_dec = now;
            log::debug!("rate: congested, target {:.0} kbps (delivered {:.0})", self.target, rate);
        } else if !congested && clear && self.last_dec.elapsed() >= Duration::from_secs(1) && self.last_inc.elapsed() >= Duration::from_millis(200) {
            self.target = (self.target * 1.06 + 100.0).min(self.max);
            self.last_inc = now;
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
    let start = Instant::now() - Duration::from_secs(10);
    let mut rc = RateCtl { target: cfg.max_kbps as f64, max: cfg.max_kbps as f64, min: cfg.min_kbps as f64, last_eval: start, last_dec: start, last_inc: start };
    hub.target_kbps.store(cfg.max_kbps, Ordering::Relaxed);
    let mut had_clients = false;
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
        if job_tx.send(Job { buf, idx, force_idr: want_key, paintover, captured_at: origin, marker_ms }).is_err() {
            return Err("encoder gone".into());
        }
    }
}

fn encode_loop(enc: &mut Encoder, rx: mpsc::Receiver<Job>, ret: mpsc::Sender<(usize, Box<I420>)>, hub: Arc<Hub>, crf: f32, po_crf: f32) {
    let epoch = Instant::now();
    let mut applied_kbps = 0u32;
    let mut applied_crf = crf;
    let mut frame_id: u32 = 0;
    let mut out = Vec::with_capacity(4 << 20);
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        out.clear();
        out.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let suv = job.buf.stride_uv();
        let planes = Planes { y: &job.buf.y, u: &job.buf.u, v: &job.buf.v, stride_y: job.buf.width, stride_uv: suv };
        let ts = job.captured_at.duration_since(epoch).as_millis() as i64;
        let want_crf = if job.paintover { po_crf } else { crf };
        let target = hub.target_kbps.load(Ordering::Relaxed);
        let rate_moved = applied_kbps == 0 || (target as f64 - applied_kbps as f64).abs() > applied_kbps as f64 * 0.04;
        if want_crf != applied_crf || rate_moved {
            // Paint-over frames get twice the budget: they only happen on a still screen.
            let cap = if job.paintover { target.saturating_mul(2) } else { target };
            enc.set_rate(want_crf, cap);
            applied_crf = want_crf;
            applied_kbps = target;
        }
        let res = enc.encode(&planes, ts, job.force_idr, None, &mut out);
        let _ = ret.send((job.idx, job.buf));
        let key = match res {
            Ok(k) => k,
            Err(e) => {
                log::error!("{e}");
                continue;
            }
        };
        if out.len() <= 10 {
            continue;
        }
        let enc_us = t0.elapsed().as_micros() as u64;
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(out.len() as u64, Ordering::Relaxed);
        if key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        frame_id = frame_id.wrapping_add(1);
        out[1] = (key as u8) | if job.paintover { 2 } else { 0 };
        out[2..6].copy_from_slice(&frame_id.to_be_bytes());
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let pipe = (job.captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        out[6..10].copy_from_slice(&pipe.to_be_bytes());
        if job.marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u32;
            out[6..10].copy_from_slice(&now.to_be_bytes());
        }
        let msg = Bytes::copy_from_slice(&out);
        let now = Instant::now();
        for c in hub.video_clients() {
            if !c.started.load(Ordering::Relaxed) {
                if !key {
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
                q.push_back(Inflight { id: frame_id, sent: now, bytes: msg.len() });
            }
            c.send(Out::Bin(msg.clone()));
        }
        if job.marker_ms >= 0 {
            log::info!("marker sent age {} ms (encode {} us)", marker_age(job.marker_ms), enc_us);
        }
        hub.stats.send_lat_us.fetch_add(job.captured_at.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// Debug: decode the bench loadgen marker at (300,150) and log its age.
fn marker_probe(buf: &[u8], stride: usize, what: &str) -> i64 {
    let mut v: u64 = 0;
    for b in 0..40 {
        let x = 300 + (b % 20) * 24 + 12;
        let y = 150 + (b / 20) * 24 + 12;
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
