//! Kitty graphics from panes, passed on to the outer terminal (zash, kitty) as tmux does: a pane's
//! image is sent to each client once under an id of zeit's own, then placed where the pane shows
//! it, at the pane line it came on, and moved or taken away as the pane scrolls, splits or hides.
//! The pane's own Term never sees images; text after one is not pushed down past it (programs
//! that place with C=1 or move the cursor themselves are right).

use std::collections::HashMap;

/// An image placed in a pane: zeit's id for it, the pane line (history counted) and column.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Img {
    pub id: u32,
    pub line: i64,
    pub col: usize,
    /// the placement's own keys (c=, r=, X=, Y=, z=…) as written
    pub keys: String,
    /// rows it covers, when the pane said (r=): hidden once it no longer fits in the pane
    pub rows: Option<usize>,
}

#[derive(Default)]
pub(crate) struct Images {
    next: u32,
    /// (pane, the pane's own image id) → zeit's
    ids: HashMap<(String, u32), u32>,
    /// zeit's id → the APC bodies that send it (a=t, quiet), chunk by chunk
    pub data: HashMap<u32, Vec<String>>,
    /// a pane's placements
    pub places: HashMap<String, Vec<Img>>,
    /// a chunked image a pane is still sending: its first chunk's keys, the chunks so far
    pending: HashMap<String, (Vec<(String, String)>, Vec<String>)>,
    /// zeit's id → the pane that sent the image
    owner: HashMap<u32, String>,
}

fn keys_of(ctrl: &str) -> Vec<(String, String)> {
    ctrl.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn get<'a>(keys: &'a [(String, String)], k: &str) -> Option<&'a str> {
    keys.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

/// The keys of a placement (what a=p takes): size, offset, layer, the source rectangle.
const PLACE_KEYS: &[&str] = &["c", "r", "X", "Y", "z", "x", "y", "w", "h"];

impl Images {
    /// One APC G from `pane` (its body: `keys;payload`), the pane's cursor at `line`/`col`. The
    /// answer for the pane, when it asked for one (`i=` and not quiet): Kitty's `i=N;OK` or error.
    /// `outer_has_it`: an attached terminal shows Kitty graphics (for a=q).
    pub fn command(&mut self, pane: &str, body: &str, line: i64, col: usize, outer_has_it: bool) -> Option<String> {
        let (ctrl, payload) = body.split_once(';').unwrap_or((body, ""));
        let mut keys = keys_of(ctrl);
        // chunks after the first carry only m=: the first chunk's keys rule
        if let Some((first, mut chunks)) = self.pending.remove(pane) {
            chunks.push(payload.to_string());
            if get(&keys, "m") == Some("1") {
                self.pending.insert(pane.to_string(), (first, chunks));
                return None;
            }
            return self.run(pane, first, chunks, line, col, outer_has_it);
        }
        if get(&keys, "m") == Some("1") {
            keys.retain(|(k, _)| k != "m");
            self.pending.insert(pane.to_string(), (keys, vec![payload.to_string()]));
            return None;
        }
        self.run(pane, keys, vec![payload.to_string()], line, col, outer_has_it)
    }

