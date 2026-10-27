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
    /// the pane is a web page (zeit web): OSC 7733;webpane;url=…
    WebPane { pane: String, url: String },
    /// keys the pane's program claims for itself (OSC 7733;keys;…)
    Keys { pane: String, msg: String },
    /// the command line the shell is about to run (OSC 7733;cmd;TEXT)
    Cmd { pane: String, text: String },
    /// the number the shell gives the command it is about to run (OSC 7733;cmdid;N)
    CmdId { pane: String, id: u32 },
    /// the shell printed a job's line (OSC 7733;job;ID) here
    JobLine { pane: String, job: String, line: i64 },
    /// a Kitty graphics command (APC G, its body) with the cursor where it came (server/images.rs)
    Image { pane: String, body: String, line: i64, col: usize },
    /// the program's floats (zett's popups) in the pane's cells, for zash to raise
    Layers { pane: String, list: String },
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
        // kitty_keyboard: a program may ask for the Kitty keyboard protocol (the client asks zash)
        let config = term::Config { scrolling_history: history, kitty_keyboard: true, ..Default::default() };
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
        if m.contains(TermMode::DISAMBIGUATE_ESC_CODES) { out |= zeit_proto::MODE_KITTY_KEYS; }
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


    /// One cell, as `zeit inspect` tells it (Neovim's :Inspect for a terminal letter): its text,
    /// colours, style and link. `abs` counted as abs_line counts.
    pub fn cell_at(&self, abs: i64, col: usize) -> Option<CellInfo> {
        let t = self.term.lock();
        let hist = t.grid().history_size() as i64;
        if abs < 0 || abs >= hist + t.screen_lines() as i64 || col >= t.columns() {
            return None;
        }
        let c = &t.grid()[Line((abs - hist) as i32)][Column(col)];
        let name = |c: AColor| match c {
            AColor::Named(n) => format!("{n:?}").to_lowercase(),
            AColor::Spec(rgb) => format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b),
            AColor::Indexed(i) => format!("palette {i}"),
        };
        let f = c.flags;
        let flags = [(Flags::BOLD, "bold"), (Flags::ITALIC, "italic"), (Flags::UNDERLINE, "underline"), (Flags::DOUBLE_UNDERLINE, "double underline"), (Flags::UNDERCURL, "undercurl"), (Flags::DIM, "dim"), (Flags::INVERSE, "reverse"), (Flags::STRIKEOUT, "strike"), (Flags::HIDDEN, "hidden"), (Flags::WIDE_CHAR, "wide")]
            .into_iter()
            .filter(|(b, _)| f.contains(*b))
            .map(|(_, n)| n)
            .collect();
        let mut text = c.c.to_string();
        if let Some(z) = c.zerowidth() {
            text.extend(z.iter());
        }
        Some(CellInfo { text, fg: name(c.fg), bg: name(c.bg), flags, link: c.hyperlink().map(|h| h.uri().to_string()) })
    }

    /// The line the view's top row shows (scrolled back or live), counted as abs_line counts.
    pub fn view_top(&self) -> i64 {
        let t = self.term.lock();
        t.grid().history_size() as i64 - t.grid().display_offset() as i64
    }

    /// The cursor: its line (as abs_line counts) and column.
    pub fn cursor_at(&self) -> (i64, usize) {
        let t = self.term.lock();
        (t.grid().history_size() as i64 + t.grid().cursor.point.line.0 as i64, t.grid().cursor.point.column.0)
    }

    /// Lines `from..to` as the program wrote them: rows the terminal wrapped joined into one
    /// (a long path or link is one thing), each with the row it starts on. A column `c` of a
    /// joined line is on row `start + c / cols`, column `c % cols`.
    pub fn logical_rows(&self, from: i64, to: i64) -> Vec<(i64, String)> {
        let t = self.term.lock();
        let hist = t.grid().history_size() as i64;
        let rows = t.screen_lines() as i64;
        let cols = t.columns();
        let mut out: Vec<(i64, String)> = Vec::new();
        let mut cur: Option<(i64, String)> = None;
        for a in from.max(0)..to.min(hist + rows) {
            let row = &t.grid()[Line((a - hist) as i32)];
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let text: String = (0..cols).map(|x| row[Column(x)].c).collect();
            let (start, mut acc) = cur.take().unwrap_or((a, String::new()));
            acc.push_str(if wrapped { &text } else { text.trim_end() });
            if wrapped {
                cur = Some((start, acc));
            } else {
                out.push((start, acc));
            }
        }
        if let Some(c) = cur {
            out.push(c);
        }
        out
    }

    pub fn cols(&self) -> usize {
        self.term.lock().columns()
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

    /// zash effects on the screen: (row, first column, end column, the cells' link), a run per
    /// row; the server sends them on with the frame (see fx_body)
    pub fn fx_runs(&self) -> Vec<(u16, u16, u16, String)> {
        let t = self.term.lock();
        let rows = t.screen_lines();
        let content = t.renderable_content();
        let offset = content.display_offset as i32;
        let mut out: Vec<(u16, u16, u16, String)> = Vec::new();
        for ic in content.display_iter {
            let Some(link) = ic.cell.hyperlink() else { continue };
            if !link.uri().starts_with(FX_SCHEME) {
                continue;
            }
            let r = ic.point.line.0 + offset;
            if r < 0 || r as usize >= rows {
                continue;
            }
            let (r, c) = (r as u16, ic.point.column.0 as u16);
            match out.last_mut() {
                Some(run) if run.0 == r && run.2 == c && run.3 == link.uri() => run.2 = c + 1,
                _ => out.push((r, c, c + 1, link.uri().to_string())),
            }
        }
        out
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
    // a Kitty image cut off at a read's end: the rest comes in the next (apcs)
    let mut apc_carry: Vec<u8> = Vec::new();
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
                let mut found = places(&bytes);
                found.extend(apcs(&mut apc_carry, &bytes));
                found.sort_by_key(|(e, _)| *e);
                for (end, what) in found {
                    let mut t = term.lock();
                    parser.advance(&mut *t, &bytes[last..end]);
                    last = end;
                    // an effect rides the cells' link, as in zash (given in order, as OSC 8: inside a
                    // synchronized update the parser holds the text back); fx_runs reads it out again
                    if let Place::Fx(body) = &what {
                        let osc = if body.is_empty() || body == "end" { "\x1b]8;;\x1b\\".to_string() } else { format!("\x1b]8;;{FX_SCHEME}{};{}\x1b\\", now_ms(), body.replace(['\x07', '\x1b'], "")) };
                        parser.advance(&mut *t, osc.as_bytes());
                        continue;
                    }
                    let line = t.grid().history_size() as i64 + t.grid().cursor.point.line.0 as i64;
                    let col = t.grid().cursor.point.column.0;
                    drop(t);
                    let _ = events.send(match what {
                        Place::Mark(kind, exit) => PaneEvent::Mark { pane: id.clone(), kind, exit, line },
                        Place::Job(job) => PaneEvent::JobLine { pane: id.clone(), job, line },
                        Place::Cmd(text) => PaneEvent::Cmd { pane: id.clone(), text },
                        Place::CmdId(n) => PaneEvent::CmdId { pane: id.clone(), id: n },
                        Place::Keys(msg) => PaneEvent::Keys { pane: id.clone(), msg },
                        Place::WebPane(url) => PaneEvent::WebPane { pane: id.clone(), url },
                        Place::Image(body) => PaneEvent::Image { pane: id.clone(), body, line, col },
                        Place::Layers(list) => PaneEvent::Layers { pane: id.clone(), list },
                        Place::Fx(_) => continue,
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
        if !chunk[start..end].starts_with(b"\x1b]7733;nvim;") && !chunk[start..end].starts_with(b"\x1b]7733;job;") && !chunk[start..end].starts_with(b"\x1b]7733;cmd;") && !chunk[start..end].starts_with(b"\x1b]7733;cmdid;") && !chunk[start..end].starts_with(b"\x1b]7733;keys;") && !chunk[start..end].starts_with(b"\x1b]7733;webpane;") && !chunk[start..end].starts_with(b"\x1b]7733;fx") && !chunk[start..end].starts_with(b"\x1b]7733;layers") {
            out.push(chunk[start..end].to_vec());
        }
        i = end;
    }
    out
}

enum Place {
    Mark(char, Option<i32>),
    WebPane(String),
    Keys(String),
    Job(String),
    Cmd(String),
    CmdId(u32),
    /// zash's effect on the text after it (fx;NAME;…), or its end (fx;end)
    Fx(String),
    /// the program's floats (OSC 7733;layers;x,y,w,h;…), in the pane's cells
    Layers(String),
    /// a Kitty graphics command (APC G): its body
    Image(String),
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
        } else if let Some(u) = body.strip_prefix("7733;webpane;url=") {
            out.push((end.min(chunk.len()), Place::WebPane(u.to_string())));
        } else if let Some(k) = body.strip_prefix("7733;keys;") {
            out.push((end.min(chunk.len()), Place::Keys(k.to_string())));
        } else if let Some(n) = body.strip_prefix("7733;cmdid;") {
            if let Ok(n) = n.trim().parse() {
                out.push((end.min(chunk.len()), Place::CmdId(n)));
            }
        } else if let Some(cmd) = body.strip_prefix("7733;cmd;") {
            out.push((end.min(chunk.len()), Place::Cmd(cmd.to_string())));
        } else if let Some(l) = body.strip_prefix("7733;layers;").or_else(|| (body == "7733;layers").then_some("")) {
            out.push((end.min(chunk.len()), Place::Layers(l.to_string())));
        } else if let Some(fx) = body.strip_prefix("7733;fx") {
            out.push((end.min(chunk.len()), Place::Fx(fx.trim_start_matches(';').to_string())));
        }
        i = end.min(chunk.len());
    }
    out
}

/// the link an effect rides on a pane's cells: zeitfx:START_MS;BODY
pub const FX_SCHEME: &str = "zeitfx:";

/// Milliseconds on the clock effects' starts are kept in.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What zash gets for a run's link: its body, and since= how long ago it began, so a run sent
/// again with a redrawn row goes on instead of starting over.
pub fn fx_body(link: &str) -> Option<String> {
    let (start, body) = link.strip_prefix(FX_SCHEME)?.split_once(';')?;
    let since = now_ms().saturating_sub(start.parse().ok()?);
    Some(format!("{body};since={since}"))
}

/// A cell as `zeit inspect` tells it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CellInfo {
    pub text: String,
    pub fg: String,
    pub bg: String,
    pub flags: Vec<&'static str>,
    pub link: Option<String>,
}

