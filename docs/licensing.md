---
title: Licensing
description: What a Selkies installation contains besides Selkies, the license of each component, and where the GPL pieces come from.
---

## Selkies itself

Selkies is licensed under the [Mozilla Public License 2.0](https://github.com/selkies-project/selkies/blob/main/LICENSE)
(`LICENSE` in the repository, `license = "MPL-2.0"` in the workspace's
`Cargo.toml` and in `pyproject.toml`). MPL-2.0 is file-level copyleft: a
modified MPL file stays MPL when distributed, the larger work around it may be
under any license. The server's crates, the web client, the addons (Input Interposer, V4L2 interposer, fake-udev, coturn and
TURN REST helpers, universal touch gamepad), and the build and packaging scripts
all carry MPL-2.0 headers.

Categories used on this page: **copyleft** (GPL: the combined binary must be
distributed under the GPL), **weak copyleft** (LGPL and MPL: the covered files
stay under their license, and the larger work around them may be anything;
LGPL additionally requires the library stay replaceable), **permissive**
(attribution only: MIT, BSD, Apache-2.0, ISC, Zlib, PSF, ...).

## Where the GPL code is

A Selkies installation has no GPL code of its own. Two dependencies bring
GPL-licensed libraries into every release build:

| Source | GPL component | How it gets in | How to leave it out |
| --- | --- | --- | --- |
| `pixelflux` (screen capture and video encoding) | libx264 (GPL-2.0-or-later), which encodes H.264 in software wherever no GPU encoder is used: the `h264enc` fallback and `h264enc-striped` alike; and x265 (GPL-2.0-or-later) behind `h265enc` in the same case | the `gpl` Cargo feature of pixelflux, which the workspace's `Cargo.toml` turns on, so the server links libx264 and x265 in every release build | name pixelflux's `openh264` feature in place of `gpl` in the workspace's `Cargo.toml`: BSD-licensed Cisco OpenH264 and kvazaar take the places of libx264 and x265; VP8/VP9 (libvpx) and AV1 (SVT-AV1) are BSD-licensed in every build. The inventory and the check that keeps that build copyleft-free live in [pixelflux's LICENSES.md](https://github.com/selkies-project/pixelflux/blob/main/LICENSES.md). |

pixelflux and pcmflux encode and decode the video and audio through the codec
libraries they link, and the RTP layer packetizes their output through an
`EncodedPacket` container of its own, so the stack carries no FFmpeg library.

Everything else a Selkies installation links or loads is LGPL or permissive.
The container images also install the distribution's `ffmpeg`, a GPL build on
Debian and Ubuntu, for the desktop's own players and browsers, so an image is
distributed under GPL terms for that part as well.

## Rust dependencies

The crates the server links, as `Cargo.lock` pins them, with the license each
declares in its manifest. "How used" names the native libraries a crate links
or loads, which is where licenses other than the crate's own come in.

| Crate | License | Category | How used | Notes |
| --- | --- | --- | --- | --- |
| `pixelflux` | MPL-2.0 | weak copyleft | screen capture and video encoding; links libvpx, SVT-AV1, dav1d, OpenH264 (BSD), libde265 (LGPL-3.0-or-later), libgbm, libpixman, libxkbcommon (MIT); libx264 and x265 (GPL) in the default build, kvazaar (BSD) otherwise | see [pixelflux LICENSES.md](https://github.com/selkies-project/pixelflux/blob/main/LICENSES.md) |
| `pcmflux` | MPL-2.0 | weak copyleft | audio capture and Opus encoding; links libpulse (LGPL-2.1-or-later) and libopus (BSD-3-Clause) | see [pcmflux LICENSES.md](https://github.com/selkies-project/pcmflux/blob/main/LICENSES.md) |
| `selkies-rtc` | MPL-2.0 | weak copyleft | the WebRTC stack, in this repository under `crates/selkies-rtc` | built over the `rtc-*` crates of [webrtc-rs](https://github.com/webrtc-rs/rtc) (MIT OR Apache-2.0), `rtc-sctp` vendored (below) |
| `tokio`, `tokio-util` | MIT | permissive | async runtime | |
| `axum`, `axum-server`, `hyper`, `hyper-util`, `tower`, `tower-http`, `http`, `http-body-util` | MIT, or MIT OR Apache-2.0 | permissive | HTTP and WebSocket server | |
| `rustls`, `tokio-rustls`, `hyper-rustls`, `rustls-pemfile`, `rustls-native-certs` | Apache-2.0 OR ISC OR MIT | permissive | HTTPS, and the outgoing requests of the TURN REST and Cloudflare sources | cryptography from `ring` (Apache-2.0 AND ISC) and `aws-lc-rs`, which builds AWS-LC (ISC, Apache-2.0, MIT, BSD-3-Clause) |
| `rcgen` | MIT OR Apache-2.0 | permissive | self-signed certificates | |
| `x11rb` | MIT OR Apache-2.0 | permissive | X11 input injection (XTEST), the clipboard (XFixes), and the display's outputs (RandR) | speaks the X protocol itself |
| `xkbcommon` | MIT | permissive | keymaps | links libxkbcommon (MIT) |
| `libpulse-binding` | MIT OR Apache-2.0 | permissive | the sound-server control plane | links libpulse (LGPL-2.1-or-later) |
| `nvml-wrapper` | MIT OR Apache-2.0 | permissive | GPU statistics | opens the NVIDIA driver's libnvidia-ml (proprietary) at run time when present |
| `sysinfo` | MIT | permissive | system and process statistics | |
| `prometheus-client` | Apache-2.0 OR MIT | permissive | metrics endpoint | |
| `image` | MIT OR Apache-2.0 | permissive | clipboard images and cursors | |
| `zip`, `flate2` | MIT; MIT OR Apache-2.0 | permissive | folder downloads, compressed messages | |
| `rmp-serde` | MIT | permissive | MessagePack to the uinput mouse socket | |

Every other crate in the graph, the dependencies of pixelflux and pcmflux
included, is under permissive terms: MIT, Apache-2.0, BSD-2-Clause,
BSD-3-Clause, ISC, Zlib, Unicode-3.0, BSL-1.0, CC0-1.0, 0BSD, or the
Unlicense, most of them dual-licensed MIT OR Apache-2.0; `r-efi`, which
offers LGPL-2.1-or-later, offers MIT and Apache-2.0 beside it.

The distribution's PulseAudio or PipeWire, X servers, Wayland compositors, and
GPU drivers, and the Python interpreter a wheel is installed into, are outside
Selkies and keep their own licenses.

## Vendored code

| Component | Where | License | Category | Notes |
| --- | --- | --- | --- | --- |
| rtc-sctp 0.21.1 | `crates/selkies-rtc/vendor/rtc-sctp` | MIT OR Apache-2.0 (webrtc-rs), MPL-2.0 for the Selkies changes | permissive, weak copyleft for the changes | the workspace patches the published crate with it; `rtc-sctp-selkies.patch` beside it is the diff |
| Built web client | `dist/web` (generated by `scripts/ci/build-web.sh`, not committed), installed at `share/selkies/web` beside the executable | MPL-2.0 plus the npm runtime dependencies below | weak copyleft | |

## Web client (npm)

Licenses as reported by `license-checker` over the installed `node_modules` of
the two addons (production dependencies are what can end up in the built
client; development dependencies only run at build time and ship nothing).

| Addon | Runtime dependencies shipped in the build | Licenses | Build-only dependencies |
| --- | --- | --- | --- |
| `addons/selkies-web-core` | the bundle is Selkies' own MPL-2.0 code; `gendb.js` converts the SDL_GameControllerDB `gamecontrollerdb.txt` (Zlib) into the `jsdb/` mapping files at build time; libopus-wasm (MIT) with the libopus it compiles to WebAssembly (BSD-3-Clause) is copied into `codecs/libopus-wasm/`, its generated module renamed to `.js`, beside its `LICENSE` and `THIRD_PARTY_NOTICES.md`, for the browsers without WebCodecs audio | MPL-2.0 (Selkies), Zlib (mappings), MIT and BSD-3-Clause (libopus-wasm, libopus) | vite, vite-plugin-minify (declared under `dependencies`, used by the build only) and 45 transitive packages: MIT, BSD-2-Clause, BSD-3-Clause, ISC, Apache-2.0, CC0-1.0 (mdn-data), BlueOak-1.0.0 (sax), MPL-2.0 (lightningcss); no copyleft beyond MPL |
| `addons/selkies-dashboard` | react, react-dom, `@base-ui/react` (MIT), lucide-react (ISC), sonner, cn (MIT), class-variance-authority (Apache-2.0), js-yaml (MIT) with argparse (Python-2.0), `@fontsource-variable/inter` (SIL OFL-1.1, the Inter font files are bundled into the build), tw-animate-css (MIT), shadcn's `tailwind.css` (MIT, kept with its notice as `src/shadcn-tailwind.css`); `universalTouchGamepad.js` (MPL-2.0); the core above | MIT, ISC, Apache-2.0, OFL-1.1, Python-2.0, MPL-2.0 | vite, typescript, eslint, tailwindcss with lightningcss under MPL-2.0, and their transitive packages: MIT, ISC, Apache-2.0, BSD, 0BSD, BlueOak-1.0.0, CC0-1.0, CC-BY-4.0; no copyleft beyond MPL |

`addons/selkies-web-core/package.json` declares `"license": "MPL-2.0"`, the
license every file of the directory carries, and
`addons/universal-touch-gamepad/README.md` names the same license and the
repository `LICENSE` file; npm license scanners read the `package.json` field,
so it matches the sources.

## Native addons and helper images

| Component | License | Links or loads | Notes |
| --- | --- | --- | --- |
| Input Interposer (`addons/input-interposer`, `selkies_input_interposer.so`) | MPL-2.0 | libc, libdl | `LD_PRELOAD` library |
| V4L2 interposer (`addons/v4l2-interposer`) | MPL-2.0 | libc, libdl; `dlopen`s libpipewire-0.3 (MIT) when a PipeWire source is configured | shares its ring layout with pixelflux's `VirtualCamera` |
| fake-udev (`addons/fake-udev`) | MPL-2.0 (own `libudev.h` declarations, no systemd code) | libc, libdl; `dlopen`s the system libudev (LGPL) for everything but the virtual gamepads | replaces the LGPL libudev only by ABI, for virtual gamepads |
| coturn addon (`addons/coturn`) | MPL-2.0 scripts around the coturn image | runs coturn (BSD-3-Clause) | TURN server |
| TURN REST (`addons/turn-rest`) | MPL-2.0 | aiohttp | |

## The default is GPL-enabled

pixelflux builds with its GPL components on by default (libx264 for software
H.264, x265 for software H.265), and that is the supported default for every
deployment — the wheel, the native packages, the container images, and the
AppImage. Naming pixelflux's `openh264` feature in place of `gpl` in the
workspace's `Cargo.toml` is the opt-out for operators who need a copyleft-free
build; the sections below describe both.

## What a non-GPL deployment contains

With pixelflux's `openh264` feature in place of `gpl`: Selkies, pixelflux,
pcmflux, and the web client under the weak copyleft of MPL-2.0, the permissive
crates above, and the LGPL libraries they load (libpulse through pcmflux and
`libpulse-binding`, glibc, libde265 through pixelflux). Nothing
GPL, and nothing that reaches beyond its own files. The WebRTC stack
packetizes pixelflux/pcmflux output and brings in no media library of its own.

## What the default (GPL-enabled) deployment adds

An installed Selkies adds libx264 and x265 through the pixelflux it links,
both GPL-2.0-or-later, and the container images add the distribution's
`ffmpeg` on top, so a deployment that ships them is distributed under GPL terms
for those parts.

## How this is kept up to date

Selkies has no license gate of its own: the copyleft switch is pixelflux's, and
pixelflux's `scripts/check-licenses.py`, `deny.toml`, and `Licenses` workflow
enforce that its non-GPL build stays copyleft-free (pcmflux carries the same
cargo-deny check). This page is the inventory of what Selkies adds on top.
Regenerate the figures with `cargo metadata --format-version 1` (each
package's `license`) for the crates and `npx license-checker --summary --production` in each of the three
addon directories after `npm install`.
