//! A pane on the server's side: holder connection, emulator, screen.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape as ACursor, NamedColor, Processor};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use zeit_proto::{read_msg, write_msg, Cell, Color, CursorShape, HolderMsg, ToHolder, VERSION};

/// What the server hears from its panes.
#[derive(Debug)]
pub enum PaneEvent {
    Output(String),
    /// an OSC 7733 message (zash's UI protocol) for the terminal the pane is shown in
    Passthrough(String, Vec<u8>),
    /// a shell mark (OSC 133: A prompt, C output starts, D done) and the line it came at
    Mark { pane: String, kind: char, exit: Option<i32>, line: i64 },
    /// the shell printed a job's line (OSC 7733;job;ID) here
    JobLine { pane: String, job: String, line: i64 },
    Exited(String, i32),
    /// the holder vanished without saying the shell ended
    Lost(String),
}

/// Replies the program asked for (cursor position, device attributes, colours)
/// go straight back to the shell; the title is kept.
#[derive(Clone)]
pub struct Replies {
    writer: Arc<Mutex<UnixStream>>,
    title: Arc<Mutex<String>>,
}

impl EventListener for Replies {
    fn send_event(&self, ev: Event) {
        match ev {
            Event::PtyWrite(s) => {
                if let Ok(mut w) = self.writer.lock() {
                    let _ = write_msg(&mut *w, &ToHolder::Input(s.into_bytes()));
                }
            }
            Event::Title(t) => *self.title.lock().unwrap() = t,
            Event::ResetTitle => self.title.lock().unwrap().clear(),
            _ => {}
        }
    }
}

pub struct Pane {
    pub id: String,
    pub term: Arc<FairMutex<Term<Replies>>>,
    writer: Arc<Mutex<UnixStream>>,
    pub title: Arc<Mutex<String>>,
    pub cols: u16,
    pub rows: u16,
    /// the shell's pid (its current folder is read from /proc)
    pub pid: u32,
    /// the folder it started in
    pub start_dir: PathBuf,
}

