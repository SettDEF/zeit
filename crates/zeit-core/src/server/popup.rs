//! Popups: panes that float over the layout (tmux's display-popup, zellij's floating panes).
//! They run while hidden, survive detaching, and keep their output when the program ends.

use super::*;

/// An offset this far is "at the edge": the frame is clamped there, so a docked zfloat stays
/// flush with that side whatever the screen's size.
const EDGE: i16 = 10_000;

/// The frame's buttons (" ⌄ – ×") start this many cells from its right edge: the × sits next to
/// the corner. zash draws them at the same place (render.rs, ZFLOAT_BUTTONS), so clicks land.
const BUTTONS_FROM_RIGHT: u16 = 7;

/// Cells from an edge that still count as at it, when a dragged zfloat is let go.
const SNAP: i16 = 2;

/// How far a zfloat of `size` can move from the middle of `total` cells, left and right (or up
/// and down). The middle is `(total - size) / 2` rounded down, as popup_rect places it, so an
/// odd spare puts the extra cell on the right: docked right is then flush, not a cell short.
fn span(total: u16, size: u16) -> (i16, i16) {
    let spare = total.saturating_sub(size) as i16;
    let mid = spare / 2;
    (-mid, spare - mid)
}

/// An offset from the middle, docked (EDGE) when it ends within SNAP cells of either limit.
fn snap(off: i16, (lo, hi): (i16, i16)) -> i16 {
    if hi - lo <= 2 * SNAP {
        return off; // nearly as wide as the screen: no room to tell docked from not
    }
    if off >= hi - SNAP {
        EDGE
    } else if off <= lo + SNAP {
        -EDGE
    } else {
        off
    }
}

/// The offsets of a side (right, top-left …): pinned to those edges.
fn pinned(side: &str) -> (i16, i16) {
    let dx = if side.contains("left") { -EDGE } else if side.contains("right") { EDGE } else { 0 };
    let dy = if side.contains("top") { -EDGE } else if side.contains("bottom") { EDGE } else { 0 };
    (dx, dy)
}

/// A popup of a session: its pane, its name, what it runs, its size in % of the screen.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct Popup {
    pub pane: String,
    pub name: String,
    pub cmd: String,
    pub w: u16,
    pub h: u16,
    /// moved from the middle (popup-move), in cells
    pub dx: i16,
    pub dy: i16,
    /// where it is drawn on its way to dx, dy (it glides there)
    #[serde(skip)]
    pub ax: f32,
    #[serde(skip)]
    pub ay: f32,
}

/// A popup being dragged by a client: its session, where the drag began, the offsets or size then.
pub(super) struct Drag {
    pub sid: String,
    pub x0: u16,
    pub y0: u16,
    pub d0: (i16, i16),
    pub s0: (u16, u16),
    /// Ctrl held: the drag resizes
    pub resize: bool,
}

impl Server {
    /// Where a session's shown popup sits: (outer frame, inside).
    /// A popup's size in cells.
    pub(super) fn popup_size(&self, p: &Popup) -> (u16, u16) {
        let area = self.area();
        let w = (area.w as u32 * p.w.clamp(20, 100) as u32 / 100) as u16;
        let h = (area.h as u32 * p.h.clamp(20, 100) as u32 / 100) as u16;
        (w.max(10).min(area.w), h.max(4).min(area.h))
    }

    /// A zfloat dropped next to an edge docks there again (it stays flush when the screen
    /// changes size); a click on its frame that moved it a cell must not unpin it for good.
    fn dock_dropped(&mut self, sid: &str) {
        let shown = self.popup_shown.get(sid).cloned();
        let Some(p) = self.popups.get(sid).and_then(|l| l.iter().find(|p| Some(&p.pane) == shown.as_ref())).cloned() else { return };
        let area = self.area();
        let (w, h) = self.popup_size(&p);
        let (dx, dy) = (snap(p.dx, span(area.w, w)), snap(p.dy, span(area.h, h)));
        if let Some(q) = self.popups.get_mut(sid).and_then(|l| l.iter_mut().find(|q| q.pane == p.pane)) {
            (q.dx, q.dy) = (dx, dy);
            (q.ax, q.ay) = (dx as f32, dy as f32);
        }
    }

