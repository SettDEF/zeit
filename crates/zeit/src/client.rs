//! The text client: zeit in any terminal, like tmux.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeit_core::keys::{chord_from_bytes, Chord, Keys};
use zeit_proto::{read_msg, write_msg, Cell, ClientMsg, Color, CursorShape, ServerMsg};

static RESIZED: AtomicBool = AtomicBool::new(false);
/// web pages the terminal shows for this client (ids from webrect)
static PINS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

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

/// the Kitty keyboard protocol asked of the outer terminal for the focused program (popped on leaving)
static KITTY_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl Drop for Raw {
    fn drop(&mut self) {
        if let Ok(p) = PINS.lock() {
            for id in p.iter() {
                print!("\x1b]7733;webhide;id={id}\x07");
            }
        }
        if KITTY_KEYS.swap(false, std::sync::atomic::Ordering::Relaxed) {
            print!("\x1b[<u");
        }
        print!("\x1b]7733;keys;none\x07\x1b]7733;zfloatclose;\x07\x1b]7733;rows;\x07\x1b[0m\x1b[?1002l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1l\x1b[?25h\x1b[0 q\x1b[?1049l");
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
                        // zash effects on this row: started at their first column, ended after their last
                        let runs: Vec<&zeit_proto::FxRun> = f.fx.iter().filter(|x| x.row == *r).collect();
                        for (x, c) in row.iter().enumerate() {
                            let x = x as u16;
                            if runs.iter().any(|run| run.to == x && run.from < x) {
                                out.push_str("\x1b]7733;fx;end\x1b\\");
                            }
                            if let Some(run) = runs.iter().find(|run| run.from == x) {
                                out.push_str(&format!("\x1b]7733;fx;{}\x1b\\", run.body));
                            }
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
                        if runs.iter().any(|run| run.to as usize >= row.len()) {
                            out.push_str("\x1b]7733;fx;end\x1b\\");
                        }
                    }
                    out.push_str("\x1b[0m");
                    // the outer terminal behaves as the focused program asked: paste, arrows, focus
                    if modes != Some(f.modes) {
                        let on = |bit: u8| f.modes & bit != 0;
                        out.push_str(if on(zeit_proto::MODE_PASTE) { "\x1b[?2004h" } else { "\x1b[?2004l" });
                        out.push_str(if on(zeit_proto::MODE_APP_CURSOR) { "\x1b[?1h" } else { "\x1b[?1l" });
                        out.push_str(if on(zeit_proto::MODE_FOCUS) { "\x1b[?1004h" } else { "\x1b[?1004l" });
                        // the Kitty keyboard protocol: pushed while the focused program wants it, popped after
                        let kitty = on(zeit_proto::MODE_KITTY_KEYS);
                        if kitty != KITTY_KEYS.swap(kitty, std::sync::atomic::Ordering::Relaxed) {
                            out.push_str(if kitty { "\x1b[>1u" } else { "\x1b[<u" });
                        }
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
                    // remember the web pages zash shows for us, to take them away on leaving
                    let text = String::from_utf8_lossy(&seq);
                    for part in text.split("7733;webrect;id=").skip(1) {
                        if let Some(id) = part.split(';').next() {
                            if let Ok(mut p) = PINS.lock() {
                                if !p.contains(&id.to_string()) {
                                    p.push(id.to_string());
                                }
                            }
                        }
                    }
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

/// `zeit view`: the session as JSON lines for a program that draws it itself (Dotos' terminal).
/// Out, one line per frame: `{"cols","rows","changed":[[row,[[text,fg,bg,flags],…]],…],"cursor":[x,y]|null,"shape","title"}`,
/// cells joined into runs of one style; colours "" (default), "i<n>" (palette) or "#rrggbb".
/// In, one line each: `k <hex bytes>` keys, `s <cols> <rows>` a new size, `q` leave.
pub fn view(mut sock: UnixStream, session: Option<String>, place: Option<String>, cols: u16, rows: u16) -> i32 {
    if write_msg(&mut sock, &ClientMsg::Attach { session, place, cols, rows }).is_err() {
        return 1;
    }
    let mut writer = match sock.try_clone() {
        Ok(w) => w,
        Err(_) => return 1,
    };
    // keys and sizes from stdin, as they come
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            let msg = match verb {
                "k" => ClientMsg::Input((0..rest.len() / 2).filter_map(|i| u8::from_str_radix(&rest[i * 2..i * 2 + 2], 16).ok()).collect()),
                "s" => {
                    let mut n = rest.split_whitespace().filter_map(|v| v.parse::<u16>().ok());
                    match (n.next(), n.next()) {
                        (Some(c), Some(r)) if c > 0 && r > 0 => ClientMsg::Resize { cols: c, rows: r },
                        _ => continue,
                    }
                }
                "q" => break,
                _ => continue,
            };
            if write_msg(&mut writer, &msg).is_err() {
                break;
            }
        }
        let _ = write_msg(&mut writer, &ClientMsg::Detach);
    });
    let mut out = std::io::stdout().lock();
    loop {
        match read_msg::<_, ServerMsg>(&mut sock) {
            Ok(ServerMsg::Frame(f)) => {
                let mut s = String::with_capacity(f.changed.len() * 64 + 128);
                s.push_str(&format!("{{\"cols\":{},\"rows\":{},\"changed\":[", f.cols, f.rows));
                for (i, (r, row)) in f.changed.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push_str(&format!("[{r},["));
                    let mut run: Option<(Color, Color, u8, String)> = None;
                    let mut first = true;
                    let mut flush = |s: &mut String, run: &mut Option<(Color, Color, u8, String)>| {
                        if let Some((fg, bg, fl, t)) = run.take() {
                            if !first {
                                s.push(',');
                            }
                            first = false;
                            s.push_str(&format!("[{},\"{}\",\"{}\",{}]", json_str(&t), color(fg), color(bg), fl));
                        }
                    };
                    for c in row {
                        if c.flags & zeit_proto::SPACER != 0 {
                            continue;
                        }
                        match &mut run {
                            Some((fg, bg, fl, t)) if *fg == c.fg && *bg == c.bg && *fl == c.flags => t.push_str(&c.text),
                            _ => {
                                flush(&mut s, &mut run);
                                run = Some((c.fg, c.bg, c.flags, c.text.clone()));
                            }
                        }
                    }
                    flush(&mut s, &mut run);
                    s.push_str("]]");
                }
                s.push_str("],\"cursor\":");
                match f.cursor {
                    Some((x, y)) if f.cursor_shape != CursorShape::Hidden => s.push_str(&format!("[{x},{y}]")),
                    _ => s.push_str("null"),
                }
                let shape = match f.cursor_shape {
                    CursorShape::Beam => "beam",
                    CursorShape::Underline => "underline",
                    CursorShape::Hidden => "hidden",
                    CursorShape::Block => "block",
                };
                s.push_str(&format!(",\"shape\":\"{shape}\",\"modes\":{},\"title\":{}}}\n", f.modes, json_str(&f.title)));
                if out.write_all(s.as_bytes()).and_then(|_| out.flush()).is_err() {
                    return 0;
                }
            }
            Ok(ServerMsg::Bye { reason }) => {
                let _ = writeln!(out, "{{\"bye\":{}}}", json_str(&reason));
                return 0;
            }
            Ok(_) => {}
            Err(_) => return 0,
        }
    }
}

fn color(c: Color) -> String {
    match c {
        Color::Default => String::new(),
        Color::Indexed(n) => format!("i{n}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
