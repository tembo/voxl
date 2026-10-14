//! Cursor shape tracking and two-way CLIPBOARD sync on a dedicated X connection.

use crate::hub::{Hub, Out};
use bytes::Bytes;
use std::os::fd::AsRawFd;
use std::sync::mpsc;
use std::sync::Arc;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, EventMask, PropMode, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Sets the remote CLIPBOARD and returns once the X server has registered us as owner,
/// so a paste shortcut injected right afterwards sees the new contents.
#[derive(Clone)]
pub struct ClipboardHandle {
    tx: mpsc::Sender<(String, mpsc::SyncSender<()>)>,
    wake_w: i32,
}

impl ClipboardHandle {
    pub fn set_blocking(&self, text: String) {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        if self.tx.send((text, done_tx)).is_ok() {
            unsafe { libc::write(self.wake_w, [1u8].as_ptr() as *const _, 1) };
            let _ = done_rx.recv_timeout(std::time::Duration::from_millis(300));
        }
    }
}

pub struct ClipboardMode {
    pub to_server: bool,
    pub to_client: bool,
}

struct Atoms {
    clipboard: u32,
    targets: u32,
    utf8: u32,
    text: u32,
    text_plain: u32,
    incr: u32,
    prop: u32,
}

fn atom(c: &RustConnection, name: &str) -> Result<u32, String> {
    Ok(c.intern_atom(false, name.as_bytes()).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
}

pub fn spawn(display: Option<String>, hub: Arc<Hub>, mode: ClipboardMode) -> Result<ClipboardHandle, String> {
    let (conn, n) = RustConnection::connect(display.as_deref()).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[n].root;
    conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
    let win = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(0, win, root, -10, -10, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))
        .map_err(|e| e.to_string())?;
    let atoms = Atoms {
        clipboard: atom(&conn, "CLIPBOARD")?,
        targets: atom(&conn, "TARGETS")?,
        utf8: atom(&conn, "UTF8_STRING")?,
        text: atom(&conn, "TEXT")?,
        text_plain: atom(&conn, "text/plain;charset=utf-8")?,
        incr: atom(&conn, "INCR")?,
        prop: atom(&conn, "SELKIES_RS_CLIP")?,
    };
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR).map_err(|e| e.to_string())?;
    if mode.to_client {
        conn.xfixes_select_selection_input(root, atoms.clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER).map_err(|e| e.to_string())?;
    }
    conn.flush().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<(String, mpsc::SyncSender<()>)>();
    let mut fds = [0i32; 2];
    unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    let (wake_r, wake_w) = (fds[0], fds[1]);
    std::thread::Builder::new()
        .name("desktop".into())
        .spawn(move || {
            let mut owned: Option<Vec<u8>> = None;
            let mut incr: Option<Vec<u8>> = None;
            send_cursor(&conn, &hub);
            let fd = conn.stream().as_raw_fd();
            loop {
                let mut drain = [0u8; 64];
                unsafe { while libc::read(wake_r, drain.as_mut_ptr() as *mut _, 64) > 0 {} }
                while let Ok((text, done)) = rx.try_recv() {
                    if mode.to_server {
                        *hub.last_clipboard.lock() = Some(text.clone());
                        owned = Some(text.into_bytes());
                        let _ = conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME);
                        // Round trip: ownership is in effect once this returns.
                        let _ = conn.get_selection_owner(atoms.clipboard).map(|c| c.reply());
                    }
                    let _ = done.send(());
                }
                let ev = match conn.poll_for_event() {
                    Ok(Some(ev)) => ev,
                    Ok(None) => {
                        let mut p = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }, libc::pollfd { fd: wake_r, events: libc::POLLIN, revents: 0 }];
                        unsafe { libc::poll(p.as_mut_ptr(), 2, 1000) };
                        continue;
                    }
                    Err(e) => {
                        log::error!("desktop X connection: {e}");
                        return;
                    }
                };
                match ev {
                    Event::XfixesCursorNotify(_) => send_cursor(&conn, &hub),
                    Event::XfixesSelectionNotify(e) => {
                        if e.owner != win && e.owner != x11rb::NONE {
                            let _ = conn.convert_selection(win, atoms.clipboard, atoms.utf8, atoms.prop, x11rb::CURRENT_TIME);
                            let _ = conn.flush();
                        }
                    }
                    Event::SelectionNotify(e) => {
                        if e.property == x11rb::NONE {
                            continue;
                        }
                        if let Ok(Ok(r)) = conn.get_property(true, win, atoms.prop, AtomEnum::ANY, 0, u32::MAX / 4).map(|c| c.reply()) {
                            if r.type_ == atoms.incr {
                                incr = Some(Vec::new());
                            } else {
                                publish(&hub, r.value);
                            }
                            let _ = conn.flush();
                        }
                    }
                    Event::PropertyNotify(e) if e.atom == atoms.prop && e.state == xproto::Property::NEW_VALUE && incr.is_some() => {
                        if let Ok(Ok(r)) = conn.get_property(true, win, atoms.prop, AtomEnum::ANY, 0, u32::MAX / 4).map(|c| c.reply()) {
                            if r.value.is_empty() {
                                publish(&hub, incr.take().unwrap());
                            } else {
                                incr.as_mut().unwrap().extend_from_slice(&r.value);
                            }
                            let _ = conn.flush();
                        }
                    }
                    Event::SelectionRequest(e) => {
                        let mut prop = e.property;
                        if prop == x11rb::NONE {
                            prop = e.target;
                        }
                        let ok = match &owned {
                            Some(data) if e.selection == atoms.clipboard => {
                                if e.target == atoms.targets {
                                    let list = [atoms.targets, atoms.utf8, u32::from(AtomEnum::STRING), atoms.text, atoms.text_plain];
                                    conn.change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::ATOM, &list).is_ok()
                                } else if e.target == atoms.utf8 || e.target == atoms.text || e.target == atoms.text_plain || e.target == u32::from(AtomEnum::STRING) {
                                    conn.change_property8(PropMode::REPLACE, e.requestor, prop, e.target, data).is_ok()
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        };
                        let notify = xproto::SelectionNotifyEvent {
                            response_type: xproto::SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: e.time,
                            requestor: e.requestor,
                            selection: e.selection,
                            target: e.target,
                            property: if ok { prop } else { x11rb::NONE },
                        };
                        let _ = conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
                        let _ = conn.flush();
                    }
                    Event::SelectionClear(_) => owned = None,
                    _ => {}
                }
            }
        })
        .unwrap();
    Ok(ClipboardHandle { tx, wake_w })
}

