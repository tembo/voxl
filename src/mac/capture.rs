//! Screen capture with ScreenCaptureKit. Frames arrive as NV12 (converted on the GPU,
//! BT.601 like the Linux path) together with the rectangles that changed, which drive
//! the same damage bookkeeping as XDamage: nothing is converted or encoded while the
//! screen is still, and x264 skips the macroblocks that didn't change.
//!
//! ScreenCaptureKit can hold a change back: on a still screen it sends idle frames, and
//! (on Apple-VM guests at least) the next change stops them without a complete frame,
//! which only comes with the change after. While someone wants frames, a watcher takes
//! the screen with a one-shot capture when idle frames stop for `STALL`, or when no
//! frame came for `BACKSTOP`. The stream then also sends the held-back frame, marked
//! all dirty. For both, the damage is the 16x16 blocks that differ: a stream frame from
//! the last stream frame exactly, and a one-shot capture (or a stream frame replacing
//! one on screen) from the frame shown, within `SHOT_TOLERANCE`.

use super::ffi::*;
use super::stream::{self, OutputType};
use super::Geometry;
use nix::unistd::{read, write};
use objc2_core_foundation::CFString;
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCFrameStatus, SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus,
};
use parking_lot::{Condvar, Mutex, MutexGuard};
use std::ffi::c_void;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub static DAMAGE_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Idle frames come about every 20 ms; this long without one (and no complete frame)
/// means a change is being held back.
const STALL: Duration = Duration::from_millis(50);
/// After a one-shot capture the stream sends a couple of frames marked all dirty.
const KICK: Duration = Duration::from_millis(250);
/// The longest a wanted screen goes without a fresh look, doubling up to `MAX_BACKSTOP`
/// while those looks find nothing new (each costs WindowServer and replayd some CPU).
const BACKSTOP: Duration = Duration::from_secs(1);
const MAX_BACKSTOP: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

/// A retained ScreenCaptureKit frame, read-locked while it is the current one.
struct Frame {
    pb: CVPixelBufferRef,
    locked: bool,
}
unsafe impl Send for Frame {}

impl Frame {
    fn retain(pb: CVPixelBufferRef) -> Self {
        unsafe { CVPixelBufferRetain(pb) };
        Frame { pb, locked: false }
    }
    /// Another reference to the same pixels.
    fn share(&self) -> Self {
        Frame::retain(self.pb)
    }
    fn lock(&mut self) {
        if !self.locked {
            unsafe { CVPixelBufferLockBaseAddress(self.pb, kCVPixelBufferLock_ReadOnly) };
            self.locked = true;
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            if self.locked {
                CVPixelBufferUnlockBaseAddress(self.pb, kCVPixelBufferLock_ReadOnly);
            }
            CVPixelBufferRelease(self.pb);
        }
    }
}

/// Handed from the capture queue to the capture thread.
struct Incoming {
    frame: Option<Frame>,
    /// Changed areas as (x0, y0, x1, y1) in frame pixels.
    rects: Vec<(u32, u32, u32, u32)>,
    /// `frame` (a one-shot capture, or a frame right after one) comes without its real
    /// changed areas: compare it with the previous one instead.
    diff: bool,
    /// `frame` is a one-shot capture.
    shot: bool,
    /// Until then, complete frames follow a one-shot capture (see `KICK`).
    kicked_until: Option<Instant>,
    /// The latest idle frame since the last complete frame or one-shot capture.
    idle_at: Option<Instant>,
    /// The latest complete frame or one-shot capture.
    fresh_at: Instant,
    /// The current backstop interval.
    backstop: Duration,
    /// A viewer or a `/wait` wants frames (set by the video thread).
    wanted: bool,
    /// A screenshot wants a one-shot capture now.
    force: bool,
    shooting: bool,
    /// One-shot captures finished, successful or not.
    shots: u64,
    closed: bool,
}

