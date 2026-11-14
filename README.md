# Baton Pass

Push the pointer off the edge of my Arch (KDE Plasma, Wayland) desktop and it carries on across the MacBook beside it, keyboard included. Push it back off the Mac's edge, or press the hotkey, and it comes home. Text and PNG clipboards come along on every switch.

The Mac sees the input as real hardware. It arrives through Karabiner's DriverKit virtual keyboard and mouse, so double clicks, drags, key repeat, window focus, password fields and the login window all behave as they would with a USB keyboard and mouse.

## How it works

```
Arch (user service)                                   Mac (root LaunchDaemon)
KWin ── InputCapture portal + libei ── Baton Pass ═══ UDP, XChaCha20-Poly1305 ═══ Baton Pass ── Karabiner VirtualHID ── macOS
            pointer barrier on the left edge          ~0.25 ms round trip on wired gigabit
```

- **Capture.** `baton-pass run` asks KDE's InputCapture portal for a pointer barrier along the outer edge of the desktop. When the pointer crosses it, KWin hands every input event to Baton Pass, Meta and other shortcuts included, and hides the local cursor. Escape hatches: the hotkey, KWin's own Meta+Shift+Esc, and an automatic release when the Mac stops answering for 1.5 s.
- **Hotkey.** Meta+Esc by default, registered as a KWin script shortcut (rebindable in System Settings → Shortcuts → KWin). On the desktop it carries input to the Mac. A private uinput mouse pushes the pointer into the barrier, which is the only way the portal starts a capture. On the Mac, the same keys bring it back, and the desktop cursor reappears exactly where it was.
- **Link.** One encrypted datagram per input frame, under a pre-shared 256-bit key. The Mac hands out a fresh session token on every handshake, and both sides reject replayed or reordered packets. Every key packet and heartbeat carries the full key and button state, so a lost packet can never leave a key stuck.
- **Riding out gaps.** Each side counts the other's silence only over time its own loop was running, and reads every queued packet before judging it, so a stall on one machine is never blamed on the other. After half a second of silence the Mac lets go of held keys and buttons but keeps control, picking up again with the next packet; the desktop takes input back after 1.5 s, and the link drops after 3 s. The desktop greets the wired address first and moves back to it from the tailnet whenever it answers again while nothing is captured. Stalls over 150 ms and handlers holding a loop for over 50 ms are logged with their cause. `cargo test` drives the real link through a relay that loses, duplicates, reorders and blacks out packets.
- **Injection.** The Mac daemon posts HID reports to Karabiner's virtual devices, one per mouse report on the desktop, so the cursor gets several fresh positions in every frame of a 120 Hz ProMotion display. It turns macOS pointer acceleration off for the virtual mouse (through a LinearMouse profile when LinearMouse runs), measures how many points one count moves, and keeps watching during use, adjusting only when that ratio really changes, never with measurement noise. The result is that one desktop pixel moves the Mac cursor one point at every speed, at a constant gain.
- **Edges.** The Mac cursor appears at the matching height on the Mac's right edge. It stays on the built-in display, even though the Mac also lists the desktop monitors' spare inputs as screens, and it parks just inside the edge when control goes back.
- **Clipboard.** On each switch, whichever side you leave pushes its clipboard over an authenticated TCP exchange, in every format the other side understands: HTML and RTF with the plain text, and PNG images (macOS TIFF is converted). Files copied in a file manager on the desktop (up to 32 MB, folders included) arrive on the Mac's pasteboard ready to paste in Finder. Neither side sends back what it just received. The desktop clipboard is read and served over Wayland's data-control protocol, so wl-clipboard is not needed.
- **Tray.** A system tray icon shows which machine has the mouse and keyboard and whether the Mac is linked, and its tooltip names the last hiccup. A click switches.

## Install

Arch:

```sh
scripts/install-arch.sh --peer <mac-lan-ip> --peer <mac-tailnet-name>
```

The first start shows KDE's input capture dialog. Tick "Allow the application to do this without asking next time" and press Allow.

Mac (needs Karabiner-Elements or Karabiner-DriverKit-VirtualHIDDevice installed):

```sh
scripts/install-mac.sh
```

It signs the binary with a Developer ID (hardened runtime, secure timestamp) when `~/Library/Keychains/baton-pass-signing.keychain-db` and its password file in `~/.appstoreconnect/private_keys` exist, and ad hoc otherwise. With a Developer ID signature, a Full Disk Access grant for `/usr/local/bin/baton-pass` survives updates; it is what lets files copied from Desktop, Documents or Downloads travel.

Then pair the two, which puts one fresh key on both (`~/.config/baton-pass/key` on Arch, `/usr/local/etc/baton-pass/key` on the Mac, mode 600). On the Mac:

```sh
sudo baton-pass pair
```

It shows an eight-digit code and waits three minutes. On Arch:

```sh
baton-pass pair <code>
```

The code is checked with SPAKE2, so watching the network does not help anyone guess it, and one wrong code closes the window. Both daemons restart on the new key.

## Use

- Move the pointer off the left edge of the desktop, or press Meta+Esc.
- Come back by moving off the Mac's right edge, or press Meta+Esc again.
- `baton-pass status` shows the link and its round trip, the Mac's virtual devices, the capture state and recent hiccups.
- `baton-pass probe` times round trips to the Mac without touching local input. `--moves`, `--cross`, `--wheel 120`, `--shift` and `--clip <text>` exercise the injection, edge, scroll, keyboard and clipboard paths.

## Configuration

`~/.config/baton-pass/config.toml` on Arch:

| key | default | meaning |
| --- | --- | --- |
| `peers` | | Mac addresses tried in order; put the wired LAN address first |
| `side` | `"left"` | the desktop edge the Mac sits beyond |
| `hotkey` | `"Meta+Escape"` | re-run `baton-pass install` after changing it |
| `edge` | `true` | also switch by pushing off the desktop edge, and back off the Mac's; `false` makes the hotkey the only switch |
| `speed` | `1.0` | Mac points per desktop pixel |
| `natural_scroll` | `true` | match the desktop's natural-scrolling setting |
| `swap_alt_meta` | `false` | send Alt as Command and Meta as Option |
| `clipboard` | `true` | sync clipboards on every switch |

`/usr/local/etc/baton-pass/config.toml` on the Mac takes `listen` (default `0.0.0.0:47474`) and `confine` (`"builtin"` keeps the cursor on the MacBook's own screen; `"all"` allows every display).

## License

GPL-3.0-or-later
