//! What Claude Code runs, live beside it: `zeit-claude hook` (its hooks) notes each tool call,
//! `zeit-claude view` draws them in a zfloat docked on the right.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The activity file of one zeit pane (where that Claude runs).
fn file_for(pane: &str) -> PathBuf {
    crate::state_dir().join("claude").join(format!("{pane}.jsonl"))
}

/// What a tool call is about, in a line: the command, the file, the pattern…
fn summary(tool: &str, input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match tool {
        "Bash" => s("command"),
        "Edit" | "MultiEdit" | "Write" | "Read" | "NotebookEdit" => s("file_path"),
        "Grep" | "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        _ => input.as_object().and_then(|o| o.values().find_map(|v| v.as_str().map(String::from))).unwrap_or_default(),
    }
}

/// `zeit-claude hook`: Claude Code's hook (PreToolUse, PostToolUse, Stop, UserPromptSubmit).
/// Quiet and quick: it never prints (Claude would read it) and gives up rather than wait.
pub fn hook() {
    let Some(pane) = std::env::var("ZEIT_PANE").ok().filter(|p| !p.is_empty()) else { return };
    let mut raw = String::new();
    let _ = std::io::stdin().take(4 << 20).read_to_string(&mut raw);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return };
    let event = v.get("hook_event_name").and_then(|e| e.as_str()).unwrap_or("");
    let tool = v.get("tool_name").and_then(|e| e.as_str()).unwrap_or("").to_string();
    let empty = serde_json::Value::Null;
    let input = v.get("tool_input").unwrap_or(&empty);
    let mut e = serde_json::json!({ "t": now_ms(), "event": event, "tool": tool, "id": v.get("tool_use_id").cloned().unwrap_or(serde_json::Value::Null) });
    match event {
        "PreToolUse" => {
            // paths short: inside the project relative to it, else from ~
            let mut what = summary(&tool, input);
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                what = what.replace(&format!("{cwd}/"), "");
            }
            if let Some(home) = std::env::var("HOME").ok().filter(|h| !h.is_empty()) {
                what = what.replace(&home, "~");
            }
            e["what"] = what.into();
            if let Some(d) = input.get("description").and_then(|d| d.as_str()) {
                e["desc"] = d.into();
            }
        }
        "PostToolUse" => {
            let r = v.get("tool_response").unwrap_or(&empty);
            let text = |k: &str| r.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let (out, err) = if r.is_string() { (r.as_str().unwrap_or("").to_string(), String::new()) } else { (text("stdout"), text("stderr")) };
            // the last lines are what matters (and the file stays small)
            let tail = |s: &str| s.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            e["out"] = tail(&out).into();
            e["err"] = tail(&err).into();
            e["failed"] = (r.get("interrupted").and_then(|x| x.as_bool()).unwrap_or(false) || (!err.is_empty() && out.is_empty())).into();
        }
        "UserPromptSubmit" => {
            // the prompt, for the view's headers (long ones folded there)
            let p = v.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
            e["prompt"] = p.chars().take(4000).collect::<String>().into();
        }
        "Stop" => {}
        _ => return,
    }
    let f = file_for(&pane);
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
        let _ = writeln!(file, "{e}");
    }
    // the view on the right, beside this Claude (the keys stay with Claude)
    if event == "PreToolUse" {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "zeit-claude".into());
        let viewer = format!("{exe} view {}", f.display());
        // its size, side and look are the zfloat "claude"'s settings in zeit's config (z.zfloat)
        let w: Vec<String> = ["zfloat-show", "-n", "claude", &viewer].iter().map(|s| s.to_string()).collect();
        let _ = crate::zeit(&w);
    }
}

struct Entry {
    tool: String,
    what: String,
    desc: String,
    start: u64,
    end: Option<u64>,
    out: String,
    err: String,
    failed: bool,
}

/// One prompt and what Claude did for it.
struct Turn {
    prompt: String,
    start: u64,
    /// when Claude was done with it (Stop)
    end: Option<u64>,
    steps: Vec<Entry>,
}

