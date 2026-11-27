//! Windows backend: the same modules as the X11 one (capture, desktop, input, audio,
//! screen) on Win32 APIs, so the rest of voxl (encoder, flow control, server, web
//! client) runs unchanged.
//!
//! - capture: DXGI Desktop Duplication, BGRA frames with dirty and move rectangles
//! - input: SendInput (scan codes, keys resolved against the current layout)
//! - desktop: cursor images from GetCursorInfo, clipboard sync (text and images)
//! - audio: system audio from WASAPI loopback
//! - screen: dynamic resolution by switching display modes
//! - mic: browser microphone played into a virtual audio cable

pub mod audio;
pub mod capture;
pub mod desktop;
pub mod input;
pub mod mic;
pub mod screen;

use parking_lot::Mutex;
use std::collections::HashMap;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};

/// Where the captured display sits on the virtual desktop, in physical pixels (voxl is
/// per-monitor DPI aware, so these are the pixels it streams).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Set by the capturer whenever it (re)starts; read by input to map coordinates.
pub static GEOMETRY: Mutex<Option<Geometry>> = Mutex::new(None);

struct Settings {
    /// Index into `outputs()` (0 is the primary display).
    display: usize,
    /// --resize-scale, or None for the display's own scaling.
    resize_scale: Option<f64>,
}

static SETTINGS: Mutex<Settings> = Mutex::new(Settings {
    display: 0,
    resize_scale: None,
});

/// One display attached to the desktop.
pub struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub desc: DXGI_OUTPUT_DESC,
}

impl Output {
    /// GDI device name (`\\.\DISPLAY1`), for display mode changes.
    pub fn device_name(&self) -> Vec<u16> {
        let n = &self.desc.DeviceName;
        let len = n.iter().position(|&c| c == 0).unwrap_or(n.len());
        n[..len].iter().copied().chain([0]).collect()
    }

    pub fn rect(&self) -> RECT {
        self.desc.DesktopCoordinates
    }
}

/// Displays attached to the desktop, the primary one (at the origin) first.
pub fn outputs() -> Vec<Output> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            o += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if desc.AttachedToDesktop.as_bool() {
                out.push(Output {
                    adapter: adapter.clone(),
                    output,
                    desc,
                });
            }
        }
    }
    out.sort_by_key(|o| {
        let r = o.rect();
        (r.left != 0 || r.top != 0, r.top, r.left)
    });
    out
}

/// The display voxl streams, as picked with --display.
pub fn output() -> Option<Output> {
    let i = SETTINGS.lock().display;
    outputs().into_iter().nth(i)
}

pub struct DisplayRequest {
    /// Index into `outputs()` of the display to stream.
    pub id: u32,
    pub sender: std::sync::mpsc::Sender<input::InputEvent>,
    pub ready: std::sync::mpsc::Receiver<()>,
}

/// The streamed display: an index into `outputs()` (0 is the primary).
pub fn display() -> u32 {
    SETTINGS.lock().display as u32
}

/// Stream display `id` (an index into `outputs()`): pauses input, retires the current
/// viewers and queues the switch for the video thread. Mirrors mac::request_display.
pub fn request_display(hub: &crate::hub::Hub, id: u32, client: u64) -> Result<bool, String> {
    if id as usize >= outputs().len() {
        return Err("display is no longer connected".into());
    }
    if id == display() {
        return Ok(false);
    }
    let mut pending = hub.display_req.lock();
    let (sender, ready) = hub
        .pause_display_input(client)
        .ok_or("display switch already in progress")?;
    *pending = Some(DisplayRequest { id, sender, ready });
    hub.wake();
    Ok(true)
}

/// Point capture at display `id` (an index into `outputs()`).
pub fn select_display(id: u32) -> Result<(), String> {
    if id as usize >= outputs().len() {
        return Err("display is no longer connected".into());
    }
    SETTINGS.lock().display = id as usize;
    Ok(())
}

/// The displays `hello` advertises to the viewer's picker.
pub fn display_info() -> serde_json::Value {
    let displays: Vec<_> = outputs()
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let suffix = if i == 0 { " · Main" } else { "" };
            serde_json::json!({ "id": i as u32, "name": format!("Display {}{suffix}", i + 1) })
        })
        .collect();
    serde_json::json!({ "selected": display(), "available": displays })
}

/// Reads the Windows options. Must run before anything opens a window or a device
/// context, so every coordinate is in physical pixels.
pub fn init(a: &HashMap<String, String>) {
    if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.is_err()
    {
        log::warn!("could not make voxl DPI aware; a scaled display may stream blurry");
    }
    let outs = outputs();
    for (i, o) in outs.iter().enumerate() {
        let r = o.rect();
        log::info!(
            "display {i}: {} {}x{} at ({}, {}){}",
            String::from_utf16_lossy(&o.device_name()[..o.device_name().len() - 1]),
            r.right - r.left,
            r.bottom - r.top,
            r.left,
            r.top,
            if i == 0 { " (primary)" } else { "" }
        );
    }
    let mut s = SETTINGS.lock();
    s.resize_scale = a
        .get("resize-scale")
        .and_then(|v| v.rsplit('-').next()?.trim().parse().ok());
    // --display: an index (0 is the primary display); ":1" style values work too.
    if let Some(n) = a
        .get("display")
        .and_then(|d| d.trim_start_matches(':').parse::<usize>().ok())
    {
        if n < outs.len() {
            s.display = n;
        } else {
            log::warn!("no display {n}; using the primary display");
        }
    }
}

/// Desktop pixels per CSS pixel to resize to: the display's scaling (1.5 at 150%).
pub fn dpi_scale() -> f64 {
    let Some(o) = output() else {
        return 1.0;
    };
    let mon: HMONITOR = o.desc.Monitor;
    let (mut x, mut y) = (96u32, 96u32);
    if unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_err() {
        return 1.0;
    }
    x as f64 / 96.0
}

/// Desktop pixels per CSS pixel to resize to: --resize-scale, else the display's
/// scaling (1.5 at 150%).
pub fn resize_scale() -> f64 {
    if let Some(s) = SETTINGS.lock().resize_scale {
        return s;
    }
    dpi_scale()
}

/// UTF-16 with a terminating zero, for wide-string APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Text from a zero-terminated (or full) UTF-16 buffer.
pub fn from_wide(s: &[u16]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// Casts a COM interface, for the `IDXGIOutput` -> `IDXGIOutput1` kind of upgrade.
pub fn cast<T: Interface, U: Interface>(v: &T) -> Result<U, String> {
    v.cast::<U>().map_err(|e| e.to_string())
}

/// Puts this thread on the desktop that receives input (the lock screen or a UAC
/// prompt, when voxl runs as SYSTEM), so capture and input follow it. Does nothing
/// for a normal user, who can't open those desktops anyway.
pub fn follow_input_desktop() {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
        DESKTOP_CONTROL_FLAGS,
    };
    // GENERIC_ALL
    let access = DESKTOP_ACCESS_FLAGS(0x1000_0000);
    if let Ok(d) = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) } {
        if unsafe { SetThreadDesktop(d) }.is_err() {
            let _ = unsafe { CloseDesktop(d) };
        }
    }
}