/// Kitty graphics commands (APC G … ESC \) in a chunk: where each ends and its body. One cut off
/// at the chunk's end waits in `carry` for the rest (an image is many kilobytes).
fn apcs(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<(usize, Place)> {
    let mut out = Vec::new();
    let mut i = 0;
    if !carry.is_empty() {
        match chunk.windows(2).position(|w| w == b"\x1b\\") {
            Some(e) => {
                carry.extend_from_slice(&chunk[..e]);
                out.push((e + 2, Place::Image(String::from_utf8_lossy(&carry[3..]).to_string())));
                carry.clear();
                i = e + 2;
            }
            None => {
                // a runaway (no end in 64 MB): dropped
                carry.extend_from_slice(chunk);
                if carry.len() > 64 << 20 {
                    carry.clear();
                }
                return out;
            }
        }
    }
    while let Some(p) = chunk[i..].windows(3).position(|w| w == b"\x1b_G") {
        let start = i + p;
        match chunk[start..].windows(2).position(|w| w == b"\x1b\\") {
            Some(e) => {
                let end = start + e + 2;
                out.push((end, Place::Image(String::from_utf8_lossy(&chunk[start + 3..start + e]).to_string())));
                i = end;
            }
            None => {
                carry.extend_from_slice(&chunk[start..]);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod apc_tests {
    use super::*;

    #[test]
    fn images_whole_and_cut_across_reads() {
        let mut carry = Vec::new();
        let got = apcs(&mut carry, b"hi\x1b_Ga=T,f=100;AAAA\x1b\\there\x1b_Ga=t,i=2;BB");
        assert!(matches!(&got[..], [(21, Place::Image(b))] if b == "a=T,f=100;AAAA"));
        assert_eq!(carry, b"\x1b_Ga=t,i=2;BB");
        let got = apcs(&mut carry, b"CC\x1b\\after");
        assert!(matches!(&got[..], [(4, Place::Image(b))] if b == "a=t,i=2;BBCC"));
        assert!(carry.is_empty());
    }
}
