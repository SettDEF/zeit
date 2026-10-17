//! Popups: panes that float over the layout (tmux's display-popup, zellij's floating panes).
//! They run while hidden, survive detaching, and keep their output when the program ends.

use super::*;

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
}

impl Server {
    /// Where a session's shown popup sits: (outer frame, inside).
    pub(super) fn popup_rect(&self, p: &Popup) -> (Rect, Rect) {
        let area = self.area();
        let w = (area.w as u32 * p.w.clamp(20, 100) as u32 / 100) as u16;
        let h = (area.h as u32 * p.h.clamp(20, 100) as u32 / 100) as u16;
        let (w, h) = (w.max(10).min(area.w), h.max(4).min(area.h));
        // centred, then moved as far as asked but never off the screen
        let x = (((area.w - w) / 2) as i32 + p.dx as i32).clamp(0, (area.w - w) as i32) as u16;
        let y = (((area.h - h) / 2) as i32 + p.dy as i32).clamp(0, (area.h - h) as i32) as u16;
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

    /// `popup [-n NAME] [-w %] [-h %] [-c DIR] [-x] [COMMAND …]` and its friends.
    pub(super) fn popup_command(&mut self, cid: Option<u64>, ctx: Option<String>, cmd: &str, args: &[String]) -> Reply {
        let sid = cid.and_then(|c| self.client_session(c)).or_else(|| ctx.as_deref().and_then(|p| self.where_is(p)).map(|w| w.0)).or_else(|| self.sessions.first().map(|s| s.id.clone())).ok_or("no session")?;
        // flags with values, then the command
        let (mut name, mut w, mut h, mut dir, mut rest) = (None::<String>, 80u16, 80u16, None::<PathBuf>, Vec::new());
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
                    w = val().trim_end_matches('%').parse().unwrap_or(80);
                    i += 2;
                }
                "-h" => {
                    h = val().trim_end_matches('%').parse().unwrap_or(80);
                    i += 2;
                }
                "-c" | "-d" => {
                    dir = Some(PathBuf::from(val()));
                    i += 2;
                }
                "-E" | "-EE" | "-B" => i += 1,
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
        let list = self.popups.get(&sid).cloned().unwrap_or_default();
        let named = |n: &str| list.iter().find(|p| p.name == n).cloned();
        match cmd {
            "popup-hide" => {
                self.popup_shown.remove(&sid);
                self.popup_below.remove(&sid);
            }
            // popup-layer [up|down]: the keyboard to the popup (up) or the panes under it (down);
            // the popup stays in view either way
            "popup-layer" => {
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
            "popup-move" => {
                let shown = self.popup_shown.get(&sid).cloned().ok_or("no popup shown")?;
                let area = self.area();
                let n: i16 = rest.first().and_then(|v| v.parse().ok()).unwrap_or(2);
                let dir = args.iter().find(|a| matches!(a.as_str(), "-U" | "-D" | "-L" | "-R" | "-c")).cloned().unwrap_or_default();
                if let Some(p) = self.popups.get_mut(&sid).and_then(|l| l.iter_mut().find(|p| p.pane == shown)) {
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
            "popup-close" => {
                let target = name.as_deref().and_then(named).map(|p| p.pane).or_else(|| self.popup_shown.get(&sid).cloned()).ok_or("no popup shown")?;
                if let Some(p) = self.panes.get(&target) {
                    p.hang_up();
                }
            }
            "popup-list" | "choose-popup" => {
                if list.is_empty() {
                    return Err("no popups (prefix f makes one)".into());
                }
                let items = list.iter().map(|p| (format!("{}  {}", p.name, if p.cmd.is_empty() { "shell".to_string() } else { p.cmd.clone() }), p.name.clone())).collect();
                if let Some(c) = cid {
                    self.open_overlay(c, Overlay::Choose { items, sel: 0, template: Some("popup-toggle -n '%%'".into()) });
                }
            }
            "popup-next" => {
                if list.is_empty() {
                    return Err("no popups".into());
                }
                let at = self.popup_shown.get(&sid).and_then(|s| list.iter().position(|p| &p.pane == s));
                let next = at.map_or(0, |i| (i + 1) % list.len());
                self.popup_shown.insert(sid.clone(), list[next].pane.clone());
            }
            // popup-toggle: show or hide one (made the first time); popup: always a new one
            _ => {
                let existing = if cmd == "popup-toggle" {
                    match &name {
                        Some(n) => named(n),
                        None => self.popup_shown.get(&sid).and_then(|s| list.iter().find(|p| &p.pane == s).cloned()).or_else(|| list.last().cloned()),
                    }
                } else {
                    None
                };
                match existing {
                    Some(p) if self.popup_shown.get(&sid) == Some(&p.pane) => {
                        self.popup_shown.remove(&sid);
                        self.popup_below.remove(&sid);
                    }
                    Some(p) => {
                        self.popup_shown.insert(sid.clone(), p.pane);
                        self.popup_below.remove(&sid);
                    }
                    None => {
                        let here = ctx.clone().or_else(|| cid.and_then(|c| self.client_pane(c)));
                        let dir = dir.or_else(|| here.as_ref().and_then(|p| self.panes.get(p)).map(|p| p.cwd())).or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"));
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
                        let p = Popup { pane: pane.clone(), name: label, cmd: command, w, h, dx: 0, dy: 0 };
                        self.popups.entry(sid.clone()).or_default().push(p);
                        self.popup_shown.insert(sid.clone(), pane);
                        self.popup_below.remove(&sid);
                        self.layout_popups();
                        self.save_popups();
                    }
                }
            }
        }
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
        write_text(grid, outer.y as usize, outer.x as usize + 2, &format!(" {}{} ", p.name, if below { " · prefix Tab: up" } else { "" }), tf, tb, zeit_proto::BOLD);
        if !others.is_empty() {
            let more = format!(" {} ", others.join(" · "));
            let x = (outer.x + outer.w).saturating_sub(more.chars().count() as u16 + 2);
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
