# zeit

A terminal multiplexer: your shells keep running, and remember.

- **Like tmux:** sessions, windows, splits, detach and attach, your tmux.conf
  prefix and bindings as they are, tmux's command names.
- **Shells outlive the server:** every pane's shell is held by its own small
  process, so a crash or an upgrade of zeit never kills it; a new server picks
  the panes up again with their screens and scrollback.
- **Places:** `zeit` in a folder brings back that folder's session.
- **Lua like Neovim:** `~/.config/zeit/init.lua`: `z.modules{…}`, `z.prefix`,
  `z.map('<prefix>-', 'split-window -v')` (`<copy>` for copy mode), `z.set` for
  tmux's options (`status-position`, `@primary` …), `z.on` hooks; plugins in
  `~/.config/zeit/plugins/`. Without `z.tmux(false)` your tmux.conf is read
  too. `config/init.lua` is a full example (a tmux.conf, moved over), and a test.
- **tmux's commands, scripting included:** `\;` chains, `if-shell`, `run-shell`,
  formats (`#{pane_current_path}`), buffers, vi copy mode; inside zeit, `tmux`
  is zeit, so scripts written for tmux keep working.
- **Command rules:** `z.rule(function(cmd) if cmd:match('^cargo build') then
  return 'bottom' end end)` sends a command to another pane (`bottom`, `right`,
  `window`, with an optional `size`). Your pane keeps one line for it, the
  command on the left and its status on the right (`⟳ 4s`, `✓ 3.2s`,
  `✗ 101 · 3.2s`). A click on it, or `job-toggle`, unfolds the output under
  it. zsh learns this from zeit's own small integration (shell marks and the
  Enter hand-off); your ZDOTDIR and rc files are left as they are.
- **Upgrades without losing anything:** `zeit restart-server` starts the new
  binary; shells keep running and attached windows reconnect.
- **A timeline** of every pane's commands, recorded from the start.

```sh
zeit                        # the session of this folder
zeit new -s work            # a named session
zeit attach -t work         # back to it
zeit ls                     # sessions
zeit split-window -h        # tmux's verbs, also from scripts inside a pane
```

Install: `./install.sh` (to `~/.local/bin`). The plan is in PLAN.md.
