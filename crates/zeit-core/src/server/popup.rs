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
/// A float is a popup too: of one window, transient, placed in cells.
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
    /// size and place before zfloat-max made it fill the screen (the toggle goes back to it)
    #[serde(default)]
    pub before_max: Option<(u16, u16, i16, i16)>,
    /// in view only with this window (by a pane of its layout); None: with every window
    pub window: Option<String>,
    /// a float: not saved, and gone with its window
    pub transient: bool,
    /// placed in cells (x, y, w, h) instead of % and offsets, as floats are
    pub cells: Option<(u16, u16, u16, u16)>,
}

impl Popup {
    fn new(pane: String, name: String, cmd: String, w: u16, h: u16) -> Self {
        Popup { pane, name, cmd, w, h, dx: 0, dy: 0, ax: 0.0, ay: 0.0, before_max: None, window: None, transient: false, cells: None }
    }
}

/// A popup as servers before the merge with floats saved it (postcard has no defaults).
#[derive(Deserialize)]
struct OldPopup {
    pane: String,
    name: String,
    cmd: String,
    w: u16,
    h: u16,
    dx: i16,
    dy: i16,
    before_max: Option<(u16, u16, i16, i16)>,
}

/// A float as those servers saved it (state.floats), brought back once as a transient popup.
#[derive(Deserialize)]
struct OldFloat {
    pane: String,
    anchor: String,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    shown: bool,
}

/// A popup being dragged by a client: its session and pane, where the drag began, the offsets,
/// size or cells then.
pub(super) struct Drag {
    pub sid: String,
    pub pane: String,
    pub x0: u16,
    pub y0: u16,
    pub d0: (i16, i16),
    pub s0: (u16, u16),
    pub c0: Option<(u16, u16, u16, u16)>,
    /// Ctrl held: the drag resizes
    pub resize: bool,
}

/// The command a popup runs: whole (; && too), its output kept when it ends; none: a shell.
fn popup_argv(command: &str) -> Vec<String> {
    if command.is_empty() {
        return Vec::new();
    }
    let shown = command.replace('\'', "'\\''");
    vec!["/bin/sh".into(), "-c".into(), format!("{command}\ns=$?\nprintf '\\n\\033[2m── %s ended (%s) · Enter closes ──\\033[0m' '{shown}' \"$s\"\nread _")]
}

/// `base`, or base-2, base-3 … when a popup of the list has that name.
fn free_name(list: &[Popup], base: &str) -> String {
    let mut label = base.to_string();
    let mut n = 2;
    while list.iter().any(|p| p.name == label) {
        label = format!("{base}-{n}");
        n += 1;
    }
    label
}

impl Server {
    /// A popup's size in cells.
    pub(super) fn popup_size(&self, p: &Popup) -> (u16, u16) {
        let area = self.area();
        if let Some((_, _, w, h)) = p.cells {
            return (w.min(area.w), h.min(area.h));
        }
        let w = (area.w as u32 * p.w.clamp(20, 100) as u32 / 100) as u16;
        let h = (area.h as u32 * p.h.clamp(20, 100) as u32 / 100) as u16;
        (w.max(10).min(area.w), h.max(4).min(area.h))
    }

    /// A popup's size in % of the screen (a float's, from its cells).
    pub(super) fn popup_pct(&self, p: &Popup) -> (u16, u16) {
        let area = self.area();
        match p.cells {
            Some((_, _, w, h)) => ((w as u32 * 100 / area.w.max(1) as u32) as u16, (h as u32 * 100 / area.h.max(1) as u32) as u16),
            None => (p.w, p.h),
        }
    }

