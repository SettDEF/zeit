//! The text client: zeit in any terminal, like tmux.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeit_core::keys::{chord_from_bytes, Chord, Keys};
use zeit_proto::{read_msg, write_msg, Cell, ClientMsg, Color, CursorShape, ServerMsg};

static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

fn size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

struct Raw(libc::termios);

impl Raw {
    fn enter() -> Option<Raw> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return None;
        }
        let old = t;
        unsafe { libc::cfmakeraw(&mut t) };
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        // alternate screen, bracketed paste and focus events passed through
        // alternate screen; the mouse always (click a pane, the wheel scrolls back)
        // and the terminal around it (zash) leaves the multiplexer keys to zeit
        print!("\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[H\x1b[2J\x1b]7733;keys;all\x07");
        let _ = std::io::stdout().flush();
        Some(Raw(old))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        print!("\x1b]7733;keys;none\x07\x1b]7733;rows;\x07\x1b[0m\x1b[?1002l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1l\x1b[?25h\x1b[0 q\x1b[?1049l");
        let _ = std::io::stdout().flush();
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

fn sgr(out: &mut String, c: &Cell) {
    out.push_str("\x1b[0");
    let f = c.flags;
    if f & zeit_proto::BOLD != 0 { out.push_str(";1"); }
    if f & zeit_proto::DIM != 0 { out.push_str(";2"); }
    if f & zeit_proto::ITALIC != 0 { out.push_str(";3"); }
    if f & zeit_proto::UNDERLINE != 0 { out.push_str(";4"); }
    if f & zeit_proto::REVERSE != 0 { out.push_str(";7"); }
    if f & zeit_proto::STRIKE != 0 { out.push_str(";9"); }
    match c.fg {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => out.push_str(&format!(";{}", 30 + i)),
        Color::Indexed(i) if i < 16 => out.push_str(&format!(";{}", 90 + i - 8)),
        Color::Indexed(i) => out.push_str(&format!(";38;5;{i}")),
        Color::Rgb(r, g, b) => out.push_str(&format!(";38;2;{r};{g};{b}")),
    }
    match c.bg {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => out.push_str(&format!(";{}", 40 + i)),
        Color::Indexed(i) if i < 16 => out.push_str(&format!(";{}", 100 + i - 8)),
        Color::Indexed(i) => out.push_str(&format!(";48;5;{i}")),
        Color::Rgb(r, g, b) => out.push_str(&format!(";48;2;{r};{g};{b}")),
    }
    out.push('m');
}

