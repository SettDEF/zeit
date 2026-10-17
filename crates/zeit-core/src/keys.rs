//! The user's tmux.conf: keys, options and colours (tmux defaults without one).

use std::collections::HashMap;

/// What a key does.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub enum Action {
    /// side by side (`split-window -h`) or one above the other (`-v`)
    Split { side_by_side: bool },
    Focus(Dir),
    FocusNext,
    /// grow/shrink the focused pane's edge in a direction, by cells
    Resize(Dir, u16),
    Zoom,
    ClosePane,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    LastTab,
    SendPrefix,
    Search,
    Paste,
    /// several, in order (`select-pane -t :.+ \; resize-pane -Z`)
    Seq(Vec<Action>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// A key as tmux writes it: modifiers and a name (`C-Space`, `M-j`, `\`, `K`, `Escape`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    /// a character (as typed, so `K` and `|` already carry the shift) or a key name
    pub key: String,
}

impl Chord {
    pub fn parse(s: &str) -> Option<Chord> {
        let mut c = Chord { ctrl: false, alt: false, key: String::new() };
        let mut rest = s;
        loop {
            if let Some(r) = rest.strip_prefix("C-").filter(|r| !r.is_empty()) {
                c.ctrl = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("M-").filter(|r| !r.is_empty()) {
                c.alt = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix('^').filter(|r| !r.is_empty()) {
                c.ctrl = true;
                rest = r;
            } else {
                break;
            }
        }
        let rest = match rest {
            "\\\\" => "\\",
            r => r,
        };
        c.key = match rest.to_lowercase().as_str() {
            "space" => "Space".into(),
            "escape" | "esc" => "Escape".into(),
            "enter" => "Enter".into(),
            "tab" => "Tab".into(),
            "bspace" => "Backspace".into(),
            "up" => "Up".into(),
            "down" => "Down".into(),
            "left" => "Left".into(),
            "right" => "Right".into(),
            "pageup" | "ppage" => "PageUp".into(),
            "pagedown" | "npage" => "PageDown".into(),
            _ if rest.chars().count() == 1 => {
                // with Ctrl, letters have no case (C-c and C-C are one key)
                if c.ctrl { rest.to_lowercase() } else { rest.to_string() }
            }
            _ => return None,
        };
        Some(c)
    }
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub action: Action,
    /// the tmux command line as written (zeit runs it as is)
    pub cmd: String,
    /// `-r`: may repeat without the prefix for a moment
    pub repeat: bool,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Keys {
    pub prefix: Chord,
    /// after the prefix
    pub table: HashMap<Chord, Binding>,
    /// `-n`: no prefix needed
    pub root: HashMap<Chord, Binding>,
    /// `-T copy-mode-vi`
    pub copy: HashMap<Chord, Binding>,
    /// `set-option` values, by name (`@user` options too)
    pub opts: HashMap<String, String>,
}

/// One tmux command line → an action zash can do.
pub fn action(cmd: &str) -> Option<Action> {
    let parts: Vec<Action> = cmd.split("\\;").filter_map(|c| one(c.trim())).collect();
    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => Some(Action::Seq(parts)),
    }
}

fn one(cmd: &str) -> Option<Action> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let (name, args) = words.split_first()?;
    let has = |f: &str| args.iter().any(|a| *a == f || (a.starts_with('-') && !a.starts_with("--") && a.len() > 2 && a[1..].contains(&f[1..])));
    let target = args.iter().position(|a| *a == "-t").and_then(|i| args.get(i + 1)).copied();
    Some(match *name {
        "split-window" | "splitw" => Action::Split { side_by_side: has("-h") },
        "select-pane" | "selectp" => match target {
            Some(t) if t.ends_with('+') => Action::FocusNext,
            _ if has("-L") => Action::Focus(Dir::Left),
            _ if has("-R") => Action::Focus(Dir::Right),
            _ if has("-U") => Action::Focus(Dir::Up),
            _ if has("-D") => Action::Focus(Dir::Down),
            _ => return None,
        },
        "resize-pane" | "resizep" => {
            if has("-Z") {
                Action::Zoom
            } else {
                let n = args.iter().filter_map(|a| a.parse::<u16>().ok()).next().unwrap_or(1);
                let d = if has("-L") { Dir::Left } else if has("-R") { Dir::Right } else if has("-U") { Dir::Up } else if has("-D") { Dir::Down } else { return None };
                Action::Resize(d, n)
            }
        }
        "kill-pane" | "killp" => Action::ClosePane,
        "new-window" | "neww" => Action::NewTab,
        "kill-window" | "killw" => Action::CloseTab,
        "next-window" | "next" => Action::NextTab,
        "previous-window" | "prev" => Action::PrevTab,
        "last-window" | "last" => Action::LastTab,
        "select-window" | "selectw" => match target {
            Some(t) if t.ends_with('+') => Action::NextTab,
            Some(t) if t.ends_with('-') => Action::PrevTab,
            _ => return None,
        },
        "send-prefix" => Action::SendPrefix,
        "copy-mode" => Action::Search,
        "paste-buffer" | "pasteb" => Action::Paste,
        _ => return None,
    })
}

impl Keys {
    /// tmux's own defaults (prefix Ctrl+B).
    pub fn defaults() -> Keys {
        let mut k = Keys { prefix: Chord::parse("C-b").unwrap(), table: HashMap::new(), root: HashMap::new(), copy: HashMap::new(), opts: HashMap::new() };
        for line in [
            "bind % split-window -h",
            "bind '\"' split-window -v",
            "bind o select-pane -t :.+",
            "bind Left select-pane -L",
            "bind Right select-pane -R",
            "bind Up select-pane -U",
            "bind Down select-pane -D",
            "bind -r M-Left resize-pane -L 5",
            "bind -r M-Right resize-pane -R 5",
            "bind -r M-Up resize-pane -U 5",
            "bind -r M-Down resize-pane -D 5",
            "bind z resize-pane -Z",
            "bind x kill-pane",
            "bind c new-window",
            "bind & kill-window",
            "bind n next-window",
            "bind p previous-window",
            "bind l last-window",
            "bind [ copy-mode",
            "bind ] paste-buffer",
        ] {
            k.line(line);
        }
        k
    }

