//! `hints` (prefix /): a pane's links, file:line places, paths, hashes and addresses lit up, each
//! kind in its colour, the ones in view with a letter. A letter picks one: lowercase copies it,
//! uppercase does what that kind is for (a link opens, a place opens in nvim at its line, a hash
//! shows its commit, a folder opens a window there). Tab / S-Tab walk them all, the scrollback
//! too. Rows the terminal wrapped count as one line, so a long path is one hint.

use super::*;

/// A hint: the line it starts on (as Pane::abs_line counts), its columns in that line as the
/// program wrote it (wrapped rows joined), and what it is.
#[derive(Clone, Debug)]
pub(super) struct Hit {
    pub line: i64,
    pub from: usize,
    pub to: usize,
    pub kind: &'static str,
    pub text: String,
}

/// What lights up when no kind is named (numbers and whole error lines would drown the rest).
const KINDS: [&str; 5] = ["url", "place", "path", "hash", "ip"];

/// The labels, easiest keys first (home row, then the rows above and below).
const LABELS: &str = "asdfghjklqwertyuiopzxcvbnm";

/// A kind's colour (the terminal's palette, so the theme gives it).
fn tone(kind: &str) -> Color {
    Color::Indexed(match kind {
        "url" => 4,
        "place" => 3,
        "path" => 2,
        "hash" => 5,
        _ => 6,
    })
}

/// Into the clipboard (wl-copy), off the server's thread.
fn to_clipboard(text: String) {
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok(mut c) = std::process::Command::new("wl-copy").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            let _ = c.wait();
        }
    });
}

impl Server {
    /// Every hint on a pane, its scrollback too, oldest first.
    pub(super) fn hint_hits(&self, pane: &str, kind: Option<&str>) -> Vec<Hit> {
        let Some(p) = self.panes.get(pane) else { return Vec::new() };
        let total = p.view_top() + p.rows as i64 + p.scrolled() as i64;
        let mut out = Vec::new();
        for (line, text) in p.logical_rows(0, total) {
            for e in crate::elements::scan(&text) {
                if kind.map_or(KINDS.contains(&e.kind), |k| e.kind == k) {
                    out.push(Hit { line, from: e.from, to: e.to, kind: e.kind, text: e.text });
                }
            }
        }
        out
    }

    /// `hints [KIND]`: lit up, the newest chosen.
    pub(super) fn open_hints(&mut self, cid: u64, pane: &str, kind: Option<String>) -> Result<String, String> {
        let hits = self.hint_hits(pane, kind.as_deref());
        if hits.is_empty() {
            return Err(format!("no {} on this pane", kind.as_deref().unwrap_or("links, paths or hashes")));
        }
        let sel = hits.len() - 1;
        self.reveal_hit(pane, &hits[sel]);
        self.open_overlay(cid, Overlay::Hints { pane: pane.to_string(), hits, sel });
        Ok(String::new())
    }

    /// Where a hint's column is on screen: its row (as abs_line counts) and column.
    fn hit_cell(cols: usize, h: &Hit, c: usize) -> (i64, usize) {
        let cols = cols.max(1);
        (h.line + (c / cols) as i64, c % cols)
    }

    /// The hints in view, newest first, with their letters.
    fn labels(&self, pane: &str, hits: &[Hit]) -> Vec<(usize, char)> {
        let Some(p) = self.panes.get(pane) else { return Vec::new() };
        let (top, cols) = (p.view_top(), p.cols());
        let bottom = top + p.rows as i64;
        hits.iter()
            .enumerate()
            .rev()
            .filter(|(_, h)| {
                let (row, _) = Self::hit_cell(cols, h, h.from);
                row >= top && row < bottom
            })
            .zip(LABELS.chars())
            .map(|((i, _), c)| (i, c))
            .collect()
    }

    /// The pane scrolled so a hint is in view.
    fn reveal_hit(&self, pane: &str, h: &Hit) {
        let Some(p) = self.panes.get(pane) else { return };
        let (row, _) = Self::hit_cell(p.cols(), h, h.from);
        let top = p.view_top();
        let bottom = top + p.rows as i64 - 1;
        if row < top {
            p.scroll((top - row) as i32);
        } else if row > bottom {
            p.scroll(-((row - bottom) as i32));
        }
    }