/// Shows a session in this terminal until detached or ended.
pub fn attach(mut sock: UnixStream, session: Option<String>, place: Option<String>) -> Result<String, String> {
    // which keys are zeit's (tmux.conf and init.lua, as the server read them)
    let mut keys = Keys { prefix: Chord::parse("C-b").unwrap(), table: Default::default(), root: Default::default(), copy: Default::default(), opts: Default::default() };
    write_msg(&mut sock, &ClientMsg::Command(vec!["__keys".into()])).map_err(|e| e.to_string())?;
    let reply = read_msg::<_, ServerMsg>(&mut sock);
    if !matches!(reply, Ok(ServerMsg::Reply { ok: true, .. })) {
        // an older server: its prefix, and tmux.conf's keys
        keys = Keys::load();
        write_msg(&mut sock, &ClientMsg::Command(vec!["show-prefix".into()])).map_err(|e| e.to_string())?;
        if let Ok(ServerMsg::Reply { ok: true, text }) = read_msg::<_, ServerMsg>(&mut sock) {
            if let Some(p) = Chord::parse(&text) {
                keys.prefix = p;
            }
        }
    }
    if let Ok(ServerMsg::Reply { ok: true, text }) = reply {
        for line in text.lines() {
            let Some((kind, k)) = line.split_once(' ') else { continue };
            let Some(c) = Chord::parse(k) else { continue };
            let b = zeit_core::keys::Binding { action: zeit_core::keys::Action::Seq(Vec::new()), cmd: String::new(), repeat: kind == "repeat" };
            match kind {
                "prefix" => keys.prefix = c,
                "root" => {
                    keys.root.insert(c, b);
                }
                _ => {
                    keys.table.insert(c, b);
                }
            }
        }
    }
    let (cols, rows) = size();
    write_msg(&mut sock, &ClientMsg::Attach { session, place, cols, rows }).map_err(|e| e.to_string())?;
    let raw = Raw::enter().ok_or("not a terminal")?;
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_winch as *const () as usize;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
    // the drawing thread pokes this pipe when the session is over, so the key loop wakes
    let mut pipe = [0i32; 2];
    unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) };
    let (wake_r, wake_w) = (pipe[0], pipe[1]);
    // the server's frames, drawn as they come
    let mut reader = sock.try_clone().map_err(|e| e.to_string())?;
    let drawer = std::thread::spawn(move || -> String {
        let mut stdout = std::io::stdout().lock();
        let mut modes: Option<u8> = None;
        loop {
            match read_msg::<_, ServerMsg>(&mut reader) {
                Ok(ServerMsg::Frame(f)) => {
                    let mut out = String::with_capacity(f.changed.len() * (f.cols as usize + 16));
                    out.push_str("\x1b[?25l");
                    for (r, row) in &f.changed {
                        out.push_str(&format!("\x1b[{};1H", r + 1));
                        let mut last: Option<(Color, Color, u8)> = None;
                        for c in row {
                            if c.flags & zeit_proto::SPACER != 0 {
                                continue;
                            }
                            let style = (c.fg, c.bg, c.flags);
                            if last != Some(style) {
                                sgr(&mut out, c);
                                last = Some(style);
                            }
                            out.push_str(&c.text);
                        }
                    }
                    out.push_str("\x1b[0m");
                    // the outer terminal behaves as the focused program asked: paste, arrows, focus
                    if modes != Some(f.modes) {
                        let on = |bit: u8| f.modes & bit != 0;
                        out.push_str(if on(zeit_proto::MODE_PASTE) { "\x1b[?2004h" } else { "\x1b[?2004l" });
                        out.push_str(if on(zeit_proto::MODE_APP_CURSOR) { "\x1b[?1h" } else { "\x1b[?1l" });
                        out.push_str(if on(zeit_proto::MODE_FOCUS) { "\x1b[?1004h" } else { "\x1b[?1004l" });
                        modes = Some(f.modes);
                    }
                    out.push_str(&format!("\x1b]2;{}\x07", f.title.replace(['\x07', '\x1b'], "")));
                    match (f.cursor, f.cursor_shape) {
                        (Some((x, y)), shape) if shape != CursorShape::Hidden => {
                            let n = match shape {
                                CursorShape::Beam => 6,
                                CursorShape::Underline => 4,
                                _ => 2,
                            };
                            out.push_str(&format!("\x1b[{n} q\x1b[{};{}H\x1b[?25h", y + 1, x + 1));
                        }
                        _ => {}
                    }
                    let _ = stdout.write_all(out.as_bytes());
                    let _ = stdout.flush();
                }
                Ok(ServerMsg::Passthrough(seq)) => {
                    let _ = stdout.write_all(&seq);
                    let _ = stdout.flush();
                }
                Ok(ServerMsg::Bye { reason }) => {
                    unsafe { libc::write(wake_w, [1u8].as_ptr().cast(), 1) };
                    return reason;
                }
                Ok(_) => {}
                Err(_) => {
                    unsafe { libc::write(wake_w, [1u8].as_ptr().cast(), 1) };
                    return "the server went away".into();
                }
            }
        }
    });
    // keys: the prefix and zeit's bindings are zeit's, everything else is the program's
    let mut writer = sock;
    let mut buf = [0u8; 4096];
    let mut waiting = false;
    let mut repeat_until: Option<Instant> = None;
    loop {
        if drawer.is_finished() {
            break;
        }
        if RESIZED.swap(false, Ordering::Relaxed) {
            let (cols, rows) = size();
            if write_msg(&mut writer, &ClientMsg::Resize { cols, rows }).is_err() {
                break;
            }
        }
        let mut pfd = [
            libc::pollfd { fd: std::io::stdin().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_r, events: libc::POLLIN, revents: 0 },
        ];
        // no timeout: idle means idle (a resize interrupts the wait, the pipe ends it)
        let n = unsafe { libc::poll(pfd.as_mut_ptr(), 2, -1) };
        if n <= 0 || pfd[1].revents != 0 {
            continue;
        }
        let got = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if got <= 0 {
            break;
        }
        let bytes = &buf[..got as usize];
        let mut i = 0;
        let mut plain: Vec<u8> = Vec::new();
        let flush = |w: &mut UnixStream, p: &mut Vec<u8>| {
            if !p.is_empty() {
                let _ = write_msg(w, &ClientMsg::Input(std::mem::take(p)));
            }
        };
        while i < bytes.len() {
            let Some((chord, used)) = chord_from_bytes(&bytes[i..]) else { break };
            let name = format!("{}{}{}", if chord.ctrl { "C-" } else { "" }, if chord.alt { "M-" } else { "" }, chord.key);
            let in_repeat = repeat_until.is_some_and(|t| Instant::now() < t) && keys.table.get(&chord).is_some_and(|b| b.repeat);
            if waiting || in_repeat {
                waiting = false;
                flush(&mut writer, &mut plain);
                repeat_until = keys.table.get(&chord).filter(|b| b.repeat).map(|_| Instant::now() + Duration::from_millis(600));
                let _ = write_msg(&mut writer, &ClientMsg::Command(vec!["__key".into(), "prefix".into(), name]));
            } else if chord == keys.prefix {
                flush(&mut writer, &mut plain);
                waiting = true;
                repeat_until = None;
            } else if keys.root.contains_key(&chord) {
                flush(&mut writer, &mut plain);
                let _ = write_msg(&mut writer, &ClientMsg::Command(vec!["__key".into(), "root".into(), name]));
            } else {
                repeat_until = None;
                plain.extend_from_slice(&bytes[i..i + used]);
            }
            i += used;
        }
        if i < bytes.len() {
            plain.extend_from_slice(&bytes[i..]);
        }
        flush(&mut writer, &mut plain);
    }
    let reason = drawer.join().unwrap_or_else(|_| "the client failed".into());
    drop(raw);
    Ok(reason)
}
