//! A window's panes as a tree of splits, placed in cells.

use crate::keys::Dir;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Layout {
    Leaf(String),
    Split {
        /// true: a | b side by side; false: a above b
        side_by_side: bool,
        /// a's share of the space (0..1)
        ratio: f32,
        a: Box<Layout>,
        b: Box<Layout>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// A border cell between two panes: where, and whether it runs up-down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Border {
    pub x: u16,
    pub y: u16,
    pub len: u16,
    pub vertical: bool,
}

impl Layout {
    pub fn ids(&self) -> Vec<String> {
        match self {
            Layout::Leaf(id) => vec![id.clone()],
            Layout::Split { a, b, .. } => {
                let mut v = a.ids();
                v.extend(b.ids());
                v
            }
        }
    }

    /// Every pane's rectangle, and the borders between them.
    pub fn place(&self, area: Rect) -> (Vec<(String, Rect)>, Vec<Border>) {
        let mut panes = Vec::new();
        let mut borders = Vec::new();
        self.place_in(area, &mut panes, &mut borders);
        (panes, borders)
    }

    fn split_rect(side_by_side: bool, ratio: f32, r: Rect) -> (Rect, Rect, Border) {
        if side_by_side {
            let room = r.w.saturating_sub(1);
            let wa = ((room as f32 * ratio).round() as u16).clamp(1.min(room), room.saturating_sub(1).max(1));
            let a = Rect { w: wa, ..r };
            let b = Rect { x: r.x + wa + 1, w: room.saturating_sub(wa), ..r };
            (a, b, Border { x: r.x + wa, y: r.y, len: r.h, vertical: true })
        } else {
            let room = r.h.saturating_sub(1);
            let ha = ((room as f32 * ratio).round() as u16).clamp(1.min(room), room.saturating_sub(1).max(1));
            let a = Rect { h: ha, ..r };
            let b = Rect { y: r.y + ha + 1, h: room.saturating_sub(ha), ..r };
            (a, b, Border { x: r.x, y: r.y + ha, len: r.w, vertical: false })
        }
    }

    fn place_in(&self, r: Rect, panes: &mut Vec<(String, Rect)>, borders: &mut Vec<Border>) {
        match self {
            Layout::Leaf(id) => panes.push((id.clone(), r)),
            Layout::Split { side_by_side, ratio, a, b } => {
                let (ra, rb, border) = Self::split_rect(*side_by_side, *ratio, r);
                borders.push(border);
                a.place_in(ra, panes, borders);
                b.place_in(rb, panes, borders);
            }
        }
    }

    /// Splits pane `at`: the new pane goes right of it or below it.
    pub fn split(&mut self, at: &str, new: &str, side_by_side: bool) -> bool {
        match self {
            Layout::Leaf(id) if id == at => {
                *self = Layout::Split { side_by_side, ratio: 0.5, a: Box::new(Layout::Leaf(at.into())), b: Box::new(Layout::Leaf(new.into())) };
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { a, b, .. } => a.split(at, new, side_by_side) || b.split(at, new, side_by_side),
        }
    }

    /// `split-window -f`: the new pane takes a full side of the window.
    pub fn split_full(&mut self, new: &str, side_by_side: bool, ratio: f32) {
        let old = std::mem::replace(self, Layout::Leaf(String::new()));
        *self = Layout::Split { side_by_side, ratio, a: Box::new(old), b: Box::new(Layout::Leaf(new.into())) };
    }

    /// Sets the share of the split that has `id` as its second half (`-l` / `-p`).
    pub fn set_ratio_of(&mut self, id: &str, ratio: f32) {
        if let Layout::Split { ratio: r, a, b, .. } = self {
            if matches!(&**b, Layout::Leaf(x) if x == id) {
                *r = ratio.clamp(0.05, 0.95);
            } else {
                a.set_ratio_of(id, ratio);
                b.set_ratio_of(id, ratio);
            }
        }
    }

    /// Two panes trade places.
    pub fn swap(&mut self, x: &str, y: &str) {
        match self {
            Layout::Leaf(id) if id == x => *id = y.to_string(),
            Layout::Leaf(id) if id == y => *id = x.to_string(),
            Layout::Leaf(_) => {}
            Layout::Split { a, b, .. } => {
                a.swap(x, y);
                b.swap(x, y);
            }
        }
    }

    /// The tree without pane `id` (its sibling takes the space); None when it was the last.
    pub fn remove(self, id: &str) -> Option<Layout> {
        match self {
            Layout::Leaf(x) if x == id => None,
            Layout::Leaf(_) => Some(self),
            Layout::Split { side_by_side, ratio, a, b } => match (a.remove(id), b.remove(id)) {
                (Some(a), Some(b)) => Some(Layout::Split { side_by_side, ratio, a: Box::new(a), b: Box::new(b) }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Moves the nearest divider across `dir` by `cells` (Right/Down push it on).
    pub fn resize(&mut self, id: &str, dir: Dir, cells: u16, area: Rect) -> bool {
        let horizontal = matches!(dir, Dir::Left | Dir::Right);
        let sign = if matches!(dir, Dir::Right | Dir::Down) { 1.0 } else { -1.0 };
        self.resize_in(id, horizontal, sign * cells as f32, area)
    }

    fn resize_in(&mut self, id: &str, horizontal: bool, delta: f32, r: Rect) -> bool {
        let Layout::Split { side_by_side, ratio, a, b } = self else { return false };
        let (ra, rb, _) = Self::split_rect(*side_by_side, *ratio, r);
        let in_a = a.ids().iter().any(|x| x == id);
        if !in_a && !b.ids().iter().any(|x| x == id) {
            return false;
        }
        if if in_a { a.resize_in(id, horizontal, delta, ra) } else { b.resize_in(id, horizontal, delta, rb) } {
            return true;
        }
        if *side_by_side == horizontal {
            let size = if horizontal { r.w } else { r.h } as f32;
            if size > 2.0 {
                *ratio = (*ratio + delta / size).clamp(0.05, 0.95);
            }
            return true;
        }
        false
    }

    /// The pane next to `from` in a direction.
    pub fn neighbour(&self, from: &str, dir: Dir, area: Rect) -> Option<String> {
        let (all, _) = self.place(area);
        let f = all.iter().find(|(id, _)| id == from)?.1;
        let centre = |r: &Rect| (r.x as i32 * 2 + r.w as i32, r.y as i32 * 2 + r.h as i32);
        let (fx, fy) = centre(&f);
        all.iter()
            .filter(|(id, _)| id != from)
            .filter(|(_, r)| match dir {
                Dir::Left => r.x + r.w <= f.x && r.y < f.y + f.h && r.y + r.h > f.y,
                Dir::Right => r.x >= f.x + f.w && r.y < f.y + f.h && r.y + r.h > f.y,
                Dir::Up => r.y + r.h <= f.y && r.x < f.x + f.w && r.x + r.w > f.x,
                Dir::Down => r.y >= f.y + f.h && r.x < f.x + f.w && r.x + r.w > f.x,
            })
            .min_by_key(|(_, r)| {
                let (x, y) = centre(r);
                (x - fx).abs() + (y - fy).abs()
            })
            .map(|(id, _)| id.clone())
    }

    pub fn at(&self, x: u16, y: u16, area: Rect) -> Option<String> {
        self.place(area).0.into_iter().find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h).map(|(id, _)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0, y: 0, w: 81, h: 25 };

    fn three() -> Layout {
        let mut l = Layout::Leaf("1".into());
        l.split("1", "2", true);
        l.split("2", "3", false);
        l
    }

    #[test]
    fn panes_and_borders() {
        let (p, b) = three().place(AREA);
        assert_eq!(p[0], ("1".into(), Rect { x: 0, y: 0, w: 40, h: 25 }));
        assert_eq!(p[1], ("2".into(), Rect { x: 41, y: 0, w: 40, h: 12 }));
        assert_eq!(p[2], ("3".into(), Rect { x: 41, y: 13, w: 40, h: 12 }));
        assert_eq!(b, vec![Border { x: 40, y: 0, len: 25, vertical: true }, Border { x: 41, y: 12, len: 40, vertical: false }]);
    }

    #[test]
    fn moving_and_closing() {
        let l = three();
        assert_eq!(l.neighbour("1", Dir::Right, AREA).as_deref(), Some("2"));
        assert_eq!(l.neighbour("3", Dir::Up, AREA).as_deref(), Some("2"));
        assert_eq!(l.at(60, 20, AREA).as_deref(), Some("3"));
        let l = l.remove("2").unwrap();
        assert_eq!(l.place(AREA).0[1].1, Rect { x: 41, y: 0, w: 40, h: 25 });
    }

    #[test]
    fn resizing() {
        let mut l = three();
        assert!(l.resize("1", Dir::Right, 8, AREA));
        assert_eq!(l.place(AREA).0[0].1.w, 48);
    }
}

/// tmux's preset layouts, for panes in this order (the first is the main one).
pub const PRESETS: &[&str] = &["even-horizontal", "even-vertical", "main-vertical", "main-horizontal", "tiled"];

impl Layout {
    /// Equal shares, side by side (true) or stacked.
    fn even(ids: &[String], side_by_side: bool) -> Layout {
        match ids {
            [] => Layout::Leaf(String::new()),
            [one] => Layout::Leaf(one.clone()),
            [first, rest @ ..] => Layout::Split { side_by_side, ratio: 1.0 / ids.len() as f32, a: Box::new(Layout::Leaf(first.clone())), b: Box::new(Layout::even(rest, side_by_side)) },
        }
    }

    /// A preset layout over `ids` (tmux's names); None for an unknown name.
    pub fn preset(name: &str, ids: &[String]) -> Option<Layout> {
        if ids.len() < 2 {
            return ids.first().map(|i| Layout::Leaf(i.clone()));
        }
        Some(match name {
            "even-horizontal" => Layout::even(ids, true),
            "even-vertical" => Layout::even(ids, false),
            "main-vertical" | "main-horizontal" => {
                let side = name == "main-vertical";
                Layout::Split { side_by_side: side, ratio: 0.6, a: Box::new(Layout::Leaf(ids[0].clone())), b: Box::new(Layout::even(&ids[1..], !side)) }
            }
            "tiled" => {
                let cols = (ids.len() as f32).sqrt().ceil() as usize;
                let rows: Vec<Layout> = ids.chunks(cols).map(|r| Layout::even(r, true)).collect();
                fn stack(rows: &[Layout]) -> Layout {
                    match rows {
                        [one] => one.clone(),
                        [first, rest @ ..] => Layout::Split { side_by_side: false, ratio: 1.0 / rows.len() as f32, a: Box::new(first.clone()), b: Box::new(stack(rest)) },
                        [] => Layout::Leaf(String::new()),
                    }
                }
                stack(&rows)
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod preset_tests {
    use super::*;

    #[test]
    fn presets_place_every_pane() {
        let ids: Vec<String> = (0..5).map(|i| format!("p{i}")).collect();
        let area = Rect { x: 0, y: 0, w: 100, h: 40 };
        for name in PRESETS {
            let l = Layout::preset(name, &ids).unwrap();
            assert_eq!(l.ids(), ids, "{name} keeps the order");
            let (placed, _) = l.place(area);
            assert!(placed.iter().all(|(_, r)| r.w > 0 && r.h > 0), "{name}: every pane has room");
        }
        // even-horizontal: equal widths (give or take the borders' rounding)
        let (placed, _) = Layout::preset("even-horizontal", &ids).unwrap().place(area);
        let ws: Vec<u16> = placed.iter().map(|(_, r)| r.w).collect();
        assert!(ws.iter().max().unwrap() - ws.iter().min().unwrap() <= 2, "{ws:?}");
    }
}
