//! The protocol between server, clients and pane holders (versioned frames).

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Bumped on any change to a message below.
pub const VERSION: u32 = 1;

/// Largest frame accepted (a full screen of a huge terminal fits easily).
const MAX_FRAME: u32 = 64 << 20;

// ── client → server ──

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello { version: u32, client: String },
    /// Show a session: by name, or the place for a folder (made if missing).
    Attach { session: Option<String>, place: Option<String>, cols: u16, rows: u16 },
    /// Bytes for the focused pane (typed keys, pasted text, mouse reports).
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// A zeit command, as words: `["split-window", "-h"]`. Replies with `Reply`.
    Command(Vec<String>),
    Detach,
}

// ── server → client ──

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINE: u8 = 4;
pub const REVERSE: u8 = 8;
pub const DIM: u8 = 16;
pub const STRIKE: u8 = 32;
/// the right half of a wide character (nothing to print there)
pub const SPACER: u8 = 64;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { text: " ".into(), fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
    Hidden,
}

/// The rows that changed since the last frame (all of them after a resize).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub changed: Vec<(u16, Vec<Cell>)>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_shape: CursorShape,
    pub title: String,
    /// what the focused program asked of the terminal (MODE_* bits)
    pub modes: u8,
    /// zash effects on the changed rows: the terminal draws them over these cells (last, so a
    /// frame with them still reads in a client from before them)
    pub fx: Vec<FxRun>,
}

/// A run of cells under one zash effect (OSC 7733;fx): row, columns from..to, the escape's body.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FxRun {
    pub row: u16,
    pub from: u16,
    pub to: u16,
    pub body: String,
}

pub const MODE_MOUSE: u8 = 1;
pub const MODE_SGR_MOUSE: u8 = 2;
pub const MODE_PASTE: u8 = 4;
pub const MODE_APP_CURSOR: u8 = 8;
pub const MODE_FOCUS: u8 = 16;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ServerMsg {
    Hello { version: u32, server: String },
    Frame(Frame),
    Reply { ok: bool, text: String },
    /// The client should leave: detached, or its session ended.
    Bye { reason: String },
    /// escape sequences for the outer terminal, passed through from the focused pane
    /// (the zash UI protocol: OSC 7733 popups and the like)
    Passthrough(Vec<u8>),
}

// ── pane holder ⇄ server ──

/// A holder owns one pane's pty and shell, so the shell outlives the server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum HolderMsg {
    Hello { version: u32, pid: u32 },
    Output(Vec<u8>),
    /// the shell ended, with its exit status
    Exited(i32),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToHolder {
    Hello { version: u32 },
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// hang up on the shell (SIGHUP to its process group)
    HangUp,
}

// ── framing ──

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::other(format!("frame too large: {len} bytes")));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_from_before_fx_reads_frames_with_it() {
        // a running client reconnects after restart-server with its old code: Frame without fx
        #[derive(Deserialize)]
        struct OldFrame { cols: u16, _rows: u16, changed: Vec<(u16, Vec<Cell>)>, _cursor: Option<(u16, u16)>, _shape: CursorShape, title: String, _modes: u8 }
        #[derive(Deserialize)]
        enum OldMsg { _Reply { _ok: bool, _text: String }, Frame(OldFrame) }
        let row = vec![Cell { text: "a".into(), ..Cell::default() }];
        let f = Frame { cols: 3, rows: 1, changed: vec![(0, row)], cursor: None, cursor_shape: CursorShape::Block, title: "t".into(), modes: 0, fx: vec![FxRun { row: 0, from: 0, to: 1, body: "underline;since=5".into() }] };
        let mut buf = Vec::new();
        write_msg(&mut buf, &ServerMsg::Frame(f)).unwrap();
        let tag = |m: &ServerMsg| postcard::to_allocvec(m).unwrap()[0];
        assert_eq!(tag(&ServerMsg::Frame(Frame { cols: 0, rows: 0, changed: vec![], cursor: None, cursor_shape: CursorShape::Block, title: String::new(), modes: 0, fx: vec![] })), 1, "Frame is the second variant, as OldMsg has it");
        let Ok(OldMsg::Frame(old)) = read_msg::<_, OldMsg>(&mut &buf[..]) else { panic!("the old client could not read it") };
        assert_eq!((old.cols, old.title.as_str(), old.changed[0].1[0].text.as_str()), (3, "t", "a"));
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        let m = ClientMsg::Command(vec!["split-window".into(), "-h".into()]);
        write_msg(&mut buf, &m).unwrap();
        write_msg(&mut buf, &ClientMsg::Input(b"ls\r".to_vec())).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_msg::<_, ClientMsg>(&mut r).unwrap(), m);
        assert_eq!(read_msg::<_, ClientMsg>(&mut r).unwrap(), ClientMsg::Input(b"ls\r".to_vec()));
        assert!(read_msg::<_, ClientMsg>(&mut r).is_err(), "end of stream");
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend([0u8; 8]);
        assert!(read_msg::<_, ClientMsg>(&mut &buf[..]).is_err());
    }
}
