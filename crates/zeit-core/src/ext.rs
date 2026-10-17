//! Extensions: init.lua (z.modules, setup{}, z.map, z.on, z.cmd) and plugins.

use crate::keys::Chord;
use mlua::{Function, Lua, RegistryKey, Table, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;

/// Built-in modules (more arrive with the features they switch on).
pub const MODULES: &[&str] = &["hintbar", "frames", "floating", "stacked", "welcome", "timeline", "palette", "layouts"];

#[derive(Default, Debug, Clone)]
pub struct Module {
    pub flags: Vec<String>,
    /// options from setup{}, as text (numbers and booleans too)
    pub opts: BTreeMap<String, String>,
}

enum Action {
    /// a command line, run as tmux would (`a ; b` runs both)
    Cmd(String),
    Lua(RegistryKey),
}

struct Map {
    /// "prefix" (after the prefix key) or "root" (no prefix)
    table: String,
    key: Chord,
    action: Action,
    repeat: bool,
}

#[derive(Default)]
struct State {
    modules: BTreeMap<String, Module>,
    prefix: Option<Chord>,
    maps: Vec<Map>,
    hooks: HashMap<String, Vec<RegistryKey>>,
    /// zeit commands asked for by Lua, run by the server after the call
    queued: Vec<String>,
    notices: Vec<String>,
    /// z.set: options, by tmux's names (status-position, @primary, history-limit …)
    opts: BTreeMap<String, String>,
    /// z.tmux(false): leave tmux.conf alone, this config is the whole story
    no_tmux: bool,
}

pub struct Ext {
    lua: Lua,
    st: Rc<RefCell<State>>,
    pub errors: Vec<String>,
    pub plugins: Vec<String>,
}

/// Splits a command line into words (quotes, single or double, keep spaces).
pub fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in line.chars() {
        match c {
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                any = true;
            }
            c if quote == Some(c) => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

fn text_of(v: &Value) -> Option<String> {
    Some(match v {
        Value::String(s) => s.to_string_lossy(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Table(t) => t.clone().sequence_values::<String>().flatten().collect::<Vec<_>>().join(","),
        _ => return None,
    })
}

impl Ext {
    pub fn new() -> Ext {
        let lua = Lua::new();
        let st = Rc::new(RefCell::new(State::default()));
        let mut e = Ext { lua, st, errors: Vec::new(), plugins: Vec::new() };
        if let Err(err) = e.install() {
            e.errors.push(format!("zeit API: {err}"));
        }
        e
    }

    /// Plugins first (so init.lua can configure them), then init.lua.
    pub fn load(&mut self, dir: &Path) {
        let plugins = dir.join("plugins");
        let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&plugins).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.join("init.lua").is_file()).collect()).unwrap_or_default();
        found.sort();
        let package: Option<Table> = self.lua.globals().get("package").ok();
        for p in found {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if let Some(pkg) = &package {
                let path: String = pkg.get("path").unwrap_or_default();
                let l = p.join("lua");
                let _ = pkg.set("path", format!("{}/?.lua;{}/?/init.lua;{path}", l.display(), l.display()));
            }
            match std::fs::read_to_string(p.join("init.lua")) {
                Ok(code) => {
                    if let Err(e) = self.lua.load(&code).set_name(format!("@plugins/{name}/init.lua")).exec() {
                        self.errors.push(format!("plugin {name}: {e}"));
                    }
                    self.plugins.push(name);
                }
                Err(e) => self.errors.push(format!("plugin {name}: {e}")),
            }
        }
        let init = dir.join("init.lua");
        if let Ok(code) = std::fs::read_to_string(&init) {
            if let Some(pkg) = &package {
                let path: String = pkg.get("path").unwrap_or_default();
                let _ = pkg.set("path", format!("{}/lua/?.lua;{}/lua/?/init.lua;{path}", dir.display(), dir.display()));
            }
            if let Err(e) = self.lua.load(&code).set_name("@init.lua").exec() {
                self.errors.push(format!("init.lua: {e}"));
            }
        }
    }

    pub fn module(&self, name: &str) -> Option<Module> {
        self.st.borrow().modules.get(name).cloned()
    }

    pub fn enabled(&self, name: &str) -> bool {
        self.st.borrow().modules.contains_key(name)
    }

    pub fn prefix(&self) -> Option<Chord> {
        self.st.borrow().prefix.clone()
    }

    pub fn modules(&self) -> Vec<(String, Module)> {
        self.st.borrow().modules.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// A key bound in Lua (`table` is "prefix" or "root"): runs it; false when none.
    pub fn key(&mut self, table: &str, key: &Chord) -> bool {
        enum Do {
            Cmd(String),
            Fun(Function),
        }
        let what = {
            let st = self.st.borrow();
            let Some(m) = st.maps.iter().rev().find(|m| m.table == table && &m.key == key) else { return false };
            match &m.action {
                Action::Cmd(c) => Do::Cmd(c.clone()),
                Action::Lua(k) => match self.lua.registry_value::<Function>(k) {
                    Ok(f) => Do::Fun(f),
                    Err(_) => return false,
                },
            }
        };
        match what {
            Do::Cmd(c) => self.st.borrow_mut().queued.push(c),
            Do::Fun(f) => {
                if let Err(e) = f.call::<()>(()) {
                    self.errors.push(format!("key {}: {e}", key.key));
                }
            }
        }
        true
    }

    /// Runs the hooks of an event with its data (`pane_exit` → {pane, status}).
    pub fn emit(&mut self, event: &str, data: &[(&str, String)]) {
        let fs: Vec<Function> = {
            let st = self.st.borrow();
            st.hooks.get(event).map(|v| v.iter().filter_map(|k| self.lua.registry_value(k).ok()).collect()).unwrap_or_default()
        };
        if fs.is_empty() {
            return;
        }
        let Ok(t) = self.lua.create_table() else { return };
        for (k, v) in data {
            let _ = t.set(*k, v.as_str());
        }
        for f in fs {
            if let Err(e) = f.call::<()>(t.clone()) {
                self.errors.push(format!("hook {event}: {e}"));
            }
        }
    }

    /// `zeit lua CODE`: runs Lua, returns what it printed or returned.
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        let v = match self.lua.load(format!("return {code}")).eval::<Value>() {
            Ok(v) => v,
            Err(_) => self.lua.load(code).eval::<Value>().map_err(|e| e.to_string())?,
        };
        Ok(text_of(&v).unwrap_or_else(|| "nil".into()))
    }

    /// Every key bound in Lua: (table, key, command line or "" for a function, repeat).
    pub fn bindings(&self) -> Vec<(String, Chord, String, bool)> {
        self.st.borrow().maps.iter().map(|m| (m.table.clone(), m.key.clone(), match &m.action { Action::Cmd(c) => c.clone(), Action::Lua(_) => String::new() }, m.repeat)).collect()
    }

    pub fn opts(&self) -> BTreeMap<String, String> {
        self.st.borrow().opts.clone()
    }

    pub fn reads_tmux(&self) -> bool {
        !self.st.borrow().no_tmux
    }

    pub fn take_commands(&self) -> Vec<String> {
        std::mem::take(&mut self.st.borrow_mut().queued)
    }

    pub fn take_notices(&mut self) -> Vec<String> {
        let mut n = std::mem::take(&mut self.st.borrow_mut().notices);
        n.append(&mut self.errors);
        n
    }

    fn install(&mut self) -> mlua::Result<()> {
        let lua = &self.lua;
        let z = lua.create_table()?;
        z.set("version", env!("CARGO_PKG_VERSION"))?;

        // z.modules { 'frames +rounded', 'floating +popout' }
        let st = self.st.clone();
        z.set("modules", lua.create_function(move |_, list: Table| {
            let mut s = st.borrow_mut();
            for item in list.sequence_values::<String>().flatten() {
                let mut parts = item.split_whitespace();
                let Some(name) = parts.next() else { continue };
                let flags: Vec<String> = parts.filter_map(|f| f.strip_prefix('+')).map(String::from).collect();
                let m = s.modules.entry(name.to_string()).or_default();
                m.flags = flags;
            }
            Ok(())
        })?)?;

        let st = self.st.clone();
        z.set("prefix", lua.create_function(move |_, key: String| {
            let c = Chord::parse(&key).ok_or_else(|| mlua::Error::RuntimeError(format!("not a key: {key}")))?;
            st.borrow_mut().prefix = Some(c);
            Ok(())
        })?)?;

        // z.map('<prefix>g', 'new-window lazygit' | function, { repeats = true })
        // keys: '<prefix>x' after the prefix, '<copy>y' in copy mode, plain 'M-j' always
        let st = self.st.clone();
        z.set("map", lua.create_function(move |lua, (keys, action, opts): (String, Value, Option<Table>)| {
            let (table, k) = if let Some(rest) = keys.strip_prefix("<prefix>") {
                ("prefix", rest.to_string())
            } else if let Some(rest) = keys.strip_prefix("<copy>") {
                ("copy", rest.to_string())
            } else {
                ("root", keys.clone())
            };
            let repeat = opts.and_then(|o| o.get::<bool>("repeats").ok()).unwrap_or(false);
            let key = Chord::parse(&k).ok_or_else(|| mlua::Error::RuntimeError(format!("not a key: {k}")))?;
            let action = match action {
                Value::String(s) => Action::Cmd(s.to_string_lossy()),
                Value::Function(f) => Action::Lua(lua.create_registry_value(f)?),
                _ => return Err(mlua::Error::RuntimeError("z.map: a command line or a function".into())),
            };
            st.borrow_mut().maps.push(Map { table: table.into(), key, action, repeat });
            Ok(())
        })?)?;

        let st = self.st.clone();
        z.set("on", lua.create_function(move |lua, (event, f): (String, Function)| {
            let k = lua.create_registry_value(f)?;
            st.borrow_mut().hooks.entry(event).or_default().push(k);
            Ok(())
        })?)?;

        let st = self.st.clone();
        z.set("cmd", lua.create_function(move |_, line: String| {
            st.borrow_mut().queued.push(line);
            Ok(())
        })?)?;

        // z.set('status-position', 'top'), z.set('@primary', '#ddc7a1')
        let st = self.st.clone();
        z.set("set", lua.create_function(move |_, (name, value): (String, Value)| {
            st.borrow_mut().opts.insert(name, text_of(&value).unwrap_or_default());
            Ok(())
        })?)?;

        // z.tmux(false): do not read tmux.conf
        let st = self.st.clone();
        z.set("tmux", lua.create_function(move |_, on: bool| {
            st.borrow_mut().no_tmux = !on;
            Ok(())
        })?)?;

        let st = self.st.clone();
        z.set("notify", lua.create_function(move |_, msg: String| {
            st.borrow_mut().notices.push(msg);
            Ok(())
        })?)?;

        // the built-in modules: require('zeit.frames').setup{…} turns one on with options
        let preload: Table = lua.globals().get::<Table>("package")?.get("preload")?;
        for name in MODULES {
            let st = self.st.clone();
            let n = name.to_string();
            preload.set(format!("zeit.{name}"), lua.create_function(move |lua, ()| {
                let m = lua.create_table()?;
                let st = st.clone();
                let n = n.clone();
                m.set("setup", lua.create_function(move |_, opts: Option<Table>| {
                    let mut s = st.borrow_mut();
                    let module = s.modules.entry(n.clone()).or_default();
                    if let Some(o) = opts {
                        for pair in o.pairs::<String, Value>().flatten() {
                            if let Some(v) = text_of(&pair.1) {
                                module.opts.insert(pair.0, v);
                            }
                        }
                    }
                    Ok(())
                })?)?;
                Ok(m)
            })?)?;
        }
        let zz = z.clone();
        preload.set("zeit", lua.create_function(move |_, ()| Ok(zz.clone()))?)?;
        lua.globals().set("zeit", z)?;
        let st = self.st.clone();
        lua.globals().set("print", lua.create_function(move |_, args: mlua::Variadic<Value>| {
            let line = args.iter().filter_map(text_of).collect::<Vec<_>>().join("\t");
            st.borrow_mut().notices.push(line);
            Ok(())
        })?)?;
        Ok(())
    }
}

