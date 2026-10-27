//! What a line of terminal text is made of, for `zeit inspect` and `zeit find`: links, file:line
//! places, paths, git hashes, addresses, numbers, and lines that report an error.

use std::sync::OnceLock;

use regex::Regex;

/// A thing found in a line: its kind, where it is (columns, end exclusive) and its text.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Element {
    pub kind: &'static str,
    pub from: usize,
    pub to: usize,
    pub text: String,
}

/// The kinds, in the order they win when two overlap.
pub const KINDS: [&str; 7] = ["url", "place", "path", "hash", "ip", "number", "error"];

fn patterns() -> &'static [(&'static str, Regex)] {
    static P: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    P.get_or_init(|| {
        [
            ("url", r#"\b(?:https?|ftp|file)://[^\s<>()\[\]{}'"`]+[^\s<>()\[\]{}'"`.,;:!?]"#),
            // src/main.rs:42 or src/main.rs:42:7 (a compiler's, grep's, a stack trace's place)
            ("place", r"(?:[~.]?/)?[\w.@+-]+(?:/[\w.@+-]+)*\.[A-Za-z]\w*:\d+(?::\d+)?"),
            ("path", r"(?:~|\.{1,2})?/[\w.@+-]+(?:/[\w.@+-]*)*"),
            ("hash", r"\b[0-9a-f]{7,40}\b"),
            ("ip", r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"),
            ("number", r"-?\b\d+(?:\.\d+)?\b"),
        ]
        .into_iter()
        .map(|(k, p)| (k, Regex::new(p).expect("element pattern")))
        .collect()
    })
}

fn error_line() -> &'static Regex {
    static E: OnceLock<Regex> = OnceLock::new();
    E.get_or_init(|| Regex::new(r"(?i)\b(error|failed|failure|panicked|fatal|exception|traceback|cannot|no such file|permission denied|not found|segmentation fault)\b").expect("error pattern"))
}

/// Columns of a byte offset in `line` (one per char; wide chars are not counted twice).
fn col(line: &str, byte: usize) -> usize {
    line[..byte].chars().count()
}

/// The elements of one line, left to right, none overlapping (the earlier kind wins); a line
/// that reports an error is one more, the whole line.
pub fn scan(line: &str) -> Vec<Element> {
    let mut out: Vec<Element> = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for (kind, re) in patterns() {
        for m in re.find_iter(line) {
            if taken.iter().any(|(a, b)| m.start() < *b && *a < m.end()) {
                continue;
            }
            // a hash is hex with a digit and a letter (not a plain word or number)
            if *kind == "hash" && !(m.as_str().bytes().any(|c| c.is_ascii_digit()) && m.as_str().bytes().any(|c| c.is_ascii_alphabetic())) {
                continue;
            }
            taken.push((m.start(), m.end()));
            out.push(Element { kind, from: col(line, m.start()), to: col(line, m.end()), text: m.as_str().to_string() });
        }
    }
    out.sort_by_key(|e| e.from);
    if error_line().is_match(line) {
        let t = line.trim_end();
        out.push(Element { kind: "error", from: 0, to: t.chars().count(), text: t.to_string() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_what_a_line_is_made_of() {
        let e = scan("error[E0308]: mismatched types --> src/main.rs:42:7 see https://doc.rust-lang.org/x.html");
        let kinds: Vec<_> = e.iter().map(|e| (e.kind, e.text.as_str())).collect();
        assert!(kinds.contains(&("place", "src/main.rs:42:7")), "{kinds:?}");
        assert!(kinds.contains(&("url", "https://doc.rust-lang.org/x.html")), "{kinds:?}");
        assert!(kinds.iter().any(|k| k.0 == "error"));
        let g = scan("6ee1399 zunder: each cell, 192.168.1.20:8080 in ~/x/y.txt");
        let kinds: Vec<_> = g.iter().map(|e| (e.kind, e.text.as_str())).collect();
        assert!(kinds.contains(&("hash", "6ee1399")) && kinds.contains(&("ip", "192.168.1.20:8080")) && kinds.contains(&("path", "~/x/y.txt")), "{kinds:?}");
        assert!(!scan("deadline").iter().any(|e| e.kind == "hash"), "a word is no hash");
    }
}
