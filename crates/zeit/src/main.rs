//! zeit: the command line, the text client, the server and the pane holders,
//! in one binary.

mod cli;
mod client;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use zeit_core::paths;
use zeit_proto::{read_msg, write_msg, ClientMsg, ServerMsg, VERSION};

const HELP: &str = "**zeit** {version}
Your shells keep running, and remember: a terminal multiplexer with tmux's keys.

**Usage:**
  `zeit`                       the session of this folder (made if new)
  `zeit` <command> [<args>]    a command (inside a pane it acts on that pane)

**Sessions:**
  `attach` [`-t` <name>]       attach (the latest session without -t)
  `new` [`-s` <name>] [`-d`]     a new session (-d: do not attach)
  `ls`                       list sessions          `kill-session -t` <name>
  `restart-server`           upgrade without losing anything (shells keep running)

**Panes and windows:**
  `split-window` [`-h`]        split (beside with -h)  `new-window` [`-n` <name>]
  `select-pane -L/-R/-U/-D`  move                    `resize-pane -Z`  zoom
  `select-layout` <name>     even-horizontal, even-vertical, main-*, tiled
  `web` [`-h`|`-v`] <address>    a pane that is a web page (drawn by zash)

**Floats** (panes floating over their window, as in zellij):
  `float` [`-w` cols] [`-h` rows] [`-c` dir] [<command>]   a new one   `toggle-floats`   show / hide the window's floats   `M-f`
  `float-embed`   a float into the layout, or the focused pane out of it   prefix `E`      `float-next`   `float-close`
  mouse: a click brings a float up, drag its frame (or Alt+drag), Ctrl+drag or the corner resizes

**Session menu** (prefix s, w: choose-tree as tmux shows it, the whole screen): the tree on top
  (0-9 picks, ←→ fold, x kill, v preview), the chosen one's panes live below.
  z.set('@tree-preview', 'off')   z.set('@tree-height', 40)   (% of the screen for the tree)

**zfloats** (panes on their own layer over the layout; they keep running while hidden):
  `zfloat` [`-n` <name>] [`-w` %] [`-h` %] [`-a` <side>] [`-B`] [<command>]   a new one (output stays when it ends)
  `zfloat-toggle` / `zfloat-show` [`-n` <name>] [<command>]   show or hide / only show (made the first time)   prefix `f`
  `zfloat-list`   choose one   prefix `F`      `zfloat-layer` [up|down]   keys to it or the panes below   prefix `Tab`
  `zfloat-align` center|top|right|bottom-left…   `zfloat-move` `-U`|`-D`|`-L`|`-R` [n]   `zfloat-hide`   `zfloat-close`
  mouse: drag the frame (or Alt+drag inside), Ctrl+drag or the corner resizes; tmux's `display-popup` works too

**Jobs and history:**
  `timeline` [`-a`]            what this pane ran (-a: every pane)
  `jobs`, `job-run` [`-c` <dir>] `--` <command>   commands sent to other panes

**Scripts:** tmux's verbs work: `send-keys`, `capture-pane -p`, `list-panes`,
`display-message -p`, `if-shell`, `run-shell`, `set-option`, `lua` <code>

**Keys:** your prefix and bindings (~/.config/zeit/init.lua, else tmux.conf);
prefix `d` detaches, `:` command prompt, `T` timeline, `q` pane numbers.

**Config:** ~/.config/zeit/init.lua: `z.modules{…}`, `z.map`, `z.set`, `z.rule`, `z.on`";

fn fail(msg: &str) -> ! {
    eprintln!("zeit: {msg}");
    std::process::exit(1);
}

