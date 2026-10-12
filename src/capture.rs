//! X11 screen capture: MIT-SHM GetImage into a persistent BGRX frame, driven by XDamage.

use std::os::fd::AsRawFd;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

pub struct ShmBuf {
    pub ptr: *mut u8,
    pub len: usize,
    shmid: i32,
    /// Attached to someone else's segment (never removed by us).
    foreign: bool,
}

unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn new(len: usize) -> Result<Self, String> {
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return Err("shmget failed".into());
            }
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                return Err("shmat failed".into());
            }
            Ok(ShmBuf { ptr: ptr as *mut u8, len, shmid, foreign: false })
        }
    }

    fn attach_readonly(shmid: i32, len: usize) -> Option<Self> {
        unsafe {
            let ptr = libc::shmat(shmid, std::ptr::null(), libc::SHM_RDONLY);
            if ptr as isize == -1 {
                return None;
            }
            Some(ShmBuf { ptr: ptr as *mut u8, len, shmid, foreign: true })
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.ptr as *const _);
            if !self.foreign {
                libc::shmctl(self.shmid, libc::IPC_RMID, std::ptr::null_mut());
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

pub struct Capturer {
    pub conn: RustConnection,
    root: u32,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    seg: u32,
    buf: ShmBuf,
    /// Byte offset of pixel data inside `buf` (non-zero for an Xvfb XWD framebuffer).
    offset: usize,
    /// Reading Xvfb's framebuffer in place: no GetImage needed.
    pub direct: bool,
    damage: u32,
    region: u32,
    pub damaged: bool,
    /// When the first damage since the last take arrived.
    pub damaged_at: Option<std::time::Instant>,
}

impl Capturer {
    pub fn new(display: Option<&str>) -> Result<Self, String> {
        let (conn, screen_num) = RustConnection::connect(display).map_err(|e| format!("X connect: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = screen.width_in_pixels as u32;
        let height = screen.height_in_pixels as u32;
        if screen.root_depth != 24 && screen.root_depth != 32 {
            return Err(format!("unsupported root depth {}", screen.root_depth));
        }
        conn.extension_information(shm::X11_EXTENSION_NAME).map_err(|e| e.to_string())?.ok_or("no MIT-SHM")?;
        conn.shm_query_version().map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        conn.damage_query_version(1, 1).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        let mut stride = width as usize * 4;
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        let (buf, offset, direct) = match find_xvfb_framebuffer(&conn, root, width, height) {
            Some((buf, offset, fb_stride)) => {
                log::info!("capturing directly from the Xvfb framebuffer (shmid {})", buf.shmid);
                stride = fb_stride;
                (buf, offset, true)
            }
            None => {
                let buf = ShmBuf::new(stride * height as usize)?;
                conn.shm_attach(seg, buf.shmid as u32, false).map_err(|e| e.to_string())?.check().map_err(|e| e.to_string())?;
                // Mark for removal now; it stays alive while attached.
                unsafe { libc::shmctl(buf.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                log::info!("capturing via MIT-SHM GetImage");
                (buf, 0, false)
            }
        };
        let damage = conn.generate_id().map_err(|e| e.to_string())?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY).map_err(|e| e.to_string())?;
        let region = conn.generate_id().map_err(|e| e.to_string())?;
        conn.xfixes_create_region(region, &[]).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        Ok(Capturer { conn, root, width, height, stride, seg, buf, offset, direct, damage, region, damaged: true, damaged_at: None })
    }

    pub fn fd(&self) -> i32 {
        self.conn.stream().as_raw_fd()
    }

    /// Drain queued X events; returns true if damage was reported.
    pub fn pump(&mut self) -> Result<(), String> {
        while let Some(ev) = self.conn.poll_for_event().map_err(|e| e.to_string())? {
            if let Event::DamageNotify(_) = ev {
                self.damaged = true;
                self.damaged_at.get_or_insert_with(std::time::Instant::now);
            }
        }
        Ok(())
    }

    /// Take accumulated damage as a row band (re-arms damage reporting).
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.conn.damage_subtract(self.damage, x11rb::NONE, self.region).map_err(|e| e.to_string())?;
        let reply = self.conn.xfixes_fetch_region(self.region).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        self.damaged = false;
        self.damaged_at = None;
        let mut y0 = u32::MAX;
        let mut y1 = 0u32;
        for r in &reply.rectangles {
            let top = r.y.max(0) as u32;
            let bot = (r.y as i32 + r.height as i32).clamp(0, self.height as i32) as u32;
            if bot > top {
                y0 = y0.min(top);
                y1 = y1.max(bot);
            }
        }
        if y1 <= y0 {
            return Ok(None);
        }
        // Align to even rows for 4:2:0.
        Ok(Some(Band { y0: y0 & !1, y1: ((y1 + 1) & !1).min(self.height) }))
    }

    pub fn full(&self) -> Band {
        Band { y0: 0, y1: self.height }
    }

    /// Current frame pixels (BGRX, `stride` bytes per row).
    pub fn pixels(&self) -> &[u8] {
        &self.buf.as_slice()[self.offset..self.offset + self.stride * self.height as usize]
    }

    /// Grab a full-width band into the shm frame at its natural offset.
    pub fn grab(&self, band: Band) -> Result<(), String> {
        let h = band.y1 - band.y0;
        if h == 0 || self.direct {
            return Ok(());
        }
        let off = band.y0 as usize * self.stride;
        self.conn
            .shm_get_image(self.root, 0, band.y0 as i16, self.width as u16, h as u16, !0, ImageFormat::Z_PIXMAP.into(), self.seg, off as u32)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(())
    }

}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]])
}

