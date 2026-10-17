//! One process per pane that owns its pty and shell, so shells outlive the server.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use zeit_proto::{read_msg, write_msg, HolderMsg, ToHolder, VERSION};

/// The latest output, always kept: a server that (re)connects replays it, so its
/// emulator shows the screen and scrollback as they were.
const KEEP: usize = 1 << 20;

/// Environment variables of other multiplexers and terminals: never passed on.
const FOREIGN: &[&str] = &["TMUX", "TMUX_PANE", "ZELLIJ", "ZELLIJ_SESSION_NAME", "STY", "WEZTERM_PANE", "KITTY_WINDOW_ID"];

/// Runs as the holder (`zeit __hold`): starts `cmd` (or the user's shell) on a new
/// pty in `cwd`, then serves its socket until the shell has ended and a server
/// has been told.
pub fn run(id: &str, sock: &Path, cwd: &Path, cols: u16, rows: u16, cmd: &[String]) -> std::io::Result<()> {
    let (master, child) = spawn(id, cwd, cols, rows, cmd)?;
    let _ = std::fs::remove_file(sock);
    if let Some(d) = sock.parent() {
        crate::paths::private_dir(d)?;
    }
    let listener = UnixListener::bind(sock)?;
    let result = serve(listener, master, child);
    let _ = std::fs::remove_file(sock);
    result
}

fn spawn(id: &str, cwd: &Path, cols: u16, rows: u16, cmd: &[String]) -> std::io::Result<(OwnedFd, libc::pid_t)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // what runs: the given command, else the login shell
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let argv: Vec<String> = if cmd.is_empty() { vec![shell] } else { cmd.to_vec() };
    let c_args: Vec<std::ffi::CString> = argv.iter().filter_map(|a| std::ffi::CString::new(a.as_str()).ok()).collect();
    let cwd_c = std::ffi::CString::new(cwd.as_os_str().as_encoded_bytes()).unwrap_or_default();
    // prepared before fork: only async-signal-safe calls happen in the child
    for v in FOREIGN {
        unsafe { std::env::remove_var(v) };
    }
    // zsh gets zeit's integration first (shell/zshenv), which hands ZDOTDIR back
    let zdot = crate::paths::runtime_dir().join("zsh");
    if argv[0].ends_with("zsh") && zdot.join(".zshenv").is_file() {
        unsafe {
            match std::env::var_os("ZDOTDIR") {
                Some(d) => std::env::set_var("ZEIT_ORIG_ZDOTDIR", d),
                None => std::env::remove_var("ZEIT_ORIG_ZDOTDIR"),
            }
            std::env::set_var("ZDOTDIR", &zdot);
        }
    }
    unsafe {
        std::env::set_var("ZEIT", "1");
        std::env::set_var("ZEIT_PANE", id);
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::chdir(cwd_c.as_ptr());
            let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            libc::execvp(ptrs[0], ptrs.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(slave) };
    Ok((unsafe { OwnedFd::from_raw_fd(master) }, pid))
}

fn serve(listener: UnixListener, master: OwnedFd, child: libc::pid_t) -> std::io::Result<()> {
    let mut pty = std::fs::File::from(master);
    let mut kept: VecDeque<u8> = VecDeque::new();
    let mut client: Option<UnixStream> = None;
    let mut exited: Option<i32> = None;
    let mut buf = vec![0u8; 64 * 1024];
    let mut lonely_since = std::time::Instant::now();
    loop {
        // the shell ended and a server knows: done
        if let (Some(status), Some(c)) = (exited, client.as_mut()) {
            let _ = write_msg(c, &HolderMsg::Exited(status));
            return Ok(());
        }
        // an ended shell nobody asks about is kept for an hour, then dropped
        if exited.is_some() && client.is_none() && lonely_since.elapsed().as_secs() > 3600 {
            return Ok(());
        }
        let mut fds = vec![
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if exited.is_none() { pty.as_raw_fd() } else { -1 }, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: client.as_ref().map(|c| c.as_raw_fd()).unwrap_or(-1), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = if exited.is_some() { 60_000 } else { -1 };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        // a server connects (a new one replaces an old one)
        if fds[0].revents & libc::POLLIN != 0 {
            if let Ok((mut c, _)) = listener.accept() {
                let ok = write_msg(&mut c, &HolderMsg::Hello { version: VERSION, pid: child as u32 }).is_ok();
                if ok {
                    // the latest output, so the new server rebuilds the screen
                    if !kept.is_empty() {
                        let (a, b) = kept.as_slices();
                        let all = [a, b].concat();
                        let _ = write_msg(&mut c, &HolderMsg::Output(all));
                    }
                    client = Some(c);
                }
            }
        }
        // the shell printed something
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match pty.read(&mut buf) {
                Ok(n) if n > 0 => {
                    kept.extend(&buf[..n]);
                    if kept.len() > KEEP {
                        let extra = kept.len() - KEEP;
                        kept.drain(..extra);
                    }
                    let sent = client.as_mut().is_some_and(|c| write_msg(c, &HolderMsg::Output(buf[..n].to_vec())).is_ok());
                    if !sent {
                        client = None;
                    }
                }
                // EIO: every end of the slave is closed, the shell is gone
                _ => {
                    let mut status = 0;
                    unsafe { libc::waitpid(child, &mut status, 0) };
                    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                    exited = Some(code);
                    lonely_since = std::time::Instant::now();
                }
            }
        }
        // the server says something
        if fds[2].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let Some(c) = client.as_mut() else { continue };
            match read_msg::<_, ToHolder>(c) {
                Ok(ToHolder::Input(bytes)) => {
                    let _ = pty.write_all(&bytes);
                }
                Ok(ToHolder::Resize { cols, rows }) => {
                    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                    unsafe { libc::ioctl(pty.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
                }
                Ok(ToHolder::HangUp) => unsafe {
                    libc::kill(-child, libc::SIGHUP);
                },
                Ok(ToHolder::Hello { .. }) => {}
                // the server went away: keep going, keep the output
                Err(_) => {
                    client = None;
                    lonely_since = std::time::Instant::now();
                }
            }
        }
    }
}

/// Starts a holder as its own process (detached from the server, so it lives on).
pub fn start(exe: &Path, id: &str, cwd: &Path, cols: u16, rows: u16, cmd: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let sock = crate::paths::pane_socket(id);
    let mut c = std::process::Command::new(exe);
    c.arg("__hold").arg(id).arg(&sock).arg(cwd).arg(cols.to_string()).arg(rows.to_string()).arg("--").args(cmd);
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn()?;
    // its socket appears in a moment
    for _ in 0..200 {
        if sock.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(std::io::Error::other(format!("pane {id} did not start")))
}

#[allow(dead_code)]
fn _assert_read<R: Read>(_: R) {}
