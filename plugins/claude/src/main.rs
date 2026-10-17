//! zeit-claude: a plugin, not part of zeit. It talks to zeit only through the `zeit` command.

mod activity;
mod mcp;

/// A zeit command, as if typed in this pane (`zeit` adds the pane itself): its reply text.
pub fn zeit(words: &[String]) -> Result<String, String> {
    let out = std::process::Command::new("zeit").args(words).stdin(std::process::Stdio::null()).output().map_err(|e| format!("zeit: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if out.status.success() { Ok(text) } else { Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("zeit: ").to_string()) }
}

/// Where zeit keeps state (the activity files live beside it).
pub fn state_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| home.join(".local/state")).join("zeit")
}

const HELP: &str = "zeit-claude: Claude Code beside zeit

  zeit-claude hook       Claude Code's hook: notes each tool call, shows the view
  zeit-claude view FILE  the view of what Claude runs (in a zfloat on the right)
  zeit-claude mcp        an MCP server: Claude opens zfloats, web pages, floats, jobs, reads panes

Installed as a Claude Code plugin: /plugin marketplace add <zeit>/plugins/claude, then
/plugin install zeit-claude@zeit. Outside zeit it does nothing.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") => activity::hook(),
        Some("view") => activity::view(args.get(1).map(String::as_str).unwrap_or("")),
        Some("mcp") => mcp::serve(),
        _ => println!("{HELP}"),
    }
}
