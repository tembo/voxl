//! Shared state between media producers (video/audio/cursor/clipboard) and websocket clients.

use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// How often a client is pinged: every 500 ms. Its round trip is the
/// shortest of its last 20 replies, so a path that changes shows within 10 s.
pub const PING_EVERY: Duration = Duration::from_millis(500);

/// The longest round trip a client's replies can show: 4 s. A client whose oldest
/// unacknowledged frame is that old is stalled, and the flow check resyncs it (video.rs),
/// so a longer round trip steers nothing; the bench's links have 80 to 140 ms. At half, a
/// path of 2 to 4 s would show no round trip and be judged by the 40 ms guess; at double,
/// a client's record would keep twice the pings for nothing.
const LONGEST_ROUND_TRIP: Duration = Duration::from_secs(4);

/// The pings a client's record keeps: one goes out every `PING_EVERY`, so this many are
/// out at once on the longest round trip, 8.
const PINGS_KEPT: usize = (LONGEST_ROUND_TRIP.as_millis() / PING_EVERY.as_millis()) as usize;

/// What names a ping, and what its reply echoes: 64 bits from the operating system's
/// random source, 16 hex digits on the wire. A ping used to be named by the time it went
/// out, which a viewer can tell before the ping reaches it (one goes every `PING_EVERY`,
/// late by the pinger's scheduling), and a burst of replies naming each microsecond
/// around it found it for a few kilobytes. A token it has not seen it can only guess, one
/// in 2^64 a reply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Token(u64);

impl Token {
    pub fn random() -> Token {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).expect("random ping token");
        Token(u64::from_ne_bytes(bytes))
    }

    /// From a reply's field: 16 hex digits, as a ping carries it.
    pub fn parse(field: &str) -> Option<Token> {
        let field = field.trim();
        let digits = field.len() == 16 && field.bytes().all(|b| b.is_ascii_hexdigit());
        digits.then(|| u64::from_str_radix(field, 16).ok().map(Token))?
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A ping sent to a client: what names it, and when it went. The time never leaves the
/// server.
struct Ping {
    token: Token,
    sent: Instant,
}

/// The pings sent to a client and not yet answered, oldest first, at most `PINGS_KEPT`. A
/// reply is a round trip only if it echoes the token of one of them.
#[derive(Default)]
pub struct Pings(VecDeque<Ping>);

impl Pings {
    /// A ping named `token`, sent at `now`. The oldest unanswered one leaves once the
    /// record holds `PINGS_KEPT`.
    pub fn send(&mut self, now: Instant, token: Token) {
        if self.0.len() == PINGS_KEPT {
            self.0.pop_front();
        }
        self.0.push_back(Ping { token, sent: now });
    }

    /// At `now`, a reply echoing `token`: the round trip of the ping it names, if that ping
    /// is in the record, which it then leaves. A reply echoing anything else (a ping
    /// already answered, one older than the record keeps, a token never sent, or a time,
    /// as replies used to) is no round trip.
    pub fn answer(&mut self, now: Instant, token: Token) -> Option<Duration> {
        let i = self.0.iter().position(|p| p.token == token)?;
        let ping = self.0.remove(i)?;
        now.checked_duration_since(ping.sent)
    }
}

pub struct Client {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Out>,
    pub video: AtomicBool,
    pub audio: AtomicBool,
    pub inflight: Mutex<VecDeque<Inflight>>,
    /// Smoothed min RTT (ms), from pings.
    pub min_rtt_ms: AtomicU32,
    /// The pings sent to it that it has not answered.
    pub pings: Mutex<Pings>,
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
            d.rate_bps = if d.rate_bps == 0.0 {
                r
            } else {
                d.rate_bps * 0.6 + r * 0.4
            };
            d.bytes = 0;
            d.window_start = Instant::now();
        }
    }

    /// Frames in flight and age of the oldest unacknowledged one.
    pub fn backlog(&self) -> (usize, u128) {
        let q = self.inflight.lock();
        (
            q.len(),
            q.front().map(|f| f.sent.elapsed().as_millis()).unwrap_or(0),
        )
    }
}

