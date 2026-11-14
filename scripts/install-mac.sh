#!/bin/sh
# Builds Baton Pass on the Mac and installs it as a root LaunchDaemon (root is what Karabiner's
# virtual HID daemon requires). Run from anywhere inside the checkout.
set -eu
cd "$(dirname "$0")/.."

vhid_app="/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice/Applications/Karabiner-VirtualHIDDevice-Daemon.app"
if [ ! -d "$vhid_app" ]; then
    echo "Karabiner-DriverKit-VirtualHIDDevice is not installed (install Karabiner-Elements)" >&2
    exit 1
fi
installed=$(defaults read "$vhid_app/Contents/Info" CFBundleShortVersionString)
pinned=$(git -C vendor/karabiner-vhid describe --tags 2>/dev/null || echo unknown)
if [ "v$installed" != "$pinned" ]; then
    echo "note: Karabiner virtual HID daemon is $installed, headers are $pinned; baton-pass status reports a protocol mismatch if they disagree" >&2
fi

git submodule update --init --depth 1
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
sudo install -d -m 755 /usr/local/bin /usr/local/etc/baton-pass
sudo install -m 755 target/release/baton-pass /usr/local/bin/baton-pass
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add /usr/local/bin/baton-pass >/dev/null
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp /usr/local/bin/baton-pass >/dev/null

if ! sudo test -f /usr/local/etc/baton-pass/key; then
    echo "no key yet: run \`sudo baton-pass pair\` here and \`baton-pass pair <code>\` on the desktop" >&2
fi

linearmouse="$HOME/.config/linearmouse/linearmouse.json"
if [ -f "$linearmouse" ]; then
    python3 - "$linearmouse" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path) as handle:
    config = json.load(handle)
schemes = config.setdefault("schemes", [])


def conditions(scheme):
    condition = scheme.get("if", [])
    return condition if isinstance(condition, list) else [condition]


ours = {"category": "mouse", "vendorID": "0x16c0", "productID": "0x27da"}
if not any(c.get("device", {}).get("productID", "").lower() == "0x27da" for s in schemes for c in conditions(s)):
    schemes.append({"if": {"device": ours}, "pointer": {"disableAcceleration": True}})
    with open(path, "w") as handle:
        handle.write(json.dumps(config, indent=2) + "\n")
    print("added a LinearMouse profile that keeps Baton Pass's virtual mouse free of acceleration")
PY
fi

plist=/Library/LaunchDaemons/dev.batonpass.BatonPass.plist
sudo tee "$plist" >/dev/null <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>dev.batonpass.BatonPass</string>
    <key>ProgramArguments</key><array><string>/usr/local/bin/baton-pass</string><string>run</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardErrorPath</key><string>/var/log/baton-pass.log</string>
</dict>
</plist>
PLIST
sudo chown root:wheel "$plist"
sudo chmod 644 "$plist"
echo "/var/log/baton-pass.log 644 3 1024 * J" | sudo tee /etc/newsyslog.d/baton-pass.conf >/dev/null

if sudo launchctl print system/dev.batonpass.BatonPass >/dev/null 2>&1; then
    sudo launchctl bootout system/dev.batonpass.BatonPass || true
    tries=0
    while sudo launchctl print system/dev.batonpass.BatonPass >/dev/null 2>&1 && [ "$tries" -lt 50 ]; do
        sleep 0.1
        tries=$((tries + 1))
    done
fi
sudo launchctl bootstrap system "$plist"
sleep 1
sudo launchctl print system/dev.batonpass.BatonPass | awk '/state =|pid =/ {print "baton-pass: " $0}'
/usr/local/bin/baton-pass --version