    fn run(&mut self, pane: &str, keys: Vec<(String, String)>, chunks: Vec<String>, line: i64, col: usize, outer_has_it: bool) -> Option<String> {
        let action = get(&keys, "a").unwrap_or("t").to_string();
        let app_id: u32 = get(&keys, "i").and_then(|v| v.parse().ok()).unwrap_or(0);
        let quiet: u8 = get(&keys, "q").and_then(|v| v.parse().ok()).unwrap_or(0);
        let answer = |msg: &str, err: bool| -> Option<String> {
            (app_id != 0 && quiet < 2 && (err || quiet == 0)).then(|| format!("\x1b_Gi={app_id};{msg}\x1b\\"))
        };
        match action.as_str() {
            // can it be shown? only as truly as the terminal outside can
            "q" => answer(if outer_has_it { "OK" } else { "ENOTSUPPORTED:no graphics in this terminal" }, !outer_has_it),
            "d" => {
                let what = get(&keys, "d").unwrap_or("a");
                let list = self.places.entry(pane.to_string()).or_default();
                match what {
                    "i" | "I" => {
                        let id = self.ids.get(&(pane.to_string(), app_id)).copied();
                        list.retain(|p| Some(p.id) != id);
                    }
                    _ => list.clear(),
                }
                None
            }
            "t" | "T" | "p" => {
                let id = match (action.as_str(), app_id) {
                    // sent before (a=p): the image of that id
                    ("p", n) => match self.ids.get(&(pane.to_string(), n)) {
                        Some(&id) => id,
                        None => return answer("ENOENT:no such image", true),
                    },
                    // a new one: an id of zeit's (one the pane gave is kept for a=p later)
                    (_, n) => {
                        self.next += 1;
                        let id = self.next;
                        if n != 0 {
                            self.ids.insert((pane.to_string(), n), id);
                        }
                        // sent on as a=t only (placed by zeit), quiet; the format keys as they were
                        let mut first: Vec<String> = keys.iter().filter(|(k, _)| !matches!(k.as_str(), "a" | "i" | "q" | "m" | "p") && !PLACE_KEYS.contains(&k.as_str())).map(|(k, v)| format!("{k}={v}")).collect();
                        first.splice(0..0, ["a=t".to_string(), format!("i={id}"), "q=2".to_string()]);
                        let last = chunks.len() - 1;
                        let bodies = chunks.iter().enumerate().map(|(i, c)| {
                            let more = if i < last { "m=1" } else { "m=0" };
                            if i == 0 { format!("{},{more};{c}", first.join(",")) } else { format!("{more};{c}") }
                        });
                        self.data.insert(id, bodies.collect());
                        self.owner.insert(id, pane.to_string());
                        id
                    }
                };
                if action != "t" {
                    let place: Vec<String> = keys.iter().filter(|(k, _)| PLACE_KEYS.contains(&k.as_str())).map(|(k, v)| format!("{k}={v}")).collect();
                    let rows = get(&keys, "r").and_then(|v| v.parse().ok());
                    let list = self.places.entry(pane.to_string()).or_default();
                    list.push(Img { id, line, col, keys: place.join(","), rows });
                    // a few hundred a pane at most: the oldest go
                    if list.len() > 300 {
                        list.remove(0);
                    }
                }
                answer("OK", false)
            }
            _ => answer("EINVAL:action", true),
        }
    }

    /// A pane closed: its images go.
    pub fn pane_gone(&mut self, pane: &str) {
        self.places.remove(pane);
        let mine: Vec<u32> = self.owner.iter().filter(|(_, p)| p.as_str() == pane).map(|(id, _)| *id).collect();
        for id in mine {
            self.owner.remove(&id);
            self.data.remove(&id);
        }
        self.ids.retain(|(p, _), _| p != pane);
        self.pending.remove(pane);
    }
}

/// Where each placement shows on a client's screen: `panes` (id, x, y, w, h, the pane's top line
/// in history-counted lines). zeit's id → (x, y, keys). One that does not fit is not shown.
pub(crate) fn visible(images: &Images, panes: &[(String, u16, u16, u16, u16, i64)]) -> HashMap<u32, (u16, u16, String)> {
    let mut out = HashMap::new();
    for (pane, x, y, w, h, top) in panes {
        for img in images.places.get(pane).into_iter().flatten() {
            let row = img.line - top;
            if row < 0 || row >= *h as i64 || img.col >= *w as usize || img.rows.is_some_and(|r| row as usize + r > *h as usize) {
                continue;
            }
            out.insert(img.id, (x + img.col as u16, y + row as u16, img.keys.clone()));
        }
    }
    out
}

