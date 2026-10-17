//! `zeit-claude mcp`: an MCP server (stdio) so Claude Code can open things in zeit and zash itself:
//! a zfloat with a command, a web page, a float, a job; read a pane; list the panes.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// One zeit command, as if typed in Claude's pane (its session, its folder): the reply text.
fn zeit(words: Vec<String>) -> Result<String, String> {
    crate::zeit(&words)
}

fn tools() -> Value {
    let s = |d: &str| json!({ "type": "string", "description": d });
    let n = |d: &str| json!({ "type": "integer", "description": d });
    json!([
        {
            "name": "show_in_zfloat",
            "description": "Run a command in a zfloat: a pane on its own layer above the user's panes, docked at a side, that keeps its output when the command ends. The user's keys stay where they are. Use it to show the user something live (a build, logs, a TUI like htop or lazygit, a test run) without taking over their screen.",
            "inputSchema": { "type": "object", "properties": {
                "command": s("the shell command to run"),
                "name": s("a short name for the zfloat (default: the program's name); the same name reuses it"),
                "side": s("where: right (default), left, top, bottom, center, top-right, bottom-right, top-left, bottom-left"),
                "width": n("width in % of the screen (default 45)"),
                "height": n("height in % of the screen (default 100 docked left/right, else 50)")
            }, "required": ["command"] }
        },
        {
            "name": "open_web",
            "description": "Open a web page in zeit: a pane that is the page, drawn by the zash terminal (a real browser view). Use it to show the user documentation, a local dev server (http://localhost:3000) or any site.",
            "inputSchema": { "type": "object", "properties": {
                "url": s("an address: a URL, a host like docs.rs, or words to search"),
                "beside": { "type": "boolean", "description": "split it beside the user's pane instead of a window of its own" }
            }, "required": ["url"] }
        },
        {
            "name": "open_float",
            "description": "Open a floating pane over the user's current window (as in zellij), focused so the user can type in it. Use it when the user should interact with something (a REPL, an editor, a prompt).",
            "inputSchema": { "type": "object", "properties": { "command": s("what to run (default: a shell)") } }
        },
        {
            "name": "run_job",
            "description": "Run a command as a zeit job: it shows in the user's status bar with its progress, a note comes when it ends, and its output can be opened from the job line.",
            "inputSchema": { "type": "object", "properties": { "command": s("the command"), "dir": s("the folder (default: Claude's)") }, "required": ["command"] }
        },
        {
            "name": "read_pane",
            "description": "Read what a zeit pane shows (and its recent scrollback): the user's other shells, a running server, a REPL. Panes are named like %3 or by id (list_panes).",
            "inputSchema": { "type": "object", "properties": { "pane": s("the pane (default: the user's active pane)"), "lines": n("how many lines of history to include (default 60)") } }
        },
        {
            "name": "list_panes",
            "description": "List the panes of the user's zeit session: id, window, what runs there, its folder.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn call(name: &str, a: &Value) -> Result<String, String> {
    let s = |k: &str| a.get(k).and_then(|v| v.as_str()).map(String::from);
    let n = |k: &str| a.get(k).and_then(|v| v.as_u64());
    match name {
        "show_in_zfloat" => {
            let cmd = s("command").ok_or("command is needed")?;
            let side = s("side").unwrap_or_else(|| "right".into());
            let docked = side == "right" || side == "left";
            let name = s("name").unwrap_or_else(|| cmd.split_whitespace().next().unwrap_or("float").rsplit('/').next().unwrap_or("float").to_string());
            let w = n("width").unwrap_or(45).clamp(20, 100);
            let h = n("height").unwrap_or(if docked { 100 } else { 50 }).clamp(20, 100);
            zeit(vec!["zfloat-show".into(), "-n".into(), name.clone(), "-w".into(), w.to_string(), "-h".into(), h.to_string(), "-a".into(), side.clone(), "-B".into(), cmd])?;
            Ok(format!("shown in the zfloat \"{name}\" ({side}); the user's keys stay in their pane (prefix Tab moves them into it, prefix F lists zfloats)"))
        }
        "open_web" => {
            let url = s("url").ok_or("url is needed")?;
            let mut w = vec!["web".to_string()];
            if a.get("beside").and_then(|v| v.as_bool()).unwrap_or(false) {
                w.push("-h".into());
            }
            w.push(url.clone());
            zeit(w)?;
            Ok(format!("opened {url} in zeit (zash draws the page)"))
        }
        "open_float" => {
            let mut w = vec!["float".to_string()];
            if let Some(c) = s("command") {
                w.push(c);
            }
            zeit(w)?;
            Ok("opened a floating pane; it has the user's keys (M-f hides the floats)".into())
        }
        "run_job" => {
            let cmd = s("command").ok_or("command is needed")?;
            let mut w = vec!["job-run".to_string()];
            if let Some(d) = s("dir") {
                w.extend(["-c".into(), d]);
            }
            w.extend(["--".into(), cmd.clone()]);
            zeit(w)?;
            Ok(format!("started as a zeit job: {cmd}"))
        }
        "read_pane" => {
            let lines = n("lines").unwrap_or(60).min(2000);
            let mut w = vec!["capture-pane".to_string(), "-p".into(), "-S".into(), format!("-{lines}")];
            if let Some(p) = s("pane") {
                w.extend(["-t".into(), p]);
            }
            zeit(w)
        }
        "list_panes" => zeit(vec!["list-panes".into(), "-a".into(), "-F".into(), "#{pane_id}  window #{window_index}  #{pane_current_command}  #{pane_current_path}".into()]),
        other => Err(format!("no tool {other}")),
    }
}

/// The server: JSON-RPC lines on stdin and stdout (MCP's stdio transport).
pub fn serve() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no answer
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "zeit", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "zeit is the user's terminal multiplexer and zash their terminal. Use these tools to show the user things on their screen (zfloats, web pages, floats, jobs) and to read their other panes. Only when Claude runs inside zeit (ZEIT_PANE is set)."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match call(name, &args) {
                    Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                })
            }
            _ => Err(json!({ "code": -32601, "message": format!("no method {method}") })),
        };
        let reply = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        };
        let _ = writeln!(out, "{reply}");
        let _ = out.flush();
    }
}
