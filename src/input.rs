//! Input injection via XTEST. Keys arrive as keysyms; they are resolved against the
//! server keymap, falling back to temporarily remapped spare keycodes for symbols the
//! layout lacks or that need a shift level the client isn't holding.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub enum InputEvent {
    Move { x: i32, y: i32 },
    Button { button: u8, down: bool },
    /// Wheel ticks: positive dy scrolls down, positive dx scrolls right.
    Scroll { dx: i32, dy: i32 },
    Key { keysym: u32, down: bool },
    ReleaseAll,
}

const SHIFT_L: u32 = 0xffe1;
const SHIFT_R: u32 = 0xffe2;

struct Injector {
    conn: RustConnection,
    root: u32,
    min_kc: u8,
    per: usize,
    syms: Vec<u32>,
    lookup: HashMap<u32, (u8, usize)>,
    scratch: Vec<u8>,
    /// keysym -> scratch keycode currently mapped (with last-use time).
    scratch_map: HashMap<u32, (u8, Instant)>,
    pressed: HashMap<u32, u8>,
    buttons: [bool; 16],
    shift_down: u32,
}

impl Injector {
    fn new(display: Option<&str>) -> Result<Self, String> {
        let (conn, n) = RustConnection::connect(display).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[n].root;
        let min_kc = conn.setup().min_keycode;
        let max_kc = conn.setup().max_keycode;
        let mut me = Injector {
            conn,
            root,
            min_kc,
            per: 0,
            syms: vec![],
            lookup: HashMap::new(),
            scratch: vec![],
            scratch_map: HashMap::new(),
            pressed: HashMap::new(),
            buttons: [false; 16],
            shift_down: 0,
        };
        me.load_keymap(max_kc)?;
        Ok(me)
    }

    fn load_keymap(&mut self, max_kc: u8) -> Result<(), String> {
        let count = max_kc - self.min_kc + 1;
        let r = self.conn.get_keyboard_mapping(self.min_kc, count).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        self.per = r.keysyms_per_keycode as usize;
        self.syms = r.keysyms;
        self.lookup.clear();
        self.scratch.clear();
        for i in 0..count as usize {
            let kc = self.min_kc + i as u8;
            let row = &self.syms[i * self.per..(i + 1) * self.per];
            if row.iter().all(|&s| s == 0) {
                self.scratch.push(kc);
                continue;
            }
            // Only the first two levels (plain, shift) are reachable without extra modifiers.
            for (lvl, &s) in row.iter().take(2).enumerate() {
                if s != 0 {
                    self.lookup.entry(s).or_insert((kc, lvl));
                }
            }
        }
        // Keep the high end free for our remaps; never touch keycodes < 200 if possible.
        self.scratch.retain(|&k| k >= 8);
        self.scratch.reverse();
        log::info!("keymap: {} keysyms, {} scratch keycodes", self.lookup.len(), self.scratch.len());
        Ok(())
    }

