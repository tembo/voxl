//! Dynamic resolution on macOS: switches the captured display to the mode that best
//! fits the viewer's window. Displays only offer fixed modes, so the desktop takes
//! the largest one that fits rather than the exact size. The change is made for this
//! process only: macOS restores the user's mode when voxl exits, even on a crash.
//!
//! With --rotate-display a portrait window (a phone held upright) also turns the
//! display on its side, so the desktop is portrait too. The rotation is the same one
//! as in System Settings > Displays; voxl turns the display back when it exits.

use super::ffi::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Smallest desktop a client may ask for.
pub const MIN: (u32, u32) = (640, 480);

/// Streamed sizes of the selectable modes, for `fit`.
static SIZES: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// --rotate-display was given.
static ROTATE_ASKED: AtomicBool = AtomicBool::new(false);

/// Portrait requests rotate the display: asked for, and the display can turn.
static ROTATES: AtomicBool = AtomicBool::new(false);

/// A display as voxl first saw it, to put it back: the angle, the mode (in points),
/// and whether voxl rotated it since. Rotating makes the mode of the moment the saved
/// one, so macOS no longer restores the user's mode by itself.
#[derive(Clone, Copy)]
struct Start {
    angle: i32,
    points: (usize, usize),
    rotated: bool,
}

static STARTS: Mutex<Vec<(u32, Start)>> = Mutex::new(Vec::new());

fn start(display: u32) -> Option<Start> {
    let starts = STARTS.lock();
    starts.iter().find(|s| s.0 == display).map(|s| s.1)
}

pub fn set_rotate(on: bool) {
    ROTATE_ASKED.store(on, Ordering::Relaxed);
}

fn long_short(s: (u32, u32)) -> (u32, u32) {
    (s.0.max(s.1), s.0.min(s.1))
}

/// The size the desktop would take for a request of `w`x`h` pixels: the largest
/// mode that fits, or the smallest mode if none does.
pub fn fit(w: u32, h: u32, max: (u32, u32)) -> (u32, u32) {
    pick(&SIZES.lock(), w, h, ROTATES.load(Ordering::Relaxed)).unwrap_or(max)
}

/// `rotates`: the display turns to the request's orientation, so sizes are compared
/// long side to long side, and the answer has the request's orientation.
fn pick(sizes: &[(u32, u32)], w: u32, h: u32, rotates: bool) -> Option<(u32, u32)> {
    let side = |s: (u32, u32)| if rotates { long_short(s) } else { s };
    let area = |s: &(u32, u32)| s.0 as u64 * s.1 as u64;
    let want = side((w, h));
    let all = sizes.iter().map(|&s| side(s));
    let best = all
        .clone()
        .filter(|s| s.0 <= want.0 && s.1 <= want.1)
        .max_by_key(area)
        .or_else(|| all.min_by_key(area))?;
    Some(if rotates && h > w {
        (best.1, best.0)
    } else {
        best
    })
}

/// Whether a mode of `points`, streamed at `size`, is one the desktop may take. Modes
/// past the pixel budget would cost more to encode than the current one, which always
/// stays available. So does the mode the display started with (`home`, in either
/// orientation), or it could not be put back.
fn selectable(
    size: (u32, u32),
    points: (usize, usize),
    current: (usize, usize),
    home: Option<(usize, usize)>,
    max_pixels: f64,
) -> bool {
    let (long, short) = long_short(size);
    if long < MIN.0 || short < MIN.1 {
        return false;
    }
    let sides = |p: (usize, usize)| (p.0.max(p.1), p.0.min(p.1));
    size.0 as f64 * size.1 as f64 <= max_pixels * 1.02
        || points == current
        || home.is_some_and(|home| sides(home) == sides(points))
}

/// The angle for a portrait or a landscape desktop on a display that started at
/// `start` degrees: the display's own angle when that has the wanted orientation, else
/// a quarter turn from it. A display found on its side goes to 0 for landscape, never
/// to 180, where the desktop would be upside down.
fn angle_for(start: i32, portrait: bool) -> i32 {
    match (portrait, start % 180 != 0) {
        (true, true) | (false, false) => start,
        (true, false) => (start + 90) % 360,
        (false, true) => 0,
    }
}