impl Incoming {
    /// When the watcher should take a one-shot capture, and why.
    fn due(&self) -> Option<(Instant, &'static str)> {
        if self.shooting {
            return None;
        }
        if self.force {
            return Some((Instant::now(), "screenshot"));
        }
        if !self.wanted {
            return None;
        }
        let backstop = (self.fresh_at + self.backstop, "backstop");
        Some(match self.idle_at {
            Some(t) if t + STALL < backstop.0 => (t + STALL, "stall"),
            _ => backstop,
        })
    }
}

struct Shared {
    inc: Mutex<Incoming>,
    /// Wakes the watcher.
    cv: Condvar,
}

pub struct Capturer {
    pub width: u32,
    pub height: u32,
    geometry: Geometry,
    shared: Arc<Shared>,
    wake_r: OwnedFd,
    /// The frame conversions read from.
    current: Option<Frame>,
    /// Newest frame, adopted at the next grab.
    next: Option<Frame>,
    /// The newest frame (`next`, else `current`) is a one-shot capture.
    shown_shot: bool,
    /// The last stream frame, which the next one is compared with exactly. A one-shot
    /// capture never replaces it, so a change it hides within the tolerance still shows.
    stream_base: Option<Frame>,
    pub damaged: bool,
    /// When the first damage since the last take arrived.
    pub damaged_at: Option<Instant>,
    acc: Option<(u32, u32)>,
    mb_dirty: Vec<u8>,
    pub mb_cols: usize,
    /// Areas (x0, y0, x1, y1) changed since the video thread last took them, for `/wait`.
    pub changes: Vec<(u32, u32, u32, u32)>,
    stream: stream::Stream,
    last_check: Instant,
    rebuild: bool,
    debug: bool,
}

/// Number from a CFNumber value of a frame-info dictionary.
unsafe fn dict_i64(dict: CFDictionaryRef, key: *const c_void) -> Option<i64> {
    let v = CFDictionaryGetValue(dict, key);
    let mut out = 0i64;
    (!v.is_null() && CFNumberGetValue(v, kCFNumberSInt64Type, &mut out as *mut i64 as *mut c_void))
        .then_some(out)
}

