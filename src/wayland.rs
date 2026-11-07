//! Experimental ext-image-copy-capture SHM backend. All protocol dispatch and pixel
//! reads happen on the capture thread. The compositor writes only the other slot.

use crate::capture::{Band, CaptureState, DAMAGE_EVENTS};
use memmap2::{MmapMut, MmapOptions};
use nix::poll::{poll, PollFd, PollFlags};
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as manager,
    ext_image_copy_capture_session_v1 as session,
};

type Rect = (u32, u32, u32, u32);

#[derive(Clone)]
pub struct Config {
    pub display: Option<String>,
    pub output: Option<String>,
}

/// Damage is a bounded bitset, so a blocked encoder cannot accumulate an unbounded
/// rectangle list. The same representation tracks each capture slot's stale pixels.
struct Damage {
    width: u32,
    height: u32,
    blocks: Vec<u8>,
}
impl Damage {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            blocks: vec![0; (width as usize).div_ceil(16) * (height as usize).div_ceil(16)],
        }
    }
    fn add(&mut self, (x0, y0, x1, y1): Rect) {
        let (x0, y0, x1, y1) = (
            x0.min(self.width),
            y0.min(self.height),
            x1.min(self.width),
            y1.min(self.height),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let cols = self.width.div_ceil(16) as usize;
        for y in y0 as usize / 16..(y1 as usize).div_ceil(16) {
            self.blocks[y * cols + x0 as usize / 16..y * cols + (x1 as usize).div_ceil(16)].fill(1);
        }
    }
    fn full(&mut self) {
        self.blocks.fill(1);
    }
    fn clear(&mut self) {
        self.blocks.fill(0);
    }
    fn rects(&self) -> Vec<Rect> {
        let cols = self.width.div_ceil(16) as usize;
        let mut out: Vec<Rect> = Vec::new();
        for (y, row) in self.blocks.chunks(cols).enumerate() {
            let mut x = 0;
            while x < cols {
                if row[x] == 0 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < cols && row[x] != 0 {
                    x += 1;
                }
                let rect = (
                    start as u32 * 16,
                    y as u32 * 16,
                    (x as u32 * 16).min(self.width),
                    (y as u32 * 16 + 16).min(self.height),
                );
                // Full-width or single-span damage needs one protocol request,
                // rather than one per macroblock row.
                if let Some(last) = out.last_mut() {
                    if last.0 == rect.0 && last.2 == rect.2 && last.3 == rect.1 {
                        last.3 = rect.3;
                        continue;
                    }
                }
                out.push(rect);
            }
        }
        out
    }
}

