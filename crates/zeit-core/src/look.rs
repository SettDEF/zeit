//! How zeit looks, from tmux-named options (`status-position`, `@primary` …).

use std::collections::HashMap;
use zeit_proto::Color;

#[derive(Clone, Debug)]
pub struct Look {
    pub top: bool,
    pub status_bg: Color,
    pub status_fg: Color,
    pub border: Color,
    pub active_border: Color,
    pub primary: Color,
    pub on_primary: Color,
    pub pill: Color,
    pub muted: Color,
    pub activity: Color,
    pub bell: Color,
    pub accent: Color,
    pub icons: HashMap<String, String>,
    /// the icon in the window-count pill (tmux.conf's status-left had one)
    pub left_icon: String,
    pub history: usize,
    pub activity_on: bool,
}

/// A tmux colour: `#rrggbb`, `colour123`, `blue`, `brightred`, `default`.
pub fn color(s: &str) -> Option<Color> {
    let s = s.trim().trim_matches('"').to_ascii_lowercase();
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return (hex.len() == 6).then_some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    if let Some(n) = s.strip_prefix("colour").or_else(|| s.strip_prefix("color")) {
        return n.parse().ok().map(Color::Indexed);
    }
    if let Some(n) = s.strip_prefix("bright") {
        return NAMES.iter().position(|x| *x == n).map(|i| Color::Indexed(i as u8 + 8));
    }
    match s.as_str() {
        "default" | "terminal" => Some(Color::Default),
        "gray" | "grey" => Some(Color::Indexed(8)),
        n => NAMES.iter().position(|x| *x == n).map(|i| Color::Indexed(i as u8)),
    }
}

/// `fg=blue,bg=black,bold` → the colour of `key` (fg or bg).
pub fn style(s: &str, key: &str) -> Option<Color> {
    s.split(',').find_map(|part| part.trim().strip_prefix(key)?.strip_prefix('=').and_then(color))
}

impl Look {
    pub fn from(opts: &HashMap<String, String>) -> Look {
        let opt = |k: &str| opts.get(k).map(String::as_str);
        let var = |k: &str, or: Color| opt(k).and_then(color).unwrap_or(or);
        let status = opt("status-style").unwrap_or("");
        let icons = opts.iter().filter_map(|(k, v)| Some((k.strip_prefix("@icon:")?.to_string(), v.clone()))).collect();
        Look {
            top: opt("status-position") == Some("top"),
            status_bg: style(status, "bg").or_else(|| opt("status-bg").and_then(color)).unwrap_or(Color::Indexed(2)),
            status_fg: style(status, "fg").or_else(|| opt("status-fg").and_then(color)).unwrap_or(Color::Indexed(0)),
            border: opt("pane-border-style").and_then(|s| style(s, "fg")).unwrap_or(Color::Indexed(8)),
            active_border: opt("pane-active-border-style").and_then(|s| style(s, "fg")).unwrap_or(Color::Indexed(2)),
            primary: var("@primary", Color::Indexed(4)),
            on_primary: var("@on_primary", Color::Indexed(0)),
            pill: var("@bg_dim", Color::Indexed(0)),
            muted: var("@muted", Color::Indexed(7)),
            activity: var("@tertiary", Color::Indexed(3)),
            bell: var("@error", Color::Indexed(1)),
            accent: var("@accent", Color::Indexed(4)),
            icons,
            left_icon: opt("@left_icon").unwrap_or("\u{f2d0}").to_string(),
            history: opt("history-limit").and_then(|v| v.parse().ok()).unwrap_or(10_000),
            activity_on: matches!(opt("monitor-activity"), Some("on" | "true" | "1")),
        }
    }

    /// Whether the status line is drawn the pill way (the tinct colours are set).
    pub fn pills(opts: &HashMap<String, String>) -> bool {
        opts.contains_key("@primary")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_styles() {
        assert_eq!(color("#ddc7a1"), Some(Color::Rgb(0xdd, 0xc7, 0xa1)));
        assert_eq!(color("blue"), Some(Color::Indexed(4)));
        assert_eq!(color("brightred"), Some(Color::Indexed(9)));
        assert_eq!(color("colour200"), Some(Color::Indexed(200)));
        assert_eq!(style("fg=blue,bg=black", "bg"), Some(Color::Indexed(0)));
        assert_eq!(style("fg=green,bg=black", "fg"), Some(Color::Indexed(2)));
    }
}