impl Default for Ext {
    fn default() -> Self {
        Ext::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_lua_modules_keys_hooks() {
        let d = std::env::temp_dir().join(format!("zeit-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("plugins/hello")).unwrap();
        std::fs::write(d.join("plugins/hello/init.lua"), "local z = require('zeit'); z.map('<prefix>H', function() z.notify('hi from a plugin') end)").unwrap();
        std::fs::write(d.join("init.lua"), r#"
            local z = require('zeit')
            z.modules { 'hintbar', 'frames +rounded +names', 'floating +popout' }
            require('zeit.floating').setup { radius = 14, popout = { 'htop', 'lazygit' } }
            z.prefix('C-Space')
            z.map('<prefix>g', 'new-window -n git lazygit')
            z.map('M-z', function() z.cmd('resize-pane -Z') end)
            z.on('pane_exit', function(e) if e.status ~= '0' then z.notify(e.pane .. ' failed') end end)
        "#).unwrap();
        let mut e = Ext::new();
        e.load(&d);
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        assert_eq!(e.plugins, vec!["hello"]);
        assert!(e.enabled("hintbar"));
        assert_eq!(e.module("frames").unwrap().flags, vec!["rounded", "names"]);
        let f = e.module("floating").unwrap();
        assert_eq!(f.opts.get("radius").map(String::as_str), Some("14"));
        assert_eq!(f.opts.get("popout").map(String::as_str), Some("htop,lazygit"));
        assert_eq!(e.prefix(), Chord::parse("C-Space"));
        assert!(e.key("prefix", &Chord::parse("g").unwrap()));
        assert!(e.key("root", &Chord::parse("M-z").unwrap()));
        assert_eq!(e.take_commands(), vec!["new-window -n git lazygit".to_string(), "resize-pane -Z".to_string()]);
        assert!(e.key("prefix", &Chord::parse("H").unwrap()));
        assert!(!e.key("prefix", &Chord::parse("q").unwrap()));
        e.emit("pane_exit", &[("pane", "p1".into()), ("status", "2".into())]);
        assert_eq!(e.take_notices(), vec!["hi from a plugin".to_string(), "p1 failed".to_string()]);
        assert_eq!(e.eval("1 + 1").unwrap(), "2");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn command_words() {
        assert_eq!(words(r#"new-window -n "my shell" zsh"#), vec!["new-window", "-n", "my shell", "zsh"]);
        assert_eq!(words("new-window -n 'conf' 'vim a'"), vec!["new-window", "-n", "conf", "vim a"]);
    }
}
