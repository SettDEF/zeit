//! What a copy puts on the clipboard: the selection with the lines a program wrapped itself
//! (Claude Code, an editor's soft wrap drawn as rows) joined back. zeit knows the pane's width, so
//! a line was wrapped when it reaches close to that width and the next line's first word would not
//! have fit after it; lines ended on purpose (lists, fences, short lines) stay apart.
//! `set -g copy-unwrap off` copies the rows as they are.

/// A line that starts something of its own: a list item, a quote, a fence, a box or table row.
fn starts_own(line: &str) -> bool {
    let t = line.trim_start();
    let num = t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    num || ["- ", "* ", "+ ", "• ", "● ", "⎿ ", "> ", "```", "│", "|", "┃", "#", "$ "].iter().any(|p| t.starts_with(p))
}

/// The selection as it goes to the clipboard, for a pane `width` columns wide.
pub fn tidy(text: &str, width: usize, unwrap: bool) -> String {
    let lines: Vec<&str> = text.split('\n').map(str::trim_end).collect();
    if !unwrap || lines.len() < 2 || width < 24 {
        return lines.join("\n");
    }
    let mut out = String::from(lines[0]);
    for (i, l) in lines.iter().enumerate().skip(1) {
        let prev = lines[i - 1];
        let next_word = l.trim_start().split(' ').next().unwrap_or("").chars().count();
        let len = prev.chars().count();
        // close to the pane's edge (a program wraps a few columns early, at a word), and the next
        // word would not have fit
        let wrapped = len * 10 >= width * 6 && !prev.trim().is_empty() && !l.trim().is_empty() && !starts_own(l) && len + 1 + next_word > width.saturating_sub(4);
        if wrapped {
            out.push(' ');
            out.push_str(l.trim_start());
        } else {
            out.push('\n');
            out.push_str(l);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tidy;

    #[test]
    fn a_wrapped_command_comes_back_whole_and_lists_stay_lists() {
        // Claude Code wrapping at a 44-column pane, two spaces in
        let wrapped = "  echo one two three four five six seven\n  eight nine ten";
        assert_eq!(tidy(wrapped, 44, true), "  echo one two three four five six seven eight nine ten");
        let list = "Things:\n- one thing\n- another thing\nshort";
        assert_eq!(tidy(list, 44, true), list);
        assert_eq!(tidy(wrapped, 44, false), wrapped);
        // a short line in a wide pane is a line, not a wrap
        assert_eq!(tidy("ls -la\ncd /tmp", 120, true), "ls -la\ncd /tmp");
    }
}