/// Xvfb started with `-shmem` keeps each screen as an XWD image in a SysV segment.
/// Find it, check the header and verify pixels against the server before trusting it.
fn find_xvfb_framebuffer(conn: &RustConnection, root: u32, width: u32, height: u32) -> Option<(ShmBuf, usize, usize)> {
    if std::env::var("SRS_NO_DIRECT_FB").is_ok() {
        return None;
    }
    let table = std::fs::read_to_string("/proc/sysvipc/shm").ok()?;
    for line in table.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        let (Ok(shmid), Ok(size)) = (f[1].parse::<i32>(), f[3].parse::<usize>()) else { continue };
        if size < (width * height * 4) as usize || size > (width * height * 4) as usize + (1 << 16) {
            continue;
        }
        let Some(buf) = ShmBuf::attach_readonly(shmid, size) else { continue };
        let b = buf.as_slice();
        let header_size = be32(b, 0) as usize;
        let (w, h, bpp, bpl, ncolors) = (be32(b, 4), be32(b, 5), be32(b, 11), be32(b, 12) as usize, be32(b, 19) as usize);
        if be32(b, 1) != 7 || w != width || h != height || bpp != 32 || bpl < (w * 4) as usize {
            continue;
        }
        let offset = header_size + ncolors * 12;
        if offset + bpl * h as usize > size {
            continue;
        }
        // Compare a strip of real pixels with what the server reports.
        let (sy, sh) = (height / 2, 16u32.min(height / 2));
        let Ok(Ok(img)) = conn.get_image(ImageFormat::Z_PIXMAP, root, 0, sy as i16, width as u16, sh as u16, !0).map(|c| c.reply()) else { continue };
        let ok = (0..sh as usize).all(|r| {
            let fb = &b[offset + (sy as usize + r) * bpl..][..width as usize * 4];
            let im = &img.data[r * width as usize * 4..][..width as usize * 4];
            fb.chunks_exact(4).zip(im.chunks_exact(4)).all(|(a, c)| a[..3] == c[..3])
        });
        if ok {
            return Some((buf, offset, bpl));
        }
    }
    None
}

