//! Floats: panes that float over their window's layout, as zellij's floating panes. A window's
//! floats show and hide together (M-f), stack (a click brings one up), and can be embedded.

use super::*;

/// A floating pane: its window (by a pane in that window's layout), where it is (cells).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct Float {
    pub pane: String,
    pub anchor: String,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub shown: bool,
}

/// A float being dragged: which, where the drag began, its rect then, and whether it resizes.
pub(super) struct FloatDrag {
    pub pane: String,
    pub x0: u16,
    pub y0: u16,
    pub r0: (u16, u16, u16, u16),
    pub resize: bool,
}

impl Server {
    /// The floats of a session's active window, bottom first.
    pub(super) fn window_floats(&self, sid: &str) -> Vec<Float> {
        let Some(s) = self.session(sid) else { return Vec::new() };
        let Some(w) = s.windows.get(s.active) else { return Vec::new() };
        let ids = w.layout.ids();
        self.floats.iter().filter(|f| ids.contains(&f.anchor)).cloned().collect()
    }

    /// The float that has the keyboard in a session (its window's floats shown).
    pub(super) fn focused_float(&self, sid: &str) -> Option<String> {
        let p = self.float_focus.get(sid)?;
        self.window_floats(sid).iter().any(|f| &f.pane == p && f.shown).then(|| p.clone())
    }

    pub(super) fn save_floats(&self) {
        if let Ok(bytes) = postcard::to_stdvec(&self.floats) {
            let f = self.state_file.with_extension("floats");
            let tmp = f.with_extension("floats-tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, f);
            }
        }
    }

    /// The floats a previous server left whose programs still run.
    pub(super) fn restore_floats(&mut self) {
        let Ok(bytes) = std::fs::read(self.state_file.with_extension("floats")) else { return };
        let Ok(saved) = postcard::from_bytes::<Vec<Float>>(&bytes) else { return };
        for f in saved {
            if self.where_is(&f.anchor).is_none() {
                continue;
            }
            if let Ok(p) = Pane::connect(&f.pane, f.w.max(3), f.h.max(3), PathBuf::from("/"), self.look.history, self.pane_tx.clone()) {
                self.panes.insert(f.pane.clone(), p);
                self.floats.push(f);
            }
        }
    }

    /// Every float's pane gets the size inside its frame.
    pub(super) fn layout_floats(&mut self) {
        let sizes: Vec<(String, u16, u16)> = self.floats.iter().map(|f| (f.pane.clone(), f.w.saturating_sub(2).max(1), f.h.saturating_sub(2).max(1))).collect();
        for (id, w, h) in sizes {
            if let Some(p) = self.panes.get_mut(&id) {
                p.set_size(w, h);
            }
        }
    }

    /// A pane left: a float that was it goes; floats anchored to it move to a pane still there.
    pub(super) fn float_pane_gone(&mut self, pane: &str) -> bool {
        let was = self.floats.iter().any(|f| f.pane == pane);
        self.floats.retain(|f| f.pane != pane);
        self.float_focus.retain(|_, p| p != pane);
        if was {
            self.save_floats();
            self.dirty = true;
        }
        was
    }

    /// Before a tiled pane leaves its window: its floats keep the window (another of its panes).
    pub(super) fn reanchor_floats(&mut self, pane: &str, window_ids: &[String]) {
        let other = window_ids.iter().find(|i| i.as_str() != pane).cloned();
        for f in self.floats.iter_mut().filter(|f| f.anchor == pane) {
            match &other {
                Some(o) => f.anchor = o.clone(),
                // the window goes: its floats go with it
                None => {
                    if let Some(p) = self.panes.get(&f.pane) {
                        p.hang_up();
                    }
                }
            }
        }
    }