    /// The defaults, then the user's tmux config on top (as tmux reads it).
    pub fn from_tmux_conf(text: &str) -> Keys {
        let mut k = Keys::defaults();
        k.text(text, 0);
        k
    }

    fn text(&mut self, text: &str, depth: u8) {
        // a line ending in a backslash goes on
        let mut joined = String::new();
        for line in text.lines() {
            let t = line.trim_end();
            if let Some(head) = t.strip_suffix('\\').filter(|h| !h.ends_with('\\')) {
                joined.push_str(head);
                joined.push(' ');
                continue;
            }
            joined.push_str(line);
            let l = std::mem::take(&mut joined);
            if let Some(path) = l.trim().strip_prefix("source-file").map(|r| r.split_whitespace().filter(|w| !w.starts_with('-')).collect::<Vec<_>>()) {
                if depth < 4 {
                    for p in path {
                        if let Ok(t) = std::fs::read_to_string(expand(unquote(p))) {
                            self.text(&t, depth + 1);
                        }
                    }
                }
                continue;
            }
            self.line(&l);
        }
    }

    /// The user's tmux config, if there is one.
    pub fn load() -> Keys {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| home.join(".config"));
        for p in [xdg.join("tmux/tmux.conf"), home.join(".tmux.conf")] {
            if let Ok(t) = std::fs::read_to_string(&p) {
                return Keys::from_tmux_conf(&t);
            }
        }
        Keys::defaults()
    }

