# ferry

Push the pointer off the edge of my Arch (KDE Plasma, Wayland) desktop and it carries on across the MacBook beside it, keyboard included. Push it back off the Mac's edge, or press the hotkey, and it comes home. Text and PNG clipboards come along on every switch.

The Mac sees the input as real hardware. It arrives through Karabiner's DriverKit virtual keyboard and mouse, so double clicks, drags, key repeat, window focus, password fields and the login window all behave as they would with a USB keyboard and mouse.

## How it works

```
Arch (user service)                                   Mac (root LaunchDaemon)
KWin ── InputCapture portal + libei ── ferry ═══ UDP, XChaCha20-Poly1305 ═══ ferry ── Karabiner VirtualHID ── macOS
            pointer barrier on the left edge          ~0.25 ms round trip on wired gigabit
```

- **Capture.** `ferry run` asks KDE's InputCapture portal for a pointer barrier along the outer edge of the desktop. When the pointer crosses it, KWin hands every input event to ferry, Meta and other shortcuts included, and hides the local cursor. Escape hatches: the hotkey, KWin's own Meta+Shift+Esc, and an automatic release when the Mac stops answering for 750 ms.
- **Hotkey.** Meta+Esc by default, registered as a KWin script shortcut (rebindable in System Settings → Shortcuts → KWin). On the desktop it carries input to the Mac. A private uinput mouse pushes the pointer into the barrier, which is the only way the portal starts a capture. On the Mac, the same keys bring it back, and the desktop cursor reappears exactly where it was.
- **Link.** One encrypted datagram per input frame, under a pre-shared 256-bit key. The Mac hands out a fresh session token on every handshake, and both sides reject replayed or reordered packets. Every key packet and heartbeat carries the full key and button state, so a lost packet can never leave a key stuck. If either side goes quiet, the Mac releases everything.
- **Injection.** The Mac daemon posts HID reports to Karabiner's virtual devices. It turns macOS pointer acceleration off for the virtual mouse (through a LinearMouse profile when LinearMouse runs), measures how many points one count moves, and keeps re-measuring during use. The result is that one desktop pixel moves the Mac cursor one point at every speed.
- **Edges.** The Mac cursor appears at the matching height on the Mac's right edge. It stays on the built-in display, even though the Mac also lists the desktop monitors' spare inputs as screens, and it parks just inside the edge when control goes back.
- **Clipboard.** On each switch, whichever side you leave pushes its clipboard (UTF-8 text or PNG; macOS TIFF is converted) over an authenticated TCP exchange. Unchanged content is never resent.

## Install

Both machines need the same key, `~/.config/ferry/key` on Arch and `/usr/local/etc/ferry/key` (root, mode 600) on the Mac.

Arch:

```sh
scripts/install-arch.sh --peer <mac-lan-ip> --peer <mac-tailnet-name>
```

The first start shows KDE's input capture dialog. Tick "Allow the application to do this without asking next time" and press Allow.

Mac (needs Karabiner-Elements or Karabiner-DriverKit-VirtualHIDDevice installed):

```sh
ssh mac 'sudo install -d -m 755 /usr/local/etc/ferry && sudo sh -c "umask 077; cat > /usr/local/etc/ferry/key"' < ~/.config/ferry/key
scripts/install-mac.sh
```

## Use

- Move the pointer off the left edge of the desktop, or press Meta+Esc.
- Come back by moving off the Mac's right edge, or press Meta+Esc again.
- `ferry status` shows the link, the Mac's virtual devices and the capture state.
- `ferry probe` times round trips to the Mac without touching local input. `--moves`, `--cross`, `--wheel 120`, `--shift` and `--clip <text>` exercise the injection, edge, scroll, keyboard and clipboard paths.

## Configuration

`~/.config/ferry/config.toml` on Arch:

| key | default | meaning |
| --- | --- | --- |
| `peers` | | Mac addresses tried in order; put the wired LAN address first |
| `side` | `"left"` | the desktop edge the Mac sits beyond |
| `hotkey` | `"Meta+Escape"` | re-run `ferry install` after changing it |
| `edge` | `true` | also switch by pushing off the desktop edge, and back off the Mac's; `false` makes the hotkey the only switch |
| `speed` | `1.0` | Mac points per desktop pixel |
| `natural_scroll` | `true` | match the desktop's natural-scrolling setting |
| `swap_alt_meta` | `false` | send Alt as Command and Meta as Option |
| `clipboard` | `true` | sync clipboards on every switch |

`/usr/local/etc/ferry/config.toml` on the Mac takes `listen` (default `0.0.0.0:47474`) and `confine` (`"builtin"` keeps the cursor on the MacBook's own screen; `"all"` allows every display).

## License

GPL-3.0-or-later
