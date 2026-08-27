---
title: Licensing
description: What a Selkies installation contains besides Selkies, the license of each component, and where the GPL pieces come from.
---

## Selkies itself

Selkies is licensed under the [Mozilla Public License 2.0](https://github.com/selkies-project/selkies/blob/main/LICENSE)
(`LICENSE` in the repository, `license = "MPL-2.0"` in `pyproject.toml`). MPL-2.0
is file-level copyleft: a modified MPL file stays MPL when distributed, the
larger work around it may be under any license. The Python package, the web
client, the addons (Joystick Interposer, V4L2 interposer, fake-udev, coturn and
TURN REST helpers, universal touch gamepad) and the build and packaging scripts
all carry MPL-2.0 headers.

Categories used on this page: **copyleft** (GPL: the combined binary must be
distributed under the GPL), **weak copyleft** (LGPL: the library stays under
its license and must remain replaceable, dynamic linking is fine),
**permissive** (attribution only: MIT, BSD, Apache-2.0, ISC, Zlib, MPL, PSF,
...).

## Where the GPL code is

A Selkies installation has no GPL code of its own. Two dependencies bring
GPL-licensed libraries into an installation made from PyPI wheels:

| Source | GPL component | How it gets in | How to leave it out |
| --- | --- | --- | --- |
| `pixelflux` (screen capture and video encoding) | libx264 (GPL-2.0-or-later), the striped software H.264 encoder | the default `gpl` Cargo feature of pixelflux; the published wheels are built with it and bundle `libx264.so` | build pixelflux with `PIXELFLUX_ENABLE_GPL=0` (`--no-default-features --features openh264`): BSD-licensed Cisco OpenH264 takes the place of libx264. The inventory and the check that keeps that build copyleft-free live in [pixelflux's LICENSES.md](https://github.com/selkies-project/pixelflux/blob/main/LICENSES.md); pixelflux's musllinux wheels additionally bundle Alpine's GPL-built FFmpeg, see there. |

The WebRTC stack adds no FFmpeg of its own: pixelflux and pcmflux encode and
decode the video and audio, and the RTP layer packetizes their output through
a libav-free `EncodedPacket` container.

Everything else a Selkies installation links or loads is LGPL or permissive.
The container images also install the distribution's `ffmpeg` and `x264`
packages, which are GPL builds on Debian and Ubuntu; pixelflux links that
libavcodec for VA-API.

## Python dependencies

`pyproject.toml` dependencies, with the license from the installed
distribution metadata (`importlib.metadata`). "How used" names native code the
wheel bundles or loads, which is where licenses other than the package's own
come in.

| Package | License | Category | How used | Notes |
| --- | --- | --- | --- | --- |
| `pixelflux` | MPL-2.0 | permissive | PyO3 extension; links FFmpeg (LGPL-2.1-or-later as built for its manylinux wheels), libgbm, libpixman, libxkbcommon (MIT); libx264 (GPL) in the default build | see [pixelflux LICENSES.md](https://github.com/selkies-project/pixelflux/blob/main/LICENSES.md) |
| `pcmflux` | MPL-2.0 | permissive | PyO3 extension; links libpulse (LGPL-2.1-or-later) and libopus (BSD-3-Clause), bundled into its wheels with libpulse's LGPL/permissive dependency tree | see [pcmflux LICENSES.md](https://github.com/selkies-project/pcmflux/blob/main/LICENSES.md) |
| `aiohttp` | Apache-2.0 AND MIT | permissive | HTTP and WebSocket server | bundles llhttp (MIT); pulls `aiohappyeyeballs` (PSF-2.0), `aiosignal`, `frozenlist`, `multidict`, `propcache`, `yarl` (Apache-2.0), `attrs` (MIT), `idna` (BSD-3-Clause) |
| `aiofiles` | Apache-2.0 | permissive | file uploads and downloads | |
| `cryptography` | Apache-2.0 OR BSD-3-Clause | permissive | DTLS/SRTP keys, self-signed certificates | bundles OpenSSL (Apache-2.0); `cffi` (MIT), `pycparser` (BSD-3-Clause) |
| `pyOpenSSL` | Apache-2.0 | permissive | DTLS transport | |
| `pylibsrtp` | BSD-3-Clause | permissive | SRTP | bundles libsrtp2 (BSD-3-Clause) |
| `google-crc32c` | Apache-2.0 | permissive | SCTP checksums | bundles crc32c (BSD-3-Clause) |
| `dnspython` | ISC | permissive | mDNS ICE candidates | |
| `ifaddr` | MIT | permissive | interface enumeration for ICE | |
| `pyee` | MIT | permissive | event emitter of the WebRTC stack | `typing_extensions` (PSF-2.0) |
| `uvloop` | MIT OR Apache-2.0 | permissive | event loop (Linux, macOS) | bundles libuv (MIT) |
| `msgpack` | Apache-2.0 | permissive | control-channel encoding | |
| `psutil` | BSD-3-Clause | permissive | system and process statistics | |
| `watchdog` | Apache-2.0 | permissive | file-change notifications | |
| `Pillow` | MIT-CMU (HPND) | permissive | clipboard images, cursors, icons | bundles libjpeg-turbo (IJG/BSD-3-Clause/Zlib), libpng, libtiff, libwebp, openjpeg, libavif, freetype (FTL or GPL-2.0-or-later, dual), harfbuzz, lcms2, brotli, zstd (BSD-3-Clause or GPL-2.0, dual), xz, xcb: all usable under permissive terms |
| `prometheus_client` | Apache-2.0 AND BSD-2-Clause | permissive | metrics endpoint | |
| `pulsectl-asyncio` | MIT | permissive | microphone routing through PulseAudio | `pulsectl` (MIT) opens the host's libpulse (LGPL-2.1-or-later) through ctypes at run time |
| `nvidia-ml-py` | BSD-3-Clause | permissive | GPU statistics | opens the NVIDIA driver's libnvidia-ml (proprietary) through ctypes at run time when present |
| `aitop` | MIT | permissive | GPU statistics | |
| `importlib_resources` (Python 3.9 only) | Apache-2.0 | permissive | package data on 3.9 | |

The Python interpreter itself (PSF-2.0) and, when installed, the distribution's
PulseAudio or PipeWire, X servers, Wayland compositors and GPU drivers are
outside the package and keep their own licenses.

## Vendored code in the Python package

| Component | Where | License | Category | Notes |
| --- | --- | --- | --- | --- |
| python-xlib 0.33 fork | `src/selkies/Xlib` | LGPL-2.1-or-later (every module header: "version 2.1 of the License, or (at your option) any later version") | weak copyleft | Imported, not modified in license terms; the wheel ships `selkies/Xlib/LICENSE` (`tool.setuptools.package-data`) as the LGPL-3.0 text: the headers grant "version 2.1 or (at your option) any later version", and the fork is carried under LGPL-3.0 for compatibility with the MPL-2.0 of the rest of Selkies. The Python sources are the preferred form, so the LGPL's replaceability requirement is met as is. |
| aiortc and aioice forks | `src/selkies/webrtc`, `src/selkies/ice` | BSD-3-Clause (Jeremy Lainé), MPL-2.0 for the Selkies changes | permissive | every file keeps the upstream copyright and license notice under the MPL header |
| Built web client | `src/selkies/selkies_web` (generated by `scripts/ci/build-web.sh`, not in git) | MPL-2.0 plus the npm runtime dependencies below | permissive | |

## Web client (npm)

Licenses as reported by `license-checker` over the installed `node_modules` of
the three addons (production dependencies are what can end up in the built
client; development dependencies only run at build time and ship nothing).

| Addon | Runtime dependencies shipped in the build | Licenses | Build-only dependencies |
| --- | --- | --- | --- |
| `addons/selkies-web-core` | none: the bundle is Selkies' own MPL-2.0 code; `gendb.js` converts the SDL_GameControllerDB `gamecontrollerdb.txt` (Zlib) into the `jsdb/` mapping files at build time | MPL-2.0 (Selkies), Zlib (mappings) | vite, vite-plugin-minify, vite-plugin-env-compatible (declared under `dependencies`, used by the build only) and 47 transitive packages: MIT, BSD-2-Clause, ISC, Apache-2.0, CC0-1.0 (mdn-data), BlueOak-1.0.0 (sax), MPL-2.0 (lightningcss); no copyleft beyond MPL |
| `addons/selkies-dashboard` | react, react-dom (MIT), js-yaml (MIT) with argparse (Python-2.0); `universalTouchGamepad.js` (MPL-2.0); the core above | MIT, Python-2.0, MPL-2.0 | 159 packages: MIT, BSD-2-Clause, Apache-2.0, ISC, BSD-3-Clause, MPL-2.0, CC0-1.0, BlueOak-1.0.0, Python-2.0, CC-BY-4.0 (caniuse-lite data) |
| `addons/selkies-dashboard-wish` | react, react-dom, radix-ui and the `@radix-ui/*` primitives, framer-motion, recharts (with d3, ISC), lucide-react (ISC), sonner, next-themes, tailwind-merge, clsx (MIT), class-variance-authority (Apache-2.0), js-yaml (MIT) with argparse (Python-2.0), `@fontsource-variable/inter` (SIL OFL-1.1, the Inter font files are bundled into the build), tw-animate-css (MIT); `universalTouchGamepad.js` (MPL-2.0); the core above | MIT, ISC, Apache-2.0, BSD-3-Clause, BSD-2-Clause, 0BSD, OFL-1.1, Python-2.0, MPL-2.0 | 390 packages (vite, typescript, eslint, tailwindcss with lightningcss under MPL-2.0, shadcn, ...): MIT, ISC, Apache-2.0, BSD, 0BSD, BlueOak-1.0.0, CC0-1.0, CC-BY-4.0; no copyleft beyond MPL |

`addons/selkies-web-core/package.json` declares `"license": "MPL-2.0"`, the
license every file of the directory carries, and
`addons/universal-touch-gamepad/README.md` names the same license and the
repository `LICENSE` file; npm license scanners read the `package.json` field,
so it matches the sources.

## Native addons and helper images

| Component | License | Links or loads | Notes |
| --- | --- | --- | --- |
| Joystick Interposer (`addons/js-interposer`, `selkies_joystick_interposer.so`) | MPL-2.0 | libc, libdl | `LD_PRELOAD` library |
| V4L2 interposer (`addons/v4l2-interposer`) | MPL-2.0 | libc, libdl; `dlopen`s libpipewire-0.3 (MIT) when a PipeWire source is configured | shares its ring layout with pixelflux's `VirtualCamera` |
| fake-udev (`addons/fake-udev`) | MPL-2.0 (own `libudev.h` declarations, no systemd code) | libc | replaces the LGPL libudev only by ABI, for virtual gamepads |
| coturn addon (`addons/coturn`) | MPL-2.0 scripts around the coturn image | runs coturn (BSD-3-Clause) | TURN server |
| TURN REST (`addons/turn-rest`) | MPL-2.0 | aiohttp | |

## The default is GPL-enabled

pixelflux builds with its GPL components on by default (libx264 for software
H.264), and that is the supported default for every deployment — PyPI wheels,
the container images and the AppImage, which may therefore bundle a GPL FFmpeg
through pixelflux. `PIXELFLUX_ENABLE_GPL=0` is the opt-out for operators who
need a copyleft-free build; the sections below describe both.

## What a non-GPL deployment contains

With pixelflux built with `PIXELFLUX_ENABLE_GPL=0`: Selkies (MPL-2.0), the
permissive Python packages above, the LGPL libraries they load (libpulse
through pcmflux and pulsectl, python-xlib, glibc, FFmpeg as an LGPL build
through pixelflux) and the permissive web client. Nothing GPL. The WebRTC
stack packetizes pixelflux/pcmflux output and brings in no media library of
its own.

## What the default (GPL-enabled) deployment adds

An installed Selkies adds libx264 through the pixelflux wheel inside it; the
container images add the distribution's FFmpeg and x264 packages. libx264 is
GPL-2.0-or-later, and an FFmpeg configured with `--enable-gpl
--enable-version3` (Debian, Ubuntu, Alpine) is GPL-3.0-or-later, so a
deployment that ships them is distributed under GPL terms for those parts.

## How this is kept up to date

Selkies has no license gate of its own: the copyleft switch is pixelflux's, and
pixelflux's `scripts/check-licenses.py`, `deny.toml` and `Licenses` workflow
enforce that its non-GPL build stays copyleft-free (pcmflux carries the same
cargo-deny check). This page is the inventory of what Selkies adds on top.
Regenerate the figures with `pip show` or `importlib.metadata` for the Python
packages and `npx license-checker --summary --production` in each of the three
addon directories after `npm install`.