struct Slot {
    buffer: wl_buffer::WlBuffer,
    pixels: MmapMut,
    stale: Damage,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

#[derive(Default)]
struct Wire {
    shm: Option<wl_shm::WlShm>,
    manager: Option<manager::ExtImageCopyCaptureManagerV1>,
    sources:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    outputs: Vec<(u32, wl_output::WlOutput, String)>,
    selected: Option<u32>,
    size: (u32, u32),
    format: Option<wl_shm::Format>,
    constraints: bool,
    initialized: bool,
    sync_done: bool,
    ready: bool,
    damage: Vec<Rect>,
    rebuild: bool,
    error: Option<String>,
}

pub struct Capturer {
    pub state: CaptureState,
    conn: Connection,
    queue: EventQueue<Wire>,
    wire: Wire,
    session: session::ExtImageCopyCaptureSessionV1,
    slots: Vec<Slot>,
    current: Option<usize>,
    pending: Option<(frame::ExtImageCopyCaptureFrameV1, usize)>,
    dirty: Damage,
    band: Option<Band>,
}

fn dispatch(
    conn: &Connection,
    queue: &mut EventQueue<Wire>,
    wire: &mut Wire,
    timeout: u16,
) -> Result<(), String> {
    queue.dispatch_pending(wire).map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    if let Some(guard) = queue.prepare_read() {
        let mut fds = [PollFd::new(conn.as_fd(), PollFlags::POLLIN)];
        poll(&mut fds, timeout).map_err(|e| e.to_string())?;
        if fds[0].revents().is_some_and(|r| !r.is_empty()) {
            guard.read().map_err(|e| e.to_string())?;
        }
    }
    queue.dispatch_pending(wire).map_err(|e| e.to_string())?;
    if let Some(e) = &wire.error {
        return Err(e.clone());
    }
    Ok(())
}

fn sync(conn: &Connection, queue: &mut EventQueue<Wire>, wire: &mut Wire) -> Result<(), String> {
    wire.sync_done = false;
    conn.display().sync(&queue.handle(), ());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !wire.sync_done {
        if Instant::now() >= deadline {
            return Err("Wayland initialization timed out".into());
        }
        dispatch(conn, queue, wire, 50)?;
    }
    Ok(())
}

impl Capturer {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let conn = if let Some(name) = &cfg.display {
            let p = PathBuf::from(name);
            let p = if p.is_absolute() {
                p
            } else {
                PathBuf::from(
                    std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is unset")?,
                )
                .join(p)
            };
            Connection::from_socket(UnixStream::connect(p).map_err(|e| e.to_string())?)
        } else {
            Connection::connect_to_env()
        }
        .map_err(|e| e.to_string())?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut wire = Wire::default();
        sync(&conn, &mut queue, &mut wire)?;
        sync(&conn, &mut queue, &mut wire)?;
        let (id, output, name) = wire
            .outputs
            .iter()
            .find(|(_, _, name)| cfg.output.as_ref().is_none_or(|wanted| wanted == name))
            .ok_or("Wayland output not found")?;
        log::info!("Wayland output {name}: ext-image-copy-capture, SHM, embedded cursor");
        wire.selected = Some(*id);
        let source = wire
            .sources
            .as_ref()
            .ok_or("compositor lacks ext-output-image-capture-source-v1")?
            .create_source(output, &qh, ());
        let session = wire
            .manager
            .as_ref()
            .ok_or("compositor lacks ext-image-copy-capture-v1")?
            .create_session(&source, manager::Options::PaintCursors, &qh, ());
        source.destroy();
        sync(&conn, &mut queue, &mut wire)?;
        if !wire.constraints {
            return Err("Wayland capture supplied no buffer constraints".into());
        }
        let (width, height) = wire.size;
        // Bounded allocation and even geometry for the existing I420/x264 pipeline.
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || width % 2 != 0
            || height % 2 != 0
        {
            return Err(format!("unsupported Wayland capture size {width}x{height}; expected even dimensions up to 16384"));
        }
        let format = wire
            .format
            .ok_or("Wayland capture requires XRGB8888 or ARGB8888 SHM")?;
        let shm = wire.shm.as_ref().ok_or("compositor lacks wl_shm")?;
        let mut slots = Vec::new();
        for _ in 0..2 {
            let fd = unsafe { libc::memfd_create(c"voxl-wayland".as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let len = width as usize * height as usize * 4;
            file.set_len(len as u64).map_err(|e| e.to_string())?;
            // The mapping outlives wl_buffer. Only the compositor writes the pending
            // slot; reads start after ready and stop before that slot is reused.
            let pixels =
                unsafe { MmapOptions::new().len(len).map_mut(&file) }.map_err(|e| e.to_string())?;
            let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                width as i32 * 4,
                format,
                &qh,
                (),
            );
            pool.destroy();
            let mut stale = Damage::new(width, height);
            stale.full();
            slots.push(Slot {
                buffer,
                pixels,
                stale,
            });
        }
        wire.initialized = true;
        let mut cap = Self {
            state: CaptureState::new(width, height),
            conn,
            queue,
            wire,
            session,
            slots,
            current: None,
            pending: None,
            dirty: Damage::new(width, height),
            band: None,
        };
        cap.request()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while cap.current.is_none() {
            if Instant::now() >= deadline {
                return Err("Wayland first frame timed out".into());
            }
            dispatch(&cap.conn, &mut cap.queue, &mut cap.wire, 50)?;
            if cap.wire.rebuild {
                return Err("Wayland output changed during initialization".into());
            }
            cap.adopt();
        }
        cap.request()?;
        Ok(cap)
    }

    fn request(&mut self) -> Result<(), String> {
        if self.pending.is_some() || self.wire.rebuild {
            return Ok(());
        }
        let i = self.current.map_or(0, |i| 1 - i);
        let frame = self.session.create_frame(&self.queue.handle(), ());
        frame.attach_buffer(&self.slots[i].buffer);
        for (x0, y0, x1, y1) in self.slots[i].stale.rects() {
            frame.damage_buffer(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32);
        }
        self.wire.damage.clear();
        self.wire.ready = false;
        frame.capture();
        self.pending = Some((frame, i));
        self.conn.flush().map_err(|e| e.to_string())
    }

