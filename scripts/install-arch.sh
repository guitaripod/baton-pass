#!/bin/sh
# Installs ferry into ~/.cargo/bin and (re)installs the user service and KWin hotkey script.
# The first run needs the Mac's address: scripts/install-arch.sh --peer <lan-ip> --peer <tailnet-name>
set -eu
cd "$(dirname "$0")/.."
cargo install --locked --path .
"$HOME/.cargo/bin/ferry" install "$@"
"$HOME/.cargo/bin/ferry" --version
