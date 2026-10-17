//! The keys window (list-keys, prefix ?) and moving through the layers: panes, floats, the zfloat.

use super::*;

fn label(c: &Chord) -> String {
    format!("{}{}{}", if c.ctrl { "C-" } else { "" }, if c.alt { "M-" } else { "" }, c.key)
}

impl Server {
    /// `list-keys [-c]`: every key, by table (Lua maps over tmux.conf binds), -c in colour.
    pub(super) fn list_keys(&self, colour: bool) -> String {
        let (b, d, a, r) = if colour { ("\x1b[1m", "\x1b[90m", "\x1b[36m", "\x1b[0m") } else { ("", "", "", "") };
        let lua = self.ext.described();
        let mut out = String::new();
        let prefix = label(&self.keys.prefix);
        for (table, title, map) in [("prefix", format!("After the prefix ({prefix})"), &self.keys.table), ("root", "Without the prefix".to_string(), &self.keys.root), ("copy", "In copy mode".to_string(), &self.keys.copy)] {
            // tmux.conf's binds, then Lua's over them (they win, as when keys are pressed)
            let mut rows: std::collections::BTreeMap<String, (String, String)> = map.iter().map(|(k, v)| (label(k), (v.cmd.clone(), String::new()))).collect();
            for (t, k, cmd, desc) in &lua {
                if t == table {
                    rows.insert(label(k), (cmd.clone(), desc.clone()));
                }
            }
            if rows.is_empty() {
                continue;
            }
            out.push_str(&format!("{b}{title}{r}\n"));
            for (k, (cmd, desc)) in rows {
                let what = if desc.is_empty() { cmd.clone() } else { format!("{desc}  {d}{cmd}{r}") };
                out.push_str(&format!("  {a}{k:<12}{r} {what}\n"));
            }
            out.push('\n');
        }
        out.push_str(&format!("{d}prefix ? shows this · q closes · / searches{r}\n"));
        out
    }

    /// The layer a client's keys are on: 0 the panes, 1 a float, 2 the zfloat.
    fn layer_of(&self, sid: &str) -> u8 {
        if self.shown_popup(sid).is_some() && !self.popup_below.contains(sid) {
            2
        } else if self.focused_float(sid).is_some() {
            1
        } else {
            0
        }
    }

    /// `layer-up` / `layer-down`: the keys one layer up (panes → floats → zfloat) or down;
    /// a layer that has nothing is skipped, and what is up there is shown.
    pub(super) fn layer_move(&mut self, cid: Option<u64>, up: bool) -> Reply {
        let sid = cid.and_then(|c| self.client_session(c)).ok_or("no session")?;
        let floats: Vec<String> = self.window_floats(&sid).iter().map(|f| f.pane.clone()).collect();
        let has_zfloat = self.popups.get(&sid).is_some_and(|l| !l.is_empty());
        let at = self.layer_of(&sid);
        let to = match (up, at) {
            (true, 0) if !floats.is_empty() => 1,
            (true, 0 | 1) if has_zfloat => 2,
            (false, 2) if !floats.is_empty() => 1,
            (false, 2 | 1) => 0,
            _ => return Err(if up { "nothing above (prefix f: a zfloat, M-f: a float)".into() } else { "already on the panes".into() }),
        };
        match to {
            2 => {
                if self.shown_popup(&sid).is_none() {
                    let last = self.popups.get(&sid).and_then(|l| l.last()).map(|p| p.pane.clone());
                    if let Some(p) = last {
                        self.popup_shown.insert(sid.clone(), p);
                    }
                }
                self.popup_below.remove(&sid);
            }
            1 => {
                if self.shown_popup(&sid).is_some() {
                    self.popup_below.insert(sid.clone());
                }
                for f in self.floats.iter_mut().filter(|f| floats.contains(&f.pane)) {
                    f.shown = true;
                }
                self.float_focus.insert(sid.clone(), floats[floats.len() - 1].clone());
            }
            _ => {
                if self.shown_popup(&sid).is_some() {
                    self.popup_below.insert(sid.clone());
                }
                self.float_focus.remove(&sid);
            }
        }
        self.dirty = true;
        Ok(["the panes", "the floats", "the zfloat"][to as usize].to_string())
    }
}
