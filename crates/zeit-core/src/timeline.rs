//! The timeline, from the first day: every pane's commands as the shell marks
//! them (OSC 133: prompt, output start, end with exit status), appended to
//! `$XDG_STATE_HOME/zeit/timeline/<pane>.jsonl`. Scrubbing through it comes
//! later (Z3); what it needs is recorded now, so nothing is lost meanwhile.

use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    /// 'A' prompt, 'C' output starts, 'D' command ended, 'X' the pane's shell ended
    pub kind: char,
    pub exit: Option<i32>,
}

/// Finds OSC 133 marks in a pane's output (a sequence split across reads is kept).
#[derive(Default)]
pub struct Scanner {
    carry: Vec<u8>,
}

impl Scanner {
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<Mark> {
        const HEAD: &[u8] = b"\x1b]133;";
        if self.carry.is_empty() && !chunk.contains(&0x1b) {
            return Vec::new();
        }
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(p) = data[i..].windows(HEAD.len()).position(|w| w == HEAD) {
            let start = i + p + HEAD.len();
            let Some(end) = data[start..].iter().position(|&b| b == 0x07 || b == 0x1b).map(|e| start + e) else {
                self.carry = data[i + p..].to_vec();
                return out;
            };
            let body = String::from_utf8_lossy(&data[start..end]);
            let mut parts = body.split(';');
            if let Some(kind) = parts.next().and_then(|k| k.chars().next()) {
                let exit = if kind == 'D' { parts.next().and_then(|v| v.parse().ok()) } else { None };
                out.push(Mark { kind, exit });
            }
            i = end + 1;
        }
        let tail = data.len().saturating_sub(HEAD.len() - 1).max(i);
        if let Some(p) = data[tail..].iter().position(|&b| b == 0x1b) {
            self.carry = data[tail + p..].to_vec();
        }
        out
    }
}

/// Appends one mark to the pane's timeline (best effort: a full disk never stops a shell).
pub fn record(pane: &str, m: Mark) {
    let dir = crate::paths::state_dir().join("timeline");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let exit = m.exit.map(|e| format!(",\"exit\":{e}")).unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{pane}.jsonl"))) {
        let _ = writeln!(f, "{{\"t\":{t},\"mark\":\"{}\"{exit}}}", m.kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_even_when_split() {
        let mut s = Scanner::default();
        let a = s.scan(b"$ ls\r\n\x1b]133;C\x07out\x1b]13");
        assert_eq!(a, vec![Mark { kind: 'C', exit: None }]);
        let b = s.scan(b"3;D;2\x07\x1b]133;A\x07");
        assert_eq!(b, vec![Mark { kind: 'D', exit: Some(2) }, Mark { kind: 'A', exit: None }]);
    }
}
