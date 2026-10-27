//! `zeit inspect`, `zeit find`, `zeit blocks`: what is on a pane, named. A letter's colours, style
//! and link (as Neovim's :Inspect tells a character's highlight), the command block it belongs
//! to (cmd:N, the number the prompt shows) and the element it is part of (a path, a link, an
//! error …); every element of a kind, or matching a pattern, with where it is.

use super::*;
use crate::blocks::Part;

/// What `find` looks for: a kind of element, a pattern (/re/), or a command's output (cmd:N).
enum Query {
    Kind(String),
    Pattern(regex::Regex),
    Block(u32),
}

impl Server {
    /// `inspect [-t PANE] [COL ROW] [--json]`: the cell at COL ROW of the pane's view (from 0), or
    /// under the cursor.
    pub(super) fn inspect(&self, pane: &str, at: Option<(usize, i64)>, json: bool) -> Result<String, String> {
        let p = self.panes.get(pane).ok_or("no pane")?;
        let (line, col) = match at {
            Some((c, r)) => (p.view_top() + r, c),
            None => p.cursor_at(),
        };
        let cell = p.cell_at(line, col).ok_or("nothing there (outside the pane)")?;
        let text = p.rows_text(line, line + 1).pop().unwrap_or_default();
        let elements = crate::elements::scan(&text);
        let here: Vec<_> = elements.iter().filter(|e| col >= e.from && col < e.to).collect();
        let block = self.blocks.get(pane).and_then(|b| b.at(line));
        if json {
            let v = serde_json::json!({
                "pane": pane, "line": line, "col": col, "cell": cell,
                "block": block.map(|(b, part)| serde_json::json!({ "id": b.id, "part": part, "cmd": b.cmd, "cwd": b.cwd, "exit": b.exit })),
                "elements": here,
            });
            return Ok(v.to_string());
        }
        let mut out = format!("{:?} at column {col}, line {line}\n  fg {}  bg {}", cell.text, cell.fg, cell.bg);
        if !cell.flags.is_empty() {
            out += &format!("  {}", cell.flags.join(", "));
        }
        if let Some(l) = &cell.link {
            out += &format!("\n  link {l}");
        }
        match block {
            Some((b, part)) => {
                let exit = b.exit.map(|e| if e == 0 { " · ok".to_string() } else { format!(" · exit {e}") }).unwrap_or_else(|| " · running".into());
                out += &format!("\n  cmd:{} {} — {}  ({}{exit})", b.id, if part == Part::Output { "output" } else { "prompt" }, b.cmd, b.cwd);
            }
            None => out += "\n  no command (no shell marks on this line)",
        }
        for e in here {
            out += &format!("\n  {} {:?} (columns {}–{})", e.kind, e.text, e.from, e.to);
        }
        Ok(out)
    }

    /// `find [-t PANE | -a] KIND|/RE/|cmd:N [--json]`: every match, oldest first, as
    /// `PANE line:col kind cmd:N text`.
    pub(super) fn find(&self, panes: &[String], what: &str, json: bool) -> Result<String, String> {
        let q = if let Some(n) = what.strip_prefix("cmd:") {
            Query::Block(n.parse().map_err(|_| format!("cmd:N needs a number, not {n:?}"))?)
        } else if what.len() > 1 && what.starts_with('/') && what.ends_with('/') {
            Query::Pattern(regex::Regex::new(&what[1..what.len() - 1]).map_err(|e| e.to_string())?)
        } else if crate::elements::KINDS.contains(&what) {
            Query::Kind(what.to_string())
        } else {
            return Err(format!("find what: {} · /pattern/ · cmd:N", crate::elements::KINDS.join(" ")));
        };
        let mut hits = Vec::new();
        for id in panes {
            let Some(p) = self.panes.get(id) else { continue };
            let blocks = self.blocks.get(id);
            let (from, to) = match &q {
                Query::Block(n) => {
                    let b = blocks.and_then(|b| b.get(*n)).ok_or_else(|| format!("no cmd:{n} in this pane"))?;
                    (b.start.unwrap_or(0), b.end.unwrap_or(i64::MAX))
                }
                _ => (0, i64::MAX),
            };
            let total = p.view_top() + p.rows as i64 + p.scrolled() as i64;
            let rows = p.rows_text(from, to.min(total));
            for (i, text) in rows.iter().enumerate() {
                let line = from + i as i64;
                let found: Vec<(usize, usize, &'static str, String)> = match &q {
                    Query::Kind(k) => crate::elements::scan(text).into_iter().filter(|e| e.kind == k).map(|e| (e.from, e.to, e.kind, e.text)).collect(),
                    Query::Pattern(re) => re.find_iter(text).map(|m| (text[..m.start()].chars().count(), text[..m.end()].chars().count(), "match", m.as_str().to_string())).collect(),
                    Query::Block(_) => (!text.is_empty()).then(|| (0, text.chars().count(), "line", text.clone())).into_iter().collect(),
                };
                let block = blocks.and_then(|b| b.at(line)).map(|(b, _)| b.id);
                for (a, z, kind, t) in found {
                    hits.push(serde_json::json!({ "pane": id, "line": line, "from": a, "to": z, "kind": kind, "cmd": block, "text": t }));
                }
            }
        }
        if json {
            return Ok(serde_json::Value::Array(hits).to_string());
        }
        if hits.is_empty() {
            return Err("nothing found".into());
        }
        Ok(hits
            .iter()
            .map(|h| format!("%{} {}:{} {} {} {}", h["pane"].as_str().unwrap_or(""), h["line"], h["from"], h["kind"].as_str().unwrap_or(""), h["cmd"].as_u64().map(|n| format!("cmd:{n}")).unwrap_or_else(|| "-".into()), h["text"].as_str().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// `blocks [-t PANE] [--json]`: the pane's commands, as cmd:N with how they ended.
    pub(super) fn blocks_list(&self, pane: &str, json: bool) -> Result<String, String> {
        let list = self.blocks.get(pane).map(|b| b.list()).unwrap_or(&[]);
        if json {
            return serde_json::to_string(list).map_err(|e| e.to_string());
        }
        if list.is_empty() {
            return Err("no commands seen in this pane yet (its shell needs zeit's hook)".into());
        }
        Ok(list.iter().map(|b| format!("cmd:{:<4} {:>6} {}", b.id, b.exit.map(|e| if e == 0 { "ok".into() } else { format!("exit {e}") }).unwrap_or_else(|| "…".into()), b.cmd)).collect::<Vec<_>>().join("\n"))
    }
}

