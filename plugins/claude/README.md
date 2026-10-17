# zeit-claude

A plugin for zeit and Claude Code, not a part of zeit: it talks to zeit only through the
`zeit` command.

- **What Claude runs, live beside it.** Run Claude Code in a zeit pane as usual; each command
  it runs (and its last output lines), each edit, read and search shows in a zfloat docked on
  the right, by itself. The keys stay with Claude. prefix C hides and shows the view.
- **Claude opens things itself** (MCP tools): `show_in_zfloat` (a command in a zfloat: a
  build, logs, htop), `open_web` (a page drawn by zash), `open_float` (a floating pane),
  `run_job` (a zeit job in the status bar), `read_pane`, `list_panes`.

Outside zeit everything is silent.

## Settings

The view is the zfloat `claude`; its settings are zeit's (the plugin's `zeit/init.lua` sets
them, your `init.lua` can change any):

    z.zfloat('claude', { width = 25, side = 'left', title = 'AI', buttons = false })

Every zfloat can have its own (`z.zfloat(name, {…})`): width, height, side, below (keeps
the keys where they are), title, buttons. A drag or resize with the mouse holds until the
config changes.

## Install

    ./install.sh                                   # zeit-claude and the zeit plugin
    # then in Claude Code:
    /plugin marketplace add /mnt/storage/dev/projects/zeit/plugins/claude
    /plugin install zeit-claude@zeit

Start a new Claude Code in a zeit pane (hooks and MCP servers load when Claude starts).
