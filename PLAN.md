# zeit — plan

**Your shells keep running, and remember. Every terminal can pick them up.**
zeit (German for *time*: sessions that outlast the window, the logout, the
reboot) is a terminal multiplexer: a server that owns your shells, their
scrollback and layouts, and clients that show them. In zash it is native:
nothing is parsed or drawn twice, popups and smooth scrolling keep working,
and it costs next to nothing. In any other terminal `zeit attach` works like
tmux, with the same sessions and keys.

## Simple first, more when wanted

Out of the box zeit is **as simple as tmux**: a prefix, splits, windows,
detach and attach, one quiet status line. Nothing new to learn, no config
needed (your tmux.conf keys are read as they are).

Everything more is configured **in Lua, like Neovim**: `~/.config/zeit/init.lua`.
Each feature is a module you `require` and `setup` with its options; what
you do not require does not exist (no cost, no keys taken). Plugins are
modules too, so a feature of zeit and a plugin look the same.

```lua
-- ~/.config/zeit/init.lua
local z = require('zeit')

-- optional: a preset as the starting point ('friendly' = the zellij-like set);
-- without one zeit is plain tmux. Presets are bundles, changed below line by line.
-- z.preset('friendly')

-- the features, as a list (like Doom's init.el): a word turns one on, +flags
-- switch its parts, a -- in front turns it off again
z.modules {
  'hintbar',                   -- modes with a bar showing the keys
  'frames +rounded +names',
  'floating +popout +overlay +blur',
  'welcome',
  'timeline +scrub',
  'palette',
  -- 'stacked',
  -- 'layouts +auto',
}

-- details, only where wanted: the same modules, configured
z.prefix('C-Space')                 -- or keep tmux.conf's

require('zeit.hintbar').setup()     -- (setup with nothing = the defaults)
require('zeit.frames').setup { names = true, rounded = true }
require('zeit.floating').setup {
  radius = 14, shadow = true, blur = 10,
  popout = { 'htop', 'lazygit' },   -- these always open as their own windows
  overlay = { key = 'M-`', edge = 'top' },  -- a drop-down over every app
}
require('zeit.welcome').setup()     -- the session manager at start
require('zeit.timeline').setup { keep = '30d', scrub = 'C-t' }
require('zeit.palette').setup { key = 'C-Space Space' }