/// The server's socket, starting the server when none runs.
fn connect() -> UnixStream {
    let sock = paths::server_socket("default");
    if let Ok(s) = UnixStream::connect(&sock) {
        return hello(s);
    }
    let exe = std::env::current_exe().unwrap_or_else(|e| fail(&e.to_string()));
    zeit_core::server::spawn_server(&exe);
    // up to 10 s: a server with many panes to bring back takes a while
    for _ in 0..2000 {
        if let Ok(s) = UnixStream::connect(&sock) {
            return hello(s);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    fail("the server did not start");
}

fn hello(mut s: UnixStream) -> UnixStream {
    write_msg(&mut s, &ClientMsg::Hello { version: VERSION, client: "zeit".into() }).unwrap_or_else(|e| fail(&e.to_string()));
    match read_msg::<_, ServerMsg>(&mut s) {
        Ok(ServerMsg::Hello { .. }) => s,
        Ok(other) => fail(&format!("unexpected reply {other:?}")),
        Err(e) => fail(&e.to_string()),
    }
}

/// A command for the server; inside a zeit pane it acts on that pane.
fn command(words: Vec<String>) -> i32 {
    let mut s = connect();
    let mut w = words;
    if let Ok(p) = std::env::var("ZEIT_PANE") {
        w.insert(0, format!("@{p}"));
    }
    if write_msg(&mut s, &ClientMsg::Command(w)).is_err() {
        fail("the server went away");
    }
    loop {
        match read_msg::<_, ServerMsg>(&mut s) {
            Ok(ServerMsg::Reply { ok, text }) => {
                if !text.is_empty() {
                    if ok { println!("{text}") } else { eprintln!("zeit: {text}") }
                }
                return if ok { 0 } else { 1 };
            }
            Ok(_) => continue,
            Err(_) => return 1,
        }
    }
}

fn attach(session: Option<String>, place: Option<String>) {
    if std::env::var_os("ZEIT_PANE").is_some() {
        fail("already inside zeit (unset ZEIT_PANE to nest anyway)");
    }
    let (mut session, mut place) = (session, place);
    loop {
        let s = connect();
        match client::attach(s, session.clone(), place.clone()) {
            // the server restarted (an upgrade): back to the same session on the new one
            Ok(reason) if reason.starts_with("__restart:") => {
                session = Some(reason["__restart:".len()..].to_string());
                place = None;
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Ok(reason) => return println!("[{reason}]"),
            Err(e) => fail(&e),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    match args.first().map(String::as_str) {
        Some("__hold") => {
            // zeit __hold ID SOCKET DIR COLS ROWS -- CMD…
            let get = |i: usize| args.get(i).cloned().unwrap_or_default();
            let cmd: Vec<String> = args.iter().skip_while(|a| *a != "--").skip(1).cloned().collect();
            if let Err(e) = zeit_core::holder::run(&get(1), &PathBuf::from(get(2)), &PathBuf::from(get(3)), get(4).parse().unwrap_or(80), get(5).parse().unwrap_or(24), &cmd) {
                fail(&e.to_string());
            }
        }
        Some("__webpane") => {
            // a web pane: the page is drawn by the terminal (zash); others see this note
            let url = args.get(1).cloned().unwrap_or_default();
            print!("\x1b]7733;webpane;url={url}\x07\x1b[?25l\r\n  \u{f0ac}  {url}\r\n\r\n  a web page: zash shows it here\r\n");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        Some("server") => {
            let exe = std::env::current_exe().unwrap_or_else(|e| fail(&e.to_string()));
            if let Err(e) = zeit_core::server::run("default", exe) {
                fail(&e.to_string());
            }
        }
        Some("-h") | Some("--help") | Some("help") => cli::print(&HELP.replace("{version}", env!("CARGO_PKG_VERSION"))),
        Some("-V") | Some("--version") => println!("zeit {}", env!("CARGO_PKG_VERSION")),
        None => {
            let here = std::env::current_dir().map(|p| p.display().to_string()).ok();
            attach(None, here);
        }
        Some("attach") | Some("a") | Some("attach-session") | Some("at") => attach(opt("-t"), None),
        Some("new") | Some("new-session") if !args.iter().any(|a| a == "-d") => {
            let mut w = args.clone();
            w.push("-d".into());
            let mut s = connect();
            let _ = write_msg(&mut s, &ClientMsg::Command(w));
            let name = loop {
                match read_msg::<_, ServerMsg>(&mut s) {
                    Ok(ServerMsg::Reply { ok: true, text }) => break text,
                    Ok(ServerMsg::Reply { ok: false, text }) => fail(&text),
                    Ok(_) => continue,
                    Err(e) => fail(&e.to_string()),
                }
            };
            drop(s);
            attach(Some(name), None);
        }
        Some(_) => std::process::exit(command(args)),
    }
}
