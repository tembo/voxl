//! Shared state between media producers (video/audio/cursor/clipboard) and websocket clients.

use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

pub enum Out {
    Bin(Bytes),
    Text(String),
}

pub struct Inflight {
    pub id: u32,
    pub sent: Instant,
    pub bytes: usize,
}

pub struct Client {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Out>,
    pub video: AtomicBool,
    pub audio: AtomicBool,
    pub inflight: Mutex<VecDeque<Inflight>>,
    /// Smoothed min RTT (ms), from pings.
    pub min_rtt_ms: AtomicU32,
    /// Exponentially-weighted estimate of delivered bytes/sec, from acks.
    pub delivered: Mutex<RateEst>,
    pub needs_keyframe: AtomicBool,
    /// Has received a keyframe since (re)starting video.
    pub started: AtomicBool,
    pub delay: Mutex<DelayEst>,
}

/// Send->ack delay tracking: `base` is the (slowly expiring) minimum, the excess over
/// it is queueing somewhere on the path (network buffers or a slow decoder).
pub struct DelayEst {
    pub base_ms: f64,
    pub base_at: Instant,
    pub queue_ms: f64,
}

pub struct RateEst {
    pub window_start: Instant,
    pub bytes: usize,
    pub rate_bps: f64,
}

impl Client {
    pub fn send(&self, m: Out) {
        let _ = self.tx.send(m);
    }

    pub fn on_ack(&self, id: u32) {
        let mut q = self.inflight.lock();
        let mut acked = 0;
        let mut sample = None;
        while let Some(f) = q.front() {
            if wrapping_le(f.id, id) {
                acked += f.bytes;
                if f.id == id {
                    sample = Some(f.sent.elapsed().as_secs_f64() * 1000.0);
                }
                q.pop_front();
            } else {
                break;
            }
        }
        drop(q);
        if let Some(ms) = sample {
            let mut e = self.delay.lock();
            if ms < e.base_ms || e.base_at.elapsed().as_secs() >= 10 {
                e.base_ms = ms;
                e.base_at = Instant::now();
            }
            let qd = ms - e.base_ms;
            e.queue_ms = e.queue_ms * 0.7 + qd * 0.3;
        }
        let mut d = self.delivered.lock();
        d.bytes += acked;
        let el = d.window_start.elapsed().as_secs_f64();
        if el >= 0.5 {
            let r = d.bytes as f64 * 8.0 / el;
            d.rate_bps = if d.rate_bps == 0.0 { r } else { d.rate_bps * 0.6 + r * 0.4 };
            d.bytes = 0;
            d.window_start = Instant::now();
        }
    }

    /// Frames in flight and age of the oldest unacknowledged one.
    pub fn backlog(&self) -> (usize, u128) {
        let q = self.inflight.lock();
        (q.len(), q.front().map(|f| f.sent.elapsed().as_millis()).unwrap_or(0))
    }
}

fn wrapping_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub grab_us: AtomicU64,
    pub convert_us: AtomicU64,
    pub flow_blocked: AtomicU64,
    pub paintovers: AtomicU64,
    pub keyframes: AtomicU64,
    pub damage_wait_us: AtomicU64,
    pub send_lat_us: AtomicU64,
}

pub struct Hub {
    pub clients: Mutex<Vec<Arc<Client>>>,
    pub next_id: AtomicU64,
    pub stats: Stats,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Encoded stripes as (y, height).
    pub stripes: Mutex<Vec<(u32, u32)>>,
    /// Adaptive bitrate cap applied by the encoder (kbit/s).
    pub target_kbps: AtomicU32,
    /// Last cursor message, replayed to new clients.
    pub cursor: Mutex<Option<Bytes>>,
    /// Pipe used to wake the capture thread (acks, joins, keyframe requests).
    wake_w: i32,
    pub wake_r: i32,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::input::InputEvent>>>,
    pub last_clipboard: Mutex<Option<String>>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        let mut fds = [0i32; 2];
        unsafe {
            libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC);
        }
        Arc::new(Hub {
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            stats: Stats::default(),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            target_kbps: AtomicU32::new(0),
            stripes: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            wake_w: fds[1],
            wake_r: fds[0],
            input: Mutex::new(None),
            last_clipboard: Mutex::new(None),
        })
    }

    pub fn wake(&self) {
        unsafe {
            libc::write(self.wake_w, [1u8].as_ptr() as *const _, 1);
        }
    }

    pub fn drain_wake(&self) {
        let mut buf = [0u8; 64];
        unsafe { while libc::read(self.wake_r, buf.as_mut_ptr() as *mut _, 64) > 0 {} }
    }

    pub fn add_client(&self, tx: mpsc::UnboundedSender<Out>) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            tx,
            video: AtomicBool::new(false),
            audio: AtomicBool::new(false),
            inflight: Mutex::new(VecDeque::new()),
            min_rtt_ms: AtomicU32::new(0),
            delivered: Mutex::new(RateEst { window_start: Instant::now(), bytes: 0, rate_bps: 0.0 }),
            needs_keyframe: AtomicBool::new(true),
            started: AtomicBool::new(false),
            delay: Mutex::new(DelayEst { base_ms: f64::MAX, base_at: Instant::now(), queue_ms: 0.0 }),
        });
        self.clients.lock().push(c.clone());
        c
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.wake();
    }

    pub fn video_clients(&self) -> Vec<Arc<Client>> {
        self.clients.lock().iter().filter(|c| c.video.load(Ordering::Relaxed)).cloned().collect()
    }

    pub fn broadcast(&self, m: impl Fn() -> Out) {
        for c in self.clients.lock().iter() {
            c.send(m());
        }
    }
}
