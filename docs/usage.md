---
title: Usage
description: Keyboard shortcuts, clipboard behaviour, and the command-line options and environment variables.
---

## Shortcuts

**Fullscreen: `Control + Shift + F` or Fullscreen Button**

**Gaming Mode (fullscreen with the pointer and keyboard held): Gaming Mode Button**

**Remote (Game) Cursor Lock: `Ctrl + Shift + Left Click`**

**Open Side Menu: Ctrl + Shift + M or Side Button**

**On-Screen Touch Gamepad: `Control + Shift + G`**

Fullscreen mode is available with the shortcut `Control + Shift + F`, or by pressing the fullscreen button in the configuration menu. It leaves the pointer and the keyboard to the browser, so the side menu and its button stay usable, and `Escape` leaves it.

The gaming mode button fullscreens the session and holds the pointer and the keyboard as well, so a game receives `Escape`, `Alt + Tab` and raw pointer motion instead of the browser. The dashboard folds away for it, and `Escape` held for at least two seconds is what leaves it, because a locked keyboard delivers a short press to the session.

The cursor can be locked into the web interface using `Control + Shift + Left Click` in web browsers supporting the Pointer Lock API. Press `Escape` to exit this remote cursor mode. This remote cursor capability is useful for most games or graphics applications where the cursor must be confined to the remote screen.

Locked movement is relayed to the remote desktop as-is, so the only acceleration applied to it is your own machine's. The client asks the browser for raw mouse movement to leave that curve out, which is what games and 3D applications expect; browsers offer it on Windows and macOS, and refuse it on Linux and Android, where locked movement keeps the local acceleration curve.

The configuration menu is available by clicking the small button on the right side of the interface, or by using the shortcut `Control + Shift + M`; gaming mode is the one mode that hides both.

`Control + Shift + G` toggles the on-screen touch gamepad overlay (the [Universal Touch Gamepad](component.md#universal-touch-gamepad)), which is also available from the configuration menu.

## Clipboard

Clipboard synchronization works in both directions and is supported across Chromium, Firefox, and Safari (a valid HTTPS context, or `localhost`, is still required by browsers).

- **Paste into the session:** `Ctrl + V` (`Cmd + V` on macOS) sends your local clipboard to the remote session.
- **Copy from the session:** `Ctrl + C` (`Cmd + C` on macOS) reads the remote session's current clipboard back to your browser. On Firefox and Safari the client requests the latest server clipboard and writes it once it arrives, falling back to a synchronous copy when the browser blocks the asynchronous clipboard API.

Image (binary) clipboard contents can also be transferred when binary clipboard support is enabled (see `enable_binary_clipboard`). Larger contents are sent in multiple parts automatically.

Clipboard behaviour is controlled by the server option `SELKIES_ENABLE_CLIPBOARD`/`--enable-clipboard`, which takes `true` (both directions), `in` (paste into session only), `out` (copy from session only), or `false`, plus `SELKIES_ENABLE_BINARY_CLIPBOARD`/`--enable-binary-clipboard` for the image clipboard. The client settings `clipboard_in_enabled` and `clipboard_out_enabled` are derived from that policy and can be toggled per browser within it.

## Command-Line Options and Environment Variables

Use `selkies --help` for all command-line options, and `selkies --version` to print the installed version.

Every command-line option has a matching environment variable, formed by capitalizing the option and prepending `SELKIES_` (such as `SELKIES_VIDEO_BITRATE` for `--video-bitrate`). The [Settings Reference](settings.md) lists every setting with its flag, environment variables, type, and default; it is generated from [`src/selkies/settings.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/settings.py), where the settings are defined.

`SELKIES_VIDEO_BITRATE` is in **kilobits per second (kbps)**, range `100-1000000`, default `8000` (8 Mbps); no unit multiplier is applied, e.g. `4000` is 4 Mbps.

## Configuring Encoders, Display Capture, or Transport Protocols

[Components](component.md#encoders)

## CI/CD Build

We use Docker® containers for building every commit. The root directory [`Dockerfile`](https://github.com/selkies-project/selkies/tree/main/Dockerfile) and Dockerfiles within the [`addons`](https://github.com/selkies-project/selkies/tree/main/addons) directory provide directions for building each component, so that you may replicate the procedures in your own setup even without Docker® by copying the commands to your own shell.