/// A non-blocking, close-on-exec pipe for waking a thread that polls file descriptors
/// (`pipe2` with those flags, which macOS lacks).
#[cfg(unix)]
pub fn wake_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    let (r, w) = nix::unistd::pipe()?;
    for fd in [&r, &w] {
        fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok((r, w))
}

/// Something `wait_any` can wait for: a file descriptor on Unix, a handle on Windows.
#[cfg(unix)]
pub type Waitable<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Waitable<'a> = std::os::windows::io::BorrowedHandle<'a>;

/// Wakes a thread blocked in `wait_any`: a pipe on Unix, an event on Windows.
pub struct Wake {
    #[cfg(unix)]
    r: std::os::fd::OwnedFd,
    #[cfg(unix)]
    w: std::os::fd::OwnedFd,
    #[cfg(windows)]
    event: std::os::windows::io::OwnedHandle,
}

impl Wake {
    #[cfg(unix)]
    pub fn new() -> std::io::Result<Self> {
        let (r, w) = wake_pipe()?;
        Ok(Wake { r, w })
    }

    #[cfg(windows)]
    pub fn new() -> std::io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        // Manual reset: signalled until drained, however many wakes came in between.
        let h = unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) }
            .map_err(std::io::Error::other)?;
        Ok(Wake {
            event: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(h.0) },
        })
    }

    pub fn signal(&self) {
        #[cfg(unix)]
        let _ = nix::unistd::write(&self.w, &[1u8]);
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Threading::SetEvent(
                windows::Win32::Foundation::HANDLE(self.event.as_raw_handle()),
            );
        }
    }

    pub fn drain(&self) {
        #[cfg(unix)]
        {
            let mut buf = [0u8; 64];
            while matches!(nix::unistd::read(&self.r, &mut buf), Ok(n) if n > 0) {}
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Threading::ResetEvent(
                windows::Win32::Foundation::HANDLE(self.event.as_raw_handle()),
            );
        }
    }

    pub fn waitable(&self) -> Waitable<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.r);
        #[cfg(windows)]
        return std::os::windows::io::AsHandle::as_handle(&self.event);
    }
}

