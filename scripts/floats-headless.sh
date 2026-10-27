#!/bin/sh
# Headless check of floats and zfloats (one kind since 2026-10-27): own runtime, state and config
# dirs, never the user's server. floats-headless.sh <zeit> [older zeit: saves the zfloat first]
NEW=$(realpath "$1")
OLD=$2
unset ZEIT ZEIT_PANE TMUX TMUX_PANE
export XDG_RUNTIME_DIR=/tmp/zt-of$$
export XDG_STATE_HOME=$XDG_RUNTIME_DIR/state
export ZEIT_CONFIG=$XDG_RUNTIME_DIR/conf
mkdir -p "$XDG_RUNTIME_DIR" "$ZEIT_CONFIG" && chmod 700 "$XDG_RUNTIME_DIR"
echo "runtime $XDG_RUNTIME_DIR"
socks() { ls "$XDG_RUNTIME_DIR/zeit/panes" 2>/dev/null | sort; }
ok() { if [ "$2" = "$3" ]; then echo "ok   $1: $2"; else echo "FAIL $1: got '$2' want '$3'"; fi; }
Z=$NEW
if [ -n "$OLD" ]; then
  Z=$OLD
fi
$Z new-session -d -s t
sleep 0.5
T=$($Z list-panes -F '#{pane_id}' | head -1 | cut -d: -f1)
echo "tiled pane $T"
d() { $Z display -p -t "$T" "$1"; }
before=$(socks)
$Z zfloat-toggle -n claude -- sleep 600
sleep 0.3
ok "named zfloat shown" "$(d '#{zfloat_shown} #{zfloat_name}')" "1 claude"
$Z zfloat-toggle -n claude
ok "named zfloat hidden" "$(d '#{zfloat_shown} #{zfloat_count}')" "0 1"
if [ -n "$OLD" ]; then
  # the old server goes without its panes (as a crash or an upgrade would leave them)
  for p in $(pgrep -f "^$OLD server" ) ; do
    if tr '\0' '\n' < /proc/$p/environ | grep -qx "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"; then kill "$p"; echo "old server $p ended"; fi
  done
  sleep 0.5
  rm -f "$XDG_RUNTIME_DIR/zeit/server"*
  Z=$NEW
  $Z list-sessions >/dev/null 2>&1
  sleep 0.8
  ok "old-format zfloat restored" "$(d '#{zfloat_count}')" "1"
fi
# two floats at once, the second on top
$Z float
$Z float -w 30 -h 8
sleep 0.3
ok "two floats, top" "$(d '#{zfloat_shown} #{zfloat_name}')" "1 float-2"
$Z float-next
ok "float-next" "$(d '#{zfloat_name}')" "float-1"
$Z toggle-floats
ok "M-f hides" "$(d '#{zfloat_shown}')" "0"
$Z toggle-floats
ok "M-f shows" "$(d '#{zfloat_shown} #{zfloat_name}')" "1 float-1"
# the named one over them, then the floats on another window
$Z zfloat-toggle -n claude
ok "claude over floats" "$(d '#{zfloat_name} #{zfloat_count}')" "claude 1"
$Z zfloat-toggle -n claude
ok "claude hidden, float back" "$(d '#{zfloat_name}')" "float-1"
# float-1 is a shell: it ends, it goes
$Z float-close
sleep 0.8
ok "float-close" "$(d '#{zfloat_name}')" "float-2"
# a float whose command ends (Enter after its last line closes it)
$Z float -- true
sleep 0.5
ok "command float" "$(d '#{zfloat_name}')" "float-1"
FP=$(socks | grep -vxF "$before")
for s in $FP; do id=${s%.sock}; $Z send-keys -t "%$id" Enter 2>/dev/null; done
sleep 0.8
ok "ended float gone (Enter)" "$(d '#{zfloat_name}')" "float-2"
# a float again, then a restart: named ones come back, floats do not
pre=$(socks)
$Z float
sleep 0.3
F=$(socks | grep -vxF "$pre" | head -1)
HP=$(pgrep -f "__hold ${F%.sock} ")
ok "the float's shell runs" "$(pgrep -P "$HP" | wc -l)" "1"
$Z restart-server
sleep 1.5
ok "restart keeps claude" "$(d '#{zfloat_count} #{zfloat_shown}')" "1 0"
$Z zfloat-toggle -n claude
ok "restored claude toggles" "$(d '#{zfloat_name}')" "claude"
ok "the float's shell ended with the restart" "$(pgrep -P "$HP" | wc -l)" "0"
$Z kill-server
sleep 0.5
# holders keep an ended program's output a while: end ours, by pid
for p in $(pgrep -f 'zeit'); do
  if tr '\0' '\n' < /proc/$p/environ 2>/dev/null | grep -qx "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"; then kill "$p"; fi
done
rm -rf "/tmp/zt-of$$"