impl Capturer {
    pub fn new(_display: Option<&str>) -> Result<Self, String> {
        let display = super::display();
        let (width, height) = super::capture_size(display);
        let geometry = Geometry {
            display,
            bounds: unsafe { CGDisplayBounds(display) },
            width,
        };
        *super::GEOMETRY.lock() = Some(geometry);
        let fps = super::fps();
        let shared = Arc::new(Shared {
            inc: Mutex::new(Incoming {
                frame: None,
                rects: Vec::new(),
                diff: false,
                shot: false,
                kicked_until: None,
                idle_at: None,
                fresh_at: Instant::now(),
                backstop: BACKSTOP,
                wanted: false,
                force: false,
                shooting: false,
                shots: 0,
                closed: false,
            }),
            cv: Condvar::new(),
        });
        let (wake_r, wake_w) = crate::hub::wake_pipe().map_err(|e| e.to_string())?;
        let wake_w = Arc::new(wake_w);
        let debug = std::env::var("VOXL_DEBUG_DAMAGE").is_ok();
        let (sh, wake) = (shared.clone(), wake_w.clone());
        let (w, h) = (width, height);
        let on_frame = move |sb: CMSampleBufferRef, ty: OutputType| unsafe {
            if ty != OutputType::Screen || !CMSampleBufferIsValid(sb) {
                return;
            }
            let att = CMSampleBufferGetSampleAttachmentsArray(sb, false);
            if att.is_null() || CFArrayGetCount(att) == 0 {
                return;
            }
            let info = CFArrayGetValueAtIndex(att, 0);
            let status_key = SCStreamFrameInfoStatus as *const _ as *const c_void;
            let status = dict_i64(info, status_key);
            if status == Some(SCFrameStatus::Idle.0 as i64) {
                let mut inc = sh.inc.lock();
                if inc.idle_at.replace(Instant::now()).is_none() && inc.wanted {
                    sh.cv.notify_one();
                }
                return;
            }
            // Only complete frames carry new pixels (idle ones repeat the last).
            if status != Some(SCFrameStatus::Complete.0 as i64) {
                return;
            }
            let pb = CMSampleBufferGetImageBuffer(sb);
            if pb.is_null() {
                return;
            }
            let mut rects = Vec::new();
            let list = CFDictionaryGetValue(info, SCStreamFrameInfoDirtyRects as *const _ as _);
            if list.is_null() {
                rects.push((0, 0, w, h));
            } else {
                for i in 0..CFArrayGetCount(list) {
                    let mut r = CGRect::default();
                    if CGRectMakeWithDictionaryRepresentation(
                        CFArrayGetValueAtIndex(list, i),
                        &mut r,
                    ) {
                        let x0 = r.origin.x.floor().clamp(0.0, w as f64) as u32;
                        let y0 = r.origin.y.floor().clamp(0.0, h as f64) as u32;
                        let x1 = (r.origin.x + r.size.width).ceil().clamp(0.0, w as f64) as u32;
                        let y1 = (r.origin.y + r.size.height).ceil().clamp(0.0, h as f64) as u32;
                        if x1 > x0 && y1 > y0 {
                            rects.push((x0, y0, x1, y1));
                        }
                    }
                }
            }
            if debug {
                log::info!(
                    "frame {}x{} dirty {:?}",
                    CVPixelBufferGetWidth(pb),
                    CVPixelBufferGetHeight(pb),
                    rects
                );
            }
            {
                let mut inc = sh.inc.lock();
                inc.frame = Some(Frame::retain(pb));
                inc.shot = false;
                if inc.kicked_until.is_some_and(|t| Instant::now() < t) {
                    inc.diff = true;
                } else {
                    inc.rects.extend(rects);
                    inc.backstop = BACKSTOP;
                }
                inc.idle_at = None;
                inc.fresh_at = Instant::now();
            }
            let _ = write(&*wake, &[1u8]);
        };
        let stream = stream::start(
            display,
            "capture",
            &[OutputType::Screen],
            |cfg| unsafe {
                cfg.setWidth(width as usize);
                cfg.setHeight(height as usize);
                cfg.setMinimumFrameInterval(objc2_core_media::CMTime {
                    value: 1,
                    timescale: fps as i32,
                    flags: objc2_core_media::CMTimeFlags::Valid,
                    epoch: 0,
                });
                cfg.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
                cfg.setColorMatrix(&*(kCGDisplayStreamYCbCrMatrix_ITU_R_601_4 as *const CFString));
                // Convert to sRGB on the GPU: without this, wide-gamut displays (P3) are
                // captured in their native space and shown desaturated as BT.709/sRGB.
                cfg.setColorSpaceName(&*(kCGColorSpaceSRGB as *const CFString));
                // Clients draw the cursor themselves (see desktop.rs), so pointer motion
                // costs no encode and has no stream latency.
                // If the system cursor can't be read, or --show-cursor is set (e.g. for viewers),
                // it has to be in the video.
                cfg.setShowsCursor(super::show_cursor() || !super::desktop::cursor_readable());
                cfg.setCaptureResolution(SCCaptureResolutionType::Best);
                cfg.setQueueDepth(6);
            },
            on_frame,
        )?;
        if let Some(shooter) = stream.shooter.clone() {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("capture-watch".into())
                .spawn(move || watch(sh, shooter, wake_w, (width, height), debug))
                .map_err(|e| e.to_string())?;
        }
        let mut me = Capturer {
            width,
            height,
            geometry,
            shared,
            wake_r,
            current: None,
            next: None,
            shown_shot: false,
            stream_base: None,
            damaged: true,
            damaged_at: None,
            acc: None,
            mb_dirty: vec![0; (width as usize).div_ceil(16) * (height as usize).div_ceil(16)],
            mb_cols: (width as usize).div_ceil(16),
            changes: Vec::new(),
            stream,
            last_check: Instant::now(),
            rebuild: false,
            debug,
        };
        // The stream opens with a full frame; have it before anything is converted.
        let t = Instant::now();
        while me.next.is_none() && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
            me.pump()?;
        }
        if me.next.is_none() {
            log::warn!("no frame from ScreenCaptureKit yet");
        }
        me.grab(me.full())?;
        log::info!("capturing display {display} with ScreenCaptureKit at {width}x{height}");
        Ok(me)
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.wake_r.as_fd()
    }

    /// Take frames and changed areas that arrived since the last call.
    pub fn pump(&mut self) -> Result<(), String> {
        let mut drain = [0u8; 64];
        while matches!(read(&self.wake_r, &mut drain), Ok(n) if n > 0) {}
        let (frame, mut rects, diff, shot) = {
            let mut inc = self.shared.inc.lock();
            let diff = std::mem::take(&mut inc.diff);
            (
                inc.frame.take(),
                std::mem::take(&mut inc.rects),
                diff,
                inc.shot,
            )
        };
        let (w, h) = (self.width, self.height);
        let mut changed = Vec::new();
        match frame {
            Some(mut f) if shot => {
                // A one-shot capture differs a little from stream frames everywhere:
                // compare it with what viewers see, within the tolerance.
                match self.next.as_mut().or(self.current.as_mut()) {
                    Some(shown) => changed = changed_blocks(shown, &mut f, w, h, SHOT_TOLERANCE),
                    None => changed.push((0, 0, w, h)),
                }
                if !changed.is_empty() {
                    self.next = Some(f);
                    self.shown_shot = true;
                }
            }
            Some(mut f) => {
                if diff {
                    // Stream frames are identical where nothing changed.
                    match self.stream_base.as_mut() {
                        Some(base) => changed = changed_blocks(base, &mut f, w, h, 0),
                        None => changed.push((0, 0, w, h)),
                    }
                }
                // Repaint where a one-shot capture on screen shows something else, such
                // as a change that has since gone.
                if self.shown_shot {
                    if let Some(shown) = self.next.as_mut().or(self.current.as_mut()) {
                        changed.extend(changed_blocks(shown, &mut f, w, h, SHOT_TOLERANCE));
                    }
                }
                self.stream_base = Some(f.share());
                self.next = Some(f);
                self.shown_shot = false;
            }
            None => {}
        }
        if self.debug && (diff || !changed.is_empty()) {
            let what = if shot {
                "one-shot capture"
            } else {
                "frame after one-shot capture"
            };
            log::info!("{what} changed {changed:?}");
        }
        if !changed.is_empty() {
            rects.extend(changed);
            self.shared.inc.lock().backstop = BACKSTOP;
        }
        for (x0, y0, x1, y1) in rects {
            DAMAGE_EVENTS.fetch_add(1, Ordering::Relaxed);
            let (c0, c1) = (x0 as usize / 16, (x1 as usize).div_ceil(16));
            for row in (y0 as usize / 16)..(y1 as usize).div_ceil(16) {
                self.mb_dirty[row * self.mb_cols + c0..row * self.mb_cols + c1].fill(1);
            }
            self.acc = Some(match self.acc {
                Some((a, b)) => (a.min(y0), b.max(y1)),
                None => (y0, y1),
            });
            self.changes.push((x0, y0, x1, y1));
            self.damaged = true;
            self.damaged_at.get_or_insert_with(Instant::now);
        }
        if self.last_check.elapsed() >= Duration::from_secs(1) {
            self.last_check = Instant::now();
            // Resolution changed (System Settings, another display arrangement).
            let b = unsafe { CGDisplayBounds(self.geometry.display) };
            if b.size != self.geometry.bounds.size
                || super::capture_size(self.geometry.display) != (self.width, self.height)
            {
                log::info!("display changed; restarting capture");
                self.rebuild = true;
            } else if b.origin != self.geometry.bounds.origin {
                self.geometry.bounds = b;
                *super::GEOMETRY.lock() = Some(self.geometry);
            }
        }
        Ok(())
    }

    /// Take accumulated damage as a row band.
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.damaged = false;
        self.damaged_at = None;
        let Some((y0, y1)) = self.acc.take() else {
            return Ok(None);
        };
        Ok(Some(Band {
            y0: y0 & !1,
            y1: ((y1 + 1) & !1).min(self.height),
        }))
    }

    /// Take the dirty-macroblock map accumulated with the current damage.
    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let n = self.mb_dirty.len();
        std::mem::replace(&mut self.mb_dirty, vec![0; n])
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    /// Make the newest frame the current one. Every ScreenCaptureKit frame is a whole
    /// screen, so the band doesn't matter.
    pub fn grab(&mut self, _band: Band) -> Result<(), String> {
        if let Some(mut f) = self.next.take() {
            f.lock();
            self.current = Some(f);
        }
        Ok(())
    }

    /// Convert rows [y0, y1) of the current frame into `dst`.
    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        let Some(f) = &self.current else {
            return;
        };
        unsafe {
            let pb = f.pb;
            if CVPixelBufferGetWidth(pb) != self.width as usize
                || CVPixelBufferGetHeight(pb) != self.height as usize
            {
                return;
            }
            crate::convert::convert_nv12_band(
                CVPixelBufferGetBaseAddressOfPlane(pb, 0),
                CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
                CVPixelBufferGetBaseAddressOfPlane(pb, 1),
                CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
                dst,
                y0,
                y1.min(self.height as usize),
                threads,
            );
        }
    }

    /// Whether a viewer or a `/wait` wants frames, which lets the watcher take one-shot
    /// captures.
    pub fn want_frames(&self, on: bool) {
        let mut inc = self.shared.inc.lock();
        if inc.wanted != on {
            inc.wanted = on;
            self.shared.cv.notify_one();
        }
    }

    /// Takes a one-shot capture if the screen may have changed unseen, and waits for it.
    fn refresh(&mut self) {
        let n = {
            let mut inc = self.shared.inc.lock();
            let stale = inc.idle_at.is_some_and(|t| t.elapsed() >= STALL)
                || inc.fresh_at.elapsed() >= BACKSTOP;
            if !stale && !inc.shooting {
                return;
            }
            inc.force = !inc.shooting;
            self.shared.cv.notify_one();
            inc.shots
        };
        let t = Instant::now();
        let mut drain = [0u8; 64];
        loop {
            // pump() runs next and takes whatever arrived; the pipe only wakes us.
            while matches!(read(&self.wake_r, &mut drain), Ok(n) if n > 0) {}
            if self.shared.inc.lock().shots != n || t.elapsed() >= Duration::from_millis(500) {
                break;
            }
            crate::hub::wait_any(&[self.wake_r.as_fd()], 50);
        }
    }

    /// The current frame as packed BGRX (`width * 4` bytes per row), for screenshots.
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        if self.stream.shooter.is_some() {
            self.refresh();
        }
        // Each frame arrives together with its changed areas, so taking them and then
        // adopting the newest frame leaves `changes` holding the damage for exactly
        // these pixels.
        self.pump()?;
        self.grab(self.full())?;
        let (w, h) = (self.width as usize, self.height as usize);
        let mut out = vec![0u8; w * h * 4];
        let Some(f) = &self.current else {
            return Ok(out);
        };
        unsafe {
            let pb = f.pb;
            if CVPixelBufferGetWidth(pb) != w || CVPixelBufferGetHeight(pb) != h {
                return Ok(out);
            }
            let (ys, uvs) = (
                CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
                CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
            );
            let y = std::slice::from_raw_parts(CVPixelBufferGetBaseAddressOfPlane(pb, 0), ys * h);
            let uv = std::slice::from_raw_parts(
                CVPixelBufferGetBaseAddressOfPlane(pb, 1),
                uvs * h.div_ceil(2),
            );
            crate::convert::nv12_to_bgrx(y, ys, uv, uvs, w, h, &mut out);
        }
        Ok(out)
    }

    /// Frames are NV12 here, so the BGRX debug marker probe doesn't apply.
    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        None
    }

    /// The display changed or the stream ended: capture must be set up again.
    pub fn needs_rebuild(&self) -> bool {
        self.rebuild || self.stream.stopped.load(Ordering::Relaxed)
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.shared.inc.lock().closed = true;
        self.shared.cv.notify_one();
    }
}

