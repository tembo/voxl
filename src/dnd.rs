//! Drops uploaded files into the app under a point on the X11 desktop, as a drag from a
//! file manager would (--drop-into-apps): voxl acts as the XDND source on its own X
//! connection and offers the files as `text/uri-list`, which browsers, GTK and Qt apps
//! take like any local drop.

use nix::poll::{poll, PollFd, PollFlags};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        XdndAware,
        XdndProxy,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndActionCopy,
        TARGETS,
        URI_LIST: b"text/uri-list",
        VOXL_DND_TIME,
    }
}

/// The newest XDND version spoken; targets get the lower of theirs and this.
const VERSION: u32 = 5;
/// How long the app under the point has to accept the files (a page answers dragover
/// from its renderer, so the first replies are usually "not yet").
const ACCEPT_WAIT: Duration = Duration::from_secs(3);
/// How long the app has to take the files once they are dropped.
const FINISH_WAIT: Duration = Duration::from_secs(10);

type Res<T> = Result<T, String>;

fn e(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// Drops `paths` at desktop position (`x`, `y`). Returns whether an app took them;
/// false when nothing there accepts dropped files.
pub fn drop_files(display: Option<&str>, x: i16, y: i16, paths: &[PathBuf]) -> Res<bool> {
    let (conn, n) = RustConnection::connect(display).map_err(e)?;
    let root = conn.setup().roots[n].root;
    let atoms = Atoms::new(&conn).map_err(e)?.reply().map_err(e)?;
    let Some(target) = find_target(&conn, &atoms, root, x, y)? else {
        return Ok(false);
    };
    let win = conn.generate_id().map_err(e)?;
    conn.create_window(
        0,
        win,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(e)?;
    let mut src = Source {
        conn: &conn,
        atoms: &atoms,
        win,
        target,
        uris: uri_list(paths),
    };
    let time = src.server_time()?;
    conn.set_selection_owner(win, atoms.XdndSelection, time)
        .map_err(e)?;
    src.send(
        atoms.XdndEnter,
        [win, target.version << 24, atoms.URI_LIST, 0, 0],
    )?;

    // Point at the spot until the app accepts the files (or says no for long enough).
    let position = ((x as u32) << 16) | (y as u16 as u32);
    let deadline = Instant::now() + ACCEPT_WAIT;
    let mut accepted = false;
    while !accepted && Instant::now() < deadline {
        src.send(
            atoms.XdndPosition,
            [win, 0, position, time, atoms.XdndActionCopy],
        )?;
        // A target answers every position with a status before it gets the next one.
        match src.wait(deadline, atoms.XdndStatus)? {
            Some(status) => accepted = status[1] & 1 == 1,
            None => break,
        }
        if !accepted {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if !accepted {
        src.send(atoms.XdndLeave, [win, 0, 0, 0, 0])?;
        return Ok(false);
    }
    src.send(atoms.XdndDrop, [win, 0, time, 0, 0])?;
    // The app fetches the files (here or already while accepting), then says it's done.
    // Version 5 targets also say whether they used them.
    let finished = src.wait(Instant::now() + FINISH_WAIT, atoms.XdndFinished)?;
    Ok(match finished {
        Some(f) if target.version >= 5 => f[1] & 1 == 1,
        Some(_) => true,
        None => {
            log::warn!("file drop: the app did not finish taking the files");
            true
        }
    })
}

#[derive(Clone, Copy)]
struct Target {
    /// The XDND-aware window under the point.
    window: Window,
    /// Where messages for it go: the window itself, or its XdndProxy.
    deliver_to: Window,
    version: u32,
}

/// The XDND-aware window at (`x`, `y`): the topmost window there, or the first of its
/// descendants that is (window manager frames and toolkits nest the app's window).
fn find_target(
    conn: &RustConnection,
    atoms: &Atoms,
    root: Window,
    x: i16,
    y: i16,
) -> Res<Option<Target>> {
    let mut w = root;
    for _ in 0..32 {
        let child = conn
            .translate_coordinates(root, w, x, y)
            .map_err(e)?
            .reply()
            .map_err(e)?
            .child;
        if child == x11rb::NONE {
            return Ok(None);
        }
        w = child;
        let aware = conn
            .get_property(false, w, atoms.XdndAware, AtomEnum::ATOM, 0, 1)
            .map_err(e)?
            .reply()
            .map_err(e)?;
        let Some(version) = aware.value32().and_then(|mut v| v.next()) else {
            continue;
        };
        if version < 3 {
            return Ok(None);
        }
        return Ok(Some(Target {
            window: w,
            deliver_to: proxy(conn, atoms, w)?.unwrap_or(w),
            version: version.min(VERSION),
        }));
    }
    Ok(None)
}

/// A window's XdndProxy, if it names a window whose own XdndProxy points to itself
/// (the spec's check against a stale property).
fn proxy(conn: &RustConnection, atoms: &Atoms, w: Window) -> Res<Option<Window>> {
    let get = |w| -> Res<Option<Window>> {
        Ok(conn
            .get_property(false, w, atoms.XdndProxy, AtomEnum::WINDOW, 0, 1)
            .map_err(e)?
            .reply()
            .map_err(e)?
            .value32()
            .and_then(|mut v| v.next()))
    };
    Ok(match get(w)? {
        Some(p) if get(p).ok().flatten() == Some(p) => Some(p),
        _ => None,
    })
}

/// `file://` URIs, one per line, as file managers offer them.
fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in paths {
        out.extend_from_slice(b"file://");
        for &b in p.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                out.push(b);
            } else {
                out.extend_from_slice(format!("%{b:02X}").as_bytes());
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The drag's source side: owns XdndSelection, hands out the URIs and reads replies.
struct Source<'a> {
    conn: &'a RustConnection,
    atoms: &'a Atoms,
    win: Window,
    target: Target,
    uris: Vec<u8>,
}

impl Source<'_> {
    fn send(&self, kind: u32, data: [u32; 5]) -> Res<()> {
        // Messages name the target window even when they go to its proxy.
        let ev = ClientMessageEvent::new(32, self.target.window, kind, data);
        self.conn
            .send_event(false, self.target.deliver_to, EventMask::NO_EVENT, ev)
            .map_err(e)?;
        self.conn.flush().map_err(e)
    }

    /// A server timestamp, from the PropertyNotify of an empty append to our window:
    /// toolkits fetch the files with the drag's timestamp, as for a real drag.
    fn server_time(&mut self) -> Res<u32> {
        self.conn
            .change_property8(
                PropMode::APPEND,
                self.win,
                self.atoms.VOXL_DND_TIME,
                AtomEnum::STRING,
                &[],
            )
            .map_err(e)?;
        self.conn.flush().map_err(e)?;
        let deadline = Instant::now() + Duration::from_secs(1);
        while let Some(ev) = self.next_event(deadline)? {
            if let Event::PropertyNotify(p) = ev {
                if p.window == self.win && p.atom == self.atoms.VOXL_DND_TIME {
                    return Ok(p.time);
                }
            }
        }
        Ok(x11rb::CURRENT_TIME)
    }

    /// Waits for the target's `kind` message, serving requests for the files meanwhile.
    fn wait(&mut self, deadline: Instant, kind: u32) -> Res<Option<[u32; 5]>> {
        while let Some(ev) = self.next_event(deadline)? {
            if let Event::ClientMessage(m) = ev {
                let data = m.data.as_data32();
                if m.window == self.win && m.type_ == kind && data[0] == self.target.window {
                    return Ok(Some(data));
                }
            }
        }
        Ok(None)
    }

    /// The next event other than a request for the files, which is answered here.
    fn next_event(&mut self, deadline: Instant) -> Res<Option<Event>> {
        loop {
            match self.conn.poll_for_event().map_err(e)? {
                Some(Event::SelectionRequest(r)) => self.answer(r)?,
                Some(ev) => return Ok(Some(ev)),
                None => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    let ms = left.as_millis().clamp(1, 1000) as u16;
                    let mut fds = [PollFd::new(self.conn.stream().as_fd(), PollFlags::POLLIN)];
                    let _ = poll(&mut fds, ms);
                }
            }
        }
    }

    fn answer(&self, r: xproto::SelectionRequestEvent) -> Res<()> {
        let atoms = self.atoms;
        let prop = if r.property == x11rb::NONE {
            r.target
        } else {
            r.property
        };
        let ok = r.selection == atoms.XdndSelection
            && if r.target == atoms.TARGETS {
                self.conn
                    .change_property32(
                        PropMode::REPLACE,
                        r.requestor,
                        prop,
                        AtomEnum::ATOM,
                        &[atoms.TARGETS, atoms.URI_LIST],
                    )
                    .is_ok()
            } else if r.target == atoms.URI_LIST {
                self.conn
                    .change_property8(
                        PropMode::REPLACE,
                        r.requestor,
                        prop,
                        atoms.URI_LIST,
                        &self.uris,
                    )
                    .is_ok()
            } else {
                false
            };
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property: if ok { prop } else { x11rb::NONE },
        };
        self.conn
            .send_event(false, r.requestor, EventMask::NO_EVENT, notify)
            .map_err(e)?;
        self.conn.flush().map_err(e)
    }
}
