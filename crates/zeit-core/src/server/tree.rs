//! choose-tree as tmux has it: the whole screen, sessions and their windows numbered on top,
//! the chosen one's panes live below. z.set('@tree-preview', 'off'), z.set('@tree-height', 40).

use super::*;

/// The tree's state while it is open; its rows are made from the sessions on every look.
#[derive(Clone, Debug)]
pub(super) struct TreeState {
    pub sel: usize,
    pub collapsed: HashSet<String>,
    pub template: Option<String>,
    pub sessions_only: bool,
    pub preview: bool,
}

/// One row: a session, or a window of one (session id, index).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Row {
    Session(String),
    Window(String, usize),
}

impl Server {
    /// The rows and their targets (`name:` or `name:index`), as they are now.
    pub(super) fn tree_rows(&self, st: &TreeState) -> Vec<(Row, String)> {
        let mut out = Vec::new();
        for s in &self.sessions {
            out.push((Row::Session(s.id.clone()), format!("{}:", s.name)));
            if st.sessions_only || st.collapsed.contains(&s.id) {
                continue;
            }
            for i in 0..s.windows.len() {
                out.push((Row::Window(s.id.clone(), i), format!("{}:{i}", s.name)));
            }
        }
        out
    }

    /// Opens on where this client is (its window, or its session with -s).
    pub(super) fn tree_open(&self, cid: u64, sessions_only: bool, template: Option<String>) -> TreeState {
        let preview = self.keys.opts.get("@tree-preview").is_none_or(|v| v != "off" && v != "0");
        let mut st = TreeState { sel: 0, collapsed: HashSet::new(), template, sessions_only, preview };
        if let Some(sid) = self.client_session(cid) {
            let active = self.session(&sid).map(|s| s.active).unwrap_or(0);
            st.sel = self.tree_rows(&st).iter().position(|(r, _)| if sessions_only { *r == Row::Session(sid.clone()) } else { *r == Row::Window(sid.clone(), active) }).unwrap_or(0);
        }
        st
    }

    /// Keys in the tree; true: it stays open. Some(target) is the one chosen.
    pub(super) fn tree_key(&mut self, cid: u64, k: &Chord, st: &mut TreeState) -> (bool, Option<Box<dyn FnOnce(&mut Server)>>) {
        let rows = self.tree_rows(st);
        let last = rows.len().saturating_sub(1);
        st.sel = st.sel.min(last);
        let cur = rows.get(st.sel).cloned();
        let choose = |t: String, tpl: Option<String>| -> Option<Box<dyn FnOnce(&mut Server)>> { Some(Box::new(move |s: &mut Server| s.chosen(cid, &t, tpl))) };
        match (k.ctrl, k.key.as_str()) {
            (false, "j" | "Down") | (true, "n") => st.sel = (st.sel + 1).min(last),
            (false, "k" | "Up") | (true, "p") => st.sel = st.sel.saturating_sub(1),
            (false, "g" | "Home") => st.sel = 0,
            (false, "G" | "End") => st.sel = last,
            (false, "PageDown") => st.sel = (st.sel + 10).min(last),
            (false, "PageUp") => st.sel = st.sel.saturating_sub(10),
            (false, "v") => st.preview = !st.preview,
            (false, "Escape" | "q") | (true, "c") => return (false, None),
            // ← closes a session (or goes up to it), → opens it
            (false, "Left" | "h") => match cur {
                Some((Row::Session(id), _)) => {
                    st.collapsed.insert(id);
                }
                Some((Row::Window(id, _), _)) => {
                    if let Some(i) = rows.iter().position(|(r, _)| *r == Row::Session(id.clone())) {
                        st.sel = i;
                    }
                }
                None => {}
            },
            (false, "Right" | "l") => {
                if let Some((Row::Session(id), _)) = cur {
                    st.collapsed.remove(&id);
                }
            }
            (false, "x") => {
                if let Some((row, t)) = cur {
                    let cmd = match row {
                        Row::Session(_) => format!("kill-session -t {}", t.trim_end_matches(':')),
                        Row::Window(..) => format!("kill-window -t {t}"),
                    };
                    // the tree stays: it shows what is left
                    return (true, Some(Box::new(move |s: &mut Server| s.choose_kill(&cmd))));
                }
            }
            (false, "Enter") => {
                if let Some((_, t)) = cur {
                    return (false, choose(t, st.template.clone()));
                }
            }
            // a row's number picks it at once, as tmux's (0)…(9)
            (false, d) if d.len() == 1 && d.chars().all(|c| c.is_ascii_digit()) => {
                let n: usize = d.parse().unwrap_or(0);
                if let Some((_, t)) = rows.get(n) {
                    return (false, choose(t.clone(), st.template.clone()));
                }
            }
            _ => {}
        }
        (true, None)
    }