    /// `float`, `toggle-floats`, `float-embed`, `float-next`, `float-close`.
    pub(super) fn float_command(&mut self, cid: Option<u64>, ctx: Option<String>, cmd: &str, args: &[String]) -> Reply {
        let here = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c)));
        let sid = cid.and_then(|c| self.client_session(c)).or_else(|| here.as_deref().and_then(|p| self.where_is(p)).map(|w| w.0)).or_else(|| self.sessions.first().map(|s| s.id.clone())).ok_or("no session")?;
        let (wi, anchor) = {
            let s = self.session(&sid).ok_or("no session")?;
            (s.active, s.windows.get(s.active).ok_or("no window")?.focus.clone())
        };
        let mine = self.window_floats(&sid);
        match cmd {
            "toggle-floats" | "float-toggle" => {
                if mine.is_empty() {
                    return self.float_command(cid, ctx, "float", args);
                }
                let show = !mine.iter().any(|f| f.shown);
                let ids: Vec<String> = mine.iter().map(|f| f.pane.clone()).collect();
                for f in self.floats.iter_mut().filter(|f| ids.contains(&f.pane)) {
                    f.shown = show;
                }
                if show {
                    self.float_focus.insert(sid.clone(), ids[ids.len() - 1].clone());
                } else {
                    self.float_focus.remove(&sid);
                }
            }
            "float-next" => {
                let shown: Vec<&Float> = mine.iter().filter(|f| f.shown).collect();
                if shown.is_empty() {
                    return Err("no floats shown (M-f shows them)".into());
                }
                // the bottom one comes up
                let p = shown[0].pane.clone();
                self.raise_float(&p);
                self.float_focus.insert(sid.clone(), p);
            }
            "float-close" => {
                let p = self.focused_float(&sid).ok_or("no float has the keyboard")?;
                if let Some(pane) = self.panes.get(&p) {
                    pane.hang_up();
                }
            }
            // a float into the layout (beside the pane it was over), or the focused pane out of it
            "float-embed" => match self.focused_float(&sid) {
                Some(p) => {
                    self.floats.retain(|f| f.pane != p);
                    self.float_focus.remove(&sid);
                    let s = self.session_mut(&sid).ok_or("no session")?;
                    let w = &mut s.windows[wi];
                    w.layout.split(&anchor, &p, true);
                    w.focus = p;
                    w.zoom = false;
                    self.layout_all();
                    self.save();
                }
                None => {
                    let ids = self.session(&sid).map(|s| s.windows[wi].layout.ids()).unwrap_or_default();
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
                    self.floats.push(Float { pane: anchor.clone(), anchor: other, x: area.x + (area.w - fw) / 2, y: area.y + (area.h - fh) / 2, w: fw, h: fh, shown: true });
                    self.float_focus.insert(sid.clone(), anchor);
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
                // a command runs whole (; && too) and its output stays when it ends; none: a shell
                let argv = if rest.is_empty() {
                    Vec::new()
                } else {
                    let command = rest.join(" ");
                    let shown = command.replace('\'', "'\\''");
                    vec!["/bin/sh".into(), "-c".into(), format!("{command}\ns=$?\nprintf '\\n\\033[2m── %s ended (%s) · Enter closes ──\\033[0m' '{shown}' \"$s\"\nread _")]
                };
                let pane = self.start_pane(dir, argv)?;
                // showing one shows the window's others too, as zellij does
                let ids: Vec<String> = mine.iter().map(|f| f.pane.clone()).collect();
                for f in self.floats.iter_mut().filter(|f| ids.contains(&f.pane)) {
                    f.shown = true;
                }
                self.floats.push(Float { pane: pane.clone(), anchor, x, y, w, h, shown: true });
                self.float_focus.insert(sid.clone(), pane);
            }
        }
        self.layout_floats();
        self.save_floats();
        self.dirty = true;
        Ok(String::new())
    }

    /// Puts a float on top of its window's others.
    pub(super) fn raise_float(&mut self, pane: &str) {
        if let Some(i) = self.floats.iter().position(|f| f.pane == pane) {
            let f = self.floats.remove(i);
            self.floats.push(f);
        }
    }

    /// Draws a session's shown floats, bottom first; returns the focused one's cursor.
    pub(super) fn draw_floats(&self, sid: &str, grid: &mut [Vec<Cell>]) -> Option<(Option<(u16, u16)>, CursorShape)> {
        let focus = self.focused_float(sid);
        let mut out = None;
        for f in self.window_floats(sid).iter().filter(|f| f.shown) {
            let Some(pane) = self.panes.get(&f.pane) else { continue };
            let on = focus.as_deref() == Some(&f.pane);
            let fg = if on { self.look.active_border } else { self.look.border };
            for y in f.y..f.y + f.h {
                for x in f.x..f.x + f.w {
                    let Some(cell) = grid.get_mut(y as usize).and_then(|r| r.get_mut(x as usize)) else { continue };
                    let (top, bottom, left, right) = (y == f.y, y + 1 == f.y + f.h, x == f.x, x + 1 == f.x + f.w);
                    let text = match (top, bottom, left, right) {
                        (true, _, true, _) => "┌",
                        (true, _, _, true) => "┐",
                        (_, true, true, _) => "└",
                        (_, true, _, true) => "┘",
                        (true, _, _, _) | (_, true, _, _) => "─",
                        (_, _, true, _) | (_, _, _, true) => "│",
                        _ => " ",
                    };
                    *cell = Cell { text: text.into(), fg, bg: Color::Default, flags: 0 };
                }
            }
            let title = format!(" {} ", foreground_name(pane.pid).unwrap_or_else(|| "float".into()));
            write_text(grid, f.y as usize, f.x as usize + 2, &title, fg, Color::Default, if on { zeit_proto::BOLD } else { 0 });
            let (screen, cur, shape) = pane.screen();
            let (ix, iy) = (f.x + 1, f.y + 1);
            for (y, row) in screen.iter().enumerate().take(f.h.saturating_sub(2) as usize) {
                for (x, cell) in row.iter().enumerate().take(f.w.saturating_sub(2) as usize) {
                    if let Some(g) = grid.get_mut(iy as usize + y).and_then(|r| r.get_mut(ix as usize + x)) {
                        *g = cell.clone();
                    }
                }
            }
            if on {
                out = Some((cur.map(|(x, y)| (ix + x, iy + y)), shape));
            }
        }
        out
    }

    /// The mouse on floats: a press brings one up and gives it the keyboard (on its frame, or
    /// with Alt anywhere, it drags; Ctrl or the corner resizes); a press beside them gives the
    /// keyboard back to the tiled panes. True when the event was the floats'.
    pub(super) fn float_mouse(&mut self, cid: u64, sid: &str, b: u32, cx: u16, cy: u16, press: bool) -> Option<(String, Rect)> {
        let motion = b & 32 != 0;
        if let Some(d) = self.float_drags.get(&cid) {
            if !press {
                self.float_drags.remove(&cid);
                self.layout_floats();
                self.save_floats();
                return Some((String::new(), Rect { x: 0, y: 0, w: 0, h: 0 }));
            }
            if motion {
                let (pane, r0, resize, x0, y0) = (d.pane.clone(), d.r0, d.resize, d.x0, d.y0);
                let area = self.area();
                let (dx, dy) = (cx as i32 - x0 as i32, cy as i32 - y0 as i32);
                if let Some(f) = self.floats.iter_mut().find(|f| f.pane == pane) {
                    if resize {
                        f.w = (r0.2 as i32 + dx).clamp(12, (area.x + area.w - f.x) as i32) as u16;
                        f.h = (r0.3 as i32 + dy).clamp(5, (area.y + area.h - f.y) as i32) as u16;
                    } else {
                        f.x = (r0.0 as i32 + dx).clamp(area.x as i32, (area.x + area.w - f.w) as i32) as u16;
                        f.y = (r0.1 as i32 + dy).clamp(area.y as i32, (area.y + area.h - f.h) as i32) as u16;
                    }
                }
                self.dirty = true;
            }
            return Some((String::new(), Rect { x: 0, y: 0, w: 0, h: 0 }));
        }
        // the top float under the pointer
        let hit = self.window_floats(sid).into_iter().filter(|f| f.shown).rev().find(|f| cx >= f.x && cx < f.x + f.w && cy >= f.y && cy < f.y + f.h);
        let fresh = press && !motion && b & 64 == 0 && b & 3 == 0;
        let Some(f) = hit else {
            // beside the floats: the tiled panes have the keyboard again (the floats stay)
            if fresh && self.float_focus.remove(sid).is_some() {
                self.dirty = true;
            }
            return None;
        };
        let inner = Rect { x: f.x + 1, y: f.y + 1, w: f.w.saturating_sub(2), h: f.h.saturating_sub(2) };
        if fresh {
            self.raise_float(&f.pane);
            self.float_focus.insert(sid.to_string(), f.pane.clone());
            self.dirty = true;
            let on_frame = !(cx >= inner.x && cx < inner.x + inner.w && cy >= inner.y && cy < inner.y + inner.h);
            let (alt, ctrl) = (b & 8 != 0, b & 16 != 0);
            if on_frame || alt || ctrl {
                let corner = cx + 1 == f.x + f.w && cy + 1 == f.y + f.h;
                self.float_drags.insert(cid, FloatDrag { pane: f.pane.clone(), x0: cx, y0: cy, r0: (f.x, f.y, f.w, f.h), resize: ctrl || corner });
                return Some((String::new(), Rect { x: 0, y: 0, w: 0, h: 0 }));
            }
        }
        Some((f.pane.clone(), inner))
    }
}
