//! Where zeit keeps things: sockets in the runtime dir (private, gone at logout),
//! the timeline and saved sessions in the state dir, config in the config dir.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// `$XDG_RUNTIME_DIR/zeit` (0700): the server's socket and one socket per pane holder.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| {
        // no runtime dir (rare): a private folder in /tmp
        PathBuf::from(format!("/tmp/zeit-{}", unsafe { libc::getuid() }))
    });
    base.join("zeit")
}

/// The server's socket; `name` is tmux's `-L` (several servers side by side).
pub fn server_socket(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
}

pub fn pane_socket(id: &str) -> PathBuf {
    runtime_dir().join("panes").join(format!("{id}.sock"))
}

/// `$XDG_STATE_HOME/zeit`: the timeline, saved sessions.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("zeit")
}

/// `$ZEIT_CONFIG` or `$XDG_CONFIG_HOME/zeit`: init.lua, plugins/.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ZEIT_CONFIG") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("zeit")
}

/// Makes a private directory (0700), as sockets in it must be.
pub fn private_dir(p: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

/// A new id that stays the pane's (or session's) through restarts: time + randomness.
pub fn new_id(prefix: &str) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut r = [0u8; 4];
    unsafe { libc::getrandom(r.as_mut_ptr().cast(), 4, 0) };
    format!("{prefix}{:x}{:08x}", t.as_millis(), u32::from_le_bytes(r))
}
