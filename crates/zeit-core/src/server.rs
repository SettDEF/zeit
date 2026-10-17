//! zeitd: sessions, windows and panes; serves clients; restores after a restart.

use crate::ext::{words, Ext};
use crate::keys::{Action, Binding, Chord, Dir, Keys};
use crate::look::Look;
use alacritty_terminal::grid::Dimensions;
use crate::layout::{Layout, Rect};
use crate::pane::{Pane, PaneEvent};
use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use zeit_proto::{read_msg, write_msg, Cell, ClientMsg, Color, CursorShape, Frame, ServerMsg, VERSION};

enum Ev {
    Accept(UnixStream),
    Msg(u64, ClientMsg),
    Gone(u64),
    Pane(PaneEvent),
    /// a line for the status line (run-shell output, errors from a thread)
    Notice(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Window {
    name: String,
    layout: Layout,
    focus: String,
    zoom: bool,
    /// a name given by the user (else the pane's program names it)
    named: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Session {
    id: String,
    name: String,
    /// the folder this session is the place of (`zeit` in that folder comes back here)
    place: Option<PathBuf>,
    windows: Vec<Window>,
    active: usize,
    last: usize,
}

struct Client {
    out: UnixStream,
    session: Option<String>,
    cols: u16,
    rows: u16,
    last: Vec<Vec<Cell>>,
    notice: Option<String>,
    /// an input line or a picker, over everything; it takes the keys while open
    overlay: Option<Overlay>,
}

#[derive(Clone, Debug)]
enum Overlay {
    Prompt { label: String, text: String, cur: usize, then: Then },
    /// (label, target) rows
    Choose { items: Vec<(String, String)>, sel: usize, template: Option<String> },
}

#[derive(Clone, Debug)]
enum Then {
    /// run the text as a command, or put it into a template (`swap-window -t '%%'`)
    Command(Option<String>),
    Search { pane: String, back: bool },
}

pub struct Server {
    exe: PathBuf,
    state_file: PathBuf,
    sessions: Vec<Session>,
    panes: HashMap<String, Pane>,
    /// where each pane started (for restarts) and its command
    clients: HashMap<u64, Client>,
    next_client: u64,
    tx: Sender<Ev>,
    pane_tx: Sender<PaneEvent>,
    ext: Ext,
    keys: Keys,
    /// the size panes are laid out for (the latest client's)
    size: (u16, u16),
    dirty: bool,
    /// passthrough sequences for a client, sent after its next frame (so the cursor is right)
    pending: HashMap<u64, Vec<Vec<u8>>>,
    /// the start of a mouse report whose end has not arrived yet
    carry: HashMap<u64, Vec<u8>>,
    look: Look,
    /// copied text, newest first (tmux's paste buffers)
    buffers: Vec<String>,
    /// panes in copy mode
    copy: HashMap<String, CopyState>,
    /// the pane focused before the current one, per session (`select-pane -t !`)
    prev_pane: HashMap<String, String>,
    /// panes with output while their window was not shown (monitor-activity)
    activity: HashSet<String>,
    /// the latest copy-mode search: text, and whether it went up
    last_search: Option<(String, bool)>,
}

type Reply = Result<String, String>;

/// Runs the server until its last session ends.
pub fn run(name: &str, exe: PathBuf) -> std::io::Result<()> {
    paths::private_dir(&paths::runtime_dir())?;
    tmux_shim(&exe);
    let sock = paths::server_socket(name);
    // a socket nobody answers on is stale
    if sock.exists() && UnixStream::connect(&sock).is_err() {
        let _ = std::fs::remove_file(&sock);
    }
    let listener = UnixListener::bind(&sock)?;
    let (tx, rx) = channel();
    let (pane_tx, pane_rx) = channel::<PaneEvent>();
    {
        let accept_tx = tx.clone();
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                if accept_tx.send(Ev::Accept(s)).is_err() {
                    return;
                }
            }
        });
        let pane_fwd = tx.clone();
        std::thread::spawn(move || {
            for e in pane_rx {
                if pane_fwd.send(Ev::Pane(e)).is_err() {
                    return;
                }
            }
        });
    }
    let mut ext = Ext::new();
    ext.load(&paths::config_dir());
    let keys = build_keys(&ext);
    let look = Look::from(&keys.opts);
    let state_file = paths::state_dir().join(format!("{name}.sessions"));
    let mut s = Server { exe, state_file, sessions: Vec::new(), panes: HashMap::new(), clients: HashMap::new(), next_client: 1, tx, pane_tx, ext, keys, size: (80, 24), dirty: false, pending: HashMap::new(), carry: HashMap::new(), look, buffers: Vec::new(), copy: HashMap::new(), prev_pane: HashMap::new(), activity: HashSet::new(), last_search: None };
    s.restore();
    loop {
        let Ok(ev) = rx.recv() else { break };
        s.handle(ev);
        while let Ok(ev) = rx.try_recv() {
            s.handle(ev);
        }
        s.after();
        // tmux's rule: the server ends with its last session
        if s.sessions.is_empty() && s.clients.is_empty() {
            break;
        }
    }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&s.state_file);
    Ok(())
}

impl Server {
    // ── sessions on disk (for the next server) ──

