//! Linux capture selection. X11 retains its direct Xvfb path; Wayland is opt-in.

use std::ops::{Deref, DerefMut};
use std::os::fd::BorrowedFd;
use std::time::Instant;

pub static DAMAGE_EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

pub struct CaptureState {
    pub width: u32,
    pub height: u32,
    pub damaged: bool,
    pub damaged_at: Option<Instant>,
    pub changes: Vec<(u32, u32, u32, u32)>,
}

impl CaptureState {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            damaged: true,
            damaged_at: None,
            changes: Vec::new(),
        }
    }
}

pub enum Capturer {
    X11(Box<crate::capture_x11::Capturer>),
    Wayland(Box<crate::wayland::Capturer>),
}

macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self {
            Self::X11(c) => c.$method($($arg),*),
            Self::Wayland(c) => c.$method($($arg),*),
        }
    };
}

impl Deref for Capturer {
    type Target = CaptureState;
    fn deref(&self) -> &CaptureState {
        match self {
            Self::X11(c) => &c.state,
            Self::Wayland(c) => &c.state,
        }
    }
}
impl DerefMut for Capturer {
    fn deref_mut(&mut self) -> &mut CaptureState {
        match self {
            Self::X11(c) => &mut c.state,
            Self::Wayland(c) => &mut c.state,
        }
    }
}
impl Capturer {
    pub fn new(
        display: Option<&str>,
        wayland: Option<&crate::wayland::Config>,
    ) -> Result<Self, String> {
        match wayland {
            Some(cfg) => crate::wayland::Capturer::new(cfg).map(|c| Self::Wayland(Box::new(c))),
            None => crate::capture_x11::Capturer::new(display).map(|c| Self::X11(Box::new(c))),
        }
    }
    pub fn fd(&self) -> BorrowedFd<'_> {
        delegate!(self, fd)
    }
    pub fn pump(&mut self) -> Result<(), String> {
        delegate!(self, pump)
    }
    pub fn needs_rebuild(&self) -> bool {
        delegate!(self, needs_rebuild)
    }
    pub fn geometry_changed(&self) -> bool {
        delegate!(self, geometry_changed)
    }
    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        delegate!(self, take_damage)
    }
    pub fn take_mb_map(&mut self) -> Vec<u8> {
        delegate!(self, take_mb_map)
    }
    pub fn grab(&mut self, band: Band) -> Result<(), String> {
        delegate!(self, grab, band)
    }
    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        delegate!(self, convert, dst, y0, y1, threads)
    }
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        delegate!(self, snapshot)
    }
    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        delegate!(self, bgrx)
    }
}
