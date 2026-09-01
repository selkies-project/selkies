---
title: Usage
description: Keyboard shortcuts, clipboard, file transfers, the microphone and webcam uplinks, and the command-line options and environment variables.
---

## Shortcuts

**Fullscreen: `Control + Shift + F` or the Fullscreen Button**

**Gaming Mode (fullscreen with the pointer and keyboard held): `Control + Shift + X` or the Gaming Mode Button**

**Remote (Game) Cursor Lock: `Control + Shift + Left Click`**

**Open Side Menu: `Control + Shift + M` or the Side Menu Button**

**On-Screen Touch Gamepad: `Control + Shift + G`**

Fullscreen mode is available with the shortcut `Control + Shift + F`, or by pressing the fullscreen button in the side menu. It leaves the pointer and the keyboard to the browser, so the side menu and its button stay usable, and `Escape` leaves it.

Gaming mode, on `Control + Shift + X` or the gaming mode button, fullscreens the session and holds the pointer and the keyboard as well, so a game receives `Escape`, `Alt + Tab` and raw pointer motion instead of the browser. The dashboard folds away for it. The same chord leaves it, as does `Escape` held for at least two seconds, because a locked keyboard delivers a short press to the session instead.

The cursor can be locked into the web interface using `Control + Shift + Left Click` in web browsers supporting the Pointer Lock API. Press `Escape` to exit this remote cursor mode. This remote cursor capability is useful for most games or graphics applications where the cursor must be confined to the remote screen.

Locked movement is relayed to the remote desktop as-is, so the only acceleration applied to it is your own machine's. The client asks the browser for raw mouse movement to leave that curve out, which is what games and 3D applications expect; browsers offer it on Windows and macOS, and refuse it on Linux and Android, where locked movement keeps the local acceleration curve.

The side menu is available by clicking the small button on the right side of the interface, or by using the shortcut `Control + Shift + M`; gaming mode is the one mode that hides both.

`Control + Shift + G` toggles the on-screen touch gamepad overlay (the [Universal Touch Gamepad](component.md#universal-touch-gamepad)), which is also available from the side menu.

## Clipboard

Clipboard synchronization works in both directions and is supported across Chromium, Firefox, and Safari (a valid HTTPS context, or `localhost`, is still required by browsers).

- **Paste into the session:** `Control + V` (`Command + V` on macOS) sends your local clipboard to the remote session.
- **Copy from the session:** `Control + C` (`Command + C` on macOS) reads the remote session's current clipboard back to your browser. On Firefox and Safari the client requests the latest server clipboard and writes it once it arrives, falling back to a synchronous copy when the browser blocks the asynchronous clipboard API.

Image (binary) clipboard contents can also be transferred when binary clipboard support is enabled (see `enable_binary_clipboard`). Larger contents are sent in multiple parts automatically.

Clipboard behaviour is controlled by the server option `SELKIES_ENABLE_CLIPBOARD`/`--enable-clipboard`, which takes `true` (both directions), `in` (paste into session only), `out` (copy from session only), or `false`, plus `SELKIES_ENABLE_BINARY_CLIPBOARD`/`--enable-binary-clipboard` for the image clipboard. The client settings `clipboard_in_enabled` and `clipboard_out_enabled` are derived from that policy and can be toggled per browser within it.

## File Transfers

The side menu's files section uploads files into the session and browses the same directory for downloads. Both directions are on by default; `--file-transfers` (`SELKIES_FILE_TRANSFERS`) narrows them to `upload`, to `download`, or to `none`, and a read-only viewer is refused uploads whatever the setting says.

`--file-manager-path` (`FILE_MANAGER_PATH`, default `~/Desktop`) is the directory both directions use, created at startup when missing. Transfers are paced against the video stream so a large one does not stall the session (`--file-transfer-cc`, on by default); `--file-transfer-limit-mbps` adds a fixed cap for links whose rate that pacing cannot measure, such as behind a reverse proxy.

## Session Sharing

The side menu's sharing section hands out links to the running session. Each one is the page's own address with a fragment on the end, and it carries no credential of its own — whatever already guards the page (HTTP Basic authentication, a reverse proxy) guards the link too, so treat a copied link as one:

| Link | Fragment | What the holder can do |
| --- | --- | --- |
| Viewer | `#shared` | watch the session; no keyboard, mouse or gamepad |
| Player 2, 3 and 4 | `#player2`, `#player3`, `#player4` | watch, and drive the gamepad in that slot |

Input authority is enforced on the server rather than in the page, so a modified client cannot exceed its role: a viewer's keyboard, mouse, and settings messages are refused whatever it sends, and its gamepad messages are refused unless they drive the slot its own link carries — a `#shared` viewer holds none and drives no gamepad at all.

`--enable-sharing=false` (`SELKIES_ENABLE_SHARING`) turns the feature off, and one page then holds the session: a second one takes it over instead of joining. `--enable-shared` and `--enable-player2` through `--enable-player4` drop individual links, and `--ui-sidebar-show-sharing=false` hides the section while leaving the links working.

These fragments apply when the server has no master token. Under [Secure Mode](secure-mode.md) a client presents a provisioned session token that carries its own role and gamepad slot, and the sharing fragments are ignored.

## Microphone and Webcam

Both send a local device into the session, are off by default, and need a secure context (HTTPS, or `localhost`) before the browser hands the device over. Each is toggled from the side menu while the session runs.

- **Microphone** (`--microphone-enabled` / `SELKIES_MICROPHONE_ENABLED`) publishes the browser's microphone as an ordinary PulseAudio source in the session, so applications record it like any capture device. It rides the audio path, so `--audio-enabled=false` disables it as well.
- **Webcam** (`--webcam-enabled` / `SELKIES_WEBCAM_ENABLED`) publishes the browser's camera as a V4L2 capture device. How applications reach it, and what each of its sinks needs, is in [V4L2 Interposer](component.md#v4l2-interposer).

## Command-Line Options and Environment Variables

Use `selkies --help` for all command-line options, and `selkies --version` to print the installed version.

Every command-line option has a matching environment variable, formed by capitalizing the option and prepending `SELKIES_` (such as `SELKIES_VIDEO_BITRATE` for `--video-bitrate`). The [Settings Reference](settings.md) lists every setting with its flag, environment variables, type, and default; it is generated from [`src/selkies/settings.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/settings.py), where the settings are defined.

`SELKIES_VIDEO_BITRATE` is in **kilobits per second (kbps)**, range `100-1000000`, default `8000` (8 Mbps); no unit multiplier is applied, e.g. `4000` is 4 Mbps.

## Configuring Encoders, Display Capture, or Transport Protocols

[Components](component.md#encoders) lists every encoder, capture backend, audio path, and transport the runtime implements, with the setting that selects each one.