    fn save(&self) {
        if let Some(d) = self.state_file.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(bytes) = postcard::to_stdvec(&self.sessions) {
            let tmp = self.state_file.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, &self.state_file);
            }
        }
    }

    /// The sessions a previous server left, with every pane whose holder still runs.
    fn restore(&mut self) {
        let Ok(bytes) = std::fs::read(&self.state_file) else { return };
        let Ok(sessions) = postcard::from_bytes::<Vec<Session>>(&bytes) else { return };
        for mut s in sessions {
            let mut windows = Vec::new();
            for mut w in s.windows {
                let mut layout = Some(w.layout.clone());
                for id in w.layout.ids() {
                    match Pane::connect(&id, self.size.0, self.size.1, PathBuf::from("/"), self.look.history, self.pane_tx.clone()) {
                        Ok(p) => {
                            self.panes.insert(id, p);
                        }
                        Err(_) => layout = layout.and_then(|l| l.remove(&id)),
                    }
                }
                if let Some(l) = layout {
                    if !l.ids().contains(&w.focus) {
                        w.focus = l.ids()[0].clone();
                    }
                    w.layout = l;
                    windows.push(w);
                }
            }
            if !windows.is_empty() {
                s.active = s.active.min(windows.len() - 1);
                s.last = s.last.min(windows.len() - 1);
                s.windows = windows;
                self.sessions.push(s);
            }
        }
        self.save();
    }

    // ── events ──

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Accept(stream) => {
                let id = self.next_client;
                self.next_client += 1;
                let Ok(reader) = stream.try_clone() else { return };
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let mut r = reader;
                    loop {
                        match read_msg::<_, ClientMsg>(&mut r) {
                            Ok(m) => {
                                if tx.send(Ev::Msg(id, m)).is_err() {
                                    return;
                                }
                            }
                            Err(_) => {
                                let _ = tx.send(Ev::Gone(id));
                                return;
                            }
                        }
                    }
                });
                self.clients.insert(id, Client { out: stream, session: None, cols: 80, rows: 24, last: Vec::new(), notice: None, overlay: None });
            }
            Ev::Gone(id) => {
                self.clients.remove(&id);
            }
            Ev::Msg(id, m) => self.message(id, m),
            Ev::Pane(PaneEvent::Output(id)) => {
                if self.look.activity_on && !self.shown(&id) {
                    self.activity.insert(id);
                }
                self.dirty = true;
            }
            Ev::Notice(text) => self.notice_all(text),
            Ev::Pane(PaneEvent::Passthrough(pane, seq)) => {
                // to the clients showing that pane in front
                let ids: Vec<u64> = self.clients.keys().copied().filter(|c| self.client_pane(*c).as_deref() == Some(pane.as_str())).collect();
                for c in ids {
                    self.pending.entry(c).or_default().push(seq.clone());
                }
                self.dirty = true;
            }
            Ev::Pane(PaneEvent::Exited(id, code)) => self.pane_gone(&id, code),
            Ev::Pane(PaneEvent::Lost(id)) => self.pane_gone(&id, -1),
        }
    }

    fn send(&mut self, client: u64, m: &ServerMsg) {
        let ok = self.clients.get_mut(&client).is_some_and(|c| write_msg(&mut c.out, m).is_ok());
        if !ok {
            self.clients.remove(&client);
        }
    }

    fn message(&mut self, cid: u64, m: ClientMsg) {
        match m {
            ClientMsg::Hello { version, .. } => {
                self.send(cid, &ServerMsg::Hello { version: VERSION, server: format!("zeit {}", env!("CARGO_PKG_VERSION")) });
                if version != VERSION {
                    self.send(cid, &ServerMsg::Reply { ok: false, text: format!("this server speaks protocol {VERSION}, the client {version}") });
                }
            }
            ClientMsg::Attach { session, place, cols, rows } => {
                let sid = match (session, place) {
                    (Some(name), _) => match self.find_session(&name) {
                        Some(s) => s,
                        None => return self.send(cid, &ServerMsg::Bye { reason: format!("no session {name:?}") }),
                    },
                    (None, Some(dir)) => {
                        let dir = PathBuf::from(dir);
                        match self.sessions.iter().find(|s| s.place.as_ref() == Some(&dir)) {
                            Some(s) => s.id.clone(),
                            None => match self.new_session(None, Some(dir), Vec::new()) {
                                Ok(s) => s,
                                Err(e) => return self.send(cid, &ServerMsg::Bye { reason: e }),
                            },
                        }
                    }
                    (None, None) => match self.sessions.last().map(|s| s.id.clone()) {
                        Some(s) => s,
                        None => match self.new_session(None, None, Vec::new()) {
                            Ok(s) => s,
                            Err(e) => return self.send(cid, &ServerMsg::Bye { reason: e }),
                        },
                    },
                };
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.session = Some(sid.clone());
                    c.cols = cols;
                    c.rows = rows;
                    c.last.clear();
                }
                self.size = (cols, rows);
                self.layout_all();
                let name = self.session(&sid).map(|s| s.name.clone()).unwrap_or_default();
                self.ext.emit("client_attached", &[("session", name)]);
                self.dirty = true;
            }
            ClientMsg::Detach => {
                self.send(cid, &ServerMsg::Bye { reason: "detached".into() });
                self.clients.remove(&cid);
            }
            ClientMsg::Input(bytes) => self.input(cid, &bytes),
            ClientMsg::Resize { cols, rows } => {
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.cols = cols;
                    c.rows = rows;
                    c.last.clear();
                }
                self.size = (cols, rows);
                self.layout_all();
                self.dirty = true;
            }
            ClientMsg::Command(w) => {
                let r = self.command(Some(cid), w);
                let (ok, text) = match r {
                    Ok(t) => (true, t),
                    Err(t) => (false, t),
                };
                self.send(cid, &ServerMsg::Reply { ok, text });
                self.dirty = true;
            }
        }
    }

    /// After a batch of events: Lua's queued commands, notices, redraws.
    fn after(&mut self) {
        for c in self.ext.take_commands() {
            if let Err(e) = self.run_line(None, None, &c) {
                self.notice_all(e);
            }
        }
        for n in self.ext.take_notices() {
            self.notice_all(n);
        }
        let seen: Vec<String> = self.activity.iter().filter(|p| self.shown(p)).cloned().collect();
        for p in seen {
            self.activity.remove(&p);
        }
        if self.dirty {
            self.dirty = false;
            let ids: Vec<u64> = self.clients.keys().copied().collect();
            for id in ids {
                self.draw(id);
            }
        }
    }

    fn notice_all(&mut self, text: String) {
        for c in self.clients.values_mut() {
            c.notice = Some(text.clone());
        }
        self.dirty = true;
    }

    // ── input: keys to the focused pane, the mouse to the pane under it ──

    fn input(&mut self, cid: u64, bytes: &[u8]) {
        if self.clients.get(&cid).is_some_and(|c| c.overlay.is_some()) {
            self.overlay_input(cid, bytes);
            return;
        }
        // a mouse report split across reads waits for its end
        let mut joined = self.carry.remove(&cid).unwrap_or_default();
        joined.extend_from_slice(bytes);
        let bytes = &joined[..];
        let mut plain: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if let Some((b, x, y, press, n)) = sgr_mouse(&bytes[i..]) {
                self.flush_keys(cid, &mut plain);
                self.mouse(cid, b, x, y, press);
                i += n;
                continue;
            }
            let rest = &bytes[i..];
            if rest.starts_with(b"\x1b[<") && rest.len() < 24 && !rest.iter().any(|c| *c == b'M' || *c == b'm') && rest[3..].iter().all(|c| c.is_ascii_digit() || *c == b';') {
                self.carry.insert(cid, rest.to_vec());
                break;
            }
            plain.push(bytes[i]);
            i += 1;
        }
        self.flush_keys(cid, &mut plain);
    }

    fn flush_keys(&mut self, cid: u64, plain: &mut Vec<u8>) {
        if plain.is_empty() {
            return;
        }
        // copy mode takes the keys
        if let Some(pane) = self.client_pane(cid).filter(|p| self.copy.contains_key(p)) {
            let bytes = std::mem::take(plain);
            let mut i = 0;
            while i < bytes.len() {
                let Some((c, n)) = crate::keys::chord_from_bytes(&bytes[i..]) else { break };
                self.copy_key(Some(cid), &pane, &c);
                i += n;
                if !self.copy.contains_key(&pane) {
                    break;
                }
            }
            return;
        }
        let Some(p) = self.client_pane(cid).and_then(|p| self.panes.get(&p)) else { return plain.clear() };
        // typing goes back to the live screen
        if p.scrolled() > 0 {
            p.scroll(0);
            self.dirty = true;
        }
        let mut out = std::mem::take(plain);
        // paste markers only for programs that asked for them
        if p.modes() & zeit_proto::MODE_PASTE == 0 {
            for m in [&b"\x1b[200~"[..], &b"\x1b[201~"[..]] {
                while let Some(at) = out.windows(m.len()).position(|w| w == m) {
                    out.drain(at..at + m.len());
                }
            }
        }
        p.input(&out);
    }

    /// A mouse report from the client (SGR, 1-based cells of the whole screen).
    fn mouse(&mut self, cid: u64, b: u32, x: u16, y: u16, press: bool) {
        let Some(sid) = self.client_session(cid) else { return };
        let area = self.area();
        let Some(s) = self.session(&sid) else { return };
        let Some(w) = s.windows.get(s.active) else { return };
        let (cx, cy) = (x.saturating_sub(1), y.saturating_sub(1));
        let placed = if w.zoom { vec![(w.focus.clone(), area)] } else { w.layout.place(area).0 };
        let Some((pane, r)) = placed.into_iter().find(|(_, r)| cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h) else { return };
        let wheel = b & 64 != 0;
        let motion = b & 32 != 0;
        // a click in another pane gives it the keyboard
        if press && !wheel && !motion && pane != w.focus {
            if let Some(s) = self.session_mut(&sid) {
                let w = &mut s.windows[s.active];
                w.focus = pane.clone();
            }
            self.dirty = true;
        }
        if wheel && self.copy.contains_key(&pane) {
            for _ in 0..3 {
                self.copy_do(Some(cid), &pane, if b & 1 == 0 { "scroll-up" } else { "scroll-down" }, None);
            }
            return;
        }
        let Some(p) = self.panes.get(&pane) else { return };
        let modes = p.modes();
        let (px, py) = (cx - r.x + 1, cy - r.y + 1);
        if modes & zeit_proto::MODE_MOUSE != 0 {
            // the program wants the mouse: the report, in its own cells
            let report = if modes & zeit_proto::MODE_SGR_MOUSE != 0 {
                format!("\x1b[<{b};{px};{py}{}", if press { 'M' } else { 'm' }).into_bytes()
            } else {
                let code = if press { b } else { 3 } + 32;
                vec![0x1b, b'[', b'M', code.min(255) as u8, (px + 32).min(255) as u8, (py + 32).min(255) as u8]
            };
            p.input(&report);
        } else if wheel {
            let up = b & 1 == 0;
            if p.alt_screen() {
                // full-screen programs without the mouse (less, man) get arrow keys
                let key: &[u8] = match (up, modes & zeit_proto::MODE_APP_CURSOR != 0) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                p.input(&key.repeat(3));
            } else {
                // the shell: scroll back through the pane
                p.scroll(if up { 3 } else { -3 });
                self.dirty = true;
            }
        }
    }

    // ── finding things ──

    fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    fn find_session(&self, name: &str) -> Option<String> {
        self.sessions.iter().find(|s| s.name == name || s.id == name).map(|s| s.id.clone())
    }

    fn client_session(&self, cid: u64) -> Option<String> {
        self.clients.get(&cid)?.session.clone()
    }

    fn client_pane(&self, cid: u64) -> Option<String> {
        let s = self.session(&self.client_session(cid)?)?;
        Some(s.windows.get(s.active)?.focus.clone())
    }

    /// The session and window a pane is in.
    fn where_is(&self, pane: &str) -> Option<(String, usize)> {
        for s in &self.sessions {
            for (i, w) in s.windows.iter().enumerate() {
                if w.layout.ids().iter().any(|p| p == pane) {
                    return Some((s.id.clone(), i));
                }
            }
        }
        None
    }

    // ── making and ending things ──

    fn start_pane(&mut self, dir: PathBuf, cmd: Vec<String>) -> Result<String, String> {
        let id = paths::new_id("p");
        let (cols, rows) = (self.size.0.max(2), self.size.1.saturating_sub(1).max(1));
        crate::holder::start(&self.exe, &id, &dir, cols, rows, &cmd).map_err(|e| e.to_string())?;
        let p = Pane::connect(&id, cols, rows, dir, self.look.history, self.pane_tx.clone()).map_err(|e| e.to_string())?;
        self.panes.insert(id.clone(), p);
        Ok(id)
    }

    fn new_session(&mut self, name: Option<String>, place: Option<PathBuf>, cmd: Vec<String>) -> Result<String, String> {
        let dir = place.clone().or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"));
        let pane = self.start_pane(dir.clone(), cmd)?;
        // a place is named after its folder; others are numbered, as in tmux
        let base = name.or_else(|| place.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| {
            (0..).map(|i| i.to_string()).find(|n| !self.sessions.iter().any(|s| &s.name == n)).unwrap_or_default()
        });
        let mut name = base.clone();
        let mut n = 2;
        while self.sessions.iter().any(|s| s.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let id = paths::new_id("s");
        self.sessions.push(Session { id: id.clone(), name: name.clone(), place, windows: vec![Window { name: shell_name(), layout: Layout::Leaf(pane.clone()), focus: pane, zoom: false, named: false }], active: 0, last: 0 });
        self.ext.emit("session_created", &[("session", name)]);
        self.save();
        Ok(id)
    }

    fn pane_gone(&mut self, pane: &str, code: i32) {
        self.panes.remove(pane);
        let _ = std::fs::remove_file(paths::pane_socket(pane));
        self.ext.emit("pane_exit", &[("pane", pane.to_string()), ("status", code.to_string())]);
        self.unlink_pane(pane);
    }

    /// Takes a pane out of its window (the window goes when it was its last pane,
    /// the session when it was its last window). The pane itself lives on.
    fn unlink_pane(&mut self, pane: &str) {
        let Some((sid, wi)) = self.where_is(pane) else { return };
        let Some(s) = self.session_mut(&sid) else { return };
        let w = &mut s.windows[wi];
        let layout = std::mem::replace(&mut w.layout, Layout::Leaf(String::new()));
        match layout.remove(pane) {
            Some(l) => {
                if w.focus == pane {
                    w.focus = l.ids()[0].clone();
                }
                w.layout = l;
                w.zoom = false;
            }
            None => {
                s.windows.remove(wi);
                if s.windows.is_empty() {
                    let name = s.name.clone();
                    self.sessions.retain(|x| x.id != sid);
                    self.ext.emit("session_closed", &[("session", name)]);
                    // its clients move to another session, or leave
                    let next = self.sessions.last().map(|s| s.id.clone());
                    let ids: Vec<u64> = self.clients.iter().filter(|(_, c)| c.session.as_deref() == Some(&sid)).map(|(k, _)| *k).collect();
                    for cid in ids {
                        match &next {
                            Some(n) => {
                                if let Some(c) = self.clients.get_mut(&cid) {
                                    c.session = Some(n.clone());
                                    c.last.clear();
                                }
                            }
                            None => {
                                self.send(cid, &ServerMsg::Bye { reason: "the session ended".into() });
                                self.clients.remove(&cid);
                            }
                        }
                    }
                } else {
                    s.active = s.active.min(s.windows.len() - 1);
                    s.last = s.last.min(s.windows.len() - 1);
                }
            }
        }
        self.save();
        self.layout_all();
        self.dirty = true;
    }

    // ── layout ──

    fn area(&self) -> Rect {
        let status = 1 + u16::from(self.ext.enabled("hintbar"));
        Rect { x: 0, y: u16::from(self.look.top), w: self.size.0, h: self.size.1.saturating_sub(status) }
    }

    /// Every pane gets the size of its place in its window.
    fn layout_all(&mut self) {
        let area = self.area();
        let mut sizes: Vec<(String, Rect)> = Vec::new();
        for s in &self.sessions {
            for w in &s.windows {
                if w.zoom {
                    sizes.push((w.focus.clone(), area));
                } else {
                    sizes.extend(w.layout.place(area).0);
                }
            }
        }
        for (id, r) in sizes {
            if let Some(p) = self.panes.get_mut(&id) {
                p.set_size(r.w.max(1), r.h.max(1));
            }
        }
    }

    // ── drawing ──

    fn draw(&mut self, cid: u64) {
        let Some(c) = self.clients.get(&cid) else { return };
        let Some(sid) = c.session.clone() else { return };
        let (cols, rows) = (c.cols, c.rows);
        let notice = c.notice.clone();
        let overlay = c.overlay.clone();
        let Some(s) = self.session(&sid) else { return };
        let Some(w) = s.windows.get(s.active) else { return };
        let mut grid: Vec<Vec<Cell>> = vec![vec![Cell::default(); cols as usize]; rows as usize];
        let area = self.area();
        let (placed, borders) = if w.zoom { (vec![(w.focus.clone(), area)], Vec::new()) } else { w.layout.place(area) };
        let mut cursor = None;
        let mut shape = CursorShape::Block;
        let focus_rect = placed.iter().find(|(id, _)| *id == w.focus).map(|(_, r)| *r);
        for (id, r) in &placed {
            let Some(p) = self.panes.get(id) else { continue };
            let (screen, cur, sh) = p.screen();
            for (y, row) in screen.iter().enumerate().take(r.h as usize) {
                let gy = r.y as usize + y;
                if gy >= grid.len() {
                    break;
                }
                for (x, cell) in row.iter().enumerate().take(r.w as usize) {
                    let gx = r.x as usize + x;
                    if gx < grid[gy].len() {
                        grid[gy][gx] = cell.clone();
                    }
                }
            }
            if *id == w.focus {
                cursor = cur.map(|(x, y)| (r.x + x, r.y + y));
                shape = sh;
            }
        }
        // copy mode: its cursor, and where in the history it is (as tmux shows it)
        if let Some((c, r)) = self.copy.get(&w.focus).zip(focus_rect) {
            if let Some(p) = self.panes.get(&w.focus) {
                let (d, hist) = { let t = p.term.lock(); (t.grid().display_offset() as i32, t.grid().history_size()) };
                let vy = c.y + d;
                if vy >= 0 && (vy as u16) < r.h {
                    cursor = Some((r.x + c.x as u16, r.y + vy as u16));
                    shape = CursorShape::Block;
                }
                let tag = format!("[{d}/{hist}]");
                let tx = (r.x + r.w).saturating_sub(tag.chars().count() as u16);
                write_text(&mut grid, r.y as usize, tx as usize, &tag, Color::Indexed(0), Color::Indexed(3), 0);
            }
        }
        // borders: the active pane's stands out (tmux.conf's pane-*-border-style)
        let active = self.look.active_border;
        let dim = self.look.border;
        for b in &borders {
            for i in 0..b.len {
                let (x, y) = if b.vertical { (b.x, b.y + i) } else { (b.x + i, b.y) };
                let Some(cell) = grid.get_mut(y as usize).and_then(|r| r.get_mut(x as usize)) else { continue };
                let near = focus_rect.is_some_and(|f| {
                    if b.vertical { (x + 1 == f.x || x == f.x + f.w) && y >= f.y && y < f.y + f.h } else { (y + 1 == f.y || y == f.y + f.h) && x >= f.x && x < f.x + f.w }
                });
                let crossing = cell.text == "│" || cell.text == "─";
                let text = if crossing && !b.vertical || crossing && cell.text == "─" { "┼" } else if b.vertical { "│" } else { "─" };
                *cell = Cell { text: text.into(), fg: if near { active } else { dim }, bg: Color::Default, flags: 0 };
            }
        }
        // the status line, and the hint bar above it when that module is on
        let status_row = if self.look.top { 0 } else { rows.saturating_sub(1) as usize };
        let mut left = format!(" {} ", s.name);
        let mut wins = String::new();
        for (i, win) in s.windows.iter().enumerate() {
            let title = self.window_title(win);
            wins.push_str(&format!(" {i}:{title}{} ", if i == s.active { "*" } else if i == s.last { "-" } else { "" }));
        }
        left.push_str(&wins);
        let scrolled = self.panes.get(&w.focus).map(|p| p.scrolled()).unwrap_or(0);
        let right = if scrolled > 0 { format!(" ↑ {scrolled} lines · type to return ") } else { notice.clone().unwrap_or_else(|| format!(" {} ", hostname())) };
        let pills = crate::look::Look::pills(&self.keys.opts);
        if pills {
            let attached = self.clients.values().filter(|c| c.session.as_deref() == Some(&s.id)).count();
            let right = if scrolled > 0 && !self.copy.contains_key(&w.focus) { right.clone() } else if let Some(n) = &notice { format!(" {n} ") } else { format!(" {} \u{f1acb} {attached} ", s.name) };
            self.status_pills(&mut grid, status_row, s, &right);
        } else {
        write_text(&mut grid, status_row, 0, &" ".repeat(cols as usize), Color::Indexed(0), Color::Indexed(2), 0);
        write_text(&mut grid, status_row, 0, &format!(" {} ", s.name), Color::Indexed(0), Color::Indexed(2), zeit_proto::BOLD);
        let mut x = s.name.chars().count() + 3;
        for (i, win) in s.windows.iter().enumerate() {
            let label = format!(" {i}:{}{} ", self.window_title(win), if i == s.last && i != s.active { "-" } else { "" });
            let (fg, bg, fl) = if i == s.active { (Color::Indexed(2), Color::Indexed(0), zeit_proto::BOLD) } else { (Color::Indexed(0), Color::Indexed(2), 0) };
            write_text(&mut grid, status_row, x, &label, fg, bg, fl);
            x += label.chars().count();
        }
        let rx = (cols as usize).saturating_sub(right.chars().count());
        write_text(&mut grid, status_row, rx.max(x), &right, Color::Indexed(0), Color::Indexed(2), 0);
        }
        if self.ext.enabled("hintbar") && rows >= 2 {
            let hint = self.hints();
            let hr = if self.look.top { rows as usize - 1 } else { status_row - 1 };
            write_text(&mut grid, hr, 0, &" ".repeat(cols as usize), Color::Default, Color::Indexed(0), 0);
            write_text(&mut grid, hr, 1, &hint, Color::Indexed(7), Color::Indexed(0), 0);
        }
        if let Some(o) = &overlay {
            let at = self.draw_overlay(o, &mut grid, status_row, area);
            if matches!(o, Overlay::Prompt { .. }) {
                cursor = at;
                shape = CursorShape::Beam;
            } else {
                cursor = None;
            }
        }
        let title = self.panes.get(&w.focus).map(|p| p.title.lock().unwrap().clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| format!("zeit: {}", s.name));
        let modes = self.panes.get(&w.focus).map(|p| p.modes()).unwrap_or(0);
        // only the rows that changed go out
        let c = self.clients.get_mut(&cid).unwrap();
        let full = c.last.len() != grid.len() || c.last.first().map(|r| r.len()) != grid.first().map(|r| r.len());
        let changed: Vec<(u16, Vec<Cell>)> = grid.iter().enumerate().filter(|(i, row)| full || c.last.get(*i) != Some(row)).map(|(i, row)| (i as u16, row.clone())).collect();
        c.last = grid;
        let frame = Frame { cols, rows, changed, cursor, cursor_shape: shape, title, modes };
        self.send(cid, &ServerMsg::Frame(frame));
        for seq in self.pending.remove(&cid).unwrap_or_default() {
            self.send(cid, &ServerMsg::Passthrough(seq));
        }
    }

    fn window_title(&self, w: &Window) -> String {
        if w.named {
            return w.name.clone();
        }
        // the focused pane's program, as tmux's automatic-rename does
        self.panes.get(&w.focus).and_then(|p| foreground_name(p.pid)).unwrap_or_else(|| w.name.clone())
    }

    /// The hint bar: what the prefix does, in the user's own keys.
    fn hints(&self) -> String {
        let key_for = |want: &dyn Fn(&Action) -> bool| {
            let mut ks: Vec<&Chord> = self.keys.table.iter().filter(|(_, b)| want(&b.action)).map(|(k, _)| k).collect();
            ks.sort_by(|a, b| a.key.cmp(&b.key));
            ks.first().map(|c| if c.ctrl { format!("^{}", c.key) } else { c.key.clone() })
        };
        let p = &self.keys.prefix;
        let mut parts = vec![format!("{}{} then:", if p.ctrl { "Ctrl+" } else { "" }, p.key)];
        let items: [(&dyn Fn(&Action) -> bool, &str); 7] = [
            (&|a| *a == Action::Split { side_by_side: true }, "split │"),
            (&|a| *a == Action::Split { side_by_side: false }, "split ─"),
            (&|a| matches!(a, Action::Focus(_)), "move"),
            (&|a| *a == Action::Zoom, "zoom"),
            (&|a| *a == Action::ClosePane, "close"),
            (&|a| *a == Action::NewTab, "window"),
            (&|a| *a == Action::NextTab, "next"),
        ];
        for (f, label) in items {
            if let Some(k) = key_for(f) {
                parts.push(format!("{k} {label}"));
            }
        }
        parts.push("d detach".into());
        parts.join("   ")
    }

    // ── commands (keys, the CLI and Lua all run these) ──

    fn command(&mut self, cid: Option<u64>, mut w: Vec<String>) -> Reply {
        // "@pane" first: the pane a command from a shell inside zeit came from
        let mut ctx_pane: Option<String> = None;
        if w.first().is_some_and(|x| x.starts_with('@')) {
            ctx_pane = Some(w.remove(0)[1..].to_string());
        }
        let Some(cmd) = w.first().cloned() else { return Ok(String::new()) };
        let args = &w[1..];
        // -fh is -f and -h
        let flag = |f: &str| args.iter().any(|a| a == f || (a.len() > 2 && a.starts_with('-') && !a.starts_with("--") && a[1..].chars().all(|c| c.is_ascii_alphabetic()) && a[1..].contains(&f[1..])));
        let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
        // the current pane: given, the client's, or the latest session's
        let current = ctx_pane.clone().or_else(|| cid.and_then(|c| self.client_pane(c))).or_else(|| {
            let s = self.sessions.last()?;
            Some(s.windows.get(s.active)?.focus.clone())
        });
        let target_pane = opt("-t").and_then(|t| self.resolve_target(&t, current.as_deref())).or(current.clone());
        let here = target_pane.as_ref().and_then(|p| self.where_is(p));
        // words after the options: a command to run
        let rest = |skip: &[&str]| -> Vec<String> {
            let mut out = Vec::new();
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if a == "--" {
                    out.extend(args[i + 1..].iter().cloned());
                    break;
                }
                if a.starts_with('-') && a.len() > 1 {
                    if skip.contains(&a.as_str()) {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                out.push(a.clone());
                i += 1;
            }
            out
        };
        let dir_of = |s: &Server| opt("-c").map(PathBuf::from).or_else(|| target_pane.as_ref().and_then(|p| s.panes.get(p)).map(|p| p.cwd()));
        let r = match cmd.as_str() {
            "__key" => {
                let table = args.first().cloned().unwrap_or_default();
                let Some(key) = args.get(1).and_then(|k| Chord::parse(k)) else { return Err("bad key".into()) };
                self.key(cid, &table, &key, ctx_pane);
                Ok(String::new())
            }
            "new-session" | "new" => {
                let place = opt("-c").map(PathBuf::from);
                let id = self.new_session(opt("-s"), place, rest(&["-s", "-c", "-n"]))?;
                if !flag("-d") {
                    if let Some(c) = cid.and_then(|c| self.clients.get_mut(&c)) {
                        c.session = Some(id.clone());
                        c.last.clear();
                    }
                }
                self.layout_all();
                Ok(self.session(&id).map(|s| s.name.clone()).unwrap_or_default())
            }
            "new-window" | "neww" => {
                let Some((sid, _)) = here.clone() else { return Err("no session".into()) };
                let dir = dir_of(self).unwrap_or_else(|| PathBuf::from("/"));
                let pane = self.start_pane(dir, rest(&["-n", "-c", "-t"]))?;
                let name = opt("-n");
                let s = self.session_mut(&sid).unwrap();
                s.last = s.active;
                s.windows.push(Window { name: name.clone().unwrap_or_else(shell_name), layout: Layout::Leaf(pane.clone()), focus: pane, zoom: false, named: name.is_some() });
                s.active = s.windows.len() - 1;
                self.layout_all();
                self.save();
                Ok(String::new())
            }
            "split-window" | "splitw" => {
                let (Some((sid, wi)), Some(at)) = (here.clone(), target_pane.clone()) else { return Err("no pane".into()) };
                let dir = dir_of(self).unwrap_or_else(|| PathBuf::from("/"));
                let pane = self.start_pane(dir, rest(&["-c", "-t", "-l", "-p"]))?;
                let size = opt("-p").and_then(|v| v.parse::<f32>().ok()).map(|p| p / 100.0).or_else(|| opt("-l").and_then(|v| v.strip_suffix('%').and_then(|v| v.parse::<f32>().ok()).map(|p| p / 100.0)));
                let s = self.session_mut(&sid).unwrap();
                let w = &mut s.windows[wi];
                let side = flag("-h");
                if flag("-f") {
                    w.layout.split_full(&pane, side, size.map(|v| 1.0 - v).unwrap_or(0.5));
                } else {
                    w.layout.split(&at, &pane, side);
                    if let Some(v) = size {
                        w.layout.set_ratio_of(&pane, 1.0 - v);
                    }
                }
                w.focus = pane;
                w.zoom = false;
                self.layout_all();
                self.save();
                Ok(String::new())
            }
            "select-pane" | "selectp" => {
                let Some((sid, wi)) = here.clone() else { return Err("no pane".into()) };
                let area = self.area();
                let s = self.session_mut(&sid).unwrap();
                let w = &mut s.windows[wi];
                let next = if opt("-t").is_some_and(|t| t.ends_with('+') || t.ends_with('-')) {
                    let ids = w.layout.ids();
                    let i = ids.iter().position(|x| *x == w.focus).unwrap_or(0) as i64;
                    let step = if opt("-t").unwrap().ends_with('+') { 1 } else { -1 };
                    Some(ids[((i + step).rem_euclid(ids.len() as i64)) as usize].clone())
                } else {
                    let d = if flag("-L") { Dir::Left } else if flag("-R") { Dir::Right } else if flag("-U") { Dir::Up } else { Dir::Down };
                    w.layout.neighbour(&w.focus, d, area)
                };
                if let Some(n) = next {
                    self.focus_pane(&n);
                }
                self.layout_all();
                Ok(String::new())
            }
            "resize-pane" | "resizep" => {
                let Some((sid, wi)) = here.clone() else { return Err("no pane".into()) };
                let area = self.area();
                let s = self.session_mut(&sid).unwrap();
                let w = &mut s.windows[wi];
                if flag("-Z") {
                    w.zoom = !w.zoom && w.layout.ids().len() > 1;
                } else {
                    let n: u16 = rest(&["-t"]).first().and_then(|v| v.parse().ok()).unwrap_or(1);
                    let d = if flag("-L") { Dir::Left } else if flag("-R") { Dir::Right } else if flag("-U") { Dir::Up } else { Dir::Down };
                    let f = w.focus.clone();
                    w.layout.resize(&f, d, n, area);
                }
                self.layout_all();
                Ok(String::new())
            }
            "kill-pane" | "killp" => {
                let p = target_pane.clone().ok_or("no pane")?;
                if let Some(p) = self.panes.get(&p) {
                    p.hang_up();
                }
                Ok(String::new())
            }
            "kill-window" | "killw" => {
                let Some((sid, wi)) = here.clone() else { return Err("no window".into()) };
                let ids = self.session(&sid).map(|s| s.windows[wi].layout.ids()).unwrap_or_default();
                for id in ids {
                    if let Some(p) = self.panes.get(&id) {
                        p.hang_up();
                    }
                }
                Ok(String::new())
            }
            "kill-session" => {
                let sid = match opt("-t") {
                    Some(t) => self.find_session(&t).ok_or_else(|| format!("no session {t:?}"))?,
                    None => here.clone().map(|h| h.0).ok_or("no session")?,
                };
                let ids: Vec<String> = self.session(&sid).map(|s| s.windows.iter().flat_map(|w| w.layout.ids()).collect()).unwrap_or_default();
                for id in ids {
                    if let Some(p) = self.panes.get(&id) {
                        p.hang_up();
                    }
                }
                Ok(String::new())
            }
            "kill-server" => {
                for p in self.panes.values() {
                    p.hang_up();
                }
                Ok(String::new())
            }
            "next-window" | "next" | "previous-window" | "prev" | "last-window" | "last" | "select-window" | "selectw" => {
                let Some((sid, _)) = here.clone() else { return Err("no session".into()) };
                let s = self.session_mut(&sid).unwrap();
                let n = s.windows.len();
                let to = match cmd.as_str() {
                    "next-window" | "next" => (s.active + 1) % n,
                    "previous-window" | "prev" => (s.active + n - 1) % n,
                    "last-window" | "last" => s.last.min(n - 1),
                    _ => match opt("-t").as_deref() {
                        Some(t) if t.ends_with('+') => (s.active + 1) % n,
                        Some(t) if t.ends_with('-') => (s.active + n - 1) % n,
                        Some(t) => t.trim_start_matches(':').parse::<usize>().unwrap_or(s.active).min(n - 1),
                        None => s.active,
                    },
                };
                if to != s.active {
                    s.last = s.active;
                    s.active = to;
                }
                self.layout_all();
                Ok(String::new())
            }
            "rename-window" | "renamew" => {
                let Some((sid, wi)) = here.clone() else { return Err("no window".into()) };
                let name = rest(&["-t"]).join(" ");
                let s = self.session_mut(&sid).unwrap();
                s.windows[wi].name = name;
                s.windows[wi].named = true;
                self.save();
                Ok(String::new())
            }
            "rename-session" | "rename" => {
                let Some((sid, _)) = here.clone() else { return Err("no session".into()) };
                let name = rest(&["-t"]).join(" ");
                if self.sessions.iter().any(|s| s.name == name) {
                    return Err(format!("a session is already called {name:?}"));
                }
                self.session_mut(&sid).unwrap().name = name;
                self.save();
                Ok(String::new())
            }
            "switch-client" | "switchc" => {
                let t = opt("-t").ok_or("switch-client -t SESSION")?;
                let sid = self.find_session(&t).ok_or_else(|| format!("no session {t:?}"))?;
                let c = cid.and_then(|c| self.clients.get_mut(&c)).ok_or("only from an attached client")?;
                c.session = Some(sid);
                c.last.clear();
                Ok(String::new())
            }
            "detach-client" | "detach" => {
                if let Some(c) = cid {
                    self.send(c, &ServerMsg::Bye { reason: "detached".into() });
                    self.clients.remove(&c);
                }
                Ok(String::new())
            }
            "send-keys" | "send" => {
                let p = target_pane.clone().ok_or("no pane")?;
                if flag("-M") {
                    return Ok(String::new());
                }
                if flag("-X") {
                    let mut r = rest(&["-t", "-N"]).into_iter();
                    if let Some(what) = r.next() {
                        self.copy_do(cid, &p, &what, r.next());
                    }
                    return Ok(String::new());
                }
                if self.copy.contains_key(&p) {
                    for k in rest(&["-t"]) {
                        if let Some(c) = Chord::parse(&k) {
                            self.copy_key(cid, &p, &c);
                        }
                    }
                    return Ok(String::new());
                }
                let literal = flag("-l");
                let mut bytes = Vec::new();
                for k in rest(&["-t"]) {
                    match (literal, Chord::parse(&k)) {
                        (false, Some(c)) if k.len() > 1 => bytes.extend(crate::keys::chord_to_bytes(&c)),
                        _ => bytes.extend(k.as_bytes()),
                    }
                }
                if let Some(p) = self.panes.get(&p) {
                    p.input(&bytes);
                }
                Ok(String::new())
            }
            "capture-pane" | "capturep" => {
                let p = target_pane.clone().ok_or("no pane")?;
                let text = self.panes.get(&p).map(|p| p.text()).unwrap_or_default();
                if flag("-p") {
                    Ok(text)
                } else {
                    self.buffers.insert(0, text);
                    Ok(String::new())
                }
            }
            "list-sessions" | "ls" => Ok(self
                .sessions
                .iter()
                .map(|s| {
                    let attached = self.clients.values().filter(|c| c.session.as_deref() == Some(&s.id)).count();
                    format!("{}: {} windows{}{}", s.name, s.windows.len(), s.place.as_ref().map(|p| format!(" ({})", p.display())).unwrap_or_default(), if attached > 0 { " (attached)" } else { "" })
                })
                .collect::<Vec<_>>()
                .join("\n")),
            "list-windows" | "lsw" => {
                let Some((sid, _)) = here.clone() else { return Err("no session".into()) };
                let s = self.session(&sid).unwrap();
                Ok(s.windows.iter().enumerate().map(|(i, w)| format!("{i}: {}{} ({} panes)", self.window_title(w), if i == s.active { " (active)" } else { "" }, w.layout.ids().len())).collect::<Vec<_>>().join("\n"))
            }
            "list-panes" | "lsp" => {
                let Some((sid, wi)) = here.clone() else { return Err("no window".into()) };
                let w = &self.session(&sid).unwrap().windows[wi];
                Ok(w.layout.ids().iter().map(|id| {
                    let p = &self.panes[id];
                    format!("%{id}: [{}x{}] {}{}", p.cols, p.rows, p.cwd().display(), if *id == w.focus { " (active)" } else { "" })
                }).collect::<Vec<_>>().join("\n"))
            }
            "has-session" | "has" => {
                let t = opt("-t").unwrap_or_default();
                self.find_session(&t).map(|_| String::new()).ok_or_else(|| format!("no session {t:?}"))
            }
            "display-message" | "display" => {
                let text = self.fmt(target_pane.as_deref(), &rest(&["-t"]).join(" "));
                if flag("-p") {
                    Ok(text)
                } else {
                    self.notice_all(text);
                    Ok(String::new())
                }
            }
            "source-file" | "source" | "reload" => {
                let mut ext = Ext::new();
                ext.load(&paths::config_dir());
                let n = ext.plugins.len();
                self.keys = build_keys(&ext);
                self.look = Look::from(&self.keys.opts);
                self.ext = ext;
                self.layout_all();
                Ok(format!("config reloaded ({n} plugins)"))
            }
            "lua" => self.ext.eval(&args.join(" ")),
            "show-prefix" => {
                let p = &self.keys.prefix;
                Ok(format!("{}{}{}", if p.ctrl { "C-" } else { "" }, if p.alt { "M-" } else { "" }, p.key))
            }
            other => self.more(cid, ctx_pane.clone(), target_pane.clone(), other, args),
        };
        self.dirty = true;
        r
    }

    /// A key the client handed over (after the prefix, or a no-prefix binding).
    fn key(&mut self, cid: Option<u64>, table: &str, key: &Chord, ctx: Option<String>) {
        // Lua first: a plugin or init.lua may bind any key
        if self.ext.key(table, key) {
            for line in self.ext.take_commands() {
                if let Err(e) = self.run_line(cid, ctx.clone(), &line) {
                    self.notice_all(e);
                }
            }
            return;
        }
        let map = if table == "root" { &self.keys.root } else { &self.keys.table };
        let Some(b) = map.get(key).cloned() else {
            // prefix d detaches unless the user bound it to something else
            if table == "prefix" && key.key == "d" && !key.ctrl {
                let _ = self.command(cid, vec!["detach-client".into()]);
            }
            return;
        };
        if let Err(e) = self.run_line(cid, ctx, &b.cmd) {
            self.notice_all(e);
        }
    }
}

fn write_text(grid: &mut [Vec<Cell>], row: usize, col: usize, text: &str, fg: Color, bg: Color, flags: u8) {
    let Some(r) = grid.get_mut(row) else { return };
    for (i, ch) in text.chars().enumerate() {
        if let Some(c) = r.get_mut(col + i) {
            *c = Cell { text: ch.to_string(), fg, bg, flags };
        }
    }
}

fn shell_name() -> String {
    std::env::var("SHELL").ok().and_then(|s| s.rsplit('/').next().map(String::from)).unwrap_or_else(|| "sh".into())
}

/// The program in front in a shell (what the pane is busy with): the terminal's
/// foreground process group, read from /proc.
fn foreground_name(shell: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{shell}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 2..];
    let tpgid: i32 = after.split_whitespace().nth(5)?.parse().ok()?;
    let pid = if tpgid > 0 { tpgid as u32 } else { shell };
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// An SGR mouse report at the start of `b`: (button, x, y, pressed, bytes used).
fn sgr_mouse(b: &[u8]) -> Option<(u32, u16, u16, bool, usize)> {
    let rest = b.strip_prefix(b"\x1b[<")?;
    let end = rest.iter().position(|c| *c == b'M' || *c == b'm')?;
    let body = std::str::from_utf8(&rest[..end]).ok()?;
    let mut it = body.split(';');
    let btn = it.next()?.parse().ok()?;
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    Some((btn, x, y, rest[end] == b'M', 3 + end + 1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_lines() {
        assert_eq!(super::split_commands(r#"send-keys C-l ; run 'tmux clear-history'"#), vec!["send-keys C-l", "run 'tmux clear-history'"]);
        assert_eq!(super::split_commands(r##"if -F "#{x}" "a" "copy-mode -e; send-keys PageUp""##).len(), 1);
        assert_eq!(super::split_commands("copy-mode -e; send-keys PageUp"), vec!["copy-mode -e", "send-keys PageUp"]);
    }

    /// The first zeit config (caesar's tmux.conf, in Lua) loads and means what it says.
    #[test]
    fn first_config() {
        use crate::keys::Chord;
        let d = std::env::temp_dir().join(format!("zeit-first-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/init.lua"), d.join("init.lua")).unwrap();
        let mut ext = crate::ext::Ext::new();
        ext.load(&d);
        assert!(ext.errors.is_empty(), "{:?}", ext.errors);
        let k = super::build_keys(&ext);
        let c = |s: &str| Chord::parse(s).unwrap();
        assert_eq!(k.prefix, c("C-Space"));
        assert!(k.table[&c("\\")].cmd.starts_with("split-window -h"));
        assert!(k.table[&c("|")].cmd.starts_with("split-window -fh"));
        assert!(k.table[&c("H")].repeat);
        assert_eq!(k.root[&c("M-j")].cmd, "select-pane -t :.+");
        assert!(k.copy[&c("y")].cmd.contains("copy-pipe"));
        assert_eq!(k.opts["status-position"], "top");
        assert_eq!(k.opts["history-limit"], "20000");
        assert_eq!(k.opts.get("@icon:nvim").map(String::as_str), Some("\u{e62b} "));
        // z.tmux(false): nothing from tmux.conf leaks in
        assert!(!k.table.contains_key(&c("M-b")));
        let look = crate::look::Look::from(&k.opts);
        assert!(look.top && look.activity_on);
        assert_eq!(look.history, 20000);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn mouse_reports() {
        assert_eq!(super::sgr_mouse(b"\x1b[<0;12;5Mrest"), Some((0, 12, 5, true, 10)));
        assert_eq!(super::sgr_mouse(b"\x1b[<64;3;4M"), Some((64, 3, 4, true, 10)));
        assert_eq!(super::sgr_mouse(b"\x1b[<0;1;1m"), Some((0, 1, 1, false, 9)));
        assert_eq!(super::sgr_mouse(b"\x1b[A"), None);
    }
}

/// The final keys: tmux.conf (unless init.lua says z.tmux(false)), then init.lua's.
fn build_keys(ext: &Ext) -> Keys {
    let mut keys = if ext.reads_tmux() { Keys::load() } else { Keys::defaults() };
    for (k, v) in ext.opts() {
        if k == "prefix" {
            if let Some(c) = Chord::parse(&v) {
                keys.prefix = c;
            }
        }
        keys.opts.insert(k, v);
    }
    if let Some(p) = ext.prefix() {
        keys.prefix = p;
    }
    for (table, key, cmd, repeat) in ext.bindings() {
        let b = Binding { action: crate::keys::action(&cmd).unwrap_or(Action::Seq(Vec::new())), cmd: if cmd.is_empty() { "__lua".into() } else { cmd }, repeat };
        match table.as_str() {
            "root" => keys.root.insert(key, b),
            "copy" => keys.copy.insert(key, b),
            _ => keys.table.insert(key, b),
        };
    }
    keys
}

/// `tmux` inside zeit is zeit: a tmux → zeit link first in PATH, so bindings and
/// scripts that call `tmux show-buffer` or `tmux kill-window` keep working.
fn tmux_shim(exe: &std::path::Path) {
    let bin = paths::runtime_dir().join("bin");
    let _ = std::fs::create_dir_all(&bin);
    let link = bin.join("tmux");
    let _ = std::fs::remove_file(&link);
    let _ = std::os::unix::fs::symlink(exe, &link);
    let path = std::env::var("PATH").unwrap_or_default();
    if !path.split(':').any(|p| std::path::Path::new(p) == bin) {
        // before any thread starts: nothing reads the environment concurrently
        unsafe { std::env::set_var("PATH", format!("{}:{path}", bin.display())) };
    }
}

/// `a \; b` and `a; b` (outside quotes) are two commands.
fn split_commands(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                out.last_mut().unwrap().push(c);
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                out.last_mut().unwrap().push(c);
            }
            None if c == '\\' && chars.get(i + 1) == Some(&';') => {
                out.push(String::new());
                i += 1;
            }
            None if c == ';' && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) => out.push(String::new()),
            None => out.last_mut().unwrap().push(c),
        }
        i += 1;
    }
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// The index of the `}` closing the `{` at `open`.
fn closing(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at commas that are not inside a nested `#{…}`.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0, 0);
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

#[derive(Clone, Copy, Debug)]
struct CopyState {
    x: usize,
    /// a grid line: 0 is the top of the live screen, history is negative
    y: i32,
    /// where the selection started
    anchor: Option<(i32, usize)>,
    lines: bool,
    rect: bool,
    /// `copy-mode -e`: scrolling back to the bottom leaves
    exit_at_bottom: bool,
    /// the latest search match, shown while nothing is selected
    hit: Option<((i32, usize), (i32, usize))>,
}

impl Server {
    /// Runs a tmux command line (`a \; b`), with formats filled in for its pane.
    fn run_line(&mut self, cid: Option<u64>, ctx: Option<String>, line: &str) -> Reply {
        let mut out = Vec::new();
        for part in split_commands(line) {
            let pane = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c)));
            let mut w = words(&part);
            if w.is_empty() {
                continue;
            }
            // if-shell keeps its formats: it decides with them, then runs a branch
            if !matches!(w[0].as_str(), "if-shell" | "if") {
                for x in w.iter_mut().skip(1) {
                    if x.contains("#{") {
                        *x = self.fmt(pane.as_deref(), x);
                    }
                    if let Some(rest) = x.strip_prefix("$HOME").or_else(|| x.strip_prefix("~/").map(|_| &x[1..])) {
                        *x = format!("{}{rest}", std::env::var("HOME").unwrap_or_default());
                    }
                }
            }
            if let Some(p) = &ctx {
                w.insert(0, format!("@{p}"));
            }
            let r = self.command(cid, w)?;
            if !r.is_empty() {
                out.push(r);
            }
        }
        Ok(out.join("\n"))
    }

    /// tmux formats: `#{var}`, `#{?cond,yes,no}`, `#{==:a,b}`.
    fn fmt(&self, pane: Option<&str>, s: &str) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with("#{") {
                if let Some(end) = closing(s, i + 1) {
                    out.push_str(&self.fmt_expr(pane, &s[i + 2..end]));
                    i = end + 1;
                    continue;
                }
            }
            let c = s[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
        out
    }

    fn fmt_expr(&self, pane: Option<&str>, e: &str) -> String {
        let truthy = |v: &str| !v.is_empty() && v != "0";
        if let Some(rest) = e.strip_prefix('?') {
            let parts = split_top(rest);
            let cond = parts.first().copied().unwrap_or("");
            let v = if cond.contains("#{") { self.fmt(pane, cond) } else { self.var(pane, cond) };
            let pick = if truthy(&v) { parts.get(1) } else { parts.get(2) };
            return pick.map(|p| self.fmt(pane, p)).unwrap_or_default();
        }
        if let Some(rest) = e.strip_prefix("==:").or_else(|| e.strip_prefix("!=:")) {
            let parts = split_top(rest);
            let same = self.fmt(pane, parts.first().unwrap_or(&"")) == self.fmt(pane, parts.get(1).unwrap_or(&""));
            return if same == e.starts_with("==") { "1" } else { "0" }.into();
        }
        if e.starts_with("m:") {
            return "0".into();
        }
        self.var(pane, e)
    }

    fn var(&self, pane: Option<&str>, name: &str) -> String {
        let p = pane.and_then(|p| self.panes.get(p));
        let here = pane.and_then(|p| self.where_is(p));
        let s = here.as_ref().and_then(|(sid, _)| self.session(sid));
        let b = |v: bool| if v { "1" } else { "0" }.to_string();
        match name {
            "pane_current_path" => p.map(|p| p.cwd().display().to_string()).unwrap_or_default(),
            "pane_current_command" => p.and_then(|p| foreground_name(p.pid)).unwrap_or_default(),
            "pane_id" => pane.map(|p| format!("%{p}")).unwrap_or_default(),
            "pane_in_mode" => b(pane.is_some_and(|p| self.copy.contains_key(p))),
            "alternate_on" => b(p.is_some_and(|p| p.alt_screen())),
            "mouse_any_flag" => b(p.is_some_and(|p| p.modes() & zeit_proto::MODE_MOUSE != 0)),
            "session_name" => s.map(|s| s.name.clone()).unwrap_or_default(),
            "session_windows" => s.map(|s| s.windows.len().to_string()).unwrap_or_default(),
            "session_attached" => s.map(|s| self.clients.values().filter(|c| c.session.as_deref() == Some(&s.id)).count().to_string()).unwrap_or_default(),
            "window_index" => here.map(|h| h.1.to_string()).unwrap_or_default(),
            "window_name" => here.as_ref().and_then(|(sid, wi)| self.session(sid).map(|s| self.window_title(&s.windows[*wi]))).unwrap_or_default(),
            "host" | "host_short" => hostname(),
            _ => String::new(),
        }
    }

    /// Whether a pane is in the window some client shows.
    fn shown(&self, pane: &str) -> bool {
        self.clients.keys().any(|c| {
            let Some(s) = self.client_session(*c).and_then(|s| self.session(&s)) else { return false };
            s.windows.get(s.active).is_some_and(|w| w.layout.ids().iter().any(|p| p == pane))
        })
    }

    /// `-t` as tmux reads it: `%pane`, `!` (the last pane), a window's name or index.
    fn resolve_target(&self, t: &str, current: Option<&str>) -> Option<String> {
        let id = t.trim_start_matches('%');
        if self.panes.contains_key(id) {
            return Some(id.to_string());
        }
        let sid = current.and_then(|p| self.where_is(p)).map(|h| h.0)?;
        if t == "!" {
            return self.prev_pane.get(&sid).filter(|p| self.panes.contains_key(*p)).cloned();
        }
        let s = self.session(&sid)?;
        let name = t.rsplit(':').next().unwrap_or(t).split('.').next().unwrap_or("");
        if name.is_empty() || name.starts_with(['+', '-']) {
            return None;
        }
        let w = match name.parse::<usize>() {
            Ok(i) => s.windows.get(i),
            Err(_) => s.windows.iter().find(|w| w.name == name || self.window_title(w) == name),
        }?;
        Some(w.focus.clone())
    }

    /// The commands beyond the core ones: tmux's scripting, buffers, copy mode.
    fn more(&mut self, cid: Option<u64>, ctx: Option<String>, target: Option<String>, cmd: &str, args: &[String]) -> Reply {
        let flag = |f: &str| args.iter().any(|a| a == f || (a.starts_with('-') && !a.starts_with("--") && a.len() > 2 && a[1..].contains(&f[1..])));
        let positional: Vec<String> = {
            let mut out = Vec::new();
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if a == "-t" || a == "-b" && cmd != "if-shell" && cmd != "if" && cmd != "run-shell" && cmd != "run" {
                    i += 2;
                    continue;
                }
                if !(a.starts_with('-') && a.len() > 1) && a != "=" {
                    out.push(a.clone());
                }
                i += 1;
            }
            out
        };
        match cmd {
            "if-shell" | "if" => {
                let cond = positional.first().cloned().unwrap_or_default();
                let yes = if flag("-F") {
                    let v = self.fmt(target.as_deref(), &cond);
                    !v.is_empty() && v != "0"
                } else {
                    std::process::Command::new("sh").arg("-c").arg(&cond).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
                };
                match positional.get(if yes { 1 } else { 2 }) {
                    Some(line) => self.run_line(cid, target.or(ctx), line),
                    None => Ok(String::new()),
                }
            }
            "run-shell" | "run" => {
                let line = positional.join(" ");
                let mut c = std::process::Command::new("sh");
                c.arg("-c").arg(&line).stdin(std::process::Stdio::null());
                if let Some(p) = &target {
                    c.env("ZEIT_PANE", p);
                    if let Some(p) = self.panes.get(p) {
                        c.current_dir(p.cwd());
                    }
                }
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    if let Ok(out) = c.output() {
                        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        let show = if !text.is_empty() { text } else { err };
                        if !show.is_empty() {
                            let _ = tx.send(Ev::Notice(show.lines().next().unwrap_or("").to_string()));
                        }
                    }
                });
                Ok(String::new())
            }
            "clear-history" | "clearhist" => {
                if let Some(p) = target.as_ref().and_then(|p| self.panes.get(p)) {
                    p.clear_history();
                }
                Ok(String::new())
            }
            "copy-mode" => {
                let p = target.ok_or("no pane")?;
                if flag("-q") {
                    self.copy_do(cid, &p, "cancel", None);
                    return Ok(String::new());
                }
                if !self.copy.contains_key(&p) {
                    let Some(pane) = self.panes.get(&p) else { return Err("no pane".into()) };
                    let (x, y) = {
                        let t = pane.term.lock();
                        let c = t.grid().cursor.point;
                        let d = t.grid().display_offset() as i32;
                        let rows = t.screen_lines() as i32;
                        (c.column.0, c.line.0.clamp(-d, rows - 1 - d))
                    };
                    self.copy.insert(p.clone(), CopyState { x, y, anchor: None, lines: false, rect: false, exit_at_bottom: flag("-e"), hit: None });
                }
                if flag("-u") {
                    self.copy_do(cid, &p, "page-up", None);
                }
                self.dirty = true;
                Ok(String::new())
            }
            "set-buffer" | "setb" => {
                let text = positional.join(" ");
                if flag("-a") && !self.buffers.is_empty() {
                    self.buffers[0].push_str(&text);
                } else {
                    self.buffers.insert(0, text);
                }
                self.buffers.truncate(50);
                Ok(String::new())
            }
            "show-buffer" | "showb" => self.buffers.first().cloned().ok_or_else(|| "no buffers".into()),
            "delete-buffer" | "deleteb" => {
                if !self.buffers.is_empty() {
                    self.buffers.remove(0);
                }
                Ok(String::new())
            }
            "save-buffer" | "saveb" => {
                let path = positional.first().ok_or("save-buffer PATH")?;
                std::fs::write(path, self.buffers.first().map(String::as_str).unwrap_or("")).map_err(|e| e.to_string())?;
                Ok(String::new())
            }
            "list-buffers" | "lsb" => Ok(self.buffers.iter().enumerate().map(|(i, b)| format!("buffer{i}: {} bytes: \"{}\"", b.len(), b.chars().take(40).collect::<String>().replace('\n', "\\n"))).collect::<Vec<_>>().join("\n")),
            "paste-buffer" | "pasteb" => {
                let text = self.buffers.first().cloned().ok_or("no buffers")?;
                let p = target.as_ref().and_then(|p| self.panes.get(p)).ok_or("no pane")?;
                let mut bytes = Vec::new();
                let bracket = p.modes() & zeit_proto::MODE_PASTE != 0;
                if bracket {
                    bytes.extend(b"\x1b[200~");
                }
                bytes.extend(text.replace('\n', "\r").as_bytes());
                if bracket {
                    bytes.extend(b"\x1b[201~");
                }
                p.input(&bytes);
                Ok(String::new())
            }
            "swap-window" | "swapw" => {
                let p = ctx.or(target).ok_or("no window")?;
                let (sid, wi) = self.where_is(&p).ok_or("no window")?;
                let t = args.iter().position(|a| a == "-t").and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
                let s = self.session_mut(&sid).unwrap();
                let n = s.windows.len();
                let to = match t.trim_start_matches(':') {
                    "-1" | "-" => (wi + n - 1) % n,
                    "+1" | "+" => (wi + 1) % n,
                    i => i.parse::<usize>().map_err(|_| format!("swap-window: no window {t}"))?.min(n - 1),
                };
                s.windows.swap(wi, to);
                if s.active == wi {
                    s.active = to;
                } else if s.active == to {
                    s.active = wi;
                }
                self.save();
                Ok(String::new())
            }
            "swap-pane" | "swapp" => {
                let p = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c))).ok_or("no pane")?;
                let (sid, wi) = self.where_is(&p).ok_or("no pane")?;
                let ids = self.session(&sid).unwrap().windows[wi].layout.ids();
                let i = ids.iter().position(|x| *x == p).unwrap_or(0);
                let other = if flag("-U") {
                    ids[(i + ids.len() - 1) % ids.len()].clone()
                } else if flag("-D") {
                    ids[(i + 1) % ids.len()].clone()
                } else {
                    target.filter(|t| *t != p).ok_or("swap-pane: which pane?")?
                };
                let s = self.session_mut(&sid).unwrap();
                s.windows[wi].layout.swap(&p, &other);
                self.layout_all();
                self.save();
                Ok(String::new())
            }
            "last-pane" | "lastp" => {
                let p = target.ok_or("no pane")?;
                let (sid, _) = self.where_is(&p).ok_or("no pane")?;
                let prev = self.prev_pane.get(&sid).cloned().ok_or("no last pane")?;
                self.focus_pane(&prev);
                Ok(String::new())
            }
            "set-option" | "set" | "setw" | "set-window-option" => {
                let mut rest = positional.into_iter();
                let name = rest.next().ok_or("set-option NAME VALUE")?;
                let value = rest.collect::<Vec<_>>().join(" ");
                if flag("-u") {
                    self.keys.opts.remove(&name);
                } else {
                    self.keys.opts.insert(name, value);
                }
                self.look = Look::from(&self.keys.opts);
                self.layout_all();
                Ok(String::new())
            }
            "show-options" | "show" => Ok(self.keys.opts.iter().filter(|(k, _)| positional.is_empty() || positional.contains(k)).map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join("\n")),
            "set-environment" | "setenv" | "refresh-client" | "refresh" => Ok(String::new()),
            "send-prefix" => {
                if let Some(p) = target.as_ref().and_then(|p| self.panes.get(p)) {
                    p.input(&crate::keys::chord_to_bytes(&self.keys.prefix));
                }
                Ok(String::new())
            }
            "restart-server" | "upgrade" => {
                // shells live on in their holders; clients reconnect to the next server
                self.save();
                let ids: Vec<u64> = self.clients.keys().copied().collect();
                for c in ids {
                    let name = self.client_session(c).and_then(|s| self.session(&s).map(|s| s.name.clone())).unwrap_or_default();
                    self.send(c, &ServerMsg::Bye { reason: format!("__restart:{name}") });
                }
                if let Some(c) = cid {
                    self.send(c, &ServerMsg::Reply { ok: true, text: "restarting".into() });
                }
                let _ = std::fs::remove_file(paths::server_socket("default"));
                std::process::exit(0);
            }
            "command-prompt" => {
                let cid = cid.ok_or("command-prompt needs an attached client")?;
                let val = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
                let (label, init) = (val("-p"), val("-I").unwrap_or_default());
                let mut skip = false;
                let mut tpl = None;
                for a in args {
                    if skip {
                        skip = false;
                    } else if matches!(a.as_str(), "-p" | "-I" | "-t") {
                        skip = true;
                    } else if !a.starts_with('-') {
                        tpl = Some(a.clone());
                    }
                }
                let label = label.map(|l| format!("{} ", l.trim_end_matches(':').trim_end())).unwrap_or_else(|| ": ".into());
                let cur = init.chars().count();
                self.open_overlay(cid, Overlay::Prompt { label, text: init, cur, then: Then::Command(tpl) });
                Ok(String::new())
            }
            "choose-tree" | "choose-window" | "choose-session" => {
                let cid = cid.ok_or("choose-tree needs an attached client")?;
                let (items, sel) = self.tree_items(flag("-s") || cmd == "choose-session");
                let template = positional.last().cloned();
                self.open_overlay(cid, Overlay::Choose { items, sel, template });
                Ok(String::new())
            }
            "break-pane" | "breakp" => {
                let p = target.or(ctx).ok_or("no pane")?;
                let (sid, wi) = self.where_is(&p).ok_or("no pane")?;
                if self.session(&sid).unwrap().windows[wi].layout.ids().len() < 2 {
                    return Err("break-pane: the window has only this pane".into());
                }
                self.unlink_pane(&p);
                let name = args.iter().position(|a| a == "-n").and_then(|i| args.get(i + 1)).cloned();
                let s = self.session_mut(&sid).ok_or("no session")?;
                s.windows.push(Window { name: name.clone().unwrap_or_else(shell_name), layout: Layout::Leaf(p.clone()), focus: p, zoom: false, named: name.is_some() });
                if !flag("-d") {
                    s.last = s.active;
                    s.active = s.windows.len() - 1;
                }
                self.layout_all();
                self.save();
                Ok(String::new())
            }
            "join-pane" | "joinp" | "move-pane" | "movep" => {
                // -s the pane that moves (this one), -t where it goes (a pane or a window)
                let src = args.iter().position(|a| a == "-s").and_then(|i| args.get(i + 1)).and_then(|t| self.resolve_target(t, ctx.as_deref().or(target.as_deref()))).or_else(|| ctx.clone()).or_else(|| cid.and_then(|c| self.client_pane(c))).or_else(|| {
                    let s = self.sessions.last()?;
                    Some(s.windows.get(s.active)?.focus.clone())
                }).ok_or("no pane")?;
                let dst = target.filter(|t| *t != src).ok_or("join-pane -t WHERE")?;
                let (dsid, _) = self.where_is(&dst).ok_or("no target")?;
                self.unlink_pane(&src);
                let (dsid, dwi) = self.where_is(&dst).unwrap_or((dsid, 0));
                let s = self.session_mut(&dsid).ok_or("no session")?;
                let w = &mut s.windows[dwi];
                if flag("-f") {
                    w.layout.split_full(&src, flag("-h"), 0.5);
                } else {
                    w.layout.split(&dst, &src, flag("-h"));
                }
                if !flag("-d") {
                    w.focus = src;
                }
                self.layout_all();
                self.save();
                Ok(String::new())
            }
            "__keys" => {
                // for the client: which keys are zeit's, and which repeat
                let name = |c: &Chord| format!("{}{}{}", if c.ctrl { "C-" } else { "" }, if c.alt { "M-" } else { "" }, c.key);
                let mut out = vec![format!("prefix {}", name(&self.keys.prefix))];
                out.extend(self.keys.root.keys().map(|c| format!("root {}", name(c))));
                out.extend(self.keys.table.iter().filter(|(_, b)| b.repeat).map(|(c, _)| format!("repeat {}", name(c))));
                Ok(out.join("\n"))
            }
            "display-panes" | "display-popup" | "popup" | "select-layout" | "next-layout" => Err(format!("{cmd}: not in zeit yet")),
            other => Err(format!("unknown command {other:?}")),
        }
    }

    fn focus_pane(&mut self, pane: &str) {
        let Some((sid, wi)) = self.where_is(pane) else { return };
        let s = self.session_mut(&sid).unwrap();
        let w = &mut s.windows[wi];
        let old = std::mem::replace(&mut w.focus, pane.to_string());
        w.zoom = false;
        if old != pane {
            self.prev_pane.insert(sid, old);
        }
        self.layout_all();
    }

    // ── copy mode ──

    /// A key typed while the pane is in copy mode: the user's copy-mode-vi keys, then vi's.
    fn copy_key(&mut self, cid: Option<u64>, pane: &str, c: &Chord) {
        if let Some(b) = self.keys.copy.get(c).cloned() {
            if let Err(e) = self.run_line(cid, Some(pane.to_string()), &b.cmd) {
                self.notice_all(e);
            }
            return;
        }
        let selecting = self.copy.get(pane).is_some_and(|s| s.anchor.is_some());
        let what = match (c.ctrl, c.key.as_str()) {
            (false, "h" | "Left") | (false, "Backspace") => "cursor-left",
            (false, "l" | "Right") => "cursor-right",
            (false, "j" | "Down") => "cursor-down",
            (false, "k" | "Up") => "cursor-up",
            (false, "0" | "Home") => "start-of-line",
            (false, "$" | "End") => "end-of-line",
            (false, "^") => "back-to-indentation",
            (false, "w") => "next-word",
            (false, "b") => "previous-word",
            (false, "e") => "next-word-end",
            (false, "g") => "history-top",
            (false, "G") => "history-bottom",
            (false, "H") => "top-line",
            (false, "M") => "middle-line",
            (false, "L") => "bottom-line",
            (true, "u") => "halfpage-up",
            (true, "d") => "halfpage-down",
            (true, "b") | (false, "PageUp") => "page-up",
            (true, "f") | (false, "PageDown") => "page-down",
            (true, "y") => "scroll-up",
            (true, "e") => "scroll-down",
            (false, "v" | "Space") => "begin-selection",
            (false, "V") => "select-line",
            (true, "v") => "rectangle-toggle",
            (false, "Enter") => "copy-selection-and-cancel",
            (false, "Escape") if selecting => "clear-selection",
            (false, "Escape" | "q") | (true, "c") => "cancel",
            (false, k @ ("/" | "?")) => {
                let back = k == "?";
                if let Some(cid) = cid {
                    let label = if back { "(search up) " } else { "(search down) " };
                    self.open_overlay(cid, Overlay::Prompt { label: label.into(), text: String::new(), cur: 0, then: Then::Search { pane: pane.to_string(), back } });
                }
                return;
            }
            (false, k @ ("n" | "N")) => {
                if let Some((text, back)) = self.last_search.clone() {
                    self.copy_search(pane, &text, back != (k == "N"));
                }
                return;
            }
            _ => return,
        };
        self.copy_do(cid, pane, what, None);
    }

    /// A copy-mode command (`send-keys -X NAME [ARG]`).
    fn copy_do(&mut self, _cid: Option<u64>, pane: &str, what: &str, arg: Option<String>) {
        use alacritty_terminal::grid::{Dimensions, Scroll};
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let Some(p) = self.panes.get(pane) else { return };
        let Some(mut c) = self.copy.get(pane).copied() else { return };
        let mut copied: Option<String> = None;
        if what != "refresh" {
            c.hit = None;
        }
        let mut cancel = false;
        {
            let mut t = p.term.lock();
            let rows = t.screen_lines() as i32;
            let cols = t.columns();
            let hist = t.grid().history_size() as i32;
            let d = t.grid().display_offset() as i32;
            let top = -d;
            let line: Vec<char> = (0..cols).map(|x| t.grid()[Line(c.y)][Column(x)].c).collect();
            let is_word = |ch: char| !ch.is_whitespace();
            let mut view: i32 = 0; // lines to scroll the view up (+) or down (-)
            match what {
                "cursor-left" => c.x = c.x.saturating_sub(1),
                "cursor-right" => c.x = (c.x + 1).min(cols - 1),
                "cursor-up" => c.y -= 1,
                "cursor-down" => c.y += 1,
                "start-of-line" => c.x = 0,
                "end-of-line" => c.x = line.iter().rposition(|ch| !ch.is_whitespace()).unwrap_or(0),
                "back-to-indentation" => c.x = line.iter().position(|ch| !ch.is_whitespace()).unwrap_or(0),
                "next-word" => {
                    let mut x = c.x;
                    while x < cols && is_word(line[x]) {
                        x += 1;
                    }
                    while x < cols && !is_word(line[x]) {
                        x += 1;
                    }
                    if x >= cols {
                        c.y += 1;
                        c.x = 0;
                    } else {
                        c.x = x;
                    }
                }
                "next-word-end" => {
                    let mut x = (c.x + 1).min(cols - 1);
                    while x < cols - 1 && !is_word(line[x]) {
                        x += 1;
                    }
                    while x + 1 < cols && is_word(line[x + 1]) {
                        x += 1;
                    }
                    c.x = x;
                }
                "previous-word" => {
                    let mut x = c.x;
                    while x > 0 && !is_word(line[x - 1]) {
                        x -= 1;
                    }
                    while x > 0 && is_word(line[x - 1]) {
                        x -= 1;
                    }
                    c.x = x;
                }
                "history-top" => {
                    c.y = -hist;
                    c.x = 0;
                }
                "history-bottom" => {
                    c.y = rows - 1;
                    view = -d;
                }
                "top-line" => c.y = top,
                "middle-line" => c.y = top + rows / 2,
                "bottom-line" => c.y = top + rows - 1,
                "halfpage-up" => {
                    c.y -= rows / 2;
                    view = rows / 2;
                }
                "halfpage-down" => {
                    c.y += rows / 2;
                    view = -(rows / 2);
                }
                "page-up" => {
                    c.y -= rows;
                    view = rows;
                }
                "page-down" => {
                    c.y += rows;
                    view = -rows;
                }
                "scroll-up" => view = 1,
                "scroll-down" => view = -1,
                "begin-selection" => {
                    c.anchor = Some((c.y, c.x));
                    c.lines = false;
                }
                "select-line" => {
                    c.anchor = Some((c.y, c.x));
                    c.lines = true;
                }
                "rectangle-toggle" => c.rect = !c.rect,
                "clear-selection" => c.anchor = None,
                "copy-selection" | "copy-selection-no-clear" | "copy-pipe" | "copy-pipe-no-clear" => copied = t.selection_to_string(),
                "copy-selection-and-cancel" | "copy-pipe-and-cancel" | "copy-end-of-line" => {
                    copied = t.selection_to_string();
                    cancel = true;
                }
                "cancel" => cancel = true,
                _ => {}
            }
            c.y = c.y.clamp(-hist, rows - 1);
            c.x = c.x.min(cols - 1);
            if view != 0 {
                t.scroll_display(Scroll::Delta(view));
            }
            // the view follows the cursor
            let d = t.grid().display_offset() as i32;
            if c.y < -d {
                t.scroll_display(Scroll::Delta(-d - c.y));
            } else if c.y > rows - 1 - d {
                t.scroll_display(Scroll::Delta(-(c.y - (rows - 1 - d))));
            }
            // the cursor stays on screen when only the view moved
            let d = t.grid().display_offset() as i32;
            c.y = c.y.clamp(-d, rows - 1 - d);
            if c.exit_at_bottom && d == 0 && matches!(what, "page-down" | "halfpage-down" | "scroll-down" | "history-bottom") && c.anchor.is_none() {
                cancel = true;
            }
            if cancel {
                t.selection = None;
                t.scroll_display(Scroll::Bottom);
            } else {
                t.selection = c.anchor.or(c.hit.map(|h| h.0)).map(|(ay, ax)| {
                    let (ey, ex) = if c.anchor.is_none() { c.hit.map(|h| h.1).unwrap_or((c.y, c.x)) } else { (c.y, c.x) };
                    let ty = if c.lines { SelectionType::Lines } else if c.rect { SelectionType::Block } else { SelectionType::Simple };
                    let mut s = Selection::new(ty, Point::new(Line(ay), Column(ax)), Side::Left);
                    s.update(Point::new(Line(ey), Column(ex)), Side::Right);
                    s
                });
            }
        }
        if cancel {
            self.copy.remove(pane);
        } else {
            self.copy.insert(pane.to_string(), c);
        }
        if let Some(text) = copied.filter(|t| !t.is_empty()) {
            self.buffers.insert(0, text.clone());
            self.buffers.truncate(50);
            if let Some(cmd) = arg.filter(|a| !a.is_empty()) {
                // copy-pipe: the text into a command's stdin
                std::thread::spawn(move || {
                    use std::io::Write;
                    if let Ok(mut ch) = std::process::Command::new("sh").arg("-c").arg(&cmd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
                        if let Some(mut i) = ch.stdin.take() {
                            let _ = i.write_all(text.as_bytes());
                        }
                        let _ = ch.wait();
                    }
                });
            }
        }
        self.dirty = true;
    }

    // ── the status line, tmux.conf's way ──

    fn status_pills(&self, grid: &mut [Vec<Cell>], row: usize, s: &Session, right: &str) {
        let l = &self.look;
        let cols = grid.first().map(|r| r.len()).unwrap_or(0);
        write_text(grid, row, 0, &" ".repeat(cols), l.status_fg, l.status_bg, 0);
        let pill = |grid: &mut [Vec<Cell>], x: usize, text: &str, fg: Color, bg: Color, flags: u8| -> usize {
            write_text(grid, row, x, "\u{e0b6}", bg, l.status_bg, 0);
            write_text(grid, row, x + 1, text, fg, bg, flags);
            let n = text.chars().count();
            write_text(grid, row, x + 1 + n, "\u{e0b4}", bg, l.status_bg, 0);
            x + n + 2
        };
        let mut x = pill(grid, 0, &format!(" {}", s.windows.len()), Color::Indexed(0), l.accent, 0) + 1;
        for (i, w) in s.windows.iter().enumerate() {
            let cmd = self.panes.get(&w.focus).and_then(|p| foreground_name(p.pid)).unwrap_or_default();
            let icon = l.icons.get(&cmd).cloned().unwrap_or_default();
            let text = format!(" {icon}{} ", self.window_title(w));
            let active = w.layout.ids().iter().any(|p| self.activity.contains(p));
            let (fg, bg, fl) = if i == s.active {
                (l.on_primary, l.primary, zeit_proto::BOLD)
            } else if active {
                (l.on_primary, l.activity, 0)
            } else {
                (l.muted, l.pill, 0)
            };
            x = pill(grid, x, &text, fg, bg, fl) + 2;
        }
        let rx = cols.saturating_sub(right.chars().count());
        write_text(grid, row, rx.max(x), right, l.muted, l.status_bg, 0);
    }
}