    fn adopt(&mut self) {
        if !self.wire.ready {
            return;
        }
        let Some((frame, i)) = self.pending.take() else {
            return;
        };
        frame.destroy();
        self.wire.ready = false;
        self.slots[i].stale.clear();
        for rect in self.wire.damage.drain(..) {
            self.dirty.add(rect);
            self.slots[1 - i].stale.add(rect);
            self.state.changes.push(rect);
            let (_, y0, _, y1) = rect;
            self.band = Some(match self.band {
                Some(b) => Band {
                    y0: b.y0.min(y0),
                    y1: b.y1.max(y1),
                },
                None => Band { y0, y1 },
            });
            self.state.damaged = true;
            self.state.damaged_at.get_or_insert_with(Instant::now);
            DAMAGE_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
        self.current = Some(i);
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.as_fd()
    }
    pub fn pump(&mut self) -> Result<(), String> {
        dispatch(&self.conn, &mut self.queue, &mut self.wire, 0)?;
        if !self.wire.rebuild {
            self.adopt();
            self.request()?;
        }
        Ok(())
    }
    pub fn needs_rebuild(&self) -> bool {
        self.wire.rebuild
    }
    pub fn geometry_changed(&self) -> bool {
        self.wire.rebuild
    }
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.state.damaged = false;
        self.state.damaged_at = None;
        Ok(self.band.take().map(|b| Band {
            y0: b.y0 & !1,
            y1: ((b.y1 + 1) & !1).min(self.state.height),
        }))
    }
    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let mut out = vec![0; self.dirty.blocks.len()];
        std::mem::swap(&mut out, &mut self.dirty.blocks);
        out
    }
    pub fn grab(&mut self, _band: Band) -> Result<(), String> {
        Ok(())
    }
    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        self.current
            .map(|i| (&self.slots[i].pixels[..], self.state.width as usize * 4))
    }
    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        if let Some((pixels, stride)) = self.bgrx() {
            crate::convert::convert_band(pixels, stride, dst, y0, y1, threads);
        }
    }
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        // Do not wait for damage on a static screen: adopt any completed capture,
        // then copy its pixels and leave the matching activity for note_changes().
        self.pump()?;
        if self.wire.rebuild {
            return Err("Wayland output changed during screenshot".into());
        }
        Ok(self.bgrx().ok_or("Wayland frame unavailable")?.0.to_vec())
    }
}
impl Drop for Capturer {
    fn drop(&mut self) {
        if let Some((frame, _)) = self.pending.take() {
            frame.destroy();
        }
        self.session.destroy();
        let _ = self.conn.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Wire {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => s.shm = Some(registry.bind(name, 1, q, ())),
                "wl_output" if version >= 4 => {
                    s.outputs
                        .push((name, registry.bind(name, 4, q, ()), String::new()))
                }
                "ext_image_copy_capture_manager_v1" => {
                    s.manager = Some(registry.bind(name, 1, q, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    s.sources = Some(registry.bind(name, 1, q, ()))
                }
                _ => (),
            },
            wl_registry::Event::GlobalRemove { name } if s.selected == Some(name) => {
                s.error = Some("captured Wayland output was removed".into())
            }
            _ => (),
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for Wire {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = s.outputs.iter_mut().find(|(_, o, _)| o == output) {
                o.2 = name;
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for Wire {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.sync_done = true;
    }
}
impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for Wire {
    fn event(
        s: &mut Self,
        _: &session::ExtImageCopyCaptureSessionV1,
        event: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            session::Event::BufferSize { width, height } => s.size = (width, height),
            session::Event::ShmFormat {
                format: WEnum::Value(f),
            } if matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) => {
                s.format = Some(f)
            }
            session::Event::Done => {
                s.constraints = true;
                if s.initialized {
                    s.rebuild = true;
                }
            }
            session::Event::Stopped => s.error = Some("Wayland capture session stopped".into()),
            _ => (),
        }
    }
}
impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for Wire {
    fn event(
        s: &mut Self,
        _: &frame::ExtImageCopyCaptureFrameV1,
        event: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            frame::Event::Ready => s.ready = true,
            frame::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                let x0 = x.max(0) as u32;
                let y0 = y.max(0) as u32;
                let x1 = (i64::from(x) + i64::from(width)).clamp(0, i64::from(s.size.0)) as u32;
                let y1 = (i64::from(y) + i64::from(height)).clamp(0, i64::from(s.size.1)) as u32;
                if x1 > x0 && y1 > y0 {
                    s.damage.push((x0, y0, x1, y1));
                }
            }
            frame::Event::Transform { transform }
                if transform != WEnum::Value(wl_output::Transform::Normal) =>
            {
                s.error = Some(
                    "rotated/flipped Wayland capture is not supported by this prototype".into(),
                )
            }
            frame::Event::Failed {
                reason: WEnum::Value(frame::FailureReason::BufferConstraints),
            } => s.rebuild = true,
            frame::Event::Failed { reason } => {
                s.error = Some(format!("Wayland capture failed: {reason:?}"))
            }
            _ => (),
        }
    }
}

delegate_noop!(Wire: ignore wl_shm::WlShm);
delegate_noop!(Wire: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Wire: ignore wl_buffer::WlBuffer);
delegate_noop!(Wire: ignore manager::ExtImageCopyCaptureManagerV1);
delegate_noop!(Wire: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(Wire: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn damage_accumulates_and_clips_to_partial_edge_blocks() {
        let mut d = Damage::new(34, 18);
        d.add((1, 1, 2, 2));
        d.add((32, 16, 100, 100));
        assert_eq!(d.blocks, [1, 0, 0, 0, 0, 1]);
        assert_eq!(d.rects(), [(0, 0, 16, 16), (32, 16, 34, 18)]);
        d.clear();
        assert!(d.rects().is_empty());
    }
}
