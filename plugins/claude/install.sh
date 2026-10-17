#!/bin/sh
# zeit-claude: builds the helper, installs it and the zeit plugin. Claude Code's side: /plugin (see README).
set -e
cd "$(dirname "$0")"
cargo build --release -j "${JOBS:-4}" -p zeit-claude
target="${CARGO_TARGET_DIR:-../../target}/release/zeit-claude"
/usr/bin/install -Dm755 "$target" "$HOME/.local/bin/zeit-claude"
/usr/bin/install -Dm644 zeit/init.lua "${XDG_CONFIG_HOME:-$HOME/.config}/zeit/plugins/claude/init.lua"
echo ":: installed zeit-claude; in Claude Code: /plugin marketplace add $(pwd), then /plugin install zeit-claude@zeit"