    /// Where a zfloat really is, as an offset from the middle (a pinned one: at its edge).
    pub(super) fn real_offset(&self, p: &Popup) -> (i16, i16) {
        let area = self.area();
        let (w, h) = self.popup_size(p);
        let ((lx, hx), (ly, hy)) = (span(area.w, w), span(area.h, h));
        (p.dx.clamp(lx, hx), p.dy.clamp(ly, hy))
    }

    pub(super) fn popup_rect(&self, p: &Popup) -> (Rect, Rect) {
        let area = self.area();
        let (w, h) = self.popup_size(p);
        // centred, then moved as far as asked but never off the screen
        let x = (((area.w - w) / 2) as i32 + p.ax.round() as i32).clamp(0, (area.w - w) as i32) as u16;
        let y = (((area.h - h) / 2) as i32 + p.ay.round() as i32).clamp(0, (area.h - h) as i32) as u16;
        let outer = Rect { x: area.x + x, y: area.y + y, w, h };
        let inner = Rect { x: outer.x + 1, y: outer.y + 1, w: outer.w.saturating_sub(2), h: outer.h.saturating_sub(2) };
        (outer, inner)
    }

    /// The popup a session shows right now.
    pub(super) fn shown_popup(&self, sid: &str) -> Option<&Popup> {
        let pane = self.popup_shown.get(sid)?;
        self.popups.get(sid)?.iter().find(|p| &p.pane == pane)
    }

    pub(super) fn save_popups(&self) {
        if let Ok(bytes) = postcard::to_stdvec(&self.popups) {
            let f = self.state_file.with_extension("popups");
            let tmp = f.with_extension("popups-tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, f);
            }
        }
    }

    /// The popups a previous server left whose programs still run.
    pub(super) fn restore_popups(&mut self) {
        let Ok(bytes) = std::fs::read(self.state_file.with_extension("popups")) else { return };
        let Ok(saved) = postcard::from_bytes::<HashMap<String, Vec<Popup>>>(&bytes) else { return };
        for (sid, list) in saved {
            if self.session(&sid).is_none() {
                continue;
            }
            let mut kept = Vec::new();
            for p in list {
                if let Ok(pane) = Pane::connect(&p.pane, self.size.0, self.size.1, PathBuf::from("/"), self.look.history, self.pane_tx.clone()) {
                    self.panes.insert(p.pane.clone(), pane);
                    let mut p = p;
                    (p.ax, p.ay) = (p.dx as f32, p.dy as f32);
                    kept.push(p);
                }
            }
            if !kept.is_empty() {
                self.popups.insert(sid, kept);
            }
        }
    }

    /// Sizes the popups' panes for their frames.
    pub(super) fn layout_popups(&mut self) {
        let sizes: Vec<(String, Rect)> = self.popups.values().flatten().map(|p| (p.pane.clone(), self.popup_rect(p).1)).collect();
        for (id, r) in sizes {
            if let Some(p) = self.panes.get_mut(&id) {
                p.set_size(r.w.max(1), r.h.max(1));
            }
        }
    }

    /// A popup's pane went away.
    pub(super) fn popup_gone(&mut self, pane: &str) -> bool {
        let mut found = false;
        for list in self.popups.values_mut() {
            let n = list.len();
            list.retain(|p| p.pane != pane);
            found |= list.len() != n;
        }
        self.popup_shown.retain(|_, p| p != pane);
        if found {
            self.save_popups();
            self.dirty = true;
        }
        found
    }