z.on('pane_exit', function(p) if p.status ~= 0 then z.notify(p.title .. ' failed') end end)
z.map('n', '<prefix>g', function() z.float { cmd = 'lazygit', popout = true } end)
```

- The list is the quick way (one word per feature, flags with `+`); `setup{…}`
  is the detailed way; both may be used together, `setup` wins. Modules have
  defaults, so a word in the list is enough; every option reloads live.
- `:checkhealth`-style `zeit doctor` shows which modules are on, what they
  cost (memory, wakeups) and any errors in init.lua.
- Plugins come from a folder or a git URL, pinned in a lockfile (as zett's).
- The timeline is recorded whenever `zeit.timeline` is required; nothing of
  it exists otherwise.

## zeit's own idea: time

tmux and zellij are the bar for what must work; they are not the model.
zeit is built around what its name says: **time**.

- **Every pane has a timeline.** Shell marks cut a pane's life into
  commands: what ran, in which folder, when, for how long, how it ended, and
  what it printed. zeit keeps that, per pane, compressed, across reboots.
- **Scrub back.** One key turns a pane into its timeline: drag (or `h` / `l`)
  and the pane shows its screen as it was at that moment, an editor session,
  a build, a test run; Enter copies a command or its output, `r` reruns it in
  a new pane. Full-screen programs are recorded as frames too, so a Neovim or
  htop pane can be rewound like a video.
- **The day at a glance.** A strip across all sessions: when each pane was
  busy, which commands failed (red), which ran long; click a point to go
  there. "What was I doing yesterday at four" has an answer.
- **Places, not session names.** A session belongs to a folder: entering a
  project (or `zeit` in it) brings back its place, with its panes, running
  servers and timeline, exactly where it was left. Names are optional.
- **Panes that remember.** A pane knows the command it is for (a dev server,
  a log tail, a REPL). After a reboot those come back running; after a crash
  they restart, with the crash kept in the timeline.
- **Intent instead of modes.** One key opens zeit's command palette at the
  cursor (native in zash): type what you want ("split right", "logs", "kill
  the server on 3000", "go back to the failing test"), zeit does it and shows
  the shortcut for next time. Modes and a prefix exist for speed; nobody needs
  them to start.
- **Layouts by what runs.** zeit arranges a place from what is in it: the
  editor big, servers and watchers stacked at the side, a finished build
  folded to one line, a pane that needs attention brought forward. Fixed
  layouts stay possible; the default thinks.
- **Quiet by default.** No permanent status bar: things appear when they
  matter (a command finished in another pane, a server crashed, a pane wants
  input) and leave again; in zash as native notices at the pane's edge.

Everything below (tmux parity, the friendly parts) is the floor zeit stands
on; the timeline is what it is for.

## Why not tmux

tmux is a terminal inside a terminal. Every byte a program writes is parsed
by tmux, redrawn as escape codes, then parsed and drawn again by the outer
terminal. That costs CPU and latency, and whatever the outer terminal can do
that tmux does not pass on is lost: images, real popups, smooth scrolling,
shell marks, font sizes, the zash UI. Configuration is a language of its own,
copy mode is a second Vim, and nothing survives a reboot without plugins.

zeit keeps what makes tmux essential (sessions that live on, splits, windows,
keys under a prefix, scripting) and fixes the rest:

| | tmux | **zeit** |
|---|---|---|
| In zash | a second emulator between program and screen | **native**: zash draws the server's screens directly; one parse, no redraw |
| Cost | parse + re-encode + parse again | parse once; damage-only updates over shared memory (local) |
| Features of the outer terminal | lost unless passed through | **kept**: images, popups, smooth scroll, marks, the zash UI protocol |
| Survives | closing the terminal | closing the terminal, logout, **reboot** (layouts, folders, commands, scrollback restored) |
| Copy mode | its own key tables | zett-edit: Vim exactly as Neovim does it, `/` search, yank to the clipboard |
| Config | its own language | **Lua like Neovim** (`init.lua`, a module per feature) + your tmux.conf keys read as they are |
| Remote | ssh, then tmux on the far side, drawn as text | `zeit attach host`: the remote session drawn **natively** by zash |
| Overview | `choose-tree` text list | a dashboard with live previews of every pane |

## The pieces

```
zeitd          the server: owns ptys, emulation, scrollback, layouts, marks;
               one per user, started on demand or as a systemd user service;
               socket: $XDG_RUNTIME_DIR/zeit/<name>.sock (dir 0700)