/// The size to ask for so that the display goes back to its own angle and mode, once
/// nobody is watching. None when voxl left the display as it was, or does not rotate.
pub fn home() -> Option<(u32, u32)> {
    if !ROTATES.load(Ordering::Relaxed) {
        return None;
    }
    let display = super::display();
    let start = start(display)?;
    let b = unsafe { CGDisplayBounds(display) };
    let now = (b.size.width as usize, b.size.height as usize);
    if unsafe { CGDisplayRotation(display) } as i32 == start.angle && now == start.points {
        return None;
    }
    Some(super::size_for(
        start.points.0 as f64,
        start.points.1 as f64,
    ))
}

/// Turns every display voxl rotated back to the angle and the mode it started with.
pub fn restore_rotation() {
    // A copy: reading a display's modes looks its start up to keep that mode listed.
    let starts = STARTS.lock().clone();
    for (display, start) in starts {
        if !start.rotated {
            continue;
        }
        let restore = || -> Result<(), String> {
            if unsafe { CGDisplayRotation(display) } as i32 != start.angle {
                turn(display, start.angle)?;
            }
            let screen = Screen::open_on(display)?;
            let m = screen
                .modes
                .iter()
                .find(|m| m.points == start.points)
                .ok_or("its mode is gone")?;
            apply(display, m.mode, kCGConfigurePermanently)
        };
        if let Err(e) = restore() {
            log::warn!("could not turn display {display} back: {e}");
        }
    }
}

fn portrait(display: u32) -> bool {
    let b = unsafe { CGDisplayBounds(display) };
    b.size.height > b.size.width
}

