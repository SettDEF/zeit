# zeit

A terminal multiplexer: your shells keep running, and remember.

- **Like tmux:** sessions, windows, splits, detach and attach, your tmux.conf
  prefix and bindings as they are, tmux's command names.
- **Shells outlive the server:** every pane's shell is held by its own small
  process, so a crash or an upgrade of zeit never kills it; a new server picks
  the panes up again with their screens and scrollback.
- **Places:** `zeit` in a folder brings back that folder's session.
- **Lua like Neovim:** `~/.config/zeit/init.lua` with a module list
  (`z.modules { 'hintbar', 'frames +names' }`), `setup{}`, `z.map`, `z.on`;
  plugins in `~/.config/zeit/plugins/`.
- **A timeline** of every pane's commands, recorded from the start.

```sh
zeit                        # the session of this folder
zeit new -s work            # a named session
zeit attach -t work         # back to it
zeit ls                     # sessions
zeit split-window -h        # tmux's verbs, also from scripts inside a pane
```

Install: `./install.sh` (to `~/.local/bin`). The plan is in PLAN.md.