zeit           the command line: attach, new, ls, kill, send-keys, capture,
               split, … (tmux's verbs, same meanings), and the text client
zash           the native client: shows zeit sessions like its own panes
zeit-core      crate: sessions, windows, panes, layout tree, pty handling,
               emulation (alacritty_terminal, shared with zash), scrollback,
               shell marks, persistence
zeit-proto     crate: the protocol between server and clients (versioned)
```

zash's own splits and tabs (built 2026-10-17, keys from tmux.conf) become the
local face of zeit: the same layout tree, the same key reader, so a pane in a
zash window and a pane in a zeit session behave the same.

## Concepts (tmux's, so nothing has to be relearned)

- **Server**: one per user and socket name (`-L name` for more, as in tmux).
- **Session**: a named set of windows; lives until killed. Several clients may
  attach at once (pairing, or the same session on two screens).
- **Window**: a tab; has a layout of panes.
- **Pane**: one pty + emulator + scrollback + shell marks.
- **Client**: a zash window, `zeit attach` in a terminal, or a control client
  (scripts, zett, the shell's widgets).

## Features

### tmux parity (nothing may be missing)

- Sessions: new, attach, detach (prefix d, or closing the window), list,
  rename, kill, switch, last session, nested sessions (prefix sent through).
- Windows: new (in the current folder), close, rename, move, swap, next /
  previous / last / by number, find by name, monitor activity / silence /
  bell with marks in the status line.
- Panes: split (side by side / stacked, in the current folder), close, zoom,
  resize (keys with repeat, mouse drag on borders), select by direction /
  number / last, swap, rotate, break out to a window, join into another,
  mark a pane, display numbers (prefix q), synchronize input (type into all),
  remain-on-exit and respawn, pipe a pane's output to a command or file.
- Layouts: even-horizontal, even-vertical, main-horizontal, main-vertical,
  tiled, custom saved layouts; cycle with a key.
- Copy mode: scrollback with zett-edit's Vim grammar (motions, text objects,
  visual / line / block, `/` `?` `n` `N`, marks), yank to the clipboard
  (Wayland and OSC 52), paste buffers with history (`choose-buffer`).
- Scrollback: per pane, size per session; search across all panes.
- Status line: left / centre / right parts, per-window labels, clock,
  host, the prefix indicator, git branch, Vitals (CPU, battery); styled with
  tinct; may be hidden; in zash drawn natively, not as a text row.
- Keys: a prefix (Ctrl+Space from your tmux.conf), key tables, `-n` (no
  prefix) and `-r` (repeat), the command prompt (prefix :) with completion,
  `bind` / `unbind` at run time; **tmux.conf's keys read as they are**.
- Mouse: select pane, resize borders, scroll, select text, middle paste,
  right-click menu.
- Terminal features passed through: true colour, styled / coloured
  underlines, OSC 8 links, OSC 52 clipboard, focus events, bracketed paste,
  Kitty keyboard protocol, synchronized output, cursor shapes, titles, bell,
  images (Kitty graphics and sixel kept per pane).
- Environment: `update-environment` (DISPLAY, WAYLAND_DISPLAY, SSH_AUTH_SOCK
  refreshed on attach), `ZEIT`, `ZEIT_PANE`, a TERM programs recognise
  (`tmux-256color`-compatible terminfo, so existing tools work).
- Scripting: every action is a command; `zeit <command>` from scripts
  (`send-keys`, `capture-pane` (text, with colours, or as JSON),
  `display-message -p` formats, `list-panes -F`), hooks (session created,
  pane exited, client attached, …), wait-for channels.
- Popups: `display-popup` (a floating pane over the layout), menus.
- `choose-tree` for sessions, windows and panes.

### Beyond tmux

- **Native in zash.** zash draws the server's screens itself (shared-memory
  grids and damage), with its GPU renderer: smooth scrolling, the cursor
  glide, the completion popup, prompt marks, Neovim mode, images. Detaching
  is just closing the window.
- **Survives reboots.** The server writes sessions down (layout, folders,
  the command each pane ran, scrollback compressed) and restores them on the
  next start: shells come back in their folders, editors reopen their files,
  long-running servers can be marked to restart.
- **A dashboard.** `zeit` with no arguments in zash, or prefix s: every
  session and pane as live miniatures, with what runs in it, its folder, git
  branch, CPU and memory (from Vitals); type to filter; Enter to attach.
- **Projects.** `zeit p [folder]` opens the session for a project (made from
  a template the first time: windows, splits, commands, like tmuxinator but in
  Lua or a plain table); zett opens a project's session next to its files.
- **Long commands tell you.** Shell marks know when a command ends; one in a
  background pane that ran longer than a minute sends a desktop notification
  (or a quiet mark in the status line), with its exit status.
- **Agents in panes.** Claude Code sessions in zeit are known to the shell's
  AI activity panel: jump to the pane that needs you.
- **Remote, natively.** `zeit attach user@host`: the server runs on the
  remote machine, the protocol goes over SSH, and zash draws it as if it were
  local (images, popups and all), with local echo prediction on slow links.
- **Share a session.** A read-only or full link for another user on the same
  machine (socket permissions), shown in the status line while active.
- **Lua.** Status-line parts, key actions, hooks and layouts in Lua, the same
  API style as zett.

### Friendly like zellij, fast like tmux

The things that make zellij pleasant to use, on zeit's cheaper core:

- **Modes with a hint bar.** Besides the prefix, zeit has modes: *pane*,
  *tab*, *resize*, *move*, *scroll*, *session*, and *locked* (all keys go to
  the program). The bar at the bottom always shows what the keys do right
  now, so nobody has to remember anything. In zash it is drawn natively (pills
  in tinct colours, a small which-key panel when a mode opens); in other
  terminals as one text row. Hidden with one setting once you know the keys.
- **Floating panes.** A pane that floats over the layout (toggle with a key),
  can be moved and resized with the mouse, pinned, or sent back into the
  layout. Scratch shells, a quick `htop`, a running build log.
- **Floating panes that can leave the terminal.** A floating pane is shown
  one of three ways, chosen per pane, per command or as a default:
  1. *inside*: drawn by zash over the layout but looking like a window of its
     own: rounded corners, a soft shadow, the text behind it blurred, a title
     bar with its name and buttons (close, back into the layout, pop out);
  2. *a window*: the pane pops out of zash as a real desktop window that
     floats over anything (the browser, a game launcher) and can be moved to
     another workspace or screen; it stays the same pane of the same session,
     and one key puts it back into the layout;
  3. *an overlay*: shown above every app on a key and hidden again (a
     drop-down or scratch terminal), on the screen edge or centre you choose.

  Everything about it is settable: size and place (or remembered per
  command), corner radius, shadow, blur, opacity, title bar or none, which
  commands always float (`htop`, a calculator, `lazygit`) and how. In a plain
  terminal an *inside* float is drawn with box lines.
- **Stacked panes.** Several panes in one slot, only the active one open, the
  others as one-line title bars; scroll through them. Many terminals in a
  small space.
- **Swap layouts.** A key cycles the window through ready layouts (columns,
  rows, main + stack, grid) that adapt to the number of panes.
- **A welcome screen and session manager.** Starting zeit without a session
  shows recent and running sessions, project folders and a field to name a new
  one; the same manager opens over any session (prefix w): switch, rename,
  kill, restore a dead one.
- **Layout files.** Sessions described as Lua tables (windows, panes, sizes,
  folders, commands, which pane floats); `zeit --layout dev.toml`; also the
  form saved sessions take, so a session can be saved as a layout.
- **Plugins in WASM.** Status-bar parts, pickers, a file tree, a git panel,
  written in Rust (or anything that builds to WASM), sandboxed, with
  permissions asked once (the same model as zett's plugins); small ones in
  Lua.
- **Pane names and frames.** Optional frames around panes with their names,
  the command running and its exit status; in zash as thin native borders
  with rounded corners, not box-drawing characters.
- **Web client, later.** A session in a browser tab (zett's web viewer
  draws the same protocol).

## The zash UI protocol

Programs today can only print characters into the grid. zash already does
more for its completion popup: the shell sends a private message (OSC 7733)
and zash draws a real overlay **on top of** the program, outside the grid.
That becomes a protocol any program can use (zeit, zett in the terminal, the
shell plugin, scripts), so their UI is really part of the window:

- **popup**: a list at a cell (items, selection, descriptions, icons)
- **overlay text**: styled text floating over cells, any size or font, not
  taking grid space (hints, inline errors, ghost text, labels)
- **notice**: a toast in the corner of the pane
- **panel**: a side or bottom panel with rows and actions
- **image**: by id, placed at a cell (with the Kitty protocol underneath)
- **status**: parts for zeit's status line drawn by zash

Every message has a text fallback, so the same program still works in a
plain terminal (and through zeit to a plain terminal): a popup becomes a
boxed list in the grid, a notice a line in the status bar.

## Protocol (server ⇄ client)

- Unix socket; length-prefixed binary frames (postcard / serde), versioned
  with a handshake; JSON for control clients.
- Local zash clients map pane grids through shared memory (memfd): the
  server writes cells and a damage list; the client reads only what changed.
- Text clients get escape-coded diffs, like tmux does, with a minimal redraw
  per frame.
- Input: keys and mouse in the client's terms; paste as one message.
- Every server action is a command; clients and scripts send the same ones.

## Performance budgets (on this laptop)

- Idle: 0 % CPU, no wakeups while nothing changes.
- Added latency in zash: under 0.5 ms per keystroke (local, shared memory).
- Throughput through zeit in zash: within 10 % of a plain zash pane; faster
  than tmux in any terminal (one parse instead of three).
- Attach to a session with 20 panes: under 50 ms to the first frame.
- Memory: under 2 MB per pane plus its scrollback (compressed when idle).
- Restore after reboot: layouts and shells back in under a second.

## Milestones

- **Z0 — sessions that outlive the window.** zeitd with sessions, windows,
  panes, layouts; zash attaches natively; closing zash detaches. Places
  (a session per folder) and the timeline recorded from the first day.
- **Z1 — zeit in any terminal.** The text client: `zeit attach`, status line,
  prefix keys from tmux.conf, mouse, the full CLI (`new`, `ls`, `kill`,
  `send-keys`, `capture-pane`, …). Modes with the hint bar.
- **Z1½ — friendly.** Floating and stacked panes, swap layouts, the welcome
  screen and session manager, layout files; floating panes inside, as their
  own desktop windows, and as overlays over every app.
- **Z2 — tmux parity.** Everything in "tmux parity" above: copy mode with
  zett-edit, layouts, swap / break / join, synchronize, popups, hooks,
  choose-tree, buffers, environment updates.
- **Z3 — time.** Scrubbing back through a pane, the day at a glance,
  panes that remember their command. Persistence and restore after reboot, scrollback on disk,
  pane templates and project sessions.
- **Z4 — the zash UI protocol** across zeit (popups, overlays, notices,
  panels, images) with text fallbacks.
- **Z5 — beyond.** Dashboard with live previews, notifications for long
  commands, agents, Vitals per pane, zett integration.
- **Z6 — remote.** `zeit attach host` over SSH with native drawing in zash and
  echo prediction; shared sessions.

## Testing

- The protocol: round trips, version mismatches, clients that vanish.
- Emulation: the same vttest screens as zash, through zeit, in zash and in a
  plain terminal (screens compared cell by cell).
- tmux behaviour: a script of tmux commands and the same in zeit, layouts and
  pane contents compared.
- Soak: a day of output through 50 panes, memory flat, no lost bytes.
- Restore: kill the server mid-work, restart, sessions back as they were.

## Decide at the start (hard to change later)

1. **Shells survive the server.** Each pane's pty is held by a tiny holder
   process (`zeit-pane`), not by zeitd itself; the server gets the pty back by
   fd passing. So a zeitd crash, or `zeit upgrade` to a new version, never
   kills a shell. tmux cannot do this; it must be in the design from day one.
2. **A versioned protocol** with a handshake and capabilities, from the first
   message: old clients keep working with new servers and the other way round.
3. **Stable ids.** Sessions, windows and panes get ids that survive restore
   and reboot (and names on top), so scripts, zett links and the timeline keep
   pointing at the right pane.
4. **One idea of character width.** Server, zash and the text client use the
   same Unicode width and grapheme tables (a shared crate); disagreeing about
   emoji or CJK widths is the classic source of garbled multiplexer screens.
5. **The timeline's storage format.** Append-only, per pane, compressed
   chunks indexed by time and command; a retention limit (days, size); a
   **private** flag per pane and patterns that are never kept (tokens,
   passwords printed by tools); optional encryption at rest. Written once,
   read for years: it must be versioned too.
6. **Each pane in its own cgroup** (a systemd user scope, as Vitals does):
   per-pane CPU and memory, freeze / thaw a pane, limits ("this build may use
   4 cores"), and kill everything a pane started, including what escaped
   into the background.
7. **Idle means idle.** No timers while nothing changes (the laptop's
   battery); status parts update on events, not every second.
8. **Sizes with several clients.** A decision per session: follow the
   latest client, the smallest, or let each client see its own size of the
   same panes.
9. **Security.** Socket directory 0700, no network listener ever; remote only
   through SSH; sharing only by an explicit grant that shows while active;
   OSC 52 clipboard writes from programs asked for or allowed per pane.
10. **Environment.** What is refreshed on attach (SSH_AUTH_SOCK, DISPLAY,
    WAYLAND_DISPLAY, …) and what a place keeps (its own variables, PATH
    additions), decided once.
11. **tmux compatibility as a promise.** The `zeit` verbs keep tmux's names;
    a `tmux` shim maps scripts that call tmux onto zeit, so tools built for
    tmux (vim-tmux-navigator, popups from scripts) keep working.
12. **A recorder for tests from the first day.** Every byte into and out of
    a pane can be recorded and replayed, so bugs come with a reproduction and
    every release is checked against hours of real sessions.
13. **Hooks into the desktop** planned, not bolted on: the zash UI protocol,
    notifications, the shell's widgets (sessions in the bar, the AI panel),
    zett links to panes and timeline moments, Vitals per pane.

## Decided

- Name: **zeit** (2026-10-17).
- Its own project (works without zash); shares the emulation core with zash.
- tmux's words and verbs, tmux.conf's keys; nothing to relearn.
- Rust, no C dependencies beyond libc.

## Open

- Restoring programs other than shells and editors after a reboot (which
  ones may safely be restarted?).
- Whether zash's own splits stay a separate (local-only) mode or always go
  through zeitd (simpler code if always; one more process if so).
