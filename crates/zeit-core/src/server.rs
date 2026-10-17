//! zeitd: owns sessions, windows and panes; serves clients over its socket.
//!
//! One thread runs the state (everything below `Server`); others only read
//! sockets and pass events in. Sessions are written down after every change, so
//! a new server (after a crash or `zeit upgrade`) finds the same sessions and
//! reconnects to the pane holders, which kept the shells running meanwhile.

use crate::ext::{words, Ext};
use crate::keys::{Action, Chord, Dir, Keys};
use crate::layout::{Layout, Rect};
use crate::pane::{Pane, PaneEvent};
use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use zeit_proto::{read_msg, write_msg, Cell, ClientMsg, Color, CursorShape, Frame, ServerMsg, VERSION};

enum Ev {
    Accept(UnixStream),
    Msg(u64, ClientMsg),
    Gone(u64),
    Pane(PaneEvent),
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
}

type Reply = Result<String, String>;

/// Runs the server until its last session ends.
pub fn run(name: &str, exe: PathBuf) -> std::io::Result<()> {
    paths::private_dir(&paths::runtime_dir())?;
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
    let mut keys = Keys::load();
    if let Some(p) = ext.prefix() {
        keys.prefix = p;
    }
    let state_file = paths::state_dir().join(format!("{name}.sessions"));
    let mut s = Server { exe, state_file, sessions: Vec::new(), panes: HashMap::new(), clients: HashMap::new(), next_client: 1, tx, pane_tx, ext, keys, size: (80, 24), dirty: false };
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
                    match Pane::connect(&id, self.size.0, self.size.1, PathBuf::from("/"), self.pane_tx.clone()) {
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
                self.clients.insert(id, Client { out: stream, session: None, cols: 80, rows: 24, last: Vec::new(), notice: None });
            }
            Ev::Gone(id) => {
                self.clients.remove(&id);
            }
            Ev::Msg(id, m) => self.message(id, m),
            Ev::Pane(PaneEvent::Output(_)) => self.dirty = true,
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
            ClientMsg::Input(bytes) => {
                if let Some(p) = self.client_pane(cid) {
                    if let Some(p) = self.panes.get(&p) {
                        p.input(&bytes);
                    }
                }
            }
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
            if let Err(e) = self.command(None, c) {
                self.notice_all(e);
            }
        }
        for n in self.ext.take_notices() {
            self.notice_all(n);
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
        let p = Pane::connect(&id, cols, rows, dir, self.pane_tx.clone()).map_err(|e| e.to_string())?;
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
        Rect { x: 0, y: 0, w: self.size.0, h: self.size.1.saturating_sub(status) }
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
        // borders: the active pane's in green, as tmux does
        let active = Color::Indexed(2);
        let dim = Color::Indexed(8);
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
        let status_row = rows.saturating_sub(1) as usize;
        let mut left = format!(" {} ", s.name);
        let mut wins = String::new();
        for (i, win) in s.windows.iter().enumerate() {
            let title = self.window_title(win);
            wins.push_str(&format!(" {i}:{title}{} ", if i == s.active { "*" } else if i == s.last { "-" } else { "" }));
        }
        left.push_str(&wins);
        let right = notice.unwrap_or_else(|| format!(" {} ", hostname()));
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
        if self.ext.enabled("hintbar") && rows >= 2 {
            let hint = self.hints();
            let hr = status_row - 1;
            write_text(&mut grid, hr, 0, &" ".repeat(cols as usize), Color::Default, Color::Indexed(0), 0);
            write_text(&mut grid, hr, 1, &hint, Color::Indexed(7), Color::Indexed(0), 0);
        }
        let title = self.panes.get(&w.focus).map(|p| p.title.lock().unwrap().clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| format!("zeit: {}", s.name));
        // only the rows that changed go out
        let c = self.clients.get_mut(&cid).unwrap();
        let full = c.last.len() != grid.len() || c.last.first().map(|r| r.len()) != grid.first().map(|r| r.len());
        let changed: Vec<(u16, Vec<Cell>)> = grid.iter().enumerate().filter(|(i, row)| full || c.last.get(*i) != Some(row)).map(|(i, row)| (i as u16, row.clone())).collect();
        c.last = grid;
        let frame = Frame { cols, rows, changed, cursor, cursor_shape: shape, title };
        self.send(cid, &ServerMsg::Frame(frame));
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
        let flag = |f: &str| args.iter().any(|a| a == f);
        let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
        // the current pane: given, the client's, or the latest session's
        let current = ctx_pane.clone().or_else(|| cid.and_then(|c| self.client_pane(c))).or_else(|| {
            let s = self.sessions.last()?;
            Some(s.windows.get(s.active)?.focus.clone())
        });
        let target_pane = opt("-t").filter(|t| self.panes.contains_key(t.trim_start_matches('%'))).map(|t| t.trim_start_matches('%').to_string()).or(current.clone());
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
                let pane = self.start_pane(dir, rest(&["-c", "-t", "-l"]))?;
                let s = self.session_mut(&sid).unwrap();
                let w = &mut s.windows[wi];
                w.layout.split(&at, &pane, flag("-h"));
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
                    w.focus = n;
                    if w.zoom {
                        w.zoom = false;
                    }
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
                Ok(self.panes.get(&p).map(|p| p.text()).unwrap_or_default())
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
                let text = rest(&["-t"]).join(" ");
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
                let mut keys = Keys::load();
                if let Some(p) = ext.prefix() {
                    keys.prefix = p;
                }
                let n = ext.plugins.len();
                self.ext = ext;
                self.keys = keys;
                self.layout_all();
                Ok(format!("config reloaded ({n} plugins)"))
            }
            "lua" => self.ext.eval(&args.join(" ")),
            "show-prefix" => {
                let p = &self.keys.prefix;
                Ok(format!("{}{}{}", if p.ctrl { "C-" } else { "" }, if p.alt { "M-" } else { "" }, p.key))
            }
            other => Err(format!("unknown command {other:?}")),
        };
        self.dirty = true;
        r
    }

    /// A key the client handed over (after the prefix, or a no-prefix binding).
    fn key(&mut self, cid: Option<u64>, table: &str, key: &Chord, ctx: Option<String>) {
        // Lua first: a plugin or init.lua may bind any key
        if self.ext.key(table, key) {
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
        for w in action_words(&b.action, &self.keys) {
            let mut w = w;
            if let Some(p) = &ctx {
                w.insert(0, format!("@{p}"));
            }
            if w.first().map(String::as_str) == Some("__send-prefix") {
                if let Some(p) = cid.and_then(|c| self.client_pane(c)).and_then(|p| self.panes.get(&p)) {
                    p.input(&crate::keys::chord_to_bytes(&self.keys.prefix));
                }
                continue;
            }
            if let Err(e) = self.command(cid, w) {
                self.notice_all(e);
            }
        }
    }
}