impl Pane {
    /// Connects to a running holder (a new pane, or one that outlived a server).
    pub fn connect(id: &str, cols: u16, rows: u16, start_dir: PathBuf, history: usize, events: Sender<PaneEvent>) -> std::io::Result<Pane> {
        let mut stream = UnixStream::connect(crate::paths::pane_socket(id))?;
        let pid = match read_msg::<_, HolderMsg>(&mut stream)? {
            HolderMsg::Hello { pid, .. } => pid,
            other => return Err(std::io::Error::other(format!("holder said {other:?}"))),
        };
        write_msg(&mut stream, &ToHolder::Hello { version: VERSION })?;
        let reader = stream.try_clone()?;
        let writer = Arc::new(Mutex::new(stream));
        let title = Arc::new(Mutex::new(String::new()));
        let replies = Replies { writer: writer.clone(), title: title.clone() };
        let size = term::test::TermSize::new(cols as usize, rows as usize);
        let config = term::Config { scrolling_history: history, ..Default::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, replies)));
        let (t, pid_s, id_s) = (term.clone(), id.to_string(), id.to_string());
        std::thread::Builder::new().name(format!("zeit pane {pid_s}")).spawn(move || read_loop(reader, t, id_s, events))?;
        let p = Pane { id: id.into(), term, writer, title, cols, rows, pid, start_dir };
        p.resize(cols, rows);
        Ok(p)
    }

    pub fn input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = write_msg(&mut *w, &ToHolder::Input(bytes.to_vec()));
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        self.term.lock().resize(term::test::TermSize::new(cols.max(1) as usize, rows.max(1) as usize));
        if let Ok(mut w) = self.writer.lock() {
            let _ = write_msg(&mut *w, &ToHolder::Resize { cols: cols.max(1), rows: rows.max(1) });
        }
    }

    pub fn set_size(&mut self, cols: u16, rows: u16) {
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.resize(cols, rows);
        }
    }

    pub fn hang_up(&self) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = write_msg(&mut *w, &ToHolder::HangUp);
        }
    }

    /// The shell's current folder (for splits and windows "in the same place").
    pub fn cwd(&self) -> PathBuf {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid)).unwrap_or_else(|_| self.start_dir.clone())
    }

    /// What the pane shows: rows of cells, and the cursor.
    pub fn screen(&self) -> (Vec<Vec<Cell>>, Option<(u16, u16)>, CursorShape) {
        let t = self.term.lock();
        let rows = t.screen_lines();
        let cols = t.columns();
        let content = t.renderable_content();
        let offset = content.display_offset as i32;
        let selection = content.selection;
        let mut out: Vec<Vec<Cell>> = vec![vec![Cell::default(); cols]; rows];
        for ic in content.display_iter {
            let r = ic.point.line.0 + offset;
            if r < 0 || r as usize >= rows || ic.point.column.0 >= cols {
                continue;
            }
            let c = ic.cell;
            let f = c.flags;
            let mut flags = 0u8;
            if f.contains(Flags::BOLD) { flags |= zeit_proto::BOLD; }
            if f.contains(Flags::ITALIC) { flags |= zeit_proto::ITALIC; }
            if f.intersects(Flags::ALL_UNDERLINES) { flags |= zeit_proto::UNDERLINE; }
            if f.contains(Flags::INVERSE) { flags |= zeit_proto::REVERSE; }
            if f.contains(Flags::DIM) { flags |= zeit_proto::DIM; }
            if f.contains(Flags::STRIKEOUT) { flags |= zeit_proto::STRIKE; }
            if f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) { flags |= zeit_proto::SPACER; }
            if selection.is_some_and(|s| s.contains(ic.point)) { flags ^= zeit_proto::REVERSE; }
            let mut text = String::new();
            text.push(if f.contains(Flags::HIDDEN) || c.c.is_control() { ' ' } else { c.c });
            if let Some(z) = c.zerowidth() {
                text.extend(z.iter());
            }
            out[r as usize][ic.point.column.0] = Cell { text, fg: color(c.fg), bg: color(c.bg), flags };
        }
        let cur = content.cursor;
        let shape = match cur.shape {
            ACursor::Hidden => CursorShape::Hidden,
            ACursor::Beam => CursorShape::Beam,
            ACursor::Underline => CursorShape::Underline,
            _ => CursorShape::Block,
        };
        let Point { line: Line(l), column: Column(c) } = cur.point;
        let r = l + offset;
        let cursor = (r >= 0 && (r as usize) < rows).then_some((c as u16, r as u16));
        (out, cursor, shape)
    }

    /// What the program asked of the terminal (zeit_proto::MODE_* bits).
    pub fn modes(&self) -> u8 {
        use alacritty_terminal::term::TermMode;
        let m = *self.term.lock().mode();
        let mut out = 0;
        if m.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) { out |= zeit_proto::MODE_MOUSE; }
        if m.contains(TermMode::SGR_MOUSE) { out |= zeit_proto::MODE_SGR_MOUSE; }
        if m.contains(TermMode::BRACKETED_PASTE) { out |= zeit_proto::MODE_PASTE; }
        if m.contains(TermMode::APP_CURSOR) { out |= zeit_proto::MODE_APP_CURSOR; }
        if m.contains(TermMode::FOCUS_IN_OUT) { out |= zeit_proto::MODE_FOCUS; }
        out
    }

    pub fn alt_screen(&self) -> bool {
        self.term.lock().mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    /// Scrolls the view back (+) or forward (-); 0 returns to the live screen.
    pub fn scroll(&self, lines: i32) {
        use alacritty_terminal::grid::Scroll;
        let mut t = self.term.lock();
        t.scroll_display(if lines == 0 { Scroll::Bottom } else { Scroll::Delta(lines) });
    }

    /// How far the view is scrolled back (0: live).
    pub fn scrolled(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn clear_history(&self) {
        self.term.lock().grid_mut().clear_history();
    }

    /// The line the cursor is on, counted from the oldest line kept.
    pub fn abs_line(&self) -> i64 {
        let t = self.term.lock();
        t.grid().history_size() as i64 + t.grid().cursor.point.line.0 as i64
    }

    /// Rows `from..to` (counted as abs_line counts) as text.
    pub fn rows_text(&self, from: i64, to: i64) -> Vec<String> {
        let t = self.term.lock();
        let hist = t.grid().history_size() as i64;
        let rows = t.screen_lines() as i64;
        let cols = t.columns();
        (from.max(0)..to.min(hist + rows))
            .map(|a| {
                let row = &t.grid()[Line((a - hist) as i32)];
                (0..cols).map(|x| row[Column(x)].c).collect::<String>().trim_end().to_string()
            })
            .collect()
    }

    /// Writes over row `abs` (a job's line): (text, colour, bold) pieces, the rest cleared.
    pub fn write_row(&self, abs: i64, pieces: &[(usize, String, Option<NamedColor>, bool)]) -> bool {
        use alacritty_terminal::term::cell::Cell as ACell;
        let mut t = self.term.lock();
        let hist = t.grid().history_size() as i64;
        let rows = t.screen_lines() as i64;
        if abs < 0 || abs >= hist + rows {
            return false;
        }
        let cols = t.columns();
        let row = &mut t.grid_mut()[Line((abs - hist) as i32)];
        for x in 0..cols {
            row[Column(x)] = ACell::default();
        }
        for (at, text, color, bold) in pieces {
            for (i, ch) in text.chars().enumerate() {
                if at + i >= cols {
                    break;
                }
                let c = &mut row[Column(at + i)];
                c.c = ch;
                if let Some(n) = color {
                    c.fg = AColor::Named(*n);
                }
                if *bold {
                    c.flags.insert(Flags::BOLD);
                }
            }
        }
        true
    }

    /// The screen as plain text (capture-pane).
    pub fn text(&self) -> String {
        let (rows, _, _) = self.screen();
        rows.iter().map(|r| r.iter().filter(|c| c.flags & zeit_proto::SPACER == 0).map(|c| c.text.as_str()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n").trim_end().to_string()
    }
}

fn color(c: AColor) -> Color {
    match c {
        AColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AColor::Indexed(i) => Color::Indexed(i),
        AColor::Named(n) => match n {
            NamedColor::Foreground | NamedColor::Background | NamedColor::BrightForeground | NamedColor::Cursor => Color::Default,
            NamedColor::DimForeground => Color::Default,
            n if (n as usize) < 16 => Color::Indexed(n as u8),
            n if (n as usize) >= NamedColor::DimBlack as usize && (n as usize) <= NamedColor::DimWhite as usize => Color::Indexed((n as usize - NamedColor::DimBlack as usize) as u8),
            _ => Color::Default,
        },
    }
}

fn read_loop(mut r: UnixStream, term: Arc<FairMutex<Term<Replies>>>, id: String, events: Sender<PaneEvent>) {
    let mut parser: Processor = Processor::new();
    let mut marks = crate::timeline::Scanner::default();
    loop {
        match read_msg::<_, HolderMsg>(&mut r) {
            Ok(HolderMsg::Output(bytes)) => {
                for m in marks.scan(&bytes) {
                    crate::timeline::record(&id, m);
                }
                for seq in passthrough(&bytes) {
                    let _ = events.send(PaneEvent::Passthrough(id.clone(), seq));
                }
                // marks and job lines: the bytes up to each go in first, so its line is known
                let mut last = 0;
                for (end, what) in places(&bytes) {
                    let mut t = term.lock();
                    parser.advance(&mut *t, &bytes[last..end]);
                    last = end;
                    let line = t.grid().history_size() as i64 + t.grid().cursor.point.line.0 as i64;
                    drop(t);
                    let _ = events.send(match what {
                        Place::Mark(kind, exit) => PaneEvent::Mark { pane: id.clone(), kind, exit, line },
                        Place::Job(job) => PaneEvent::JobLine { pane: id.clone(), job, line },
                    });
                }
                parser.advance(&mut *term.lock(), &bytes[last..]);
                if events.send(PaneEvent::Output(id.clone())).is_err() {
                    return;
                }
            }
            Ok(HolderMsg::Exited(code)) => {
                crate::timeline::record(&id, crate::timeline::Mark { kind: 'X', exit: Some(code) });
                let _ = events.send(PaneEvent::Exited(id, code));
                return;
            }
            Ok(HolderMsg::Hello { .. }) => {}
            Err(_) => {
                let _ = events.send(PaneEvent::Lost(id));
                return;
            }
        }
    }
}

#[allow(dead_code)]
fn _dims<T: Dimensions>(_: &T) {}

/// zash's UI messages (OSC 7733 …, ended by BEL) in a chunk of output, whole.
fn passthrough(chunk: &[u8]) -> Vec<Vec<u8>> {
    const HEAD: &[u8] = b"\x1b]7733;";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = chunk[i..].windows(HEAD.len()).position(|w| w == HEAD) {
        let start = i + p;
        let Some(end) = chunk[start..].iter().position(|&b| b == 0x07).map(|e| start + e + 1) else { break };
        // popups only: Neovim mode (7733;nvim) belongs to the pane, not to the outer window
        if !chunk[start..end].starts_with(b"\x1b]7733;nvim;") && !chunk[start..end].starts_with(b"\x1b]7733;job;") {
            out.push(chunk[start..end].to_vec());
        }
        i = end;
    }
    out
}

enum Place {
    Mark(char, Option<i32>),
    Job(String),
}

/// Where in a chunk the shell marks (OSC 133) and job lines (OSC 7733;job;ID) end.
fn places(chunk: &[u8]) -> Vec<(usize, Place)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = chunk[i..].windows(2).position(|w| w == b"\x1b]") {
        let start = i + p + 2;
        let Some(len) = chunk[start..].iter().position(|&b| b == 0x07 || b == 0x1b) else { break };
        let body = String::from_utf8_lossy(&chunk[start..start + len]).to_string();
        let end = start + len + if chunk[start + len] == 0x1b { 2 } else { 1 };
        if let Some(rest) = body.strip_prefix("133;") {
            let mut parts = rest.split(';');
            if let Some(kind) = parts.next().and_then(|k| k.chars().next()) {
                let exit = if kind == 'D' { parts.next().and_then(|v| v.parse().ok()) } else { None };
                out.push((end.min(chunk.len()), Place::Mark(kind, exit)));
            }
        } else if let Some(job) = body.strip_prefix("7733;job;") {
            out.push((end.min(chunk.len()), Place::Job(job.to_string())));
        }
        i = end.min(chunk.len());
    }
    out
}