/// Rotates the display to `degrees` and waits for it.
fn turn(display: u32, degrees: i32) -> Result<(), String> {
    rotation::set(display, degrees)?;
    let t = Instant::now();
    while unsafe { CGDisplayRotation(display) } as i32 != degrees {
        if t.elapsed() > Duration::from_secs(3) {
            return Err("the display did not rotate".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    log::info!("display rotated to {degrees} degrees");
    Ok(())
}

fn apply(display: u32, mode: CGDisplayModeRef, option: u32) -> Result<(), String> {
    unsafe {
        let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut cfg) != 0 {
            return Err("CGBeginDisplayConfiguration failed".into());
        }
        let e = CGConfigureDisplayWithDisplayMode(cfg, display, mode, std::ptr::null());
        if e != 0 {
            CGCancelDisplayConfiguration(cfg);
            return Err(format!("CGConfigureDisplayWithDisplayMode: {e}"));
        }
        let e = CGCompleteDisplayConfiguration(cfg, option);
        if e != 0 {
            return Err(format!("CGCompleteDisplayConfiguration: {e}"));
        }
    }
    Ok(())
}

/// Display rotation through MonitorPanel, the private framework behind System
/// Settings > Displays (Apple Silicon has no public API for it).
mod rotation {
    use objc2::msg_send;
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::{AnyClass, AnyObject};

    fn find(display: u32) -> Option<Retained<AnyObject>> {
        static LOAD: std::sync::Once = std::sync::Once::new();
        LOAD.call_once(|| unsafe {
            libc::dlopen(
                c"/System/Library/PrivateFrameworks/MonitorPanel.framework/MonitorPanel".as_ptr(),
                libc::RTLD_NOW,
            );
        });
        let class = AnyClass::get(c"MPDisplayMgr")?;
        unsafe {
            let manager: Option<Retained<AnyObject>> = msg_send![class, new];
            let displays: Option<Retained<AnyObject>> = msg_send![&*manager?, displays];
            let displays = displays?;
            let count: usize = msg_send![&*displays, count];
            for i in 0..count {
                let d: Option<Retained<AnyObject>> = msg_send![&*displays, objectAtIndex: i];
                let d = d?;
                let id: i32 = msg_send![&*d, displayID];
                if id as u32 == display {
                    return Some(d);
                }
            }
        }
        None
    }

    pub fn supported(display: u32) -> bool {
        autoreleasepool(|_| {
            find(display).is_some_and(|d| unsafe { msg_send![&*d, canChangeOrientation] })
        })
    }

    pub fn set(display: u32, degrees: i32) -> Result<(), String> {
        autoreleasepool(|_| {
            let d = find(display).ok_or("the display cannot be rotated")?;
            let _: () = unsafe { msg_send![&*d, setOrientation: degrees] };
            Ok(())
        })
    }
}

struct Mode {
    mode: CGDisplayModeRef,
    points: (usize, usize),
    size: (u32, u32),
}

pub struct Screen {
    display: u32,
    modes: Vec<Mode>,
    max: (u32, u32),
    current: (u32, u32),
}

unsafe impl Send for Screen {}

impl Screen {
    pub fn open(_display: Option<&str>) -> Result<Self, String> {
        Self::open_on(super::display())
    }

    fn open_on(display: u32) -> Result<Self, String> {
        let mut modes: Vec<Mode> = Vec::new();
        unsafe {
            let cur = CGDisplayCopyDisplayMode(display);
            if cur.is_null() {
                return Err("no display mode".into());
            }
            let hidpi = CGDisplayModeGetPixelWidth(cur) > CGDisplayModeGetWidth(cur);
            let current = (CGDisplayModeGetWidth(cur), CGDisplayModeGetHeight(cur));
            let rate = CGDisplayModeGetRefreshRate(cur);
            let mut starts = STARTS.lock();
            if !starts.iter().any(|s| s.0 == display) {
                starts.push((
                    display,
                    Start {
                        angle: CGDisplayRotation(display) as i32,
                        points: current,
                        rotated: false,
                    },
                ));
            }
            let home = starts.iter().find(|s| s.0 == display).map(|s| s.1.points);
            drop(starts);
            CGDisplayModeRelease(cur);
            let key = kCGDisplayShowDuplicateLowResolutionModes;
            let opts = objc2_core_foundation::CFDictionary::from_slices(
                &[&*(key as *const objc2_core_foundation::CFString)],
                &[objc2_core_foundation::CFBoolean::new(true)],
            );
            let all = CGDisplayCopyAllDisplayModes(display, &*opts as *const _ as CFDictionaryRef);
            if all.is_null() {
                return Err("no display modes".into());
            }
            for i in 0..CFArrayGetCount(all) {
                let m = CFArrayGetValueAtIndex(all, i) as CGDisplayModeRef;
                let (w, h) = (CGDisplayModeGetWidth(m), CGDisplayModeGetHeight(m));
                // Same kind of mode as now (Retina or not, same refresh), usable as a desktop.
                if !CGDisplayModeIsUsableForDesktopGUI(m)
                    || (CGDisplayModeGetPixelWidth(m) > w) != hidpi
                    || (CGDisplayModeGetRefreshRate(m) - rate).abs() > 0.5
                    || modes.iter().any(|x| x.points == (w, h))
                {
                    continue;
                }
                let size = super::size_for(w as f64, h as f64);
                if !selectable(size, (w, h), current, home, super::max_pixels()) {
                    continue;
                }
                CFRetain(m as CFTypeRef);
                modes.push(Mode {
                    mode: m,
                    points: (w, h),
                    size,
                });
            }
            CFRelease(all);
        }
        if modes.len() < 2 {
            return Err("the display offers no other modes".into());
        }
        modes.sort_by_key(|m| m.size.0 as u64 * m.size.1 as u64);
        *SIZES.lock() = modes.iter().map(|m| m.size).collect();
        ROTATES.store(
            ROTATE_ASKED.load(Ordering::Relaxed) && rotation::supported(display),
            Ordering::Relaxed,
        );
        let max = modes.last().unwrap().size;
        log::info!(
            "display modes: {}",
            modes
                .iter()
                .map(|m| format!("{}x{}", m.points.0, m.points.1))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(Screen {
            display,
            modes,
            max,
            current: super::capture_size(display),
        })
    }

    pub fn max(&self) -> (u32, u32) {
        self.max
    }

    pub fn fit(&self, w: u32, h: u32) -> (u32, u32) {
        fit(w, h, self.max)
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        let want = self.fit(w, h);
        if ROTATES.load(Ordering::Relaxed) && (want.1 > want.0) != self.portrait() {
            self.rotate(want.1 > want.0)?;
        }
        self.current = super::capture_size(self.display);
        if want == self.current {
            return Ok(());
        }
        let m = self
            .modes
            .iter()
            .find(|m| m.size == want)
            .ok_or("no such mode")?;
        apply(self.display, m.mode, kCGConfigureForAppOnly)?;
        // Capture restarts right after; let the new geometry settle first.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(2) {
            let b = unsafe { CGDisplayBounds(self.display) };
            if (b.size.width as usize, b.size.height as usize) == m.points {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.current = want;
        log::info!(
            "display set to {}x{} points ({}x{} streamed)",
            m.points.0,
            m.points.1,
            want.0,
            want.1
        );
        Ok(())
    }
}

impl Screen {
    fn portrait(&self) -> bool {
        portrait(self.display)
    }

    /// Turns the display to the angle for a portrait or a landscape desktop, then
    /// reads its modes again: they are portrait ones on a display on its side.
    fn rotate(&mut self, portrait: bool) -> Result<(), String> {
        let display = self.display;
        let start = start(display).ok_or("unknown display")?;
        turn(display, angle_for(start.angle, portrait))?;
        if let Some(s) = STARTS.lock().iter_mut().find(|s| s.0 == display) {
            s.1.rotated = true;
        }
        *self = Screen::open_on(display)?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        for m in &self.modes {
            unsafe { CGDisplayModeRelease(m.mode) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{angle_for, pick, selectable};

    /// --capture-scale=2 puts a 2560x1600-point display past the default pixel budget.
    #[test]
    fn the_mode_the_display_started_with_can_always_be_put_back() {
        let budget = 4_096_000.0;
        let home = Some((2560, 1600));
        // At startup it is the current mode.
        assert!(selectable(
            (5120, 3200),
            (2560, 1600),
            (2560, 1600),
            home,
            budget
        ));
        // After a resize it no longer is, and after a rotation its sides are swapped.
        assert!(selectable(
            (5120, 3200),
            (2560, 1600),
            (1280, 800),
            home,
            budget
        ));
        assert!(selectable(
            (3200, 5120),
            (1600, 2560),
            (800, 1280),
            home,
            budget
        ));
        // Other modes past the budget stay out, as do modes too small to use.
        assert!(!selectable(
            (4096, 2560),
            (2048, 1280),
            (1280, 800),
            home,
            budget
        ));
        assert!(!selectable(
            (5120, 3200),
            (2560, 1600),
            (1280, 800),
            None,
            budget
        ));
        assert!(selectable(
            (2560, 1600),
            (1280, 800),
            (2560, 1600),
            home,
            budget
        ));
        assert!(!selectable(
            (600, 400),
            (300, 200),
            (2560, 1600),
            home,
            budget
        ));
    }

    #[test]
    fn the_display_never_ends_upside_down() {
        // A landscape display turns a quarter for portrait, and back.
        assert_eq!(angle_for(0, true), 90);
        assert_eq!(angle_for(0, false), 0);
        assert_eq!(angle_for(180, true), 270);
        assert_eq!(angle_for(180, false), 180);
        // A display found on its side stays so for portrait, and goes upright for landscape.
        assert_eq!(angle_for(90, true), 90);
        assert_eq!(angle_for(90, false), 0);
        assert_eq!(angle_for(270, true), 270);
        assert_eq!(angle_for(270, false), 0);
    }

    const LANDSCAPE: [(u32, u32); 3] = [(864, 486), (1128, 634), (1920, 1080)];
    const PORTRAIT: [(u32, u32); 3] = [(486, 864), (634, 1128), (1080, 1920)];

    #[test]
    fn a_fixed_display_takes_the_largest_mode_that_fits() {
        assert_eq!(pick(&LANDSCAPE, 1300, 700, false), Some((1128, 634)));
        assert_eq!(pick(&LANDSCAPE, 4000, 4000, false), Some((1920, 1080)));
        // Nothing fits an upright phone: the smallest mode, still landscape.
        assert_eq!(pick(&LANDSCAPE, 292, 633, false), Some((864, 486)));
        assert_eq!(pick(&[], 292, 633, false), None);
    }

    #[test]
    fn a_rotating_display_takes_the_orientation_of_the_window() {
        // Whichever way the display is turned now.
        for sizes in [LANDSCAPE, PORTRAIT] {
            assert_eq!(pick(&sizes, 700, 1300, true), Some((634, 1128)));
            assert_eq!(pick(&sizes, 1300, 700, true), Some((1128, 634)));
            assert_eq!(pick(&sizes, 292, 633, true), Some((486, 864)));
            assert_eq!(pick(&sizes, 1170, 2532, true), Some((1080, 1920)));
            assert_eq!(pick(&sizes, 1000, 1000, true), Some((864, 486)));
        }
    }
}