/// A key action as zeit commands.
fn action_words(a: &Action, keys: &Keys) -> Vec<Vec<String>> {
    let w = |s: &str| words(s);
    let _ = keys;
    match a {
        Action::Split { side_by_side } => vec![w(if *side_by_side { "split-window -h" } else { "split-window -v" })],
        Action::Focus(d) => vec![w(match d {
            Dir::Left => "select-pane -L",
            Dir::Right => "select-pane -R",
            Dir::Up => "select-pane -U",
            Dir::Down => "select-pane -D",
        })],
        Action::FocusNext => vec![w("select-pane -t :.+")],
        Action::Resize(d, n) => vec![w(&format!("resize-pane {} {n}", match d {
            Dir::Left => "-L",
            Dir::Right => "-R",
            Dir::Up => "-U",
            Dir::Down => "-D",
        }))],
        Action::Zoom => vec![w("resize-pane -Z")],
        Action::ClosePane => vec![w("kill-pane")],
        Action::NewTab => vec![w("new-window")],
        Action::CloseTab => vec![w("kill-window")],
        Action::NextTab => vec![w("next-window")],
        Action::PrevTab => vec![w("previous-window")],
        Action::LastTab => vec![w("last-window")],
        Action::SendPrefix => vec![w("__send-prefix")],
        Action::Search => vec![w("display-message copy mode comes in Z2")],
        Action::Paste => vec![],
        Action::Seq(list) => list.iter().flat_map(|a| action_words(a, keys)).collect(),
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
