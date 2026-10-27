//! zeit's options in one table, as Neovim keeps its own: name, kind, default, what it does.
//! `set` checks a name and value against it (a typo gets "did you mean"), `show-options -A`
//! lists every option with its value, `zeit options` prints the reference. Names starting with
//! `@` are variables (themes, plugins) and always allowed; so are unknown names, with a warning,
//! because plugins keep their own settings the same way.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    Int,
    Text,
    /// tmux style: `fg=blue,bg=black,bold`
    Style,
    /// a colour: a name (blue, brightred), colour123, #rrggbb
    Color,
    Choice(&'static [&'static str]),
}

pub struct Opt {
    pub name: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    pub about: &'static str,
}

const fn o(name: &'static str, kind: Kind, default: &'static str, about: &'static str) -> Opt {
    Opt { name, kind, default, about }
}

pub const OPTIONS: &[Opt] = &[
    o("prefix", Kind::Text, "C-b", "the key before zeit's own keys (C-a, C-Space, M-a…)"),
    o("default-command", Kind::Text, "", "what a new pane runs (empty: the shell)"),
    o("default-shell", Kind::Text, "", "the shell for new panes (empty: $SHELL)"),
    o("history-limit", Kind::Int, "10000", "lines each pane keeps above the screen"),
    o("monitor-activity", Kind::Bool, "off", "mark windows whose panes print while you are elsewhere"),
    o("copy-unwrap", Kind::Bool, "on", "a mouse copy joins lines a program wrapped (Claude Code, editors) back into one; off: rows as they are"),
    o("job-notify", Kind::Choice(&["long", "failed", "all", "off"]), "long", "a desktop notice when a job ends: long (failed, or longer than job-notify-after), failed, all, off"),
    o("job-notify-after", Kind::Int, "10", "seconds a job runs before its end is worth a notice (job-notify long)"),
    o("status-position", Kind::Choice(&["bottom", "top"]), "bottom", "where the status bar is"),
    o("status-style", Kind::Style, "", "the status bar's colours, tmux style: fg=…,bg=…"),
    o("status-bg", Kind::Color, "green", "the status bar's background (status-style wins)"),
    o("status-fg", Kind::Color, "black", "the status bar's text (status-style wins)"),
    o("status-right-gap", Kind::Int, "0", "columns left free at the bar's right end"),
    o("status-right", Kind::Text, "", "the bar's right end, a format: #{session_name}, #{window_zoomed_flag}, #{zfloat_name}, #{?zfloat_zoomed,max,} … (empty: the session and its clients)"),
    o("pane-border-style", Kind::Style, "fg=brightblack", "the lines between panes"),
    o("pane-active-border-style", Kind::Style, "fg=green", "the lines around the focused pane"),
];

pub fn find(name: &str) -> Option<&'static Opt> {
    OPTIONS.iter().find(|o| o.name == name)
}

/// The known option nearest to a mistyped name (at most two edits away).
pub fn closest(name: &str) -> Option<&'static str> {
    fn dist(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut prev = row[0];
            row[0] = i;
            for j in 1..=b.len() {
                let cur = row[j];
                row[j] = (row[j] + 1).min(row[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
                prev = cur;
            }
        }
        row[b.len()]
    }
    OPTIONS.iter().map(|o| (dist(name, o.name), o.name)).filter(|(d, _)| *d <= 2).min().map(|(_, n)| n)
}

/// Is `value` right for `name`? Ok(None): fine; Ok(Some(warning)): kept, but worth saying;
/// Err: refused.
pub fn check(name: &str, value: &str) -> Result<Option<String>, String> {
    if name.starts_with('@') {
        return Ok(None);
    }
    let Some(o) = find(name) else {
        return Ok(Some(match closest(name) {
            Some(c) => format!("{name} is not one of zeit's options (did you mean {c}?); kept for plugins"),
            None => format!("{name} is not one of zeit's options; kept for plugins (zeit options lists them)"),
        }));
    };
    match o.kind {
        Kind::Bool if !matches!(value, "on" | "off" | "true" | "false" | "1" | "0" | "yes" | "no") => Err(format!("{name} is on or off")),
        Kind::Int if value.parse::<i64>().is_err() => Err(format!("{name} is a number")),
        Kind::Choice(c) if !c.contains(&value) => Err(format!("{name} is one of: {}", c.join(", "))),
        Kind::Color if crate::look::color(value).is_none() => Err(format!("{name}: {value} is not a colour (blue, brightred, colour123, #rrggbb)")),
        _ => Ok(None),
    }
}

/// Every option: its value now (set ones marked), and what it does; `@` variables after them.
pub fn listing(set: &std::collections::HashMap<String, String>) -> String {
    let w = OPTIONS.iter().map(|o| o.name.len()).max().unwrap_or(10);
    let mut out = Vec::new();
    for o in OPTIONS {
        let (v, mark) = match set.get(o.name) {
            Some(v) => (v.as_str(), "*"),
            None => (o.default, " "),
        };
        let v = if v.is_empty() { "\"\"" } else { v };
        out.push(format!("{mark} {:<w$}  {:<16}  {}", o.name, v, o.about));
    }
    let mut vars: Vec<_> = set.iter().filter(|(k, _)| k.starts_with('@') || find(k).is_none()).collect();
    vars.sort();
    for (k, v) in vars {
        out.push(format!("* {k:<w$}  {v}"));
    }
    out.join("\n")
}

/// `zeit options`: the reference, as `:help options` reads.
pub fn reference() -> String {
    let mut out = String::from("zeit options: set -g NAME VALUE (in zeit.conf, init.lua with zeit.set, or `zeit set`)\n\n");
    for o in OPTIONS {
        let kind = match o.kind {
            Kind::Bool => "on/off".to_string(),
            Kind::Int => "number".into(),
            Kind::Text => "text".into(),
            Kind::Style => "style".into(),
            Kind::Color => "colour".into(),
            Kind::Choice(c) => c.join("|"),
        };
        let d = if o.default.is_empty() { "\"\"" } else { o.default };
        out.push_str(&format!("{}  ({kind}, default {d})\n    {}\n", o.name, o.about));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_values_and_typos() {
        assert_eq!(check("copy-unwrap", "off"), Ok(None));
        assert!(check("copy-unwrap", "maybe").is_err());
        assert!(check("history-limit", "lots").is_err());
        assert!(check("job-notify", "sometimes").unwrap_err().contains("long, failed, all, off"));
        assert!(check("status-bg", "notacolour").is_err());
        assert_eq!(check("@primary", "#89b4fa"), Ok(None));
        let w = check("histroy-limit", "5").unwrap().unwrap();
        assert!(w.contains("did you mean history-limit"), "{w}");
    }

    #[test]
    fn the_listing_marks_what_is_set() {
        let mut set = std::collections::HashMap::new();
        set.insert("copy-unwrap".to_string(), "off".to_string());
        set.insert("@primary".to_string(), "#89b4fa".to_string());
        let l = listing(&set);
        assert!(l.lines().any(|x| x.starts_with("* copy-unwrap") && x.contains("off")));
        assert!(l.lines().any(|x| x.starts_with("  history-limit") && x.contains("10000")));
        assert!(l.lines().last().unwrap().contains("@primary"));
    }
}