    /// The whole area: the tree on top, the chosen session's or window's panes below.
    pub(super) fn draw_tree_view(&self, st: &TreeState, grid: &mut [Vec<Cell>], area: Rect) {
        let l = &self.look;
        let rows = self.tree_rows(st);
        let sel = st.sel.min(rows.len().saturating_sub(1));
        let (x0, w) = (area.x as usize, area.w as usize);
        // clear the area
        for y in area.y..area.y + area.h {
            write_text(grid, y as usize, x0, &" ".repeat(w), Color::Default, Color::Default, 0);
        }
        let pct: u16 = self.keys.opts.get("@tree-height").and_then(|v| v.trim_end_matches('%').parse().ok()).unwrap_or(40).clamp(15, 90);
        let list_h = if st.preview { (area.h * pct / 100).max(3).min(area.h.saturating_sub(4)) } else { area.h };
        // the rows, scrolled to keep the chosen one in view
        let top = sel.saturating_sub(list_h.saturating_sub(1) as usize);
        let here = self.clients.values().find_map(|c| c.session.clone());
        for (i, (row, _)) in rows.iter().enumerate().skip(top).take(list_h as usize) {
            let y = area.y as usize + i - top;
            let num = if i < 10 { format!("({i}) ") } else { "    ".to_string() };
            let text = match row {
                Row::Session(id) => {
                    let Some(s) = self.session(id) else { continue };
                    let attached = self.clients.values().filter(|c| c.session.as_deref() == Some(id)).count();
                    let mark = if st.sessions_only { " " } else if st.collapsed.contains(id) { "+" } else { "-" };
                    format!("{num}{mark} {}: {} windows{}{}", s.name, s.windows.len(), if attached > 0 { " (attached)" } else { "" }, if here.as_deref() == Some(id) { "  ●" } else { "" })
                }
                Row::Window(id, wi) => {
                    let Some(s) = self.session(id) else { continue };
                    let wnd = &s.windows[*wi];
                    let last = *wi + 1 == s.windows.len();
                    let panes = wnd.layout.ids().len();
                    format!("{num}  {}─> {wi}: {}{}{}", if last { "└" } else { "├" }, self.window_title(wnd), if *wi == s.active { "*" } else { "" }, if panes > 1 { format!(" ({panes} panes)") } else { String::new() })
                }
            };
            let shown: String = text.chars().take(w).collect();
            if i == sel {
                write_text(grid, y, x0, &format!("{shown:<w$}"), l.on_primary, l.primary, zeit_proto::BOLD);
            } else {
                let (fg, flags) = if matches!(row, Row::Session(_)) { (Color::Default, zeit_proto::BOLD) } else { (Color::Default, 0) };
                write_text(grid, y, x0, &shown, fg, Color::Default, flags);
            }
        }
        if !st.preview {
            return;
        }
        // the preview: a box over the rest, the window's panes inside as they are now
        let py = area.y + list_h;
        let ph = area.h - list_h;
        if ph < 3 {
            return;
        }
        let window = match rows.get(sel).map(|r| &r.0) {
            Some(Row::Session(id)) => self.session(id).and_then(|s| s.windows.get(s.active).map(|w| (s.name.clone(), s.active, w))),
            Some(Row::Window(id, wi)) => self.session(id).and_then(|s| s.windows.get(*wi).map(|w| (s.name.clone(), *wi, w))),
            None => None,
        };
        let b = l.active_border;
        write_text(grid, py as usize, x0, &format!("┌{}┐", "─".repeat(w.saturating_sub(2))), b, Color::Default, 0);
        for y in py + 1..py + ph - 1 {
            write_text(grid, y as usize, x0, "│", b, Color::Default, 0);
            write_text(grid, y as usize, x0 + w - 1, "│", b, Color::Default, 0);
        }
        write_text(grid, (py + ph - 1) as usize, x0, &format!("└{}┘", "─".repeat(w.saturating_sub(2))), b, Color::Default, 0);
        let hint = " enter go · ←→ fold · 0-9 pick · x kill · v preview · q close ";
        if w > hint.chars().count() + 4 {
            write_text(grid, (py + ph - 1) as usize, x0 + w - 2 - hint.chars().count(), hint, l.muted, Color::Default, 0);
        }
        let Some((sname, wi, wnd)) = window else { return };
        write_text(grid, py as usize, x0 + 2, &format!(" {sname}:{wi} {} ", self.window_title(wnd)), b, Color::Default, zeit_proto::BOLD);
        let inner = Rect { x: area.x + 1, y: py + 1, w: area.w.saturating_sub(2), h: ph.saturating_sub(2) };
        let (placed, borders) = if wnd.zoom { (vec![(wnd.focus.clone(), inner)], Vec::new()) } else { wnd.layout.place(inner) };
        for (id, r) in &placed {
            let Some(p) = self.panes.get(id) else { continue };
            let (screen, cur, _) = p.screen();
            // from the top, as tmux shows it; scrolled just enough to keep the cursor's line in view
            let cy = cur.map_or(0, |c| c.1 as usize);
            let skip = (cy + 1).saturating_sub(r.h as usize);
            for (y, row) in screen.iter().skip(skip).enumerate().take(r.h as usize) {
                for (x, cell) in row.iter().enumerate().take(r.w as usize) {
                    if let Some(g) = grid.get_mut(r.y as usize + y).and_then(|rr| rr.get_mut(r.x as usize + x)) {
                        *g = cell.clone();
                    }
                }
            }
        }
        for br in &borders {
            for i in 0..br.len {
                let (x, y) = if br.vertical { (br.x, br.y + i) } else { (br.x + i, br.y) };
                if let Some(c) = grid.get_mut(y as usize).and_then(|r| r.get_mut(x as usize)) {
                    *c = Cell { text: if br.vertical { "│" } else { "─" }.into(), fg: l.border, bg: Color::Default, flags: 0 };
                }
            }
        }
    }
}