/// Takes one-shot captures when `Incoming::due` says so, until the capturer is dropped.
fn watch(
    sh: Arc<Shared>,
    shooter: Arc<stream::Shooter>,
    wake: Arc<OwnedFd>,
    (w, h): (u32, u32),
    debug: bool,
) {
    let mut inc = sh.inc.lock();
    while !inc.closed {
        match inc.due() {
            None => sh.cv.wait(&mut inc),
            Some((t, _)) if t > Instant::now() => {
                sh.cv.wait_until(&mut inc, t);
            }
            Some((_, why)) => {
                if debug {
                    log::info!("one-shot capture ({why})");
                }
                if why == "backstop" {
                    inc.backstop = (inc.backstop * 2).min(MAX_BACKSTOP);
                }
                inc.shooting = true;
                inc.kicked_until = Some(Instant::now() + KICK);
                inc.force = false;
                inc.idle_at = None;
                inc.fresh_at = Instant::now();
                let (sh2, wake) = (sh.clone(), wake.clone());
                MutexGuard::unlocked(&mut inc, || {
                    shooter.capture(move |sb| unsafe {
                        let pb =
                            sb.map_or(std::ptr::null_mut(), |sb| CMSampleBufferGetImageBuffer(sb));
                        let mut inc = sh2.inc.lock();
                        if !pb.is_null()
                            && CVPixelBufferGetWidth(pb) == w as usize
                            && CVPixelBufferGetHeight(pb) == h as usize
                        {
                            inc.frame = Some(Frame::retain(pb));
                            inc.diff = true;
                            inc.shot = true;
                        }
                        inc.shooting = false;
                        inc.shots += 1;
                        sh2.cv.notify_one();
                        drop(inc);
                        let _ = write(&*wake, &[1u8]);
                    })
                });
            }
        }
    }
}