    /// A zfloat dropped next to an edge docks there again (it stays flush when the screen
    /// changes size); a click on its frame that moved it a cell must not unpin it for good.
    fn dock_dropped(&mut self, sid: &str, pane: &str) {
        let Some(p) = self.popups.get(sid).and_then(|l| l.iter().find(|p| p.pane == pane)).cloned() else { return };
        if p.cells.is_some() {
            return;
        }
        let area = self.area();
        let (w, h) = self.popup_size(&p);
        let (dx, dy) = (snap(p.dx, span(area.w, w)), snap(p.dy, span(area.h, h)));
        if let Some(q) = self.popup_mut(sid, pane) {
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

    /// Where a popup sits: (outer frame, inside).
    pub(super) fn popup_rect(&self, p: &Popup) -> (Rect, Rect) {
        let area = self.area();
        let (w, h) = self.popup_size(p);
        let outer = match p.cells {
            // in cells, kept on the screen
            Some((x, y, _, _)) => Rect { x: x.clamp(area.x, area.x + area.w - w), y: y.clamp(area.y, area.y + area.h - h), w, h },
            // centred, then moved as far as asked but never off the screen
            None => {
                let x = (((area.w - w) / 2) as i32 + p.ax.round() as i32).clamp(0, (area.w - w) as i32) as u16;
                let y = (((area.h - h) / 2) as i32 + p.ay.round() as i32).clamp(0, (area.h - h) as i32) as u16;
                Rect { x: area.x + x, y: area.y + y, w, h }
            }
        };
        let inner = Rect { x: outer.x + 1, y: outer.y + 1, w: outer.w.saturating_sub(2), h: outer.h.saturating_sub(2) };
        (outer, inner)
    }

    /// Whether a popup is in view: it has no window, or its window is the session's current one.
    fn in_view(&self, sid: &str, p: &Popup) -> bool {
        let Some(a) = &p.window else { return true };
        self.session(sid).and_then(|s| s.windows.get(s.active)).is_some_and(|w| w.layout.ids().contains(a))
    }

    /// A session's shown popups in view, bottom first: the last one is focused.
    pub(super) fn shown_popups(&self, sid: &str) -> Vec<&Popup> {
        let (Some(stack), Some(list)) = (self.popup_stack.get(sid), self.popups.get(sid)) else { return Vec::new() };
        stack.iter().filter_map(|pane| list.iter().find(|p| &p.pane == pane)).filter(|p| self.in_view(sid, p)).collect()
    }

    /// The popup on top of a session's shown ones (it has the keys, unless the layer below does).
    pub(super) fn shown_popup(&self, sid: &str) -> Option<&Popup> {
        self.shown_popups(sid).pop()
    }

    fn is_shown(&self, sid: &str, pane: &str) -> bool {
        self.shown_popups(sid).iter().any(|p| p.pane == pane)
    }

    /// Shows a popup on top of the others (it has the keyboard).
    pub(super) fn show_popup(&mut self, sid: &str, pane: &str) {
        let s = self.popup_stack.entry(sid.to_string()).or_default();
        s.retain(|p| p != pane);
        s.push(pane.to_string());
        // a float goes last in the list too, so M-f shows them again in this order
        if let Some(l) = self.popups.get_mut(sid) {
            if let Some(i) = l.iter().position(|p| p.pane == pane && p.transient) {
                let p = l.remove(i);
                l.push(p);
            }
        }
    }

    pub(super) fn hide_popup(&mut self, sid: &str, pane: &str) {
        if let Some(s) = self.popup_stack.get_mut(sid) {
            s.retain(|p| p != pane);
        }
    }

    fn popup_mut(&mut self, sid: &str, pane: &str) -> Option<&mut Popup> {
        self.popups.get_mut(sid)?.iter_mut().find(|p| p.pane == pane)
    }

    /// A popup placed in cells moves to % and offsets, where it is (zfloat-move, -align,
    /// -resize and -max work on those).
    fn to_percent(&mut self, sid: &str, pane: &str) {
        let Some(p) = self.popups.get(sid).and_then(|l| l.iter().find(|p| p.pane == pane)).filter(|p| p.cells.is_some()).cloned() else { return };
        let area = self.area();
        let (o, _) = self.popup_rect(&p);
        let (pw, ph) = self.popup_pct(&p);
        let mut q = p;
        (q.w, q.h, q.cells) = (pw.clamp(20, 100), ph.clamp(20, 100), None);
        let (w, h) = self.popup_size(&q);
        q.dx = (o.x - area.x) as i16 - ((area.w - w) / 2) as i16;
        q.dy = (o.y - area.y) as i16 - ((area.h - h) / 2) as i16;
        (q.ax, q.ay) = (q.dx as f32, q.dy as f32);
        if let Some(p) = self.popup_mut(sid, pane) {
            *p = q;
        }
    }

    /// The popups saved: named ones (floats are transient, they are not kept).
    pub(super) fn save_popups(&self) {
        let kept: HashMap<&String, Vec<&Popup>> = self.popups.iter().map(|(s, l)| (s, l.iter().filter(|p| !p.transient).collect::<Vec<_>>())).filter(|(_, l)| !l.is_empty()).collect();
        if let Ok(bytes) = postcard::to_stdvec(&kept) {
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
        // a server from before floats were popups saved fewer fields
        let saved = postcard::from_bytes::<HashMap<String, Vec<Popup>>>(&bytes).ok().or_else(|| {
            let old = postcard::from_bytes::<HashMap<String, Vec<OldPopup>>>(&bytes).ok()?;
            Some(old.into_iter().map(|(s, l)| (s, l.into_iter().map(|o| Popup { dx: o.dx, dy: o.dy, before_max: o.before_max, ..Popup::new(o.pane, o.name, o.cmd, o.w, o.h) }).collect())).collect())
        });
        for (sid, list) in saved.unwrap_or_default() {
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
        self.restore_old_floats();
    }

    /// The floats an older server saved (state.floats) come back as floats, once.
    fn restore_old_floats(&mut self) {
        let file = self.state_file.with_extension("floats");
        let Ok(bytes) = std::fs::read(&file) else { return };
        let _ = std::fs::remove_file(&file);
        let Ok(saved) = postcard::from_bytes::<Vec<OldFloat>>(&bytes) else { return };
        for f in saved {
            let Some((sid, _)) = self.where_is(&f.anchor) else { continue };
            let Ok(pane) = Pane::connect(&f.pane, f.w.max(3), f.h.max(3), PathBuf::from("/"), self.look.history, self.pane_tx.clone()) else { continue };
            self.panes.insert(f.pane.clone(), pane);
            let list = self.popups.entry(sid.clone()).or_default();
            let name = (1..).map(|n| format!("float-{n}")).find(|n| !list.iter().any(|p| &p.name == n)).unwrap_or_default();
            list.push(Popup { window: Some(f.anchor), transient: true, cells: Some((f.x, f.y, f.w, f.h)), ..Popup::new(f.pane.clone(), name, String::new(), 50, 50) });
            if f.shown {
                self.show_popup(&sid, &f.pane);
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
        for s in self.popup_stack.values_mut() {
            s.retain(|p| p != pane);
        }
        if found {
            self.save_popups();
            self.dirty = true;
        }
        found
    }

    /// Before a tiled pane leaves its window: the window's floats keep it (by another of its
    /// panes); when the window goes, its floats go with it.
    pub(super) fn reanchor_popups(&mut self, pane: &str, window_ids: &[String]) {
        let other = window_ids.iter().find(|i| i.as_str() != pane).cloned();
        let mut hang = Vec::new();
        for p in self.popups.values_mut().flatten().filter(|p| p.window.as_deref() == Some(pane)) {
            match &other {
                Some(o) => p.window = Some(o.clone()),
                None if p.transient => hang.push(p.pane.clone()),
                None => p.window = None,
            }
        }
        for h in hang {
            if let Some(p) = self.panes.get(&h) {
                p.hang_up();
            }
        }
    }

    /// The server restarts: floats are not kept, so their programs end with it.
    pub(super) fn end_transient_popups(&self) {
        for p in self.popups.values().flatten().filter(|p| p.transient) {
            if let Some(pane) = self.panes.get(&p.pane) {
                pane.hang_up();
            }
        }
    }

    /// The session (asked with -s, the client's, the pane's) a popup verb is for.
    fn popup_session(&self, cid: Option<u64>, ctx: Option<&str>, args: &[String]) -> Result<String, String> {
        // -s SESSION: for config hooks, which run without a client
        let asked = args.iter().position(|a| a == "-s").and_then(|i| args.get(i + 1)).and_then(|n| self.find_session(n));
        asked.or_else(|| cid.and_then(|c| self.client_session(c))).or_else(|| ctx.and_then(|p| self.where_is(p)).map(|w| w.0)).or_else(|| self.sessions.first().map(|s| s.id.clone())).ok_or_else(|| "no session".to_string())
    }

    /// A popup becomes a pane of its session's window, beside the focused one (below it with `vertical`).
    fn popup_to_pane(&mut self, sid: &str, pane: &str, vertical: bool) -> Reply {
        if let Some(l) = self.popups.get_mut(sid) {
            l.retain(|p| p.pane != pane);
        }
        self.hide_popup(sid, pane);
        if self.shown_popup(sid).is_none() {
            self.popup_below.remove(sid);
        }
        let s = self.session_mut(sid).ok_or("no session")?;
        let w = s.windows.get_mut(s.active).ok_or("no window")?;
        let at = w.focus.clone();
        w.layout.split(&at, pane, !vertical);
        w.focus = pane.to_string();
        w.zoom = false;
        self.layout_all();
        self.save();
        self.save_popups();
        Ok(String::new())
    }

    /// `float [-n NAME] [-w %] [-h %] [-c DIR] [-a SIDE] [-B] [-s SESSION] [COMMAND …]` and its friends.
    pub(super) fn popup_command(&mut self, cid: Option<u64>, ctx: Option<String>, cmd: &str, args: &[String]) -> Reply {
        let sid = self.popup_session(cid, ctx.as_deref(), args)?;
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
        // the one a verb is for: -n NAME, else the shown one on top
        let target = match &name {
            Some(n) => named(n).map(|p| p.pane),
            None => self.shown_popup(&sid).map(|p| p.pane.clone()),
        };
        match cmd {
            // zfloat-shader [-n name] <shader> [-m seconds]: what zash draws it with (it morphs to it)
            "zfloat-shader" => {
                let mut it = rest.iter();
                let (mut shader, mut morph) = (None, None);
                while let Some(a) = it.next() {
                    if a == "-m" {
                        morph = it.next().and_then(|v| v.parse::<f32>().ok());
                    } else {
                        shader = Some(a.clone());
                    }
                }
                let shader = shader.ok_or("zfloat-shader <shader> (ps3/wave, a path, none)")?;
                let shown = self.shown_popup(&sid).map(|p| p.name.clone());
                let target = name.clone().or(shown).ok_or("no zfloat shown (or -n name)")?;
                self.ext.set_zfloat_shader(&target, shader, morph);
                self.dirty = true;
            }
            "zfloat-hide" => {
                if let Some(t) = &target {
                    self.hide_popup(&sid, t);
                }
                if self.shown_popup(&sid).is_none() {
                    self.popup_below.remove(&sid);
                }
            }
            // popup-layer [up|down]: the keyboard to the popup (up) or the panes under it (down);
            // the popup stays in view either way
            "zfloat-layer" => {
                if self.shown_popup(&sid).is_none() {
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
                let shown = target.ok_or("no popup shown")?;
                self.to_percent(&sid, &shown);
                let area = self.area();
                let n: i16 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(2);
                let dir = args.iter().find(|a| matches!(a.as_str(), "-U" | "-D" | "-L" | "-R" | "-c")).cloned().unwrap_or_default();
                // from where it really is (a pinned one leaves its edge)
                let real = self.popups.get(&sid).and_then(|l| l.iter().find(|p| p.pane == shown)).map(|p| self.real_offset(p));
                if let Some(p) = self.popup_mut(&sid, &shown) {
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
                let shown = target.ok_or("no popup shown")?;
                let to = rest.first().cloned().unwrap_or_else(|| "center".into());
                let (dx, dy) = pinned(&to);
                if !matches!(to.as_str(), "center" | "centre" | "top" | "bottom" | "left" | "right" | "top-left" | "top-right" | "bottom-left" | "bottom-right") {
                    return Err("zfloat-align center|top|bottom|left|right|top-left|top-right|bottom-left|bottom-right".into());
                }
                self.to_percent(&sid, &shown);
                if let Some(p) = self.popup_mut(&sid, &shown) {
                    (p.dx, p.dy) = (dx, dy);
                }
                self.save_popups();
            }
            // zfloat-resize W [H]: in % of the screen; +N / -N grow or shrink (one value: both)
            "zfloat-resize" => {
                let shown = target.ok_or("no zfloat shown")?;
                // its z.zfloat settings: the step of a bare + / -, and how small / big it may get
                let fname = list.iter().find(|p| p.pane == shown).map(|p| p.name.clone()).unwrap_or_default();
                let o = self.ext.zfloat_opts(&fname);
                let step = o.step.unwrap_or(5) as i32;
                let ((minw, minh), (maxw, maxh)) = (o.min.unwrap_or((20, 20)), o.max.unwrap_or((100, 100)));
                let vals: Vec<String> = rest.iter().filter_map(|a| match a.as_str() { "+" => Some(format!("+{step}")), "-" => Some(format!("-{step}")), v if v.parse::<i32>().is_ok() => Some(v.to_string()), _ => None }).collect();
                if vals.is_empty() {
                    return Err("zfloat-resize W [H] (% of the screen; +N / -N grow or shrink, a bare + / - by the zfloat's step)".into());
                }
                let at = |cur: u16, v: &str, lo: u16, hi: u16| -> u16 {
                    let n: i32 = v.parse().unwrap_or(0);
                    (if v.starts_with('+') || v.starts_with('-') { cur as i32 + n } else { n }).clamp(lo.max(20) as i32, hi.min(100) as i32) as u16
                };
                self.to_percent(&sid, &shown);
                if let Some(p) = self.popup_mut(&sid, &shown) {
                    p.w = at(p.w, &vals[0], minw, maxw);
                    p.h = at(p.h, vals.get(1).unwrap_or(&vals[0]), minh, maxh);
                    p.before_max = None;
                }
                self.layout_popups();
                self.save_popups();
            }
            // zfloat-max: the whole screen (or its z.zfloat max size), and back to its size and place
            "zfloat-max" => {
                let shown = target.ok_or("no zfloat shown")?;
                let fname = list.iter().find(|p| p.pane == shown).map(|p| p.name.clone()).unwrap_or_default();
                let (mw, mh) = self.ext.zfloat_opts(&fname).max.unwrap_or((100, 100));
                self.to_percent(&sid, &shown);
                if let Some(p) = self.popup_mut(&sid, &shown) {
                    match p.before_max.take() {
                        Some((w, h, dx, dy)) => (p.w, p.h, p.dx, p.dy) = (w, h, dx, dy),
                        None => {
                            p.before_max = Some((p.w, p.h, p.dx, p.dy));
                            (p.w, p.h, p.dx, p.dy) = (mw.clamp(20, 100), mh.clamp(20, 100), 0, 0);
                        }
                    }
                }
                self.layout_popups();
                self.save_popups();
            }
            // zfloat-to-pane [-v]: the shown zfloat becomes a pane of the window, beside the focused one
            "zfloat-to-pane" => {
                let shown = target.ok_or("no zfloat shown")?;
                self.popup_to_pane(&sid, &shown, args.iter().any(|a| a == "-v"))?;
            }
            // zfloat-from-pane [-n name]: the focused pane leaves the layout and floats (it keeps running)
            "zfloat-from-pane" => {
                let pane = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c))).ok_or("no pane")?;
                let (psid, wi) = self.where_is(&pane).ok_or("no pane")?;
                if self.session(&psid).map(|s| s.windows[wi].layout.ids().len()).unwrap_or(0) < 2 {
                    return Err("zfloat-from-pane: the window has only this pane".into());
                }
                self.unlink_pane(&pane);
                let label = free_name(&list, name.as_deref().unwrap_or("pane"));
                self.popups.entry(psid.clone()).or_default().push(Popup::new(pane.clone(), label, String::new(), w, h));
                self.show_popup(&psid, &pane);
                self.popup_below.remove(&psid);
                self.layout_all();
                self.save();
                self.save_popups();
            }
            "zfloat-close" => {
                let target = target.ok_or("no popup shown")?;
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
            // the next zfloat (floats aside) in place of the one on top
            "zfloat-next" => {
                let named: Vec<&Popup> = list.iter().filter(|p| !p.transient).collect();
                if named.is_empty() {
                    return Err("no popups".into());
                }
                let top = self.shown_popup(&sid).filter(|p| !p.transient).map(|p| p.pane.clone());
                let at = top.as_ref().and_then(|t| named.iter().position(|p| &p.pane == t));
                let next = named[at.map_or(0, |i| (i + 1) % named.len())].pane.clone();
                if let Some(t) = top {
                    self.hide_popup(&sid, &t);
                }
                self.show_popup(&sid, &next);
            }
            // popup-toggle: show or hide one (made the first time); popup: always a new one
            _ => {
                let existing = if cmd == "zfloat-toggle" || cmd == "zfloat-show" {
                    match &name {
                        Some(n) => named(n),
                        // no name: the zfloat on top, else the last one (floats have M-f)
                        None => self.shown_popup(&sid).filter(|p| !p.transient).cloned().or_else(|| list.iter().rev().find(|p| !p.transient).cloned()),
                    }
                } else {
                    None
                };
                match existing {
                    // zfloat-show never hides (a config hook calls it on every attach); a shown
                    // one under others comes up first
                    Some(p) if cmd != "zfloat-show" && self.shown_popup(&sid).is_some_and(|t| t.pane == p.pane) => {
                        self.hide_popup(&sid, &p.pane);
                        if self.shown_popup(&sid).is_none() {
                            self.popup_below.remove(&sid);
                        }
                    }
                    Some(p) => {
                        self.show_popup(&sid, &p.pane);
                        if stay_below { self.popup_below.insert(sid.clone()); } else { self.popup_below.remove(&sid); }
                    }
                    None => {
                        let here = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c)));
                        let dir = dir.or_else(|| here.as_ref().and_then(|p| self.panes.get(p)).map(|p| p.cwd()))
                            // from a config hook (no pane asked): the folder of the session's current pane
                            .or_else(|| self.session(&sid).and_then(|s| s.windows.get(s.active)).and_then(|w| self.panes.get(&w.focus)).map(|p| p.cwd()))
                            .or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"));
                        let base = name.clone().unwrap_or_else(|| command.split_whitespace().next().map(|c| c.rsplit('/').next().unwrap_or(c).to_string()).unwrap_or_else(|| "shell".into()));
                        let label = free_name(&list, &base);
                        let pane = self.start_pane(dir, popup_argv(&command))?;
                        let mut p = Popup::new(pane.clone(), label, command, w, h);
                        if let Some(s) = &side {
                            // at that side from the start (no glide in from the middle)
                            (p.dx, p.dy) = pinned(s);
                            let (rx, ry) = self.real_offset(&p);
                            (p.ax, p.ay) = (rx as f32, ry as f32);
                        }
                        self.popups.entry(sid.clone()).or_default().push(p);
                        self.show_popup(&sid, &pane);
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

    /// `float`, `toggle-floats`, `float-embed`, `float-next`, `float-close`: floats are popups of
    /// one window, transient, placed in cells, named float-N.
    pub(super) fn float_command(&mut self, cid: Option<u64>, ctx: Option<String>, cmd: &str, args: &[String]) -> Reply {
        let here = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c)));
        let sid = self.popup_session(cid, here.as_deref(), &[])?;
        let (wi, anchor, ids) = {
            let s = self.session(&sid).ok_or("no session")?;
            let w = s.windows.get(s.active).ok_or("no window")?;
            (s.active, w.focus.clone(), w.layout.ids())
        };
        // this window's floats
        let mine: Vec<String> = self.popups.get(&sid).map(|l| l.iter().filter(|p| p.window.as_ref().is_some_and(|a| ids.contains(a))).map(|p| p.pane.clone()).collect()).unwrap_or_default();
        let top = self.shown_popup(&sid).map(|p| p.pane.clone()).filter(|_| !self.popup_below.contains(&sid));
        match cmd {
            "toggle-floats" | "float-toggle" => {
                if mine.is_empty() {
                    return self.float_command(cid, ctx, "float", args);
                }
                if mine.iter().any(|p| self.is_shown(&sid, p)) {
                    for p in &mine {
                        self.hide_popup(&sid, p);
                    }
                    if self.shown_popup(&sid).is_none() {
                        self.popup_below.remove(&sid);
                    }
                } else {
                    for p in &mine {
                        self.show_popup(&sid, p);
                    }
                    self.popup_below.remove(&sid);
                }
            }
            "float-next" => {
                let shown = self.shown_popups(&sid);
                let p = shown.first().map(|p| p.pane.clone()).ok_or("no floats shown (M-f shows them)")?;
                // the bottom one comes up
                self.show_popup(&sid, &p);
                self.popup_below.remove(&sid);
            }
            "float-close" => {
                let p = top.ok_or("no float has the keyboard")?;
                if let Some(pane) = self.panes.get(&p) {
                    pane.hang_up();
                }
            }
            // a float into the layout (beside the pane it was over), or the focused pane out of it
            "float-embed" => match top {
                Some(p) => {
                    self.popup_to_pane(&sid, &p, false)?;
                }
                None => {
                    if ids.len() < 2 {
                        return Err("the last pane of a window stays in it".into());
                    }
                    let other = ids.iter().find(|i| **i != anchor).cloned().ok_or("no other pane")?;
                    let s = self.session_mut(&sid).ok_or("no session")?;
                    let w = &mut s.windows[wi];
                    let layout = std::mem::replace(&mut w.layout, Layout::Leaf(String::new()));
                    w.layout = layout.remove(&anchor).unwrap_or(Layout::Leaf(other.clone()));
                    w.focus = other.clone();
                    w.zoom = false;
                    let area = self.area();
                    let (fw, fh) = (area.w * 2 / 3, area.h * 2 / 3);
                    self.new_float(&sid, anchor, other, (area.x + (area.w - fw) / 2, area.y + (area.h - fh) / 2, fw, fh), String::new());
                    self.layout_all();
                    self.save();
                }
            },
            // float [-c DIR] [-w COLS] [-h ROWS] [COMMAND …]: a new one, a little lower and right of the last
            _ => {
                let area = self.area();
                let (mut w, mut h, mut dir, mut rest) = (area.w / 2, area.h / 2, None::<PathBuf>, Vec::new());
                let mut i = 0;
                while i < args.len() {
                    let val = || args.get(i + 1).cloned().unwrap_or_default();
                    match args[i].as_str() {
                        "-w" => {
                            w = val().parse().unwrap_or(w);
                            i += 2;
                        }
                        "-h" => {
                            h = val().parse().unwrap_or(h);
                            i += 2;
                        }
                        "-c" => {
                            dir = Some(PathBuf::from(val()));
                            i += 2;
                        }
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
                let (w, h) = (w.clamp(12, area.w), h.clamp(5, area.h));
                let step = mine.len() as u16 * 2;
                let x = (area.x + (area.w - w) / 2 + step).min(area.x + area.w - w);
                let y = (area.y + (area.h - h) / 2 + step / 2).min(area.y + area.h - h);
                let dir = dir.or_else(|| here.as_ref().and_then(|p| self.panes.get(p)).map(|p| p.cwd())).or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"));
                let command = rest.join(" ");
                let pane = self.start_pane(dir, popup_argv(&command))?;
                // showing one shows the window's others too, as zellij does
                for p in &mine {
                    if !self.is_shown(&sid, p) {
                        self.show_popup(&sid, p);
                    }
                }
                self.new_float(&sid, pane, anchor, (x, y, w, h), command);
            }
        }
        self.layout_popups();
        self.dirty = true;
        Ok(String::new())
    }

    /// A float of the window `anchor` is in, shown on top with the keyboard.
    fn new_float(&mut self, sid: &str, pane: String, anchor: String, cells: (u16, u16, u16, u16), cmd: String) {
        let list = self.popups.entry(sid.to_string()).or_default();
        let name = (1..).map(|n| format!("float-{n}")).find(|n| !list.iter().any(|p| &p.name == n)).unwrap_or_default();
        list.push(Popup { window: Some(anchor), transient: true, cells: Some(cells), ..Popup::new(pane.clone(), name, cmd, 50, 50) });
        self.show_popup(sid, &pane);
        self.popup_below.remove(sid);
    }

    /// Draws a session's shown popups over the grid, bottom first: a frame (rounded for zfloats,
    /// and for floats with 'frames +rounded'), the title, the screen. `top_only`: only the top one (it
    /// is drawn after zunder's look), else the others. Returns the cursor in the top one.
    pub(super) fn draw_popups(&self, sid: &str, grid: &mut [Vec<Cell>], top_only: bool) -> Option<(Option<(u16, u16)>, CursorShape)> {
        let mut shown = self.shown_popups(sid);
        let top = shown.last()?.pane.clone();
        let last = shown.pop();
        if top_only {
            shown = last.into_iter().collect();
        }
        let below = self.popup_below.contains(sid);
        let rounded = self.ext.module("frames").is_some_and(|m| m.flags.iter().any(|f| f == "rounded"));
        let mut out = None;
        for p in shown {
            let Some(pane) = self.panes.get(&p.pane) else { continue };
            let (outer, inner) = self.popup_rect(p);
            // the one with the keyboard stands out: a dim frame for the others, and for all when the panes below have it
            let on = p.pane == top && !below;
            let fg = if on { self.look.active_border } else { self.look.border };
            let (tl, tr, bl, br) = if rounded || !p.transient { ("╭", "╮", "╰", "╯") } else { ("┌", "┐", "└", "┘") };
            for y in outer.y..outer.y + outer.h {
                for x in outer.x..outer.x + outer.w {
                    let Some(cell) = grid.get_mut(y as usize).and_then(|r| r.get_mut(x as usize)) else { continue };
                    let (t, b, l, r) = (y == outer.y, y + 1 == outer.y + outer.h, x == outer.x, x + 1 == outer.x + outer.w);
                    let text = match (t, b, l, r) {
                        (true, _, true, _) => tl,
                        (true, _, _, true) => tr,
                        (_, true, true, _) => bl,
                        (_, true, _, true) => br,
                        (true, _, _, _) | (_, true, _, _) => "─",
                        (_, _, true, _) | (_, _, _, true) => "│",
                        _ => " ",
                    };
                    *cell = Cell { text: text.into(), fg, bg: Color::Default, flags: 0 };
                }
            }
            // its own title and buttons, if its settings say so (z.zfloat); a float is named after its program
            let opts = self.ext.zfloat_opts(&p.name);
            let title = opts.title.clone().or_else(|| p.transient.then(|| foreground_name(pane.pid).unwrap_or_else(|| "float".into()))).unwrap_or_else(|| p.name.clone());
            if p.transient {
                write_text(grid, outer.y as usize, outer.x as usize + 2, &format!(" {title} "), fg, Color::Default, if on { zeit_proto::BOLD } else { 0 });
            } else {
                let (tf, tb) = if on { (self.look.on_primary, self.look.primary) } else { (Color::Indexed(0), self.look.border) };
                let hint = if below && p.pane == top { " · prefix Tab: up" } else { "" };
                write_text(grid, outer.y as usize, outer.x as usize + 2, &format!(" {title}{hint} "), tf, tb, zeit_proto::BOLD);
            }
            // the buttons on the right of the frame: presets ⌄, hide –, close ×
            let bx = (outer.x + outer.w).saturating_sub(BUTTONS_FROM_RIGHT);
            if opts.buttons.unwrap_or(!p.transient) {
                write_text(grid, outer.y as usize, bx as usize, " ⌄ – ×", fg, Color::Default, zeit_proto::BOLD);
            }
            // the other zfloats' names on its right
            let others: Vec<String> = if p.transient { Vec::new() } else { self.popups.get(sid).map(|l| l.iter().filter(|o| o.pane != p.pane && !o.transient).map(|o| o.name.clone()).collect()).unwrap_or_default() };
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
            if p.pane == top {
                out = Some((if below { None } else { cur.map(|(x, y)| (inner.x + x, inner.y + y)) }, shape));
            }
        }
        out
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


    /// The mouse on the shown popups: a press on one brings it up with the keyboard; on its frame
    /// (or anywhere in it with Alt) it drags, with Ctrl or on the corner it resizes. True when the
    /// event was the drag's (or a frame button's).
    pub(super) fn popup_drag(&mut self, cid: u64, sid: &str, b: u32, cx: u16, cy: u16, press: bool) -> bool {
        let motion = b & 32 != 0;
        if let Some(d) = self.popup_drags.get(&cid) {
            if !press {
                let d = self.popup_drags.remove(&cid).expect("a drag");
                if !d.resize {
                    self.dock_dropped(&d.sid, &d.pane);
                }
                self.layout_popups();
                self.save_popups();
                return true;
            }
            if !motion {
                return true;
            }
            let (sid, pane, d0, s0, c0, resize, x0, y0) = (d.sid.clone(), d.pane.clone(), d.d0, d.s0, d.c0, d.resize, d.x0, d.y0);
            let area = self.area();
            if let Some(p) = self.popup_mut(&sid, &pane) {
                let (ddx, ddy) = (cx as i32 - x0 as i32, cy as i32 - y0 as i32);
                match (c0, resize) {
                    // in cells: the top-left stays, the corner follows
                    (Some((x, y, w, h)), true) => p.cells = Some((x, y, (w as i32 + ddx).clamp(12, (area.x + area.w - x) as i32) as u16, (h as i32 + ddy).clamp(5, (area.y + area.h - y) as i32) as u16)),
                    (Some((x, y, w, h)), false) => p.cells = Some(((x as i32 + ddx).clamp(area.x as i32, (area.x + area.w - w) as i32) as u16, (y as i32 + ddy).clamp(area.y as i32, (area.y + area.h - h) as i32) as u16, w, h)),
                    // centred: each side moves by the drag, so the size changes by twice it
                    (None, true) => {
                        p.w = ((s0.0 as i32 + 2 * ddx).max(10) * 100 / area.w.max(1) as i32).clamp(20, 100) as u16;
                        p.h = ((s0.1 as i32 + 2 * ddy).max(4) * 100 / area.h.max(1) as i32).clamp(20, 100) as u16;
                    }
                    (None, false) => {
                        p.dx = (d0.0 as i32 + ddx).clamp(-(area.w as i32), area.w as i32) as i16;
                        p.dy = (d0.1 as i32 + ddy).clamp(-(area.h as i32), area.h as i32) as i16;
                        // the hand moves it: no glide
                        (p.ax, p.ay) = (p.dx as f32, p.dy as f32);
                    }
                }
            }
            self.dirty = true;
            return true;
        }
        // a new left press
        if !press || motion || b & 64 != 0 || b & 3 != 0 {
            return false;
        }
        let inside = |r: Rect| cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h;
        // the top one under the pointer comes up and has the keyboard
        let Some(p) = self.shown_popups(sid).into_iter().rev().find(|p| inside(self.popup_rect(p).0)).cloned() else { return false };
        self.show_popup(sid, &p.pane);
        self.popup_below.remove(sid);
        self.dirty = true;
        let (outer, inner) = self.popup_rect(&p);
        // the frame's buttons (when it has them)
        if cy == outer.y && self.ext.zfloat_opts(&p.name).buttons.unwrap_or(!p.transient) {
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
                        self.hide_popup(sid, &p.pane);
                        if self.shown_popup(sid).is_none() {
                            self.popup_below.remove(sid);
                        }
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
                return true;
            }
        }
        let (alt, ctrl) = (b & 8 != 0, b & 16 != 0);
        if inside(inner) && !alt && !ctrl {
            return false;
        }
        // the frame's bottom-right corner resizes too
        let corner = cx + 1 == outer.x + outer.w && cy + 1 == outer.y + outer.h;
        let size = self.popup_size(&p);
        let c0 = p.cells.map(|_| (outer.x, outer.y, outer.w, outer.h));
        self.popup_drags.insert(cid, Drag { sid: sid.to_string(), pane: p.pane.clone(), x0: cx, y0: cy, d0: self.real_offset(&p), s0: size, c0, resize: ctrl || corner });
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
    /// ".", each "text,fg,bg[,flags]" with the text in hex (UTF-8), colours d / iN / rrggbb and zeit-proto's style flags (when any); cells inside
    /// the band are empty. Empty when there is no such drag.
    pub(super) fn under_dragged(&self, cid: u64, sid: &str, grid: &[Vec<Cell>]) -> String {
        let Some(d) = self.popup_drags.get(&cid).filter(|d| !d.resize && d.sid == sid) else { return String::new() };
        let Some(p) = self.shown_popup(sid).filter(|p| p.pane == d.pane) else { return String::new() };
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
                            // its style flags last (bold, italic, reverse …), when it has any: zash draws it as the pane does
                            Some(c) if c.flags != 0 => format!("{},{},{},{}", c.text.bytes().map(|b| format!("{b:02x}")).collect::<String>(), color(&c.fg), color(&c.bg), c.flags),
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

#[cfg(test)]
mod stack_tests {
    use super::*;

    /// A server with one session of two windows (panes t0, t1) and client 1 on it, no sockets.
    fn server() -> Server {
        let (tx, _) = channel();
        let (pane_tx, _) = channel::<PaneEvent>();
        let ext = Ext::new();
        let keys = Keys::defaults();
        let look = Look::from(&keys.opts);
        let state_file = std::env::temp_dir().join(format!("zeit-stack-{}-{:?}.sessions", std::process::id(), std::thread::current().id()));
        let win = |p: &str| Window { name: "sh".into(), layout: Layout::Leaf(p.into()), focus: p.into(), zoom: false, named: false };
        let mut s = Server { exe: PathBuf::from("/nonexistent"), state_file, sessions: vec![Session { id: "s".into(), name: "t".into(), place: None, windows: vec![win("t0"), win("t1")], active: 0, last: 0 }], panes: HashMap::new(), clients: HashMap::new(), next_client: 2, tx, pane_tx, ext, keys, size: (80, 24), dirty: false, next_frame: std::time::Instant::now(), pending: HashMap::new(), carry: HashMap::new(), look, buffers: Vec::new(), copy: HashMap::new(), prev_pane: HashMap::new(), activity: HashSet::new(), last_search: None, layout_at: HashMap::new(), webs: HashMap::new(), claims: HashMap::new(), running: HashMap::new(), blocks: HashMap::new(), histories: HashMap::new(), jobs: Vec::new(), ticking: Default::default(), popups: HashMap::new(), popup_stack: HashMap::new(), popup_below: HashSet::new(), popup_drags: HashMap::new(), mouse_sel: HashMap::new(), mouse_state: HashMap::new(), images: Default::default(), pane_layers: HashMap::new() };
        let (out, _) = UnixStream::pair().unwrap();
        s.clients.insert(1, Client { out, session: Some("s".into()), cols: 80, rows: 24, last: Vec::new(), notice: None, overlay: None, last_rows: String::new(), last_pins: HashMap::new(), last_zelt: String::new(), last_under: String::new(), last_fx: Vec::new(), graphics: false, last_imgs: HashMap::new(), sent_imgs: HashSet::new(), zash: false, last_panes: String::new(), last_layers: String::new() });
        s
    }

    fn names(s: &Server) -> Vec<String> {
        s.shown_popups("s").iter().map(|p| p.name.clone()).collect()
    }

    /// Two floats of window 0: f1 at (5,3) and f2 at (20,8), 30x10 each.
    fn two_floats(s: &mut Server) {
        s.new_float("s", "f1".into(), "t0".into(), (5, 3, 30, 10), String::new());
        s.new_float("s", "f2".into(), "t0".into(), (20, 8, 30, 10), String::new());
    }

    /// A left click at a 0-based cell (press and release).
    fn click(s: &mut Server, x: u16, y: u16) {
        s.mouse(1, 0, x + 1, y + 1, true);
        s.mouse(1, 0, x + 1, y + 1, false);
    }

    #[test]
    fn two_floats_shown_and_a_click_raises() {
        let mut s = server();
        two_floats(&mut s);
        assert_eq!(names(&s), ["float-1", "float-2"]);
        assert_eq!(s.client_pane(1).as_deref(), Some("f2"));
        // inside f1 only: it comes up with the keys
        click(&mut s, 7, 5);
        assert_eq!(names(&s), ["float-2", "float-1"]);
        assert_eq!(s.client_pane(1).as_deref(), Some("f1"));
        // where both are, the one on top keeps it
        click(&mut s, 25, 10);
        assert_eq!(names(&s), ["float-2", "float-1"]);
        // on f2's frame: up too (and a drag starts and ends)
        click(&mut s, 49, 17);
        assert_eq!(names(&s), ["float-1", "float-2"]);
        // beside them: the keys go down to the pane, the floats stay in view
        click(&mut s, 70, 20);
        assert_eq!(names(&s), ["float-1", "float-2"]);
        assert_eq!(s.client_pane(1).as_deref(), Some("t0"));
        // float-next: the bottom one up, with the keys
        s.float_command(Some(1), None, "float-next", &[]).unwrap();
        assert_eq!(names(&s), ["float-2", "float-1"]);
        assert_eq!(s.client_pane(1).as_deref(), Some("f1"));
    }

    #[test]
    fn a_drag_moves_and_ctrl_resizes_in_cells() {
        let mut s = server();
        two_floats(&mut s);
        // f2's top edge, dragged 3 right and 2 down
        s.mouse(1, 0, 31, 9, true);
        s.mouse(1, 32, 34, 11, true);
        s.mouse(1, 0, 34, 11, false);
        assert_eq!(s.shown_popup("s").unwrap().cells, Some((23, 10, 30, 10)));
        // Ctrl inside: it grows by the drag
        s.mouse(1, 16, 31, 15, true);
        s.mouse(1, 48, 35, 16, true);
        s.mouse(1, 16, 35, 16, false);
        assert_eq!(s.shown_popup("s").unwrap().cells, Some((23, 10, 34, 11)));
    }

    #[test]
    fn toggle_floats_is_per_window() {
        let mut s = server();
        two_floats(&mut s);
        s.new_float("s", "f3".into(), "t1".into(), (0, 0, 20, 6), String::new());
        // window 1's float is not in view on window 0
        assert_eq!(names(&s), ["float-1", "float-2"]);
        s.float_command(Some(1), None, "toggle-floats", &[]).unwrap();
        assert!(names(&s).is_empty());
        assert_eq!(s.client_pane(1).as_deref(), Some("t0"));
        s.float_command(Some(1), None, "toggle-floats", &[]).unwrap();
        assert_eq!(names(&s), ["float-1", "float-2"]);
        // they come back in the order they were left in
        click(&mut s, 7, 5);
        s.float_command(Some(1), None, "toggle-floats", &[]).unwrap();
        s.float_command(Some(1), None, "toggle-floats", &[]).unwrap();
        assert_eq!(names(&s), ["float-2", "float-1"]);
        // on window 1: only its own, and M-f hides only that one
        s.sessions[0].active = 1;
        assert_eq!(names(&s), ["float-3"]);
        s.float_command(Some(1), None, "toggle-floats", &[]).unwrap();
        assert!(names(&s).is_empty());
        s.sessions[0].active = 0;
        assert_eq!(names(&s), ["float-2", "float-1"]);
    }

    #[test]
    fn a_named_zfloat_toggles_over_the_floats() {
        let mut s = server();
        two_floats(&mut s);
        s.popups.get_mut("s").unwrap().push(Popup::new("c".into(), "claude".into(), "claude".into(), 60, 80));
        let toggle = |s: &mut Server| s.popup_command(Some(1), None, "zfloat-toggle", &["-n".into(), "claude".into()]).unwrap();
        toggle(&mut s);
        assert_eq!(names(&s), ["float-1", "float-2", "claude"]);
        assert_eq!(s.client_pane(1).as_deref(), Some("c"));
        assert_eq!(s.fmt(Some("t0"), "#{zfloat_name} #{zfloat_width} #{zfloat_count}"), "claude 60 1");
        // a float raised over it: the toggle brings it up again, then hides it
        click(&mut s, 7, 5);
        assert_eq!(s.fmt(Some("t0"), "#{zfloat_name} #{zfloat_width}"), "float-1 37");
        toggle(&mut s);
        assert_eq!(names(&s), ["float-2", "float-1", "claude"]);
        toggle(&mut s);
        assert_eq!(names(&s), ["float-2", "float-1"]);
        // it is in view on every window
        toggle(&mut s);
        s.sessions[0].active = 1;
        assert_eq!(names(&s), ["claude"]);
    }

    #[test]
    fn closing_and_saving() {
        let mut s = server();
        two_floats(&mut s);
        s.popups.get_mut("s").unwrap().push(Popup::new("c".into(), "claude".into(), "claude".into(), 60, 80));
        s.show_popup("s", "c");
        // the focused one's program ends: it goes, the next one down has the keys
        s.popup_gone("c");
        assert_eq!(s.client_pane(1).as_deref(), Some("f2"));
        s.popup_gone("f2");
        assert_eq!(names(&s), ["float-1"]);
        assert!(s.popup_command(Some(1), None, "zfloat-close", &["-n".into(), "nope".into()]).is_err());
        // only named ones are saved
        s.popups.get_mut("s").unwrap().push(Popup::new("c".into(), "claude".into(), "claude".into(), 60, 80));
        s.save_popups();
        let bytes = std::fs::read(s.state_file.with_extension("popups")).unwrap();
        let saved: HashMap<String, Vec<Popup>> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(saved["s"].iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["claude"]);
        let _ = std::fs::remove_file(s.state_file.with_extension("popups"));
        // its window gone: a float goes with it (hung up); a zfloat stays
        s.popups.get_mut("s").unwrap()[1].window = Some("t0".into());
        s.reanchor_popups("t0", &["t0".into()]);
        assert_eq!(s.popups["s"][1].window, None);
    }

    #[test]
    fn a_float_takes_percent_for_the_zfloat_verbs() {
        let mut s = server();
        two_floats(&mut s);
        s.popup_command(Some(1), None, "zfloat-align", &["right".into()]).unwrap();
        let p = s.shown_popup("s").unwrap();
        assert_eq!((p.cells, p.w, p.dx), (None, 37, EDGE));
        // flush with the right edge, once it has glided there
        for _ in 0..30 {
            s.animate_popups();
        }
        let p = s.shown_popup("s").unwrap();
        assert_eq!(s.popup_rect(p).0.x + s.popup_rect(p).0.w, 80);
    }

    #[test]
    fn old_saved_popups_still_read() {
        #[derive(Serialize)]
        struct Was { pane: String, name: String, cmd: String, w: u16, h: u16, dx: i16, dy: i16, before_max: Option<(u16, u16, i16, i16)> }
        let old: HashMap<String, Vec<Was>> = [("s".to_string(), vec![Was { pane: "c".into(), name: "claude".into(), cmd: "claude".into(), w: 60, h: 80, dx: 3, dy: -2, before_max: None }])].into_iter().collect();
        let bytes = postcard::to_stdvec(&old).unwrap();
        assert!(postcard::from_bytes::<HashMap<String, Vec<Popup>>>(&bytes).is_err());
        let read = postcard::from_bytes::<HashMap<String, Vec<OldPopup>>>(&bytes).unwrap();
        assert_eq!((read["s"][0].name.as_str(), read["s"][0].dx), ("claude", 3));
    }
}
