-- zeit: caesar's config (from ~/.config/tmux/tmux.conf)
local z = require('zeit')

z.tmux(false) -- this file is the whole story\; tmux.conf stays for tmux

z.modules {
  -- 'hintbar',        -- a line of key hints
  'timeline',
}

-- ── look ──────────────────────────────────────────────────────────────
z.set('default-shell', '/bin/zsh') -- the desktop's SHELL is bash
z.set('status-position', 'top')
z.set('status-style', 'fg=white,bg=default')
z.set('pane-border-style', 'fg=blue')
z.set('pane-active-border-style', 'fg=green')
z.set('history-limit', 20000)
z.set('monitor-activity', 'on')
-- cells kept blank at the status line's right end (window buttons a compositor floats there)
-- z.set('status-right-gap', 8)

-- the wallpaper's colours: tinct writes them for tmux, zeit reads the same file
local f = io.open(os.getenv('HOME') .. '/.config/tmux/tinct_colors.conf')
if f then
  for line in f:lines() do
    local name, value = line:match('set %-gq (@[%w_]+)%s+"([^"]+)"')
    if name then z.set(name, value) end
  end
  f:close()
end

local icons = {
  nvim = "\u{e62b} ",
  vim = "\u{e62b} ",
  hx = "\u{e62b} ",
  fish = "\u{f023a} ",
  zsh = "\u{f120} ",
  bash = "\u{f120} ",
  ssh = "\u{f08c0} ",
  git = "\u{f02a}2 ",
  lazygit = "\u{f02a}2 ",
  node = "\u{e718} ",
  python = "\u{e606} ",
  python3 = "\u{e606} ",
  cargo = "\u{e7a8} ",
  go = "\u{e626} ",
  docker = "\u{f308} ",
  btop = "\u{f0e7} ",
  htop = "\u{f0e7} ",
  yazi = "\u{f024b} ",
  lf = "\u{f024b} ",
  ranger = "\u{f024b} ",
  claude = "\u{f09d1} ",
  man = "\u{f02d} ",
  paru = "\u{f08c7} ",
  pacman = "\u{f08c7} ",
  mpv = "\u{f04b} ",
}
for cmd, icon in pairs(icons) do z.set('@icon:' .. cmd, icon) end

-- ── keys ──────────────────────────────────────────────────────────────
z.prefix('C-Space')

local P = function(key, cmd, opts) z.map('<prefix>' .. key, cmd, opts) end
local R = function(key, cmd) z.map(key, cmd) end -- no prefix

P('C-Space', 'last-window')
P('a', 'send-prefix')
P('r', 'source-file \\; display-message "Configuration reloaded"')
P('e', "new-window -n conf 'nvim ~/.config/zeit/init.lua'")
R('C-l', "send-keys C-l \\; clear-history")

-- panes
P('h', 'select-pane -L'); P('j', 'select-pane -D')
P('k', 'select-pane -U'); P('l', 'select-pane -R')
for key, dir in pairs { H = 'L', J = 'D', K = 'U', L = 'R' } do
  P(key, 'resize-pane -' .. dir .. ' 1', { repeats = true })
end
for key, dir in pairs { ['C-M-h'] = 'L', ['C-M-j'] = 'D', ['C-M-k'] = 'U', ['C-M-l'] = 'R' } do
  P(key, 'select-pane -' .. dir .. ' \\; swap-pane -t !', { repeats = true })
end
R('M-o', 'select-pane -t :.+ \\; resize-pane -Z')
R('M-j', 'select-pane -t :.+')
P('\\', 'split-window -h -c "#{pane_current_path}"')
P('-', 'split-window -v -c "#{pane_current_path}"')
P('|', 'split-window -fh -c "#{pane_current_path}"')
P('_', 'split-window -fv -c "#{pane_current_path}"')
P('x', 'kill-pane')

-- windows
P('c', 'new-window -c "#{pane_current_path}"')
P('C-c', 'new-window -c "#{pane_current_path}"')
P('&', 'kill-window')
R('M-h', 'select-window -t :-')
R('M-l', 'select-window -t :+')
R('M-Left', 'swap-window -t -1')
R('M-Right', 'swap-window -t +1')

-- copy mode and buffers
P('Escape', 'copy-mode')
P('p', 'paste-buffer')
P('+', 'delete-buffer \\; display-message "Deleted the newest buffer"')
P('<', 'run-shell "tmux set-buffer -- \\"$(wl-paste -n)\\""')
P('>', "run-shell 'tmux show-buffer | wl-copy'")
P('v', "capture-pane \\; save-buffer /tmp/zeit-buffer \\; new-window -n buffer 'nvim /tmp/zeit-buffer'")
R('PageUp', 'if-shell -F "#{alternate_on}" "send-keys PageUp" "copy-mode -e; send-keys PageUp"')

local C = function(key, cmd) z.map('<copy>' .. key, cmd) end
C('v', 'send-keys -X begin-selection')
C('V', 'send-keys -X select-line')
C('C-v', 'send-keys -X rectangle-toggle')
C('y', 'send-keys -X copy-pipe "~/.scripts/apps/tmux/tmux-clip-write"')
local search = function(key, name, cmd)
  C(key, "send-keys -X copy-selection \\; new-window -n " .. name ..
    " \\; send-keys -t " .. name .. " '" .. cmd .. " \"$(tmux show-buffer)\" && tmux kill-window' Enter")
end
search('o', 'google', 'sr google')
search('i', 'imagegoogle', 'sr imagesgoogle')
search('p', 'wikipedia', 'sr wikipedia')
search('O', 'w3m', 'w3m')
search('t', 'translate', 'trans es')
search('d', 'dict', 'sdcv -n')
C('s', 'send-keys -X copy-pipe espeak')

-- workspaces
P('M-m', "new-window -n mus -c $HOME \\; send-keys -t mus cmus Enter")
P('M-r', "new-window -n rss -c $HOME \\; send-keys -t rss newsboat Enter")
P('M-e', "new-window -n email -c $HOME \\; send-keys -t email mutt Enter")
P('M-c', "new-window -n chat -c $HOME \\; send-keys -t chat weechat-curses Enter \\; " ..
  "split-window -t chat -fh -p 27 \\; send-keys -t chat 'nvim -c \"set nowrap\" ~/.config/redpill/upcoming' Enter")

-- pickers and prompts
P('W', [[choose-tree -Zw "swap-window -t '%%'"]])
P('P', [[choose-tree -Zw "swap-pane -t '%%'"]])
P('C-M-w', [[command-prompt -p "Swap current window to? (e.g. 3)" "swap-window -t '%%'"]])
P('C-M-p', [[command-prompt -p "Swap current pane to? (e.g. 2)" "swap-pane -t '%%'"]])
P('C-p', [[choose-tree -Zw "move-pane -t '%%'"]])
P('M-p', [[command-prompt -p "Move current pane to? (e.g. 3)" "move-pane -t '%%'"]])
P('!', 'break-pane')

-- commands that run in another pane, leaving a line here:
-- ▸ the command … ✓ 3.2s; a click on it (or prefix Enter) unfolds the output
z.rule(function(cmd)
  for _, p in ipairs { '^cargo%s+build', '^cargo%s+test', '^cargo%s+check', '^cargo%s+clippy', '^make%f[%s]', '^make$' } do
    if cmd:match(p) then return { where = 'bottom', size = 30 } end
  end
end)
P('Enter', 'job-toggle')