/// One-shot captures are scaled differently from stream frames: the same screen
/// differs by up to about 13 in luma around text and edges. Smaller differences don't
/// count as changes.
const SHOT_TOLERANCE: u8 = 32;

/// 16x16 blocks where two NV12 frames differ by more than `tol`, as (x0, y0, x1, y1).
fn changed_blocks(
    a: &mut Frame,
    b: &mut Frame,
    w: u32,
    h: u32,
    tol: u8,
) -> Vec<(u32, u32, u32, u32)> {
    a.lock();
    b.lock();
    let size = |pb| unsafe {
        (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb)) == (w as usize, h as usize)
    };
    if !size(a.pb) || !size(b.pb) {
        return vec![(0, 0, w, h)];
    }
    let pbs = [a.pb, b.pb];
    diff_blocks(w, h, tol, |i, plane, y| unsafe {
        let base = CVPixelBufferGetBaseAddressOfPlane(pbs[i], plane);
        let stride = CVPixelBufferGetBytesPerRowOfPlane(pbs[i], plane);
        // Luma is one byte per pixel; chroma one CbCr pair per two pixels.
        let len = if plane == 0 { w } else { w.div_ceil(2) * 2 };
        std::slice::from_raw_parts(base.add(y as usize * stride), len as usize)
    })
}