/// Blocks until one of `w` is ready or `timeout_ms` passes (negative: no timeout).
pub fn wait_any(w: &[Waitable], timeout_ms: i32) {
    #[cfg(unix)]
    {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        let mut p: Vec<PollFd> = w
            .iter()
            .map(|&fd| PollFd::new(fd, PollFlags::POLLIN))
            .collect();
        let _ = poll(
            &mut p,
            PollTimeout::try_from(timeout_ms).unwrap_or(PollTimeout::NONE),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        let hs: Vec<HANDLE> = w.iter().map(|h| HANDLE(h.as_raw_handle())).collect();
        let ms = if timeout_ms < 0 {
            windows::Win32::System::Threading::INFINITE
        } else {
            timeout_ms as u32
        };
        unsafe { windows::Win32::System::Threading::WaitForMultipleObjects(&hs, false, ms) };
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
    pub refines: AtomicU64,
    pub damage_wait_us: AtomicU64,
    pub send_lat_us: AtomicU64,
}

pub struct Hub {
    #[cfg(target_os = "macos")]
    pub display_req: Mutex<Option<crate::mac::DisplayRequest>>,
    /// Connections older than this may no longer inject input after a display switch.
    #[cfg(target_os = "macos")]
    pub input_floor: AtomicU64,
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
    /// Wakes the capture thread (acks, joins, keyframe requests).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::input::InputEvent>>>,
    pub last_clipboard: Mutex<Option<String>>,
    /// Desktop size a client asked for (dynamic resolution), applied by the video thread.
    pub resize_req: Mutex<Option<(u32, u32)>>,
    /// Largest desktop size (Some when dynamic resolution is on).
    pub resize_max: Mutex<Option<(u32, u32)>>,
    /// Server settings sent in every `hello` besides the geometry (audio, files, ...).
    pub hello_extra: Mutex<serde_json::Map<String, serde_json::Value>>,
    /// Screenshot requests, answered by the capture thread with the current screen.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<crate::record::Snapshot>>>>,
    /// When each part of the screen last changed (`/wait`).
    pub activity: crate::wait::Activity,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Hub {
            #[cfg(target_os = "macos")]
            display_req: Mutex::new(None),
            #[cfg(target_os = "macos")]
            input_floor: AtomicU64::new(0),
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            stats: Stats::default(),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            target_kbps: AtomicU32::new(0),
            stripes: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            waker: Wake::new().expect("wake"),
            input: Mutex::new(None),
            last_clipboard: Mutex::new(None),
            resize_req: Mutex::new(None),
            resize_max: Mutex::new(None),
            hello_extra: Mutex::new(serde_json::Map::new()),
            snapshots: Mutex::new(Vec::new()),
            activity: Default::default(),
        })
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    pub fn add_client(&self, tx: mpsc::UnboundedSender<Out>) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            tx,
            video: AtomicBool::new(false),
            audio: AtomicBool::new(false),
            inflight: Mutex::new(VecDeque::new()),
            min_rtt_ms: AtomicU32::new(0),
            pings: Mutex::new(Pings::default()),
            delivered: Mutex::new(RateEst {
                window_start: Instant::now(),
                bytes: 0,
                rate_bps: 0.0,
            }),
            needs_keyframe: AtomicBool::new(true),
            started: AtomicBool::new(false),
            delay: Mutex::new(DelayEst {
                base_ms: f64::MAX,
                base_at: Instant::now(),
                queue_ms: 0.0,
            }),
        });
        self.clients.lock().push(c.clone());
        c
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.wake();
    }

    pub fn video_clients(&self) -> Vec<Arc<Client>> {
        self.clients
            .lock()
            .iter()
            .filter(|c| c.video.load(Ordering::Relaxed))
            .cloned()
            .collect()
    }

    /// The `hello` message: current geometry plus the server's settings.
    pub fn hello(&self) -> String {
        let mut m = self.hello_extra.lock().clone();
        m.insert("t".into(), "hello".into());
        m.insert("w".into(), self.width.load(Ordering::Relaxed).into());
        m.insert("h".into(), self.height.load(Ordering::Relaxed).into());
        m.insert("stripes".into(), serde_json::json!(*self.stripes.lock()));
        serde_json::Value::Object(m).to_string()
    }

    /// After the stream restarts (new size): every client needs the new geometry and
    /// a keyframe, and acks for the old stream's frames no longer apply.
    pub fn restart_clients(&self) {
        let hello = self.hello();
        for c in self.clients.lock().iter() {
            c.inflight.lock().clear();
            c.started.store(false, Ordering::Relaxed);
            c.needs_keyframe.store(true, Ordering::Relaxed);
            c.send(Out::Text(hello.clone()));
        }
    }

    pub fn broadcast(&self, m: impl Fn() -> Out) {
        for c in self.clients.lock().iter() {
            c.send(m());
        }
    }

    /// Share the admission lock with input senders, so the barrier cannot overtake input.
    #[cfg(target_os = "macos")]
    pub fn pause_display_input(&self, id: u32, client: u64) -> Option<crate::mac::DisplayRequest> {
        let mut input = self.input.lock();
        if client < self.input_floor.load(Ordering::Relaxed) {
            return None;
        }
        let sender = input.take()?;
        let (done, ready) = std::sync::mpsc::channel();
        if sender
            .send(crate::input::InputEvent::DisplayBarrier(done))
            .is_err()
        {
            *input = Some(sender);
            return None;
        }
        self.input_floor
            .store(self.next_id.load(Ordering::Relaxed), Ordering::Relaxed);
        // Retire their subscriptions too: an old client ignoring close must not
        // stall flow control for the replacement connections.
        for client in self.clients.lock().drain(..) {
            client.send(Out::Text("{\"t\":\"close\"}".into()));
        }
        Some(crate::mac::DisplayRequest { id, sender, ready })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply is a round trip only if it echoes the token of a ping that was sent and not
    /// yet answered: the latest, or an older one still out. One already answered, one never
    /// sent, and one older than the record keeps are none. A token goes out and comes back
    /// as 16 hex digits, and a reply in any other shape, a time as replies used to be among
    /// them, is no token.
    #[test]
    fn a_reply_is_a_round_trip_only_to_a_ping_still_out() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut pings = Pings::default();
        for (at, token) in [(0, 10), (500, 11), (1000, 12)] {
            pings.send(t0 + ms(at), Token(token));
        }
        for (now, echoes, round_trip) in [
            // The latest ping.
            (1040, 12, Some(ms(40))),
            // An older one, still out.
            (1060, 11, Some(ms(560))),
            // One already answered.
            (1080, 12, None),
            // One never sent.
            (1100, 13, None),
        ] {
            let got = pings.answer(t0 + ms(now), Token(echoes));
            assert_eq!(got, round_trip, "token {echoes} at {now} ms");
        }
        // One older than the record keeps: the first of PINGS_KEPT + 1, a ping apart.
        let mut pings = Pings::default();
        for i in 0..=PINGS_KEPT as u32 {
            pings.send(t0 + PING_EVERY * i, Token(i.into()));
        }
        let now = t0 + PING_EVERY * (PINGS_KEPT as u32 + 1);
        assert_eq!(pings.answer(now, Token(0)), None);
        assert_eq!(
            pings.answer(now, Token(1)),
            Some(PING_EVERY * PINGS_KEPT as u32)
        );
        assert_eq!(PINGS_KEPT, 8);

        let token = Token(0x0123_4567_89ab_cdef);
        assert_eq!(token.to_string(), "0123456789abcdef");
        assert_eq!(Token::parse(&token.to_string()), Some(token));
        for field in [
            "",
            "1",
            "9999000",
            "+123456789abcdef",
            "0123456789abcdeg",
            "0123456789abcdef0",
        ] {
            assert_eq!(Token::parse(field), None, "{field:?}");
        }
    }

    /// A reply cannot answer a ping before it arrives: what a
    /// viewer could know of the next ping before having it is when it is due, one
    /// `PING_EVERY` after the last, and the tokens of the pings it has had. A burst of
    /// replies naming each microsecond of the millisecond the ping went out in, written as
    /// replies wrote times and as tokens, and each token within 500 of one it has had, is
    /// no round trip.
    #[test]
    fn a_reply_cannot_answer_a_ping_before_it_arrives() {
        let us = Duration::from_micros;
        let real_round_trip = Duration::from_millis(100);
        let t0 = Instant::now();
        let mut pings = Pings::default();
        // Pings as the pinger sends them: due every 500 ms, each run a little late.
        let due = |k: u32| t0 + PING_EVERY * k;
        let late = [us(173), us(412), us(58), us(731)];
        let mut had = vec![];
        for (k, &l) in late.iter().enumerate().take(3) {
            let token = Token::random();
            pings.send(due(k as u32) + l, token);
            assert!(pings
                .answer(due(k as u32) + l + real_round_trip, token)
                .is_some());
            had.push(token);
        }
        // The fourth goes out; the viewer has not had it yet. 1 ms after it is due, a
        // burst arrives naming everything it could have guessed.
        pings.send(due(3) + late[3], Token::random());
        let now = due(3) + Duration::from_millis(1);
        // Each microsecond it could have gone out at, as replies wrote it and as a token.
        let times = (0..1000u64).flat_map(|j| {
            let us = (PING_EVERY * 3 + us(j)).as_micros();
            [us.to_string(), format!("{us:016x}")]
        });
        let neighbours = had.iter().flat_map(|t| {
            (0..1000u64).map(move |j| Token(t.0.wrapping_add(j).wrapping_sub(500)).to_string())
        });
        let shortest = times
            .chain(neighbours)
            .filter_map(|guess| pings.answer(now, Token::parse(&guess)?))
            .min();
        assert!(
            shortest.is_none_or(|rt| rt >= real_round_trip),
            "a {shortest:?} round trip on a {real_round_trip:?} path"
        );
    }
}
