#!/bin/sh
# Installs Baton Pass into ~/.cargo/bin and (re)installs the user service and KWin hotkey script.
# The first run needs the Mac's address: scripts/install-arch.sh --peer <lan-ip> --peer <tailnet-name>
set -eu
cd "$(dirname "$0")/.."
cargo install --locked --path .
"$HOME/.cargo/bin/baton-pass" install "$@"
"$HOME/.cargo/bin/baton-pass" --version