/// What a client is sent to show `want` after `had`: deletes for what went or moved, the image
/// data it has not had yet (`sent`), and the placements (cursor kept: ESC 7 … ESC 8).
pub(crate) fn changes(images: &Images, had: &HashMap<u32, (u16, u16, String)>, want: &HashMap<u32, (u16, u16, String)>, sent: &mut std::collections::HashSet<u32>) -> String {
    let mut out = String::new();
    for (id, p) in had {
        if want.get(id) != Some(p) {
            out.push_str(&format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\"));
        }
    }
    for (id, (x, y, keys)) in want {
        if had.get(id) == Some(&(*x, *y, keys.clone())) {
            continue;
        }
        if sent.insert(*id) {
            for body in images.data.get(id).into_iter().flatten() {
                out.push_str(&format!("\x1b_G{body}\x1b\\"));
            }
        }
        let keys = if keys.is_empty() { String::new() } else { format!(",{keys}") };
        out.push_str(&format!("\x1b7\x1b[{};{}H\x1b_Ga=p,i={id},C=1,q=2{keys}\x1b\\\x1b8", y + 1, x + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn an_image_is_sent_once_and_follows_its_pane() {
        let mut im = Images::default();
        // a=T in two chunks, the pane's own id 7, quiet off: the answer comes at the end
        assert_eq!(im.command("p1", "a=T,f=100,i=7,c=4,r=2,m=1;AAAA", 10, 3, true), None);
        assert_eq!(im.command("p1", "m=0;BBBB", 10, 3, true).as_deref(), Some("\x1b_Gi=7;OK\x1b\\"));
        assert_eq!(im.data[&1], vec!["a=t,i=1,q=2,f=100,m=1;AAAA".to_string(), "m=0;BBBB".to_string()]);
        // the pane shows lines 5.. at (2, 1): line 10 is row 5
        let want = visible(&im, &[("p1".into(), 2, 1, 40, 20, 5)]);
        assert_eq!(want[&1], (5, 6, "c=4,r=2".into()));
        let mut sent = HashSet::new();
        let first = changes(&im, &HashMap::new(), &want, &mut sent);
        assert!(first.starts_with("\x1b_Ga=t,i=1,q=2,f=100,m=1;AAAA\x1b\\\x1b_Gm=0;BBBB\x1b\\"), "{first:?}");
        assert!(first.ends_with("\x1b7\x1b[7;6H\x1b_Ga=p,i=1,C=1,q=2,c=4,r=2\x1b\\\x1b8"), "{first:?}");
        // scrolled by 3: deleted and placed again, the data not sent again
        let moved = visible(&im, &[("p1".into(), 2, 1, 40, 20, 8)]);
        let again = changes(&im, &want, &moved, &mut sent);
        assert!(again.starts_with("\x1b_Ga=d,d=i,i=1,q=2\x1b\\") && !again.contains("AAAA"), "{again:?}");
        // scrolled out of the pane: only the delete
        let gone = visible(&im, &[("p1".into(), 2, 1, 40, 20, 11)]);
        assert!(gone.is_empty());
        assert_eq!(changes(&im, &moved, &gone, &mut sent), "\x1b_Ga=d,d=i,i=1,q=2\x1b\\");
    }

    #[test]
    fn placing_again_deleting_and_asking() {
        let mut im = Images::default();
        im.command("p", "a=t,i=3,q=2,f=100;AAAA", 0, 0, true);
        assert!(im.places.get("p").is_none_or(|l| l.is_empty()), "a=t only sends");
        im.command("p", "a=p,i=3,q=2", 4, 2, true);
        assert_eq!(im.places["p"].len(), 1);
        im.command("p", "a=d,d=i,i=3,q=2", 0, 0, true);
        assert!(im.places["p"].is_empty());
        // a query: as true as the terminal outside
        assert_eq!(im.command("p", "a=q,i=9,f=100;AAAA", 0, 0, false).as_deref(), Some("\x1b_Gi=9;ENOTSUPPORTED:no graphics in this terminal\x1b\\"));
        assert_eq!(im.command("p", "a=q,i=9,f=100;AAAA", 0, 0, true).as_deref(), Some("\x1b_Gi=9;OK\x1b\\"));
        im.pane_gone("p");
        assert!(im.data.is_empty());
    }
}
