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
        "UserPromptSubmit" | "Stop" => {}
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

fn read_entries(path: &PathBuf) -> (Vec<Entry>, Option<u64>) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut list: Vec<Entry> = Vec::new();
    let mut idle_since = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let t = v.get("t").and_then(|x| x.as_u64()).unwrap_or(0);
        match s("event").as_str() {
            // a new prompt: what came before is history
            "UserPromptSubmit" => {
                list.clear();
                idle_since = None;
            }
            "PreToolUse" => {
                list.push(Entry { tool: s("tool"), what: s("what"), desc: s("desc"), start: t, end: None, out: String::new(), err: String::new(), failed: false });
                idle_since = None;
            }
            "PostToolUse" => {
                // the latest unfinished call of that tool
                let tool = s("tool");
                if let Some(e) = list.iter_mut().rev().find(|e| e.end.is_none() && e.tool == tool) {
                    e.end = Some(t);
                    e.out = s("out");
                    e.err = s("err");
                    e.failed = v.get("failed").and_then(|x| x.as_bool()).unwrap_or(false);
                }
            }
            "Stop" => idle_since = Some(t),
            _ => {}
        }
    }
    (list, idle_since)
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

fn secs(ms: u64) -> String {
    if ms < 1000 { format!("{ms} ms") } else if ms < 60_000 { format!("{:.1} s", ms as f64 / 1000.0) } else { format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60) }
}

/// `zeit-claude view FILE`: the view. Redraws when the file changes, and while something runs.
pub fn view(path: &str) {
    let path = PathBuf::from(path);
    print!("\x1b[?25l\x1b[?7l");
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let (mut last_len, mut last_size, mut tick) = (u64::MAX, (0, 0), 0usize);
    loop {
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        let (entries, idle) = read_entries(&path);
        let running = entries.iter().any(|e| e.end.is_none());
        let sz = size();
        if len != last_len || sz != last_size || running {
            last_len = len;
            last_size = sz;
            let (w, h) = sz;
            let mut lines: Vec<String> = Vec::new();
            for e in &entries {
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
                let room = w.saturating_sub(took.chars().count() + 6);
                lines.push(format!("{icon} \x1b[{colour}m{glyph}\x1b[0m {}{}\x1b[90m{took}\x1b[0m", cut(&e.what, room), " ".repeat(room.saturating_sub(e.what.chars().count().min(room)) + 1)));
                if e.tool == "Bash" {
                    if !e.desc.is_empty() && e.end.is_none() {
                        lines.push(format!("    \x1b[90m{}\x1b[0m", cut(&e.desc, w.saturating_sub(4))));
                    }
                    // the command's last lines, its errors in red
                    for l in e.out.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
                        lines.push(format!("    \x1b[2m{}\x1b[0m", cut(l, w.saturating_sub(4))));
                    }
                    for l in e.err.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
                        lines.push(format!("    \x1b[31m{}\x1b[0m", cut(l, w.saturating_sub(4))));
                    }
                }
            }
            let done = entries.iter().filter(|e| e.end.is_some()).count();
            let head = match (running, idle) {
                (true, _) => format!(" \x1b[33m●\x1b[0m \x1b[1mworking\x1b[0m \x1b[90m· {done} done\x1b[0m"),
                (false, Some(_)) => format!(" \x1b[32m●\x1b[0m \x1b[1mdone\x1b[0m \x1b[90m· {} steps\x1b[0m", entries.len()),
                _ if entries.is_empty() => " \x1b[90mwhat Claude runs shows here\x1b[0m".to_string(),
                _ => format!(" \x1b[90m●\x1b[0m \x1b[1mwaiting\x1b[0m \x1b[90m· {} steps\x1b[0m", entries.len()),
            };
            // the newest at the bottom, as a terminal does
            let body = h.saturating_sub(2);
            let start = lines.len().saturating_sub(body);
            let mut out = String::from("\x1b[H\x1b[2J");
            out.push_str(&head);
            out.push_str("\r\n\r\n");
            out.push_str(&lines[start..].join("\r\n"));
            print!("{out}");
            let _ = std::io::stdout().flush();
        }
        tick += 1;
        std::thread::sleep(Duration::from_millis(if running { 120 } else { 400 }));
    }
}

