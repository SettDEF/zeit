//! The keys window (list-keys, prefix ?) and moving through the layers: the panes and the popups (floats, zfloats).

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

    /// `layer-up` / `layer-down`: the keys up to the popups (floats and zfloats) or down to the
    /// panes; going up with none shown shows the window's floats, else the last zfloat.
    pub(super) fn layer_move(&mut self, cid: Option<u64>, up: bool) -> Reply {
        let sid = cid.and_then(|c| self.client_session(c)).ok_or("no session")?;
        let on_popup = self.shown_popup(&sid).is_some() && !self.popup_below.contains(&sid);
        if !up {
            if !on_popup {
                return Err("already on the panes".into());
            }
            self.popup_below.insert(sid.clone());
            self.dirty = true;
            return Ok("the panes".into());
        }
        if on_popup {
            return Err("already on the floats".into());
        }
        if self.shown_popup(&sid).is_none() {
            let ids = self.session(&sid).and_then(|s| s.windows.get(s.active)).map(|w| w.layout.ids()).unwrap_or_default();
            let list = self.popups.get(&sid).cloned().unwrap_or_default();
            let mine: Vec<String> = list.iter().filter(|p| p.window.as_ref().is_some_and(|a| ids.contains(a))).map(|p| p.pane.clone()).collect();
            let show = if mine.is_empty() { list.iter().rev().find(|p| p.window.is_none()).map(|p| vec![p.pane.clone()]).unwrap_or_default() } else { mine };
            if show.is_empty() {
                return Err("nothing above (prefix f: a zfloat, M-f: a float)".into());
            }
            for p in &show {
                self.show_popup(&sid, p);
            }
        }
        self.popup_below.remove(&sid);
        self.dirty = true;
        Ok("the floats".into())
    }
}