    fn row(&self, kc: u8) -> &[u32] {
        let i = (kc - self.min_kc) as usize;
        &self.syms[i * self.per..(i + 1) * self.per]
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) {
        let _ = self.conn.xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.root, x, y, 0);
    }

    fn resolve(&mut self, keysym: u32) -> u8 {
        let want_level = if self.shift_down > 0 { 1 } else { 0 };
        if let Some(&(kc, lvl)) = self.lookup.get(&keysym) {
            let row = self.row(kc);
            let l0 = row[0];
            let l1 = if row.len() > 1 { row[1] } else { 0 };
            let shift_neutral = l1 == 0 || l0 == l1 || keysym >= 0xfd00 && keysym < 0x10000;
            if lvl == want_level || shift_neutral {
                return kc;
            }
        }
        if let Some((kc, t)) = self.scratch_map.get_mut(&keysym) {
            *t = Instant::now();
            return *kc;
        }
        if self.scratch.is_empty() {
            return 0;
        }
        // Map the keysym onto a spare keycode at every level.
        let kc = if self.scratch_map.len() < self.scratch.len() {
            self.scratch[self.scratch_map.len()]
        } else {
            let (&old, _) = self.scratch_map.iter().min_by_key(|(_, (_, t))| *t).unwrap();
            self.scratch_map.remove(&old).unwrap().0
        };
        let row = vec![keysym; self.per];
        let _ = self.conn.change_keyboard_mapping(1, kc, self.per as u8, &row);
        let _ = self.conn.sync();
        // Clients must process MappingNotify before they see the key.
        std::thread::sleep(Duration::from_millis(8));
        let i = (kc - self.min_kc) as usize;
        let per = self.per;
        self.syms[i * per..(i + 1) * per].copy_from_slice(&row);
        self.scratch_map.insert(keysym, (kc, Instant::now()));
        kc
    }

    fn handle(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Move { x, y } => self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16),
            InputEvent::Button { button, down } => {
                if (button as usize) < self.buttons.len() {
                    if self.buttons[button as usize] == down {
                        return;
                    }
                    self.buttons[button as usize] = down;
                }
                self.fake(if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT }, button, 0, 0)
            }
            InputEvent::Scroll { dx, dy } => {
                let mut click = |b: u8, n: i32| {
                    for _ in 0..n.min(20) {
                        self.fake(xproto::BUTTON_PRESS_EVENT, b, 0, 0);
                        self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
                    }
                };
                if dy > 0 { click(5, dy) } else if dy < 0 { click(4, -dy) }
                if dx > 0 { click(7, dx) } else if dx < 0 { click(6, -dx) }
            }
            InputEvent::Key { keysym, down } => {
                if down {
                    if self.pressed.contains_key(&keysym) {
                        // Autorepeat: re-press without release is fine for X.
                        let kc = self.pressed[&keysym];
                        self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0);
                        return;
                    }
                    let kc = self.resolve(keysym);
                    if kc == 0 {
                        return;
                    }
                    if keysym == SHIFT_L || keysym == SHIFT_R {
                        self.shift_down += 1;
                    }
                    self.pressed.insert(keysym, kc);
                    self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0);
                } else if let Some(kc) = self.pressed.remove(&keysym) {
                    if keysym == SHIFT_L || keysym == SHIFT_R {
                        self.shift_down = self.shift_down.saturating_sub(1);
                    }
                    self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0);
                }
            }
            InputEvent::ReleaseAll => {
                for (_, kc) in std::mem::take(&mut self.pressed) {
                    self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0);
                }
                self.shift_down = 0;
                for b in 1..self.buttons.len() {
                    if self.buttons[b] {
                        self.buttons[b] = false;
                        self.fake(xproto::BUTTON_RELEASE_EVENT, b as u8, 0, 0);
                    }
                }
            }
        }
    }
}

pub fn spawn(display: Option<String>) -> Result<mpsc::Sender<InputEvent>, String> {
    let mut inj = Injector::new(display.as_deref())?;
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut pending_move: Option<(i32, i32)> = None;
                let handle = |inj: &mut Injector, ev: InputEvent, pm: &mut Option<(i32, i32)>| match ev {
                    InputEvent::Move { x, y } => *pm = Some((x, y)),
                    other => {
                        if let Some((x, y)) = pm.take() {
                            inj.handle(InputEvent::Move { x, y });
                        }
                        inj.handle(other);
                    }
                };
                handle(&mut inj, first, &mut pending_move);
                // Coalesce whatever queued up meanwhile; moves collapse to the latest.
                while let Ok(ev) = rx.try_recv() {
                    handle(&mut inj, ev, &mut pending_move);
                }
                if let Some((x, y)) = pending_move {
                    inj.handle(InputEvent::Move { x, y });
                }
                let _ = inj.conn.flush();
            }
        })
        .unwrap();
    Ok(tx)
}
