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
- **Paths.** The Mac reports its own addresses and what each sits on, and the desktop keeps them for next time. The link takes the best path that answers (a cable, then the first configured peer, then Wi-Fi, then the tailnet) and moves up whenever a better one comes back, so a new DHCP address or an unplugged adapter never strands it.
- **Wi-Fi.** On Wi-Fi the Mac's radio leaves its channel for AWDL (AirDrop, Universal Control, Sidecar) about twice a second and delivers 100 to 140 ms of input at once. Input is stamped with the desktop's clock so the Mac can tell late from on time. The desktop restates held keys every 10 ms and the Mac holds a key only while that keeps coming: one nobody vouches for is let go just before macOS's key repeat would start, so a release stuck in a stall never types a run of repeats. A late burst of motion glides out over a few frames instead of jumping, and clicks still land where they were aimed. `baton-pass status` counts the stalls.
- **Gaps.** A short silence releases held keys but keeps control; input returns to the desktop after 1.5 s and the link drops after 3 s. Each side counts only time its own loop was running.
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

Both machines must run the same release: each logs it when the other speaks an older or newer wire version. On first start, allow KDE's input capture dialog and tick "without asking". Pairing checks the code with SPAKE2 and puts one fresh key on both machines. `baton-pass uninstall` (with sudo on the Mac) removes it again; `--purge` also removes the key.

From source, `scripts/install-arch.sh --peer <mac-lan-ip>` and `scripts/install-mac.sh` build and install; the Mac script signs with a Developer ID when `~/Library/Keychains/baton-pass-signing.keychain-db` exists. With a Developer ID signature, a Full Disk Access grant for `/usr/local/bin/baton-pass` survives updates and lets files from Desktop, Documents and Downloads travel.

## Use

- `baton-pass status`: link, path and round trip, stalls in the last minute, the Mac's addresses and virtual devices, capture state, recent hiccups.
- `baton-pass probe`: tests the Mac side without touching local input (`--moves`, `--cross`, `--wheel 120`, `--shift`, `--taps 40`, `--clip <text>`). Stop the service first.

## Configuration

`~/.config/baton-pass/config.toml`:

| key | default | |
| --- | --- | --- |
| `peers` | | Mac addresses to start from, wired first; the Mac adds its own once linked |
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
