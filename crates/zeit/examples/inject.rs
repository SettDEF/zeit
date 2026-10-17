// Test helper: attach to a session headless and send raw input bytes (\e = ESC).
use std::os::unix::net::UnixStream;
use zeit_proto::*;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut s = UnixStream::connect(zeit_core::paths::server_socket("default")).unwrap();
    write_msg(&mut s, &ClientMsg::Hello { version: VERSION, client: "inject".into() }).unwrap();
    let _: ServerMsg = read_msg(&mut s).unwrap();
    write_msg(&mut s, &ClientMsg::Attach { session: Some(a[1].clone()), place: None, cols: std::env::var("COLS").ok().and_then(|v| v.parse().ok()).unwrap_or(90), rows: 20 }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    write_msg(&mut s, &ClientMsg::Input(a[2].replace("\\e", "\x1b").into_bytes())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
}