fn publish(hub: &Hub, data: Vec<u8>) {
    let Ok(text) = String::from_utf8(data) else { return };
    if text.is_empty() {
        return;
    }
    {
        let mut last = hub.last_clipboard.lock();
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        *last = Some(text.clone());
    }
    let msg = serde_json::json!({"t": "clip", "d": text}).to_string();
    hub.broadcast(|| Out::Text(msg.clone()));
}

fn send_cursor(conn: &RustConnection, hub: &Hub) {
    let Ok(Ok(img)) = conn.xfixes_get_cursor_image().map(|c| c.reply()) else { return };
    let (w, h) = (img.width as usize, img.height as usize);
    let mut msg = Vec::with_capacity(14 + w * h * 4);
    msg.push(0x03);
    msg.push(0);
    msg.extend_from_slice(&(w as u16).to_be_bytes());
    msg.extend_from_slice(&(h as u16).to_be_bytes());
    msg.extend_from_slice(&img.xhot.to_be_bytes());
    msg.extend_from_slice(&img.yhot.to_be_bytes());
    msg.extend_from_slice(&img.cursor_serial.to_be_bytes());
    for &p in img.cursor_image.iter().take(w * h) {
        let a = (p >> 24) & 0xff;
        let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
        msg.extend_from_slice(&[un((p >> 16) & 0xff), un((p >> 8) & 0xff), un(p & 0xff), a as u8]);
    }
    let b = Bytes::from(msg);
    *hub.cursor.lock() = Some(b.clone());
    hub.broadcast(|| Out::Bin(b.clone()));
}