fn read_turns(path: &PathBuf) -> Vec<Turn> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut turns: Vec<Turn> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let t = v.get("t").and_then(|x| x.as_u64()).unwrap_or(0);
        match s("event").as_str() {
            "UserPromptSubmit" => turns.push(Turn { prompt: s("prompt"), start: t, end: None, steps: Vec::new() }),
            "PreToolUse" => {
                if turns.is_empty() {
                    turns.push(Turn { prompt: String::new(), start: t, end: None, steps: Vec::new() });
                }
                let turn = turns.last_mut().unwrap();
                turn.end = None;
                turn.steps.push(Entry { tool: s("tool"), what: s("what"), desc: s("desc"), start: t, end: None, out: String::new(), err: String::new(), failed: false });
            }
            "PostToolUse" => {
                // the latest unfinished call of that tool
                let tool = s("tool");
                if let Some(e) = turns.iter_mut().rev().flat_map(|t| t.steps.iter_mut().rev()).find(|e| e.end.is_none() && e.tool == tool) {
                    e.end = Some(t);
                    e.out = s("out");
                    e.err = s("err");
                    e.failed = v.get("failed").and_then(|x| x.as_bool()).unwrap_or(false);
                }
            }
            "Stop" => {
                if let Some(turn) = turns.last_mut() {
                    turn.end = Some(t);
                }
            }
            _ => {}
        }
    }
    turns
}

fn size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
    ((ws.ws_col as usize).max(20), (ws.ws_row as usize).max(5))
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Lines of at most `w` characters (a long prompt, read in full when its turn is open).
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > w {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&cut(word, w));
        }
        out.push(line);
    }
    out
}

fn secs(ms: u64) -> String {
    if ms < 1000 { format!("{ms} ms") } else if ms < 60_000 { format!("{:.1} s", ms as f64 / 1000.0) } else { format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60) }
}

/// The view's keys and clicks, read without waiting (raw, the mouse asked for).
struct Input {
    old: libc::termios,
}

impl Input {
    fn start() -> Input {
        let mut old: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::tcgetattr(0, &mut old) };
        let mut raw = old;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_oflag = old.c_oflag; // keep \n → \r\n
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
        unsafe { libc::fcntl(0, libc::F_SETFL, libc::fcntl(0, libc::F_GETFL) | libc::O_NONBLOCK) };
        // clicks (and the wheel) come to the view, as SGR reports
        print!("\x1b[?1000h\x1b[?1006h");
        Input { old }
    }

    fn read(&self) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 { buf[..n as usize].to_vec() } else { Vec::new() }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        print!("\x1b[?1000l\x1b[?1006l");
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.old) };
    }
}

