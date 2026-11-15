# Baton Pass

Hands my Arch desktop's mouse and keyboard to the MacBook beside it and back. Press Meta+Esc, or push the pointer off the screen edge, and the clipboard comes along.

The Mac sees real hardware: input arrives through Karabiner's DriverKit virtual keyboard and mouse, so drags, key repeat, password fields and the login window behave as they would with a USB keyboard and mouse.

```
Arch (user service)                                       Mac (root LaunchDaemon)
KWin InputCapture + libei ── baton-pass ══ encrypted UDP ══ baton-pass ── Karabiner VirtualHID
```

## How it works

- **Capture.** KDE's InputCapture portal hands over all input once the pointer crosses a barrier on the desktop edge; the hotkey pushes the pointer into it with a private uinput mouse. KWin's Meta+Shift+Esc always takes input back.
- **Link.** One XChaCha20-Poly1305 datagram per input frame, about 0.25 ms round trip on wired gigabit, with a fresh session per handshake and replay protection. Every packet carries the full key and button state, so nothing gets stuck.
- **Gaps.** A short silence releases held keys but keeps control; input returns to the desktop after 1.5 s and the link drops after 3 s. Each side counts only time its own loop was running. The link prefers the wired address and moves back to it from the tailnet.
- **Pointer.** One HID report per mouse report, several per 120 Hz frame, with acceleration off and a calibration that makes one desktop pixel one Mac point.
- **Clipboard.** Text, HTML, RTF and PNG both ways, and files and folders up to 32 MB between Dolphin and Finder.
- **Tray.** Shows which machine has the input and the last hiccup; click to switch.

## Install

From a [release](https://github.com/guitaripod/baton-pass/releases):

```sh
./baton-pass install --peer <mac-lan-ip>   # desktop: KDE Plasma 6 on Wayland
sudo ./baton-pass install                  # Mac, with Karabiner-Elements installed
sudo baton-pass pair                       # Mac: shows an eight-digit code
baton-pass pair <code>                     # desktop
```

On first start, allow KDE's input capture dialog and tick "without asking". Pairing checks the code with SPAKE2 and puts one fresh key on both machines. `baton-pass uninstall` (with sudo on the Mac) removes it again; `--purge` also removes the key.

From source, `scripts/install-arch.sh --peer <mac-lan-ip>` and `scripts/install-mac.sh` build and install; the Mac script signs with a Developer ID when `~/Library/Keychains/baton-pass-signing.keychain-db` exists. With a Developer ID signature, a Full Disk Access grant for `/usr/local/bin/baton-pass` survives updates and lets files from Desktop, Documents and Downloads travel.

## Use

- `baton-pass status`: link and round trip, the Mac's virtual devices, capture state, recent hiccups.
- `baton-pass probe`: tests the Mac side without touching local input (`--moves`, `--cross`, `--wheel 120`, `--shift`, `--clip <text>`). Stop the service first.

## Configuration

`~/.config/baton-pass/config.toml`:

| key | default | |
| --- | --- | --- |
| `peers` | | Mac addresses, tried in order (wired first) |
| `side` | `"left"` | the desktop edge the Mac sits beyond |
| `hotkey` | `"Meta+Escape"` | re-run `baton-pass install` after changing it |
| `edge` | `true` | also switch by pushing off the edge |
| `speed` | `1.0` | Mac points per desktop pixel |
| `natural_scroll` | `true` | match the desktop's scroll direction |
| `swap_alt_meta` | `false` | Alt as Command, Meta as Option |
| `clipboard` | `true` | sync clipboards on every switch |

On the Mac, `/usr/local/etc/baton-pass/config.toml` takes `listen` (`0.0.0.0:47474`) and `confine` (`"builtin"` or `"all"` displays).

## License

GPL-3.0-or-later
