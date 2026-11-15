#!/bin/sh
# Builds Baton Pass on the Mac, signs it, and installs it as a root LaunchDaemon with
# `baton-pass install`. Run from anywhere inside the checkout.
set -eu
cd "$(dirname "$0")/.."

vhid_app="/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice/Applications/Karabiner-VirtualHIDDevice-Daemon.app"
if [ ! -d "$vhid_app" ]; then
    echo "Karabiner-DriverKit-VirtualHIDDevice is not installed (install Karabiner-Elements)" >&2
    exit 1
fi
git submodule update --init --depth 1
installed=$(defaults read "$vhid_app/Contents/Info" CFBundleShortVersionString)
pinned=$(git -C vendor/karabiner-vhid describe --tags 2>/dev/null || echo unknown)
if [ "v$installed" != "$pinned" ]; then
    echo "note: Karabiner virtual HID daemon is $installed, headers are $pinned; the desktop's \`baton-pass status\` reports a protocol mismatch if they disagree" >&2
fi
cargo build --release

# Signs with the Developer ID kept in its own keychain (unlocked with a password file, so this
# works over ssh, where the login keychain stays locked). A stable signature keeps the Full Disk
# Access grant across updates; without the identity the binary is signed ad hoc.
signing_keychain="$HOME/Library/Keychains/baton-pass-signing.keychain-db"
signing_password="$HOME/.appstoreconnect/private_keys/baton-pass-signing.password"
identity=""
if [ -f "$signing_keychain" ] && [ -f "$signing_password" ]; then
    security unlock-keychain -p "$(cat "$signing_password")" "$signing_keychain"
    identity=$(security find-identity -v -p codesigning "$signing_keychain" | awk -F'"' '/Developer ID Application/ {print $2; exit}')
fi
if [ -n "$identity" ]; then
    codesign --force --options runtime --timestamp --identifier dev.batonpass.BatonPass \
        --keychain "$signing_keychain" --sign "$identity" target/release/baton-pass
    echo "signed with $identity"
else
    codesign --force --sign - --identifier dev.batonpass.BatonPass target/release/baton-pass
    echo "no Developer ID identity; signed ad hoc" >&2
fi
sudo target/release/baton-pass install
/usr/local/bin/baton-pass --version