    /// `float [-n NAME] [-w %] [-h %] [-c DIR] [-a SIDE] [-B] [-s SESSION] [COMMAND …]` and its friends.
    pub(super) fn popup_command(&mut self, cid: Option<u64>, ctx: Option<String>, cmd: &str, args: &[String]) -> Reply {
        // -s SESSION: for config hooks, which run without a client
        let asked = args.iter().position(|a| a == "-s").and_then(|i| args.get(i + 1)).and_then(|n| self.find_session(n));
        let sid = asked.or_else(|| cid.and_then(|c| self.client_session(c))).or_else(|| ctx.as_deref().and_then(|p| self.where_is(p)).map(|w| w.0)).or_else(|| self.sessions.first().map(|s| s.id.clone())).ok_or("no session")?;
        // flags with values, then the command
        let (mut name, mut w, mut h, mut dir, mut rest) = (None::<String>, None::<u16>, None::<u16>, None::<PathBuf>, Vec::new());
        // -a SIDE: docked when made (right, left, top-right …); -B: the keys stay with the panes
        let (mut side, mut stay_below) = (None::<String>, false);
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            let val = || args.get(i + 1).cloned().unwrap_or_default();
            match a {
                "-n" | "-T" => {
                    name = Some(val());
                    i += 2;
                }
                "-w" => {
                    w = val().trim_end_matches('%').parse().ok();
                    i += 2;
                }
                "-h" => {
                    h = val().trim_end_matches('%').parse().ok();
                    i += 2;
                }
                "-c" | "-d" => {
                    dir = Some(PathBuf::from(val()));
                    i += 2;
                }
                "-a" => {
                    side = Some(val());
                    i += 2;
                }
                "-s" => i += 2,
                "-B" => {
                    stay_below = true;
                    i += 1;
                }
                "-E" | "-EE" => i += 1,
                "--" => {
                    rest.extend(args[i + 1..].iter().cloned());
                    break;
                }
                _ => {
                    rest.extend(args[i..].iter().cloned());
                    break;
                }
            }
        }
        let command = rest.join(" ");
        // what the command line leaves out, this zfloat's own settings give (z.zfloat in the config)
        let opts = self.ext.zfloat_opts(name.as_deref().unwrap_or(""));
        let (w, h) = (w.or(opts.width).unwrap_or(80), h.or(opts.height).unwrap_or(80));
        let side = side.or(opts.side.clone());
        let stay_below = stay_below || opts.below.unwrap_or(false);
        let list = self.popups.get(&sid).cloned().unwrap_or_default();
        let named = |n: &str| list.iter().find(|p| p.name == n).cloned();
        match cmd {
            "zfloat-hide" => {
                self.popup_shown.remove(&sid);
                self.popup_below.remove(&sid);
            }
            // popup-layer [up|down]: the keyboard to the popup (up) or the panes under it (down);
            // the popup stays in view either way
            "zfloat-layer" => {
                if !self.popup_shown.contains_key(&sid) {
                    return Err("no popup shown (prefix f shows one)".into());
                }
                let down = match rest.first().map(String::as_str) {
                    Some("up") => false,
                    Some("down") => true,
                    _ => !self.popup_below.contains(&sid),
                };
                if down {
                    self.popup_below.insert(sid.clone());
                } else {
                    self.popup_below.remove(&sid);
                }
            }
            // popup-move -U|-D|-L|-R [N] (or -c: back to the middle)
            "zfloat-move" => {
                let shown = self.popup_shown.get(&sid).cloned().ok_or("no popup shown")?;
                let area = self.area();
                let n: i16 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(2);
                let dir = args.iter().find(|a| matches!(a.as_str(), "-U" | "-D" | "-L" | "-R" | "-c")).cloned().unwrap_or_default();
                // from where it really is (a pinned one leaves its edge)
                let real = self.popups.get(&sid).and_then(|l| l.iter().find(|p| p.pane == shown)).map(|p| self.real_offset(p));
                if let Some(p) = self.popups.get_mut(&sid).and_then(|l| l.iter_mut().find(|p| p.pane == shown)) {
                    if let Some((rx, ry)) = real {
                        (p.dx, p.dy) = (rx, ry);
                    }
                    match dir.as_str() {
                        "-U" => p.dy -= n,
                        "-D" => p.dy += n,
                        "-L" => p.dx -= n * 2,
                        "-R" => p.dx += n * 2,
                        _ => (p.dx, p.dy) = (0, 0),
                    }
                    // no further than the edge (so the way back is as long)
                    p.dx = p.dx.clamp(-(area.w as i16) / 2, area.w as i16 / 2);
                    p.dy = p.dy.clamp(-(area.h as i16) / 2, area.h as i16 / 2);
                }
                self.save_popups();
            }
            // popup-align center|top|bottom|left|right|top-left|top-right|bottom-left|bottom-right
            "zfloat-align" => {
                let shown = self.popup_shown.get(&sid).cloned().ok_or("no popup shown")?;
                let area = self.area();
                let to = rest.first().cloned().unwrap_or_else(|| "center".into());
                let _ = area;
                let (dx, dy) = pinned(&to);
                if !matches!(to.as_str(), "center" | "centre" | "top" | "bottom" | "left" | "right" | "top-left" | "top-right" | "bottom-left" | "bottom-right") {
                    return Err("zfloat-align center|top|bottom|left|right|top-left|top-right|bottom-left|bottom-right".into());
                }
                if let Some(p) = self.popups.get_mut(&sid).and_then(|l| l.iter_mut().find(|p| p.pane == shown)) {
                    (p.dx, p.dy) = (dx, dy);
                }
                self.save_popups();
            }
            "zfloat-close" => {
                let target = name.as_deref().and_then(named).map(|p| p.pane).or_else(|| self.popup_shown.get(&sid).cloned()).ok_or("no popup shown")?;
                if let Some(p) = self.panes.get(&target) {
                    p.hang_up();
                }
            }
            "zfloat-list" => {
                if list.is_empty() {
                    return Err("no popups (prefix f makes one)".into());
                }
                let items = list.iter().map(|p| (format!("{}  {}", p.name, if p.cmd.is_empty() { "shell".to_string() } else { p.cmd.clone() }), p.name.clone())).collect();
                if let Some(c) = cid {
                    self.open_overlay(c, Overlay::Choose { items, sel: 0, template: Some("zfloat-toggle -n '%%'".into()) });
                }
            }
            "zfloat-next" => {
                if list.is_empty() {
                    return Err("no popups".into());
                }
                let at = self.popup_shown.get(&sid).and_then(|s| list.iter().position(|p| &p.pane == s));
                let next = at.map_or(0, |i| (i + 1) % list.len());
                self.popup_shown.insert(sid.clone(), list[next].pane.clone());
            }
            // popup-toggle: show or hide one (made the first time); popup: always a new one
            _ => {
                let existing = if cmd == "zfloat-toggle" || cmd == "zfloat-show" {
                    match &name {
                        Some(n) => named(n),
                        None => self.popup_shown.get(&sid).and_then(|s| list.iter().find(|p| &p.pane == s).cloned()).or_else(|| list.last().cloned()),
                    }
                } else {
                    None
                };
                match existing {
                    // zfloat-show never hides (a config hook calls it on every attach)
                    Some(p) if cmd != "zfloat-show" && self.popup_shown.get(&sid) == Some(&p.pane) => {
                        self.popup_shown.remove(&sid);
                        self.popup_below.remove(&sid);
                    }
                    Some(p) => {
                        self.popup_shown.insert(sid.clone(), p.pane);
                        if stay_below { self.popup_below.insert(sid.clone()); } else { self.popup_below.remove(&sid); }
                    }
                    None => {
                        let here = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c)));
                        let dir = dir.or_else(|| here.as_ref().and_then(|p| self.panes.get(p)).map(|p| p.cwd()))
                            // from a config hook (no pane asked): the folder of the session's current pane
                            .or_else(|| self.session(&sid).and_then(|s| s.windows.get(s.active)).and_then(|w| self.panes.get(&w.focus)).map(|p| p.cwd()))
                            .or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"));
                        // a command keeps its output on screen when it ends; a shell is just a shell
                        let argv = if command.is_empty() {
                            Vec::new()
                        } else {
                            let shown = command.replace('\'', "'\\''");
                            vec!["/bin/sh".into(), "-c".into(), format!("{command}\ns=$?\nprintf '\\n\\033[2m── %s ended (%s) · Enter closes ──\\033[0m' '{shown}' \"$s\"\nread _")]
                        };
                        let base = name.clone().unwrap_or_else(|| command.split_whitespace().next().map(|c| c.rsplit('/').next().unwrap_or(c).to_string()).unwrap_or_else(|| "shell".into()));
                        let mut label = base.clone();
                        let mut n = 2;
                        while list.iter().any(|p| p.name == label) {
                            label = format!("{base}-{n}");
                            n += 1;
                        }
                        let pane = self.start_pane(dir, argv)?;
                        let mut p = Popup { pane: pane.clone(), name: label, cmd: command, w, h, dx: 0, dy: 0, ax: 0.0, ay: 0.0 };
                        if let Some(s) = &side {
                            // at that side from the start (no glide in from the middle)
                            (p.dx, p.dy) = pinned(s);
                            let (rx, ry) = self.real_offset(&p);
                            (p.ax, p.ay) = (rx as f32, ry as f32);
                        }
                        self.popups.entry(sid.clone()).or_default().push(p);
                        self.popup_shown.insert(sid.clone(), pane);
                        if stay_below { self.popup_below.insert(sid.clone()); } else { self.popup_below.remove(&sid); }
                        self.layout_popups();
                        self.save_popups();
                    }
                }
            }
        }
        // a new place: glide there
        self.animate_popups();
        self.dirty = true;
        Ok(String::new())
    }

    /// Draws a session's shown popup over the grid: a rounded frame, its name, its screen.
    /// Returns the cursor inside it.
    pub(super) fn draw_popup(&self, sid: &str, grid: &mut [Vec<Cell>]) -> Option<(Option<(u16, u16)>, CursorShape)> {
        let p = self.shown_popup(sid)?;
        let pane = self.panes.get(&p.pane)?;
        let (outer, inner) = self.popup_rect(p);
        // the layer with the keyboard stands out: a dim frame when the panes below have it
        let below = self.popup_below.contains(sid);
        let fg = if below { self.look.border } else { self.look.active_border };
        for y in outer.y..outer.y + outer.h {
            for x in outer.x..outer.x + outer.w {
                let Some(cell) = grid.get_mut(y as usize).and_then(|r| r.get_mut(x as usize)) else { continue };
                let top = y == outer.y;
                let bottom = y == outer.y + outer.h - 1;
                let left = x == outer.x;
                let right = x == outer.x + outer.w - 1;
                let text = match (top, bottom, left, right) {
                    (true, _, true, _) => "╭",
                    (true, _, _, true) => "╮",
                    (_, true, true, _) => "╰",
                    (_, true, _, true) => "╯",
                    (true, _, _, _) | (_, true, _, _) => "─",
                    (_, _, true, _) | (_, _, _, true) => "│",
                    _ => " ",
                };
                *cell = Cell { text: text.into(), fg, bg: Color::Default, flags: 0 };
            }
        }
        // the name on the frame, the others' names on its right
        let others: Vec<String> = self.popups.get(sid).map(|l| l.iter().filter(|o| o.pane != p.pane).map(|o| o.name.clone()).collect()).unwrap_or_default();
        let (tf, tb) = if below { (Color::Indexed(0), self.look.border) } else { (self.look.on_primary, self.look.primary) };
        // its own title and buttons, if its settings say so (z.zfloat)
        let opts = self.ext.zfloat_opts(&p.name);
        let title = opts.title.clone().unwrap_or_else(|| p.name.clone());
        write_text(grid, outer.y as usize, outer.x as usize + 2, &format!(" {}{} ", title, if below { " · prefix Tab: up" } else { "" }), tf, tb, zeit_proto::BOLD);
        // the buttons on the right of the frame: presets ⌄, hide –, close ×
        let bx = (outer.x + outer.w).saturating_sub(BUTTONS_FROM_RIGHT);
        if opts.buttons.unwrap_or(true) {
            write_text(grid, outer.y as usize, bx as usize, " ⌄ – ×", fg, Color::Default, zeit_proto::BOLD);
        }
        if !others.is_empty() {
            let more = format!(" {} ", others.join(" · "));
            let x = bx.saturating_sub(more.chars().count() as u16);
            write_text(grid, outer.y as usize, x as usize, &more, fg, Color::Default, 0);
        }
        let (screen, cur, shape) = pane.screen();
        for (y, row) in screen.iter().enumerate().take(inner.h as usize) {
            for (x, cell) in row.iter().enumerate().take(inner.w as usize) {
                if let Some(g) = grid.get_mut(inner.y as usize + y).and_then(|r| r.get_mut(inner.x as usize + x)) {
                    *g = cell.clone();
                }
            }
        }
        Some((if below { None } else { cur.map(|(x, y)| (inner.x + x, inner.y + y)) }, shape))
    }
}