/// A literal search for alacritty's regex search; lower case finds any case.
fn search_pattern(text: &str) -> String {
    let mut out = String::new();
    if !text.chars().any(|c| c.is_uppercase()) {
        out.push_str("(?i)");
    }
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Server {
    // ── overlays: the input line and the picker ──

    fn open_overlay(&mut self, cid: u64, o: Overlay) {
        if let Some(c) = self.clients.get_mut(&cid) {
            c.overlay = Some(o);
            c.notice = None;
        }
        self.dirty = true;
    }

    fn overlay_input(&mut self, cid: u64, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let Some((k, n)) = crate::keys::chord_from_bytes(&bytes[i..]) else { break };
            i += n;
            let Some(mut o) = self.clients.get_mut(&cid).and_then(|c| c.overlay.take()) else { return };
            let mut keep = true;
            let mut run: Option<Box<dyn FnOnce(&mut Server)>> = None;
            match &mut o {
                Overlay::Prompt { text, cur, then, .. } => {
                    let chars: Vec<char> = text.chars().collect();
                    match (k.ctrl, k.key.as_str()) {
                        (false, "Enter") => {
                            keep = false;
                            let (t, then) = (text.clone(), then.clone());
                            run = Some(Box::new(move |s: &mut Server| s.prompt_done(cid, t, then)));
                        }
                        (false, "Escape") | (true, "c") | (true, "g") => keep = false,
                        (false, "Backspace") | (true, "h") => {
                            if chars.is_empty() {
                                keep = false;
                            } else if *cur > 0 {
                                let mut c = chars;
                                c.remove(*cur - 1);
                                *text = c.into_iter().collect();
                                *cur -= 1;
                            }
                        }
                        (false, "Delete") | (true, "d") if *cur < chars.len() => {
                            let mut c = chars;
                            c.remove(*cur);
                            *text = c.into_iter().collect();
                        }
                        (false, "Left") | (true, "b") => *cur = cur.saturating_sub(1),
                        (false, "Right") | (true, "f") => *cur = (*cur + 1).min(chars.len()),
                        (false, "Home") | (true, "a") => *cur = 0,
                        (false, "End") | (true, "e") => *cur = chars.len(),
                        (true, "u") => {
                            *text = chars[*cur..].iter().collect();
                            *cur = 0;
                        }
                        (true, "w") => {
                            let mut start = *cur;
                            while start > 0 && chars[start - 1] == ' ' {
                                start -= 1;
                            }
                            while start > 0 && chars[start - 1] != ' ' {
                                start -= 1;
                            }
                            *text = chars[..start].iter().chain(chars[*cur..].iter()).collect();
                            *cur = start;
                        }
                        (false, key) if !k.alt && (key.chars().count() == 1 || key == "Space") => {
                            let ch = if key == "Space" { ' ' } else { key.chars().next().unwrap() };
                            let mut c = chars;
                            c.insert(*cur, ch);
                            *text = c.into_iter().collect();
                            *cur += 1;
                        }
                        _ => {}
                    }
                }
                Overlay::Choose { items, sel, template } => {
                    let last = items.len().saturating_sub(1);
                    match (k.ctrl, k.key.as_str()) {
                        (false, "j" | "Down") | (true, "n") => *sel = (*sel + 1).min(last),
                        (false, "k" | "Up") | (true, "p") => *sel = sel.saturating_sub(1),
                        (false, "g" | "Home") => *sel = 0,
                        (false, "G" | "End") => *sel = last,
                        (false, "Escape" | "q") | (true, "c") => keep = false,
                        (false, "x") => {
                            if let Some((_, t)) = items.get(*sel).cloned() {
                                keep = false;
                                run = Some(Box::new(move |s: &mut Server| {
                                    let cmd = if t.ends_with(':') { format!("kill-session -t {}", t.trim_end_matches(':')) } else { format!("kill-window -t {t}") };
                                    s.choose_kill(&cmd);
                                }));
                            }
                        }
                        (false, "Enter") => {
                            keep = false;
                            if let Some((_, t)) = items.get(*sel).cloned() {
                                let tpl = template.clone();
                                run = Some(Box::new(move |s: &mut Server| s.chosen(cid, &t, tpl)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Some(c) = self.clients.get_mut(&cid) {
                if keep {
                    c.overlay = Some(o);
                }
            }
            if let Some(f) = run {
                f(self);
            }
            self.dirty = true;
        }
    }

    fn prompt_done(&mut self, cid: u64, text: String, then: Then) {
        match then {
            Then::Command(tpl) => {
                let line = match tpl {
                    Some(t) => t.replace("%%", &text).replace("%1", &text),
                    None => text,
                };
                if line.trim().is_empty() {
                    return;
                }
                match self.run_line(Some(cid), None, &line) {
                    Ok(out) if !out.is_empty() => self.notice_all(out.lines().next().unwrap_or("").to_string()),
                    Ok(_) => {}
                    Err(e) => self.notice_all(e),
                }
            }
            Then::Search { pane, back } => {
                if !text.is_empty() {
                    self.last_search = Some((text.clone(), back));
                    self.copy_search(&pane, &text, back);
                }
            }
        }
    }

    /// The next match from the copy cursor, wrapping around; true when found.
    fn copy_search(&mut self, pane: &str, text: &str, back: bool) -> bool {
        use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
        use alacritty_terminal::term::search::RegexSearch;
        let Some(p) = self.panes.get(pane) else { return false };
        let Some(mut c) = self.copy.get(pane).copied() else { return false };
        let Ok(mut re) = RegexSearch::new(&search_pattern(text)) else { return false };
        let found = {
            let t = p.term.lock();
            let cols = t.columns();
            let hist = t.grid().history_size() as i32;
            let rows = t.screen_lines() as i32;
            // one cell on, so the same match is not found again
            let (y, x) = if back {
                if c.x > 0 { (c.y, c.x - 1) } else if c.y > -hist { (c.y - 1, cols - 1) } else { (rows - 1, cols - 1) }
            } else if c.x + 1 < cols {
                (c.y, c.x + 1)
            } else if c.y < rows - 1 {
                (c.y + 1, 0)
            } else {
                (-hist, 0)
            };
            let dir = if back { Direction::Left } else { Direction::Right };
            t.search_next(&mut re, Point::new(Line(y), Column(x)), dir, Side::Left, None)
        };
        let Some(m) = found else {
            self.notice_all(format!("not found: {text}"));
            return false;
        };
        let (s, e) = (m.start(), m.end());
        c.y = s.line.0;
        c.x = s.column.0;
        c.hit = Some(((s.line.0, s.column.0), (e.line.0, e.column.0)));
        self.copy.insert(pane.to_string(), c);
        self.copy_do(None, pane, "refresh", None);
        true
    }

    /// The picker's choice: into the template, or go there.
    fn chosen(&mut self, cid: u64, target: &str, template: Option<String>) {
        if let Some(t) = template {
            if let Err(e) = self.run_line(Some(cid), None, &t.replace("%%", target)) {
                self.notice_all(e);
            }
            return;
        }
        let (name, idx) = target.split_once(':').unwrap_or((target, ""));
        let Some(sid) = self.find_session(name) else { return };
        if let Some(c) = self.clients.get_mut(&cid) {
            c.session = Some(sid.clone());
            c.last.clear();
        }
        if let (Ok(i), Some(s)) = (idx.parse::<usize>(), self.session_mut(&sid)) {
            if i < s.windows.len() && i != s.active {
                s.last = s.active;
                s.active = i;
            }
        }
        self.layout_all();
    }

    fn choose_kill(&mut self, cmd: &str) {
        if let Err(e) = self.run_line(None, None, cmd) {
            self.notice_all(e);
        }
    }

    /// choose-tree's rows: every session, and its windows under it.
    fn tree_items(&self, sessions_only: bool) -> (Vec<(String, String)>, usize) {
        let mut items = Vec::new();
        let mut sel = 0;
        let here = self.clients.values().find_map(|c| c.session.clone());
        for s in &self.sessions {
            let attached = self.clients.values().filter(|c| c.session.as_deref() == Some(&s.id)).count();
            if here.as_deref() == Some(&s.id) && sessions_only {
                sel = items.len();
            }
            items.push((format!("{}  {} windows{}", s.name, s.windows.len(), if attached > 0 { "  (attached)" } else { "" }), format!("{}:", s.name)));
            if sessions_only {
                continue;
            }
            for (i, w) in s.windows.iter().enumerate() {
                if here.as_deref() == Some(&s.id) && i == s.active {
                    sel = items.len();
                }
                let panes = w.layout.ids().len();
                items.push((format!("  {i}: {}{}", self.window_title(w), if panes > 1 { format!("  ({panes} panes)") } else { String::new() }), format!("{}:{i}", s.name)));
            }
        }
        (items, sel)
    }

    /// The overlay over the screen: the input line on the status row, the picker in a box.
    fn draw_overlay(&self, o: &Overlay, grid: &mut [Vec<Cell>], status_row: usize, area: Rect) -> Option<(u16, u16)> {
        let l = &self.look;
        let cols = grid.first().map(|r| r.len()).unwrap_or(0);
        match o {
            Overlay::Prompt { label, text, cur, .. } => {
                write_text(grid, status_row, 0, &" ".repeat(cols), l.status_fg, l.status_bg, 0);
                write_text(grid, status_row, 0, "\u{e0b6}", l.primary, l.status_bg, 0);
                write_text(grid, status_row, 1, label, l.on_primary, l.primary, zeit_proto::BOLD);
                let n = label.chars().count();
                write_text(grid, status_row, 1 + n, "\u{e0b4}", l.primary, l.status_bg, 0);
                let start = 3 + n;
                // a long line scrolls so the cursor stays visible
                let room = cols.saturating_sub(start + 1).max(1);
                let skip = cur.saturating_sub(room - 1);
                let shown: String = text.chars().skip(skip).take(room).collect();
                write_text(grid, status_row, start, &shown, Color::Default, l.status_bg, 0);
                Some(((start + cur - skip) as u16, status_row as u16))
            }
            Overlay::Choose { items, sel, .. } => {
                let w = (items.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(10) + 6).clamp(24, (area.w as usize).saturating_sub(4).max(24));
                let h = (items.len() + 2).min((area.h as usize).saturating_sub(2).max(3));
                let x0 = area.x as usize + (area.w as usize).saturating_sub(w) / 2;
                let y0 = area.y as usize + (area.h as usize).saturating_sub(h) / 2;
                let inner = h - 2;
                let top = sel.saturating_sub(inner.saturating_sub(1));
                let b = l.active_border;
                write_text(grid, y0, x0, &format!("╭{}╮", "─".repeat(w - 2)), b, Color::Default, 0);
                write_text(grid, y0, x0 + 2, " choose ", b, Color::Default, zeit_proto::BOLD);
                for r in 0..inner {
                    let y = y0 + 1 + r;
                    write_text(grid, y, x0, &format!("│{}│", " ".repeat(w - 2)), b, Color::Default, 0);
                    if let Some((label, _)) = items.get(top + r) {
                        let text: String = format!(" {label}").chars().take(w - 3).collect();
                        let pad = format!("{text:<width$}", width = w - 2);
                        if top + r == *sel {
                            write_text(grid, y, x0 + 1, &pad, l.on_primary, l.primary, zeit_proto::BOLD);
                        } else {
                            write_text(grid, y, x0 + 1, &pad, Color::Default, Color::Default, 0);
                        }
                    }
                }
                write_text(grid, y0 + h - 1, x0, &format!("╰{}╯", "─".repeat(w - 2)), b, Color::Default, 0);
                let hint = " enter go · x kill · q close ";
                if w > hint.chars().count() + 4 {
                    write_text(grid, y0 + h - 1, x0 + w - 2 - hint.chars().count(), hint, l.muted, Color::Default, 0);
                }
                None
            }
        }
    }
}
