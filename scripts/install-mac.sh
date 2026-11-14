#!/bin/sh
# Builds ferry on the Mac and installs it as a root LaunchDaemon (root is what Karabiner's
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
    echo "note: Karabiner virtual HID daemon is $installed, headers are $pinned; ferry status reports a protocol mismatch if they disagree" >&2
fi

git submodule update --init --depth 1
cargo build --release
sudo install -d -m 755 /usr/local/bin /usr/local/etc/ferry
sudo install -m 755 target/release/ferry /usr/local/bin/ferry
identity=$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development/ {print $2; exit}')
if [ -n "$identity" ] && sudo codesign -f -s "$identity" /usr/local/bin/ferry 2>/dev/null; then
    echo "signed with $identity"
else
    sudo codesign -f -s - /usr/local/bin/ferry
fi
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add /usr/local/bin/ferry >/dev/null
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp /usr/local/bin/ferry >/dev/null

if ! sudo test -f /usr/local/etc/ferry/key; then
    echo "no key yet: run \`sudo ferry pair\` here and \`ferry pair <code>\` on the desktop" >&2
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
    print("added a LinearMouse profile that keeps ferry's virtual mouse free of acceleration")
PY
fi

plist=/Library/LaunchDaemons/dev.ferry.ferry.plist
sudo tee "$plist" >/dev/null <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>dev.ferry.ferry</string>
    <key>ProgramArguments</key><array><string>/usr/local/bin/ferry</string><string>run</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardErrorPath</key><string>/var/log/ferry.log</string>
</dict>
</plist>
PLIST
sudo chown root:wheel "$plist"
sudo chmod 644 "$plist"
echo "/var/log/ferry.log 644 3 1024 * J" | sudo tee /etc/newsyslog.d/ferry.conf >/dev/null

if sudo launchctl print system/dev.ferry.ferry >/dev/null 2>&1; then
    sudo launchctl bootout system/dev.ferry.ferry || true
    tries=0
    while sudo launchctl print system/dev.ferry.ferry >/dev/null 2>&1 && [ "$tries" -lt 50 ]; do
        sleep 0.1
        tries=$((tries + 1))
    done
fi
sudo launchctl bootstrap system "$plist"
sleep 1
sudo launchctl print system/dev.ferry.ferry | awk '/state =|pid =/ {print "ferry: " $0}'
/usr/local/bin/ferry --version