impl Server {
    /// One step of every popup gliding to its place (ease-out); while any still moves,
    /// the next step comes in a frame's time.
    pub(super) fn animate_popups(&mut self) {
        let mut moving = false;
        // where each really goes (a pinned one: its edge, so the glide ends there)
        let targets: Vec<(i16, i16)> = self.popups.values().flatten().map(|p| self.real_offset(p)).collect();
        for (p, (rx, ry)) in self.popups.values_mut().flatten().zip(targets) {
            let (tx, ty) = (rx as f32, ry as f32);
            for (a, t) in [(&mut p.ax, tx), (&mut p.ay, ty)] {
                let d = t - *a;
                if d.abs() < 0.5 {
                    *a = t;
                } else {
                    *a += d * 0.35;
                    moving = true;
                }
            }
        }
        self.dirty = true;
        if moving {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(16));
                let _ = tx.send(Ev::Animate);
            });
        }
    }

    /// The mouse on a shown popup: a press on its frame (or anywhere in it with Alt) drags it,
    /// with Ctrl it resizes; true when the event was the drag's.
    pub(super) fn popup_drag(&mut self, cid: u64, sid: &str, b: u32, cx: u16, cy: u16, press: bool) -> bool {
        let motion = b & 32 != 0;
        if let Some(d) = self.popup_drags.get(&cid) {
            if !press {
                let d = self.popup_drags.remove(&cid).expect("a drag");
                if !d.resize {
                    self.dock_dropped(&d.sid);
                }
                self.layout_popups();
                self.save_popups();
                return true;
            }
            if !motion {
                return true;
            }
            let (sid, d0, s0, resize, x0, y0) = (d.sid.clone(), d.d0, d.s0, d.resize, d.x0, d.y0);
            let area = self.area();
            let shown = self.popup_shown.get(&sid).cloned();
            if let Some(p) = self.popups.get_mut(&sid).and_then(|l| l.iter_mut().find(|p| Some(&p.pane) == shown.as_ref())) {
                let (ddx, ddy) = (cx as i32 - x0 as i32, cy as i32 - y0 as i32);
                if resize {
                    // centred: each side moves by the drag, so the size changes by twice it
                    p.w = ((s0.0 as i32 + 2 * ddx).max(10) * 100 / area.w.max(1) as i32).clamp(20, 100) as u16;
                    p.h = ((s0.1 as i32 + 2 * ddy).max(4) * 100 / area.h.max(1) as i32).clamp(20, 100) as u16;
                } else {
                    p.dx = (d0.0 as i32 + ddx).clamp(-(area.w as i32), area.w as i32) as i16;
                    p.dy = (d0.1 as i32 + ddy).clamp(-(area.h as i32), area.h as i32) as i16;
                    // the hand moves it: no glide
                    (p.ax, p.ay) = (p.dx as f32, p.dy as f32);
                }
            }
            self.dirty = true;
            return true;
        }
        // a new left press
        if !press || motion || b & 64 != 0 || b & 3 != 0 {
            return false;
        }
        let Some(p) = self.shown_popup(sid).cloned() else { return false };
        let (outer, inner) = self.popup_rect(&p);
        // the frame's buttons (when it has them)
        if cy == outer.y && self.ext.zfloat_opts(&p.name).buttons.unwrap_or(true) {
            let bx = (outer.x + outer.w).saturating_sub(BUTTONS_FROM_RIGHT);
            let what = match cx.checked_sub(bx) {
                // two cells each: the terminal may draw them up to a cell further right (flush with its edge)
                Some(1 | 2) => Some("menu"),
                Some(3 | 4) => Some("hide"),
                Some(5 | 6) => Some("close"),
                _ => None,
            };
            if let Some(w) = what {
                match w {
                    "hide" => {
                        self.popup_shown.remove(sid);
                        self.popup_below.remove(sid);
                    }
                    "close" => {
                        if let Some(pane) = self.panes.get(&p.pane) {
                            pane.hang_up();
                        }
                    }
                    _ => {
                        let items = ["center", "top", "bottom", "left", "right", "top-left", "top-right", "bottom-left", "bottom-right"].iter().map(|s| (s.replace('-', " "), s.to_string())).collect();
                        self.open_overlay(cid, Overlay::Choose { items, sel: 0, template: Some("zfloat-align %%".into()) });
                    }
                }
                self.dirty = true;
                return true;
            }
        }
        let inside = |r: Rect| cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h;
        let (alt, ctrl) = (b & 8 != 0, b & 16 != 0);
        if !inside(outer) || (inside(inner) && !alt && !ctrl) {
            return false;
        }
        // the frame's bottom-right corner resizes too
        let corner = cx + 1 == outer.x + outer.w && cy + 1 == outer.y + outer.h;
        let size = self.popup_size(&p);
        self.popup_drags.insert(cid, Drag { sid: sid.to_string(), x0: cx, y0: cy, d0: self.real_offset(&p), s0: size, resize: ctrl || corner });
        self.popup_below.remove(sid);
        self.dirty = true;
        true
    }
}

