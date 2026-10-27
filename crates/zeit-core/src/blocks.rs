//! Each pane's commands as blocks (cmd:N, the number the prompt shows): the shell's marks and
//! command lines with the lines they are on, so `zeit inspect` and `zeit find` can say what a
//! line belongs to.

/// One command: where its prompt is, where its output starts and ends (lines as Pane::abs_line
/// counts them), how it ended.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Block {
    pub id: u32,
    pub cmd: String,
    pub cwd: String,
    pub prompt: Option<i64>,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub exit: Option<i32>,
    /// when it started (ms since the epoch)
    pub t: u64,
}

/// The part of a block a line is in.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Part {
    Prompt,
    Output,
}

const KEEP: usize = 1000;

#[derive(Default)]
pub struct Blocks {
    list: Vec<Block>,
    /// the prompt now showing (its line), and the number the shell gave the next command
    prompt: Option<i64>,
    next: Option<u32>,
}

impl Blocks {
    /// A shell mark: A a prompt, C the output starts, D it ended (with its exit code).
    pub fn mark(&mut self, kind: char, exit: Option<i32>, line: i64) {
        match kind {
            'A' => self.prompt = Some(line),
            'C' => {
                if let Some(b) = self.list.last_mut().filter(|b| b.start.is_none()) {
                    b.start = Some(line);
                }
            }
            'D' => {
                if let Some(b) = self.list.last_mut().filter(|b| b.end.is_none()) {
                    b.end = Some(line);
                    b.exit = exit;
                }
            }
            _ => {}
        }
    }

    pub fn cmd_id(&mut self, id: u32) {
        self.next = Some(id);
    }

    /// The command line the shell is about to run: a new block (numbered by the shell, else one
    /// more than the last).
    pub fn cmd(&mut self, text: &str, cwd: &str, t: u64) {
        let id = self.next.take().unwrap_or_else(|| self.list.last().map_or(1, |b| b.id + 1));
        self.list.push(Block { id, cmd: text.to_string(), cwd: cwd.to_string(), prompt: self.prompt.take(), start: None, end: None, exit: None, t });
        if self.list.len() > KEEP {
            self.list.remove(0);
        }
    }

    /// The block a line is in, and which part of it.
    pub fn at(&self, line: i64) -> Option<(&Block, Part)> {
        self.list.iter().rev().find_map(|b| {
            let start = b.start?;
            if line >= start && b.end.is_none_or(|e| line < e.max(start + 1)) {
                Some((b, Part::Output))
            } else if b.prompt.is_some_and(|p| line >= p && line < start) {
                Some((b, Part::Prompt))
            } else {
                None
            }
        })
    }

    pub fn get(&self, id: u32) -> Option<&Block> {
        self.list.iter().rev().find(|b| b.id == id)
    }

    pub fn list(&self) -> &[Block] {
        &self.list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_belong_to_their_command() {
        let mut b = Blocks::default();
        b.mark('A', None, 0);
        b.cmd_id(7);
        b.cmd("ls", "/tmp", 1);
        b.mark('C', None, 2);
        b.mark('D', Some(0), 5);
        b.mark('A', None, 5);
        b.cmd("false", "/tmp", 2);
        b.mark('C', None, 7);
        b.mark('D', Some(1), 7);
        assert_eq!(b.at(1).map(|(b, p)| (b.id, p)), Some((7, Part::Prompt)));
        assert_eq!(b.at(3).map(|(b, p)| (b.id, p)), Some((7, Part::Output)));
        assert_eq!(b.at(6).map(|(b, p)| (b.id, p)), Some((8, Part::Prompt)), "the next one numbered after it");
        assert_eq!(b.get(8).and_then(|b| b.exit), Some(1));
    }
}