    /// A key in `hints`. A letter picks a hint (lowercase copies, uppercase acts); Tab / S-Tab /
    /// arrows walk; Enter copies, Space acts on the chosen one; C-o opens it, C-e edits it; Esc
    /// leaves. False: it closes.
    pub(super) fn hints_key(&mut self, cid: u64, k: &crate::keys::Chord, pane: &str, hits: &[Hit], sel: &mut usize) -> bool {
        let n = hits.len();
        let step = |s: &mut usize, d: i64| *s = ((*s as i64 + d).rem_euclid(n as i64)) as usize;
        let one = k.key.chars().count() == 1 && !k.ctrl && !k.alt;
        match (k.ctrl, k.key.as_str()) {
            (false, "Tab" | "Down" | "Right") => step(sel, 1),
            (false, "BTab" | "Up" | "Left") => step(sel, -1),
            (false, "Enter") => return self.hint_copy(cid, &hits[*sel]),
            (false, "Space" | " ") => return self.hint_act(cid, pane, &hits[*sel]),
            (true, "o") => return self.hint_open(pane, &hits[*sel]),
            (true, "e") => return self.hint_edit(cid, pane, &hits[*sel]),
            (_, "Escape") | (true, "c" | "g") => return false,
            (false, key) if one => {
                let c = key.chars().next().unwrap_or(' ');
                if let Some((i, _)) = self.labels(pane, hits).into_iter().find(|(_, l)| *l == c.to_ascii_lowercase()) {
                    return if c.is_uppercase() { self.hint_act(cid, pane, &hits[i]) } else { self.hint_copy(cid, &hits[i]) };
                }
                return true;
            }
            _ => {}
        }
        self.reveal_hit(pane, &hits[*sel]);
        self.dirty = true;
        true
    }

    fn hint_copy(&mut self, cid: u64, h: &Hit) -> bool {
        self.buffers.insert(0, h.text.clone());
        to_clipboard(h.text.clone());
        if let Some(c) = self.clients.get_mut(&cid) {
            c.notice = Some(format!("copied {}", h.text));
        }
        false
    }

    fn hint_open(&mut self, pane: &str, h: &Hit) -> bool {
        let t = self.hit_target(pane, h).0;
        let _ = std::process::Command::new("xdg-open").arg(&t).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
        false
    }

    fn hint_edit(&mut self, cid: u64, pane: &str, h: &Hit) -> bool {
        let (file, line) = self.hit_target(pane, h);
        let mut cmd = vec!["new-window".to_string(), "--".into(), "nvim".into()];
        if let Some(l) = line {
            cmd.push(format!("+{l}"));
        }
        cmd.push(file);
        let _ = self.command(Some(cid), cmd);
        false
    }

    /// What the kind is for: a link opens, a place opens at its line, a hash shows its commit (in
    /// the repository of the command that printed it), a folder opens a window there, a file opens
    /// in nvim; an address is copied.
    fn hint_act(&mut self, cid: u64, pane: &str, h: &Hit) -> bool {
        match h.kind {
            "url" => self.hint_open(pane, h),
            "place" => self.hint_edit(cid, pane, h),
            "hash" => {
                // a repository: a folder with .git in it or above it
                let dir = self.hit_dir_where(pane, h, |p| p.ancestors().any(|a| a.join(".git").exists()));
                let cmd = vec!["new-window".to_string(), "-c".into(), dir.display().to_string(), "--".into(), "git".into(), "-c".into(), "core.pager=less -FRX".into(), "show".into(), h.text.clone()];
                let _ = self.command(Some(cid), cmd);
                false
            }
            "path" => {
                let (file, _) = self.hit_target(pane, h);
                if std::path::Path::new(&file).is_dir() {
                    let _ = self.command(Some(cid), vec!["new-window".into(), "-c".into(), file]);
                    false
                } else {
                    self.hint_edit(cid, pane, h)
                }
            }
            _ => self.hint_copy(cid, h),
        }
    }

    /// The folders a hint may be relative to: where the command that printed it started, then the
    /// pane's folder now (a command that began with `cd` printed from another one).
    fn hit_dirs(&self, pane: &str, h: &Hit) -> Vec<std::path::PathBuf> {
        let mut d: Vec<std::path::PathBuf> = self.blocks.get(pane).and_then(|b| b.at(h.line)).map(|(b, _)| std::path::PathBuf::from(&b.cwd)).into_iter().collect();
        d.extend(self.panes.get(pane).map(|p| p.cwd()));
        d
    }

    /// The folder for a hint: the first of them where `there` holds (else the first).
    fn hit_dir_where(&self, pane: &str, h: &Hit, there: impl Fn(&std::path::Path) -> bool) -> std::path::PathBuf {
        let d = self.hit_dirs(pane, h);
        d.iter().find(|p| there(p)).or(d.first()).cloned().unwrap_or_default()
    }

    /// What a hint opens: a link as it is; a path or place as a file and its line.
    fn hit_target(&self, pane: &str, h: &Hit) -> (String, Option<u32>) {
        if h.kind == "url" {
            return (h.text.clone(), None);
        }
        let mut parts = h.text.splitn(3, ':');
        let file = parts.next().unwrap_or_default().to_string();
        let line = if h.kind == "place" { parts.next().and_then(|l| l.parse().ok()) } else { None };
        let file = match file.strip_prefix("~/") {
            Some(rest) => crate::paths::home().join(rest),
            None => self.hit_dir_where(pane, h, |p| p.join(&file).exists()).join(&file),
        };
        (file.display().to_string(), line)
    }