impl Server {
    /// The config changed: each zfloat takes its settings' size and side again (z.zfloat).
    pub(super) fn apply_zfloat_opts(&mut self) {
        let names: Vec<(String, String)> = self.popups.iter().flat_map(|(sid, l)| l.iter().map(|p| (sid.clone(), p.name.clone()))).collect();
        for (sid, name) in names {
            let o = self.ext.zfloat_opts(&name);
            if let Some(p) = self.popups.get_mut(&sid).and_then(|l| l.iter_mut().find(|p| p.name == name)) {
                if let Some(w) = o.width {
                    p.w = w;
                }
                if let Some(h) = o.height {
                    p.h = h;
                }
                if let Some(s) = &o.side {
                    (p.dx, p.dy) = pinned(s);
                }
            }
        }
        self.layout_popups();
        self.save_popups();
        self.animate_popups();
    }
}

#[cfg(test)]
mod snap_tests {
    use super::{snap, span, EDGE};

    #[test]
    fn an_odd_spare_reaches_both_edges() {
        // 167 columns, 50 wide: 117 spare, middle at 58; docked right is x = 58 + 59 = 117
        assert_eq!(span(167, 50), (-58, 59));
        assert_eq!(span(166, 50), (-58, 58));
        assert_eq!(span(40, 50), (0, 0));
    }