    fn line(&mut self, line: &str) {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            return;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words[0] {
            "set" | "set-option" | "setw" | "set-window-option" => {
                let rest: Vec<&str> = words[1..].iter().skip_while(|w| w.starts_with('-')).copied().collect();
                let Some(name) = rest.first() else { return };
                let value = unquote(&rest[1..].join(" ")).to_string();
                if *name == "prefix" {
                    if let Some(c) = Chord::parse(&value) {
                        self.prefix = c;
                    }
                }
                self.opts.insert(name.to_string(), value);
            }
            "unbind" | "unbind-key" => {
                let no_prefix = words.iter().any(|w| *w == "-n");
                if let Some(c) = words[1..].iter().find(|w| !w.starts_with('-')).and_then(|k| Chord::parse(unquote(k))) {
                    if no_prefix { self.root.remove(&c) } else { self.table.remove(&c) };
                }
            }
            "bind" | "bind-key" => {
                let (mut table, mut repeat, mut i) = ("prefix", false, 1);
                while let Some(w) = words.get(i) {
                    match *w {
                        "-n" => table = "root",
                        "-r" => repeat = true,
                        "-T" => {
                            table = match words.get(i + 1).copied() {
                                Some("root") => "root",
                                Some("prefix") => "prefix",
                                Some("copy-mode-vi") => "copy",
                                _ => return,
                            };
                            i += 1;
                        }
                        w if w.starts_with('-') && w.len() > 1 && w != "-" => {
                            if w.contains('n') {
                                table = "root";
                            }
                            repeat |= w.contains('r');
                        }
                        _ => break,
                    }
                    i += 1;
                }
                let Some(key) = words.get(i).and_then(|k| Chord::parse(unquote(k))) else { return };
                let cmd = words[i + 1..].join(" ");
                let b = Binding { action: action(&cmd).unwrap_or(Action::Seq(Vec::new())), cmd, repeat };
                match table {
                    "root" => self.root.insert(key, b),
                    "copy" => self.copy.insert(key, b),
                    _ => self.table.insert(key, b),
                };
            }
            _ => {}
        }
    }
}

/// The line without its comment (an unquoted #).
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') if prev.is_whitespace() => return &line[..i],
            _ => {}
        }
        prev = c;
    }
    line
}

fn expand(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(r) => std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(r),
        None => std::path::PathBuf::from(p),
    }
}