    /// The hints drawn on their pane: each in its kind's colour (underlined), the chosen one in the
    /// accent, a letter on each in view; and the line saying what is chosen and the keys.
    pub(super) fn draw_hints(&self, grid: &mut [Vec<Cell>], placed: &[(String, Rect)], status_row: usize, pane: &str, hits: &[Hit], sel: usize) {
        let l = &self.look;
        let labels = self.labels(pane, hits);
        if let (Some(p), Some((_, r))) = (self.panes.get(pane), placed.iter().find(|(id, _)| id == pane)) {
            let (top, cols) = (p.view_top(), p.cols());
            for (i, h) in hits.iter().enumerate() {
                let label = labels.iter().find(|(j, _)| *j == i).map(|(_, c)| *c);
                for c in h.from..h.to {
                    let (row, col) = Self::hit_cell(cols, h, c);
                    let y = row - top;
                    if y < 0 || y >= r.h as i64 || col >= r.w as usize {
                        continue;
                    }
                    let Some(cell) = grid.get_mut(r.y as usize + y as usize).and_then(|g| g.get_mut(r.x as usize + col)) else { continue };
                    if c == h.from && label.is_some() {
                        // its letter over its first character
                        cell.text = label.unwrap().to_string();
                        (cell.fg, cell.bg) = (Color::Indexed(0), if i == sel { l.primary } else { tone(h.kind) });
                        cell.flags = zeit_proto::BOLD;
                    } else if i == sel {
                        (cell.fg, cell.bg) = (l.on_primary, l.primary);
                        cell.flags |= zeit_proto::BOLD;
                    } else {
                        cell.fg = tone(h.kind);
                        cell.flags |= zeit_proto::UNDERLINE;
                    }
                }
            }
        }
        let h = &hits[sel];
        let line = format!(" {}/{} {} {}  ·  letter: copy · Letter: {} · Tab next · Enter copy · Space {} · Esc ", sel + 1, hits.len(), h.kind, h.text, act_word(h.kind), act_word(h.kind));
        let cols = grid.first().map(|r| r.len()).unwrap_or(0);
        write_text(grid, status_row, 0, &format!("{line:<cols$}"), l.on_primary, l.primary, 0);
    }
}

/// What acting on a kind does, for the help line.
fn act_word(kind: &str) -> &'static str {
    match kind {
        "url" => "open",
        "place" => "edit at line",
        "hash" => "git show",
        "path" => "open",
        _ => "copy",
    }
}

impl Server {
    /// `open-at --client PID COL ROW`: what is at a cell of that client's screen (zash's Ctrl+click)
    /// done as its hint would be (a place opens at its line, a hash shows its commit …).
    pub(super) fn open_at(&mut self, pid: i32, x: u16, y: u16) -> Result<String, String> {
        let cid = self.clients.iter().find(|(_, c)| c.session.is_some() && peer_pid(&c.out) == Some(pid)).map(|(k, _)| *k).ok_or("no zeit client with that pid")?;
        self.open_at_client(cid, x, y)
    }

    /// What is at a cell (0-based) of a client's screen, done as its hint; Ctrl+click.
    pub(super) fn open_at_client(&mut self, cid: u64, x: u16, y: u16) -> Result<String, String> {
        let sid = self.client_session(cid).ok_or("that client shows nothing")?;
        let area = self.area();
        let s = self.session(&sid).ok_or("no session")?;
        let w = s.windows.get(s.active).ok_or("no window")?;
        let placed = if w.zoom { vec![(w.focus.clone(), area)] } else { w.layout.place(area).0 };
        let (pane, r) = placed.into_iter().find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h).ok_or("no pane there")?;
        let p = self.panes.get(&pane).ok_or("no pane")?;
        let (cols, line) = (p.cols().max(1), p.view_top() + (y - r.y) as i64);
        let col = (x - r.x) as usize;
        // the line as the program wrote it (wrapped rows joined) and the column in it
        for (start, text) in p.logical_rows(line - 200, line + 1) {
            let rows = text.chars().count().div_ceil(cols).max(1) as i64;
            if line < start || line >= start + rows {
                continue;
            }
            let c = (line - start) as usize * cols + col;
            let e = crate::elements::scan(&text).into_iter().find(|e| KINDS.contains(&e.kind) && c >= e.from && c < e.to).ok_or("nothing to open there")?;
            let h = Hit { line: start, from: e.from, to: e.to, kind: e.kind, text: e.text };
            self.hint_act(cid, &pane, &h);
            self.dirty = true;
            return Ok(format!("{} {}", h.kind, h.text));
        }
        Err("nothing to open there".into())
    }
}