    #[test]
    fn dropped_next_to_an_edge_docks_there() {
        let s = span(167, 50);
        assert_eq!(snap(58, s), EDGE); // a click that moved a docked panel one cell
        assert_eq!(snap(57, s), EDGE);
        assert_eq!(snap(56, s), 56);
        assert_eq!(snap(-57, s), -EDGE);
        assert_eq!(snap(0, s), 0);
        assert_eq!(snap(1, span(52, 50)), 1); // nearly full width: left as it is
    }
}

impl Server {
    /// While client `cid` drags the shown zfloat (a move, not a resize): the cells under its outer
    /// rect's edge band (2 cells), as OSC 7733;zunder for zash. Rows of cells joined by "/", cells by
    /// ".", each "text,fg,bg" with the text in hex (UTF-8) and colours d / iN / rrggbb; cells inside
    /// the band are empty. Empty when there is no such drag.
    pub(super) fn under_dragged(&self, cid: u64, sid: &str, grid: &[Vec<Cell>]) -> String {
        if !self.popup_drags.get(&cid).is_some_and(|d| !d.resize && d.sid == sid) {
            return String::new();
        }
        let Some(p) = self.shown_popup(sid) else { return String::new() };
        let (o, _) = self.popup_rect(p);
        let band = 2u16;
        let color = |c: &Color| match c {
            Color::Default => "d".to_string(),
            Color::Indexed(i) => format!("i{i}"),
            Color::Rgb(r, g, b) => format!("{r:02x}{g:02x}{b:02x}"),
        };
        let rows: Vec<String> = (0..o.h)
            .map(|dy| {
                (0..o.w)
                    .map(|dx| {
                        let edge = dy < band || dy + band >= o.h || dx < band || dx + band >= o.w;
                        match grid.get((o.y + dy) as usize).and_then(|r| r.get((o.x + dx) as usize)).filter(|_| edge) {
                            Some(c) => format!("{},{},{}", c.text.bytes().map(|b| format!("{b:02x}")).collect::<String>(), color(&c.fg), color(&c.bg)),
                            None => String::new(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .collect();
        format!("\x1b]7733;zunder;id={};x={};y={};w={};h={};cells={}\x07", pin_id(&p.pane), o.x, o.y, o.w, o.h, rows.join("/"))
    }
}