/// `zeit-claude view FILE`: what Claude did, by prompt. The newest prompt is open, older ones
/// folded to a line; a click on a prompt (or j/k and Enter) opens or folds it.
pub fn view(path: &str) {
    let path = PathBuf::from(path);
    let input = Input::start();
    print!("\x1b[?25l\x1b[?7l");
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    // which turns are open (by their start), which header is chosen
    let mut open: std::collections::HashMap<u64, bool> = std::collections::HashMap::new();
    // top: the first line shown, None follows the newest at the bottom; anchor: a turn to bring to the top
    let (mut sel, mut tick) = (usize::MAX, 0usize);
    let (mut top, mut anchor, mut shown_from): (Option<usize>, Option<u64>, usize) = (None, None, 0);
    let (mut last_len, mut last_size, mut dirty) = (u64::MAX, (0, 0), true);
    // which screen row shows which turn's header (for clicks)
    let mut header_rows: Vec<(usize, usize)> = Vec::new();
    loop {
        let turns = read_turns(&path);
        let n = turns.len();
        if sel >= n {
            sel = n.saturating_sub(1);
        }
        // keys and clicks
        let bytes = input.read();
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            if let Some(end) = rest.starts_with(b"\x1b[<").then(|| rest.iter().position(|c| *c == b'M' || *c == b'm')).flatten() {
                let spec = String::from_utf8_lossy(&rest[3..end]).to_string();
                let parts: Vec<u32> = spec.split(';').filter_map(|p| p.parse().ok()).collect();
                if let [b, _x, y] = parts[..] {
                    let press = rest[end] == b'M';
                    match b {
                        64 => top = Some(shown_from.saturating_sub(3)),
                        65 => top = top.map(|_| shown_from + 3),
                        0 if press => {
                            if let Some(&(_, t)) = header_rows.iter().find(|(row, _)| *row + 1 == y as usize) {
                                sel = t;
                                let k = turns[t].start;
                                let now = *open.get(&k).unwrap_or(&(t + 1 == n));
                                open.insert(k, !now);
                                anchor = Some(k);
                            }
                        }
                        _ => {}
                    }
                }
                i += end + 1;
                dirty = true;
                continue;
            }
            match rest[0] {
                b'j' if n > 0 => { sel = (sel + 1).min(n - 1); anchor = Some(turns[sel].start); }
                b'k' if n > 0 => { sel = sel.saturating_sub(1); anchor = Some(turns[sel].start); }
                b'\r' | b' ' if n > 0 => {
                    let k = turns[sel].start;
                    let now = *open.get(&k).unwrap_or(&(sel + 1 == n));
                    open.insert(k, !now);
                    anchor = Some(k);
                }
                b'a' => turns.iter().for_each(|t| { open.insert(t.start, true); }),
                b'c' => turns.iter().for_each(|t| { open.insert(t.start, false); }),
                b'G' => { top = None; sel = n.saturating_sub(1); }
                _ => {}
            }
            i += 1;
            dirty = true;
        }
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        let running = turns.last().is_some_and(|t| t.steps.iter().any(|e| e.end.is_none()));
        let sz = size();
        if len != last_len {
            // something new: the newest is chosen, unless the reader went up to look at something
            if top.is_none() {
                sel = n.saturating_sub(1);
            }
        }
        if dirty || len != last_len || sz != last_size || running {
            (last_len, last_size, dirty) = (len, sz, false);
            let (w, h) = sz;
            let mut lines: Vec<String> = Vec::new();
            let mut heads: Vec<(usize, usize)> = Vec::new();
            for (ti, turn) in turns.iter().enumerate() {
                let is_open = *open.get(&turn.start).unwrap_or(&(ti + 1 == n));
                let working = turn.steps.iter().any(|e| e.end.is_none()) || (turn.end.is_none() && ti + 1 == n);
                let took = secs(turn.end.unwrap_or_else(now_ms).saturating_sub(turn.start));
                let state = if working { format!("\x1b[33m{}\x1b[39m", frames[tick % frames.len()]) } else { "\x1b[32m✔\x1b[39m".into() };
                let arrow = if is_open { "▾" } else { "▸" };
                let first = turn.prompt.lines().next().unwrap_or("").trim();
                let first = if first.is_empty() { "(Claude)" } else { first };
                let info = format!("{} · {took}", turn.steps.len());
                let room = w.saturating_sub(info.chars().count() + 7);
                let chosen = ti == sel;
                let (on, off) = if chosen { ("\x1b[7m", "\x1b[27m") } else { ("", "") };
                heads.push((lines.len(), ti));
                lines.push(format!("{on}{arrow} {state} \x1b[1m{}\x1b[22m{}\x1b[90m{info}\x1b[39m{off}\x1b[0m", cut(first, room), " ".repeat(room.saturating_sub(first.chars().count().min(room)) + 1)));
                if !is_open {
                    continue;
                }
                // the whole prompt, when it is longer than its header
                let full = wrap(turn.prompt.trim(), w.saturating_sub(4));
                if full.len() > 1 || turn.prompt.trim().chars().count() > room {
                    for l in full.iter().take(12) {
                        lines.push(format!("  \x1b[3;90m{l}\x1b[0m"));
                    }
                    if full.len() > 12 {
                        lines.push(format!("  \x1b[90m… {} more lines\x1b[0m", full.len() - 12));
                    }
                }
                for e in &turn.steps {
                    let icon = match (e.end, e.failed) {
                        (None, _) => format!("\x1b[33m{}\x1b[0m", frames[tick % frames.len()]),
                        (Some(_), true) => "\x1b[31m✖\x1b[0m".into(),
                        (Some(_), false) => "\x1b[32m✔\x1b[0m".into(),
                    };
                    let took = e.end.map(|t| secs(t.saturating_sub(e.start))).unwrap_or_else(|| secs(now_ms().saturating_sub(e.start)));
                    let (glyph, colour) = match e.tool.as_str() {
                        "Bash" => ("❯", "36"),
                        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ("✎", "35"),
                        "Read" | "Grep" | "Glob" => ("·", "90"),
                        "WebFetch" | "WebSearch" => ("⌕", "34"),
                        "Task" | "Agent" => ("⧉", "34"),
                        _ => ("•", "90"),
                    };
                    let room = w.saturating_sub(took.chars().count() + 8);
                    lines.push(format!("  {icon} \x1b[{colour}m{glyph}\x1b[0m {}{}\x1b[90m{took}\x1b[0m", cut(&e.what, room), " ".repeat(room.saturating_sub(e.what.chars().count().min(room)) + 1)));
                    if e.tool == "Bash" {
                        if !e.desc.is_empty() && e.end.is_none() {
                            lines.push(format!("      \x1b[90m{}\x1b[0m", cut(&e.desc, w.saturating_sub(6))));
                        }
                        for l in e.out.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
                            lines.push(format!("      \x1b[2m{}\x1b[0m", cut(l, w.saturating_sub(6))));
                        }
                        for l in e.err.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
                            lines.push(format!("      \x1b[31m{}\x1b[0m", cut(l, w.saturating_sub(6))));
                        }
                    }
                }
            }
            if lines.is_empty() {
                lines.push(" \x1b[90mwhat Claude runs shows here\x1b[0m".into());
            }
            // the newest at the bottom, as a terminal does; a prompt just opened (or chosen)
            // comes to the top and stays there while more arrives, until G
            let body = h.saturating_sub(1).max(1);
            let last = lines.len().saturating_sub(body);
            if let Some(k) = anchor.take() {
                if let Some(&(l, _)) = heads.iter().find(|(_, t)| turns[*t].start == k) {
                    top = Some(l);
                }
            }
            top = top.map(|t| t.min(last)).filter(|t| *t < last);
            let start = top.unwrap_or(last);
            let end = (start + body).min(lines.len());
            shown_from = start;
            let mut shown: Vec<String> = lines[start..end].to_vec();
            header_rows = heads.iter().filter(|(l, _)| *l >= start && *l < end).map(|(l, t)| (l - start, *t)).collect();
            // the header of the turn on screen stays at the top when it scrolled off (which prompt this is)
            if let Some(&(l, t)) = heads.iter().rev().find(|(l, _)| *l < start) {
                if !header_rows.iter().any(|(_, ht)| *ht == t) && end > start && heads.iter().all(|(hl, _)| *hl <= l || *hl > start) {
                    shown[0] = lines[l].clone();
                    header_rows.retain(|(row, _)| *row != 0);
                    header_rows.push((0, t));
                }
            }
            let mut out = String::from("\x1b[H\x1b[2J");
            out.push_str(&shown.join("\r\n"));
            let hint = if top.is_some() { "↑ paused · G follows the newest".to_string() } else { "click a prompt · j k Enter · a c".to_string() };

            out.push_str(&format!("\x1b[{h};1H\x1b[2K\x1b[90m{}\x1b[0m", cut(&hint, w)));
            print!("{out}");
            let _ = std::io::stdout().flush();
        }
        tick += 1;
        std::thread::sleep(Duration::from_millis(if running { 120 } else { 200 }));
    }
}
