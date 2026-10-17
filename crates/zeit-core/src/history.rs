//! What each pane ran: the command, where, when, how it ended, what it printed.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// started, in ms since 1970
    pub t: u64,
    pub cmd: String,
    pub cwd: String,
    pub exit: i32,
    pub secs: f64,
    /// the last lines it printed
    pub out: Vec<String>,
}

/// Lines of output kept per command.
pub const KEEP_LINES: usize = 300;

fn file(pane: &str) -> PathBuf {
    crate::paths::state_dir().join("history").join(format!("{pane}.jsonl"))
}

/// Appends one entry (best effort: a full disk never stops a shell).
pub fn append(pane: &str, e: &Entry) {
    let f = file(pane);
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let (Ok(mut w), Ok(line)) = (std::fs::OpenOptions::new().create(true).append(true).open(&f), serde_json::to_string(e)) {
        let _ = writeln!(w, "{line}");
    }
}

/// A pane's entries, oldest first (at most `max`, the newest ones).
pub fn load(pane: &str, max: usize) -> VecDeque<Entry> {
    let text = std::fs::read_to_string(file(pane)).unwrap_or_default();
    let mut out: VecDeque<Entry> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    while out.len() > max {
        out.pop_front();
    }
    out
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `14:03` for today, `Mon 14:03` this week, else `10-12 14:03` (local time, from the TZ offset libc knows).
pub fn when(t: u64) -> String {
    let secs = (t / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let mut now_tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = (now_ms() / 1000) as libc::time_t;
    unsafe { libc::localtime_r(&now, &mut now_tm) };
    let hm = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    if tm.tm_yday == now_tm.tm_yday && tm.tm_year == now_tm.tm_year {
        hm
    } else if now - secs < 6 * 86400 {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        format!("{} {hm}", DAYS[tm.tm_wday as usize % 7])
    } else {
        format!("{:02}-{:02} {hm}", tm.tm_mon + 1, tm.tm_mday)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip() {
        let e = Entry { t: 1, cmd: "cargo build".into(), cwd: "/tmp".into(), exit: 101, secs: 3.2, out: vec!["error: x".into()] };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Entry>(&s).unwrap(), e);
        assert_eq!(when(now_ms()).len(), 5, "today is just the time");
    }
}

/// Every pane's entries together, newest first (closed panes too): the day at a glance.
pub fn load_all(max: usize) -> Vec<Entry> {
    let dir = crate::paths::state_dir().join("history");
    let mut all: Vec<Entry> = std::fs::read_dir(&dir)
        .map(|r| r.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".jsonl")).map(|p| load(p, max))).flatten().collect())
        .unwrap_or_default();
    all.sort_by(|a, b| b.t.cmp(&a.t));
    all.truncate(max);
    all
}