fn unquote(s: &str) -> &str {
    let s = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(s);
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = r##"
unbind-key C-Space
set-option -g prefix C-Space
bind-key C-Space last-window
bind-key a send-prefix
bind-key k select-pane -U
bind-key h select-pane -L
bind-key -n M-o select-pane -t :.+ \; resize-pane -Z	# next pane in zoomed mode
bind-key -n M-j select-pane -t :.+	# next pane
bind-key -n M-h select-window -t :-	# previous window
bind-key -r J resize-pane -D 1
bind-key \\ split-window -h -c "#{pane_current_path}"	# vertical split
bind-key - split-window -v -c "#{pane_current_path}"
bind-key c new-window -c "#{pane_current_path}"
bind-key x kill-pane
bind-key e new-window -n 'conf' 'vim ~/.config/tmux/tmux.conf'
bind-key r source-file ~/.config/tmux/tmux.conf \; display-message "reloaded"
"##;

    fn c(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn reads_the_users_tmux_bindings() {
        let k = Keys::from_tmux_conf(CONF);
        assert_eq!(k.prefix, c("C-Space"));
        assert_eq!(k.table[&c("\\")].action, Action::Split { side_by_side: true });
        assert_eq!(k.table[&c("-")].action, Action::Split { side_by_side: false });
        assert_eq!(k.table[&c("h")].action, Action::Focus(Dir::Left));
        assert_eq!(k.table[&c("C-Space")].action, Action::LastTab);
        assert_eq!(k.table[&c("a")].action, Action::SendPrefix);
        assert!(k.table[&c("J")].repeat);
        assert_eq!(k.table[&c("J")].action, Action::Resize(Dir::Down, 1));
        assert_eq!(k.root[&c("M-j")].action, Action::FocusNext);
        assert_eq!(k.root[&c("M-h")].action, Action::PrevTab);
        assert_eq!(k.root[&c("M-o")].action, Action::Seq(vec![Action::FocusNext, Action::Zoom]));
        assert_eq!(k.table[&c("c")].action, Action::NewTab);
        // every binding keeps its command line, which zeit runs
        assert_eq!(k.table[&c("r")].cmd, r#"source-file ~/.config/tmux/tmux.conf \; display-message "reloaded""#);
        assert_eq!(k.opts["prefix"], "C-Space");
        // tmux defaults the config did not touch stay
        assert_eq!(k.table[&c("%")].action, Action::Split { side_by_side: true });
    }

    #[test]
    fn chords() {
        assert_eq!(c("C-c"), c("C-C"));
        assert_ne!(c("K"), c("k"));
        assert_eq!(c("M-j"), Chord { ctrl: false, alt: true, key: "j".into() });
        assert_eq!(c("C-Space").key, "Space");
    }
}

/// The next key in raw terminal input (what a text client reads from its tty),
/// and how many bytes it took. Enough to recognise prefixes and bindings; the
/// bytes themselves are passed on unchanged when a key is not zeit's.
pub fn chord_from_bytes(b: &[u8]) -> Option<(Chord, usize)> {
    let first = *b.first()?;
    let c = |ctrl: bool, alt: bool, key: &str| Chord { ctrl, alt, key: key.to_string() };
    Some(match first {
        0x00 => (c(true, false, "Space"), 1),
        0x09 => (c(false, false, "Tab"), 1),
        0x0d => (c(false, false, "Enter"), 1),
        0x7f => (c(false, false, "Backspace"), 1),
        0x1b => match b.get(1) {
            None => (c(false, false, "Escape"), 1),
            Some(b'[') => {
                let (name, n) = match b.get(2) {
                    Some(b'A') => ("Up", 3),
                    Some(b'B') => ("Down", 3),
                    Some(b'C') => ("Right", 3),
                    Some(b'D') => ("Left", 3),
                    Some(b'5') if b.get(3) == Some(&b'~') => ("PageUp", 4),
                    Some(b'6') if b.get(3) == Some(&b'~') => ("PageDown", 4),
                    _ => return Some((c(false, false, "Escape"), 1)),
                };
                (c(false, false, name), n)
            }
            Some(&x) if (0x20..0x7f).contains(&x) => (c(false, true, &(x as char).to_string()), 2),
            Some(&x) if (0x01..=0x1a).contains(&x) => (c(true, true, &((x - 1 + b'a') as char).to_string()), 2),
            _ => (c(false, false, "Escape"), 1),
        },
        0x01..=0x1a => (c(true, false, &((first - 1 + b'a') as char).to_string()), 1),
        _ => {
            // one UTF-8 character
            let len = match first {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            let s = std::str::from_utf8(b.get(..len)?).ok()?;
            (c(false, false, s), len)
        }
    })
}

/// A chord as the bytes a terminal sends for it (send-prefix, tests).
pub fn chord_to_bytes(k: &Chord) -> Vec<u8> {
    let base: Vec<u8> = match k.key.as_str() {
        "Space" => vec![if k.ctrl { 0 } else { b' ' }],
        "Enter" => vec![b'\r'],
        "Tab" => vec![b'\t'],
        "Escape" => vec![0x1b],
        "Backspace" => vec![0x7f],
        "Up" => b"\x1b[A".to_vec(),
        "Down" => b"\x1b[B".to_vec(),
        "Right" => b"\x1b[C".to_vec(),
        "Left" => b"\x1b[D".to_vec(),
        s if k.ctrl && s.len() == 1 && s.as_bytes()[0].is_ascii_lowercase() => vec![s.as_bytes()[0] - b'a' + 1],
        s => s.as_bytes().to_vec(),
    };
    if k.alt { [vec![0x1b], base].concat() } else { base }
}

#[cfg(test)]
mod byte_tests {
    use super::*;

    #[test]
    fn raw_input_to_chords_and_back() {
        let (k, n) = chord_from_bytes(&[0x00, b'x']).unwrap();
        assert_eq!((k.clone(), n), (Chord::parse("C-Space").unwrap(), 1));
        assert_eq!(chord_to_bytes(&k), vec![0]);
        assert_eq!(chord_from_bytes(b"\x1bj").unwrap().0, Chord::parse("M-j").unwrap());
        assert_eq!(chord_from_bytes(b"\x1b[A").unwrap(), (Chord::parse("Up").unwrap(), 3));
        assert_eq!(chord_from_bytes(&[0x02]).unwrap().0, Chord::parse("C-b").unwrap());
        assert_eq!(chord_from_bytes("ä".as_bytes()).unwrap().1, 2);
        assert_eq!(chord_from_bytes(b"\\").unwrap().0, Chord::parse("\\\\").unwrap());
    }
}