/// 16x16 blocks where two NV12 frames differ by more than `tol`, merged into runs along
/// each 16-row band and then down, as (x0, y0, x1, y1). `row(frame, plane, y)` is row y of
/// plane 0 (luma) or 1 (chroma) of frame 0 or 1. The rectangles drive the encoder's
/// macroblock map and `/wait` regions, so they keep the changed columns too.
fn diff_blocks<'a>(
    w: u32,
    h: u32,
    tol: u8,
    row: impl Fn(usize, usize, u32) -> &'a [u8],
) -> Vec<(u32, u32, u32, u32)> {
    let cols = w.div_ceil(16) as usize;
    let mut dirty = vec![false; cols];
    let mut out: Vec<(u32, u32, u32, u32)> = Vec::new();
    // Runs ending at the current band, which a run in it can extend.
    let (mut above, mut here) = (Vec::<usize>::new(), Vec::new());
    for y0 in (0..h).step_by(16) {
        let y1 = (y0 + 16).min(h);
        dirty.fill(false);
        for y in y0..y1 {
            // Chroma rows cover two luma rows; their bytes line up with luma columns.
            let planes: &[(usize, u32)] = if y % 2 == 0 {
                &[(0, y), (1, y / 2)]
            } else {
                &[(0, y)]
            };
            for &(plane, py) in planes {
                let (ra, rb) = (row(0, plane, py), row(1, plane, py));
                if ra == rb {
                    continue;
                }
                for (d, (ca, cb)) in dirty.iter_mut().zip(ra.chunks(16).zip(rb.chunks(16))) {
                    *d = *d || (ca != cb && ca.iter().zip(cb).any(|(x, y)| x.abs_diff(*y) > tol));
                }
            }
            if dirty.iter().all(|&d| d) {
                break;
            }
        }
        here.clear();
        let mut c = 0;
        while c < cols {
            if !dirty[c] {
                c += 1;
                continue;
            }
            let c0 = c;
            while c < cols && dirty[c] {
                c += 1;
            }
            let (x0, x1) = (c0 as u32 * 16, (c as u32 * 16).min(w));
            let i = match above.iter().find(|&&i| out[i].0 == x0 && out[i].2 == x1) {
                Some(&i) => {
                    out[i].3 = y1;
                    i
                }
                None => {
                    out.push((x0, y0, x1, y1));
                    out.len() - 1
                }
            };
            here.push(i);
        }
        std::mem::swap(&mut above, &mut here);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 64x48 NV12 frames with flat luma and chroma; `edit` changes the second.
    fn diff(tol: u8, edit: impl Fn(&mut [Vec<u8>; 2])) -> Vec<(u32, u32, u32, u32)> {
        let (w, h) = (64u32, 48u32);
        let a = [
            vec![100u8; (w * h) as usize],
            vec![128u8; (w * h / 2) as usize],
        ];
        let mut b = a.clone();
        edit(&mut b);
        let frames = [a, b];
        diff_blocks(w, h, tol, |i, plane, y| {
            let s = &frames[i][plane];
            &s[(y * w) as usize..((y + 1) * w) as usize]
        })
    }

    #[test]
    fn changed_blocks_keep_their_columns() {
        // One pixel at the left edge changes only the leftmost block of its band.
        assert_eq!(diff(0, |b| b[0][20 * 64] = 120), vec![(0, 16, 16, 32)]);
        // Neighbouring blocks merge along the band, and down when they line up.
        let r = diff(0, |b| {
            for y in 0..48 {
                b[0][y * 64 + 20] = 0;
                b[0][y * 64 + 40] = 0;
            }
        });
        assert_eq!(r, vec![(16, 0, 48, 48)]);
        // Chroma changes count; differences within the tolerance don't.
        assert_eq!(diff(0, |b| b[1][5 * 64 + 63] = 0), vec![(48, 0, 64, 16)]);
        assert_eq!(diff(32, |b| b[0][0] = 120), vec![]);
        assert_eq!(diff(0, |b| b[0][0] = 101), vec![(0, 0, 16, 16)]);
    }
}
