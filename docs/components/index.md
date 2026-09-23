---
title: Components
description: What Selkies is built from, the encoders and interfaces each part provides, and where each one is documented.
---

At runtime Selkies is a **single Python application**, the `selkies` wheel. The HTML5 web client is bundled into it, and screen and audio capture and encoding come from the `pixelflux` and `pcmflux` extensions, installed as dependencies of the wheel. Everything else on this page is optional: container images that carry a whole desktop, libraries that give a container's applications gamepads and a webcam, and the TURN pieces the opt-in WebRTC transport may need.

**Refer to [Getting Started](../start.md) on how you can get on board.**

## Core Components

| Component | What it does | Documented at |
| --- | --- | --- |
| Python application (`selkies`) | Serves the web client and every endpoint on one port, drives the display's input, clipboard, files and printing, and streams over WebSockets or WebRTC | [Usage](../usage.md), [Settings Reference](../settings.md), the [Developer Reference](../development.md#developer-reference) |
| Web client and dashboards | The bundled `selkies-web-core` client and the reference dashboards built on it | [Web Client and Dashboards](web-client.md) |
| `pixelflux` | Screen capture on X11 and Wayland and video encoding, on the GPU where it carries the codec and in software where it does not | [pixelflux](pixelflux.md), the Rust reference at <https://pixelflux.selkies.io> |
| `pcmflux` | Audio capture from PulseAudio or PipeWire-Pulse, Opus encoding, and the microphone and recording paths | [pcmflux](pcmflux.md), the Rust reference at <https://pcmflux.selkies.io> |

## Optional Components

| Component | What it is for | Documented at |
| --- | --- | --- |
| Base Container | The whole session with no desktop in it: display servers, audio, GPU wiring, s6, coTURN, and Selkies, to build a desktop on | [Base Container](base-image.md) |
| Desktop Container | The reference LXQt desktop on the base, the quickest way to try Selkies | [Desktop Container](desktop-image.md) |
| KDE Plasma desktops | `docker-selkies-egl-desktop` and `docker-selkies-glx-desktop`, full desktops with hardware acceleration in separate repositories | [KDE Plasma Desktops](kde-images.md) |
| Input Interposer and fake-udev | Gamepads for a container's applications without kernel devices, and kernel gamepads where `/dev/uinput` is writable | [Gamepads](input-interposer.md) |
| V4L2 Interposer | The browser's camera as a V4L2 device for the session's applications | [Webcam](v4l2-interposer.md) |
| coTURN and TURN-REST | A TURN server and a credential service for the WebRTC transport behind restrictive networks | [TURN](turn.md) |
| Universal Touch Gamepad | An on-screen gamepad for touch devices, part of the web client | [Web Client and Dashboards](web-client.md#universal-touch-gamepad) |

[Sealskin](https://github.com/selkies-project/sealskin) is a separate project of the same organization, an example of orchestrating these images one desktop container per user on a single server, with its own web, mobile and browser-extension clients; its documentation is at <https://sealskin.selkies.io>.

## Container Images

Retrieve the latest `SELKIES_VERSION` release, and pick the `DISTRIB_FLAVOR` of the container images (`ubuntu26.04` or `debiantrixie`). The flavor names the distribution inside the image, so it is a free choice and not a property of the host:

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name')"
export DISTRIB_FLAVOR="ubuntu26.04"
```

| Image | Built from | Tags |
| --- | --- | --- |
| `ghcr.io/selkies-project/selkies/base` | [`addons/base`](https://github.com/selkies-project/selkies/tree/main/addons/base) | `main-${DISTRIB_FLAVOR}` for the newest commit, `${SELKIES_VERSION}-${DISTRIB_FLAVOR}` per release, `latest-${DISTRIB_FLAVOR}` for the newest release |
| `ghcr.io/selkies-project/selkies/desktop` | [`addons/desktop`](https://github.com/selkies-project/selkies/tree/main/addons/desktop) | the same |
| `ghcr.io/selkies-project/selkies-egl-desktop`, `ghcr.io/selkies-project/selkies-glx-desktop` | [their repositories](kde-images.md) | `26.04`, `26.04-<build>`, `latest` |
| `ghcr.io/selkies-project/selkies/coturn`, `ghcr.io/selkies-project/selkies/turn-rest` | [`addons/coturn`](https://github.com/selkies-project/selkies/tree/main/addons/coturn), [`addons/turn-rest`](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest) | `main`, `${SELKIES_VERSION}`, `latest` |

Every image is multi-architecture (`x86_64` and `aarch64`). When instructed to install [binfmt](https://github.com/tonistiigi/binfmt) for building the other architecture, use the following command with Docker®/Podman:

```bash
docker run --rm --privileged tonistiigi/binfmt:latest --install all
```

Each release attaches, per flavor and architecture, the digest of every image, the digest of the distribution image it was built from, and the list of packages it holds, which is what a downstream qualification build pins.

## Encoders and Interfaces

This section lists the encoders and interfaces that are actually implemented in the current runtime. The set of available video encoders depends on the transport mode.

### Encoders

Video is encoded by [`pixelflux`](pixelflux.md). `SELKIES_ENCODER` or `--encoder=` names the allowed encoders as a comma-separated list whose first item is the default, and a single value locks the choice; the dashboard chooses among them.

The dashboards list every encoder the server allows. One this browser cannot play on the transport (no WebCodecs decoder for its codec over WebSockets, no RTP receiver for it over WebRTC) stays in the menu disabled and marked as unsupported by the browser, so a missing H.265 or AV1 reads as the browser's limit rather than a server option left out; Chrome and Firefox on Linux, for one, decode no HEVC. A stream the browser cannot play at all, from an encoder the server holds, is reported on the page on both transports.

| Encoder (`--encoder=`) | Codec | Acceleration | Notes |
|---|---|---|---|
| `h264enc` (default) | H.264 AVC | NVIDIA NVENC / Intel & AMD VA-API / Tegra and V4L2 M2M engines, software fallback (`x264`, or OpenH264 in a GPL-free `pixelflux`) | Uses hardware encoding when a supported GPU is available; add `--use-cpu=true` to force software |
| `h265enc` | H.265 HEVC | NVIDIA NVENC / Intel & AMD VA-API / Tegra and V4L2 M2M engines, software fallback (`x265`, or kvazaar in a GPL-free `pixelflux`) | Carries 4:4:4 like H.264; a browser without an HEVC decoder falls back to `h264enc` |
| `vp8enc` | VP8 | Intel & AMD VA-API, software fallback (libvpx) | Decodes everywhere |
| `vp9enc` | VP9 | Intel & AMD VA-API, software fallback (libvpx) | Carries 4:4:4 as profile 1 where the browser decodes it |
| `av1enc` | AV1 | NVIDIA NVENC (Ada and newer) / Intel & AMD VA-API / Tegra Orin, software fallback (SVT-AV1) | Best quality per bit at low bitrates |
| `h264enc-striped` | H.264 AVC | Software (`x264`, or OpenH264 in a GPL-free `pixelflux`) | Striped/parallel software H.264 |
| `jpeg` | Motion JPEG | Software | Maximum-compatibility fallback |

When a codec cannot be served as asked, whether the host has no encoder for it or the browser declines it, the session steps down one ladder on both transports: the codecs the host encodes in hardware, most efficient first (AV1, H.265, VP9, H.264, VP8), then the ones it encodes in software in order of encode time (H.264, AV1, VP8, H.265, VP9), then striped H.264, and JPEG last of all. Only the encoders the deployment allows are stepped to, and a single allowed value is held: a stream the browser cannot play is reported on the page rather than replaced.

**WebSocket mode (default)** — every encoder above is available. The full-frame encoders are decoded by WebCodecs in the browser; the striped encoders are decoded per stripe, `jpeg` without WebCodecs at all.

**WebRTC mode (`--mode=webrtc`)** — the same allowed set and dashboard choice drive both transports. WebRTC carries the full-frame encoders (`h264enc`, `h265enc`, `vp8enc`, `vp9enc`, `av1enc`), packetized by the vendored RTP stack (RFC 6184, RFC 7798, RFC 7741, RFC 9628, and the AV1 RTP payload format); the striped framings of `h264enc-striped` and `jpeg` are WebSocket-only, so in this mode the published menu is filtered to the five and either of those two falls back to the default with a logged warning; switching back to WebSockets restores the configured menu and value. The offer puts the display's codec first and the rest of the menu behind it down the ladder: a browser that declines the codec answers with the next one it decodes and the display moves to that encoder for every viewer, logged as a warning, unless the operator's menu holds the encoder, in which case that peer gets no video rather than another codec's bitstream; a live encoder change switches each peer's payload type to the codec it already negotiated, with no renegotiation. Which codecs a browser takes over WebRTC is the browser's own RTP receiver's business (its `RTCRtpReceiver.getCapabilities`, which the dashboards filter the menu by), not WebCodecs': Chromium and Firefox take VP8, VP9 and AV1 everywhere and H.265 only where the platform decodes it, Safari takes H.265 as well.

Full color (4:4:4 chroma, `--video-fullcolor`) is carried by H.264 and H.265 on NVENC, VA-API, x264 and x265, and by VP9 profile 1 on VA-API and libvpx. The server learns from `pixelflux` which of its encoders carry it on the host, and the browser tells the server which 4:4:4 profiles its decoder takes, so a full-color stream is only ever sent where both ends carry it; elsewhere the session streams 4:2:0 and says so.

### Display Capture

| Interface | Device Selector | Input Injection | Operating Systems | Notes |
|---|---|---|---|---|
| X.Org / X11 (via `pixelflux`) | `DISPLAY` environment | vendored [`python-xlib`](https://github.com/python-xlib/python-xlib) (XTEST/XFixes), under `src/selkies/Xlib/` | Linux | Default backend |
| Wayland (via `pixelflux`) | headless compositor started by Selkies (`--wayland=true` / `SELKIES_WAYLAND=true`), or an external compositor captured through its screencopy and portal protocols (`--wayland-host-display`) | input injection through the `pixelflux` Wayland backend | Linux | Native Wayland mode; Mac and Windows support is planned |

### Audio Encoder

Opus is currently the only adequate full-band audio codec supported in web browsers by specification.

| Encoder | Codec | Operating Systems | Browsers | Notes |
|---|---|---|---|---|
| `pcmflux` | Opus | Linux | All major | Bitrate via `--audio-bitrate`; Opus RED (RFC 2198) redundancy via `--audio-redundancy` |

### Audio Capture

| Interface | Device Selector | Operating Systems | Notes |
|---|---|---|---|
| PulseAudio or PipeWire-Pulse (via `pcmflux`) | `PULSE_SERVER` or `PULSE_RUNTIME_PATH` environment, `--audio-device-name` | Linux | Default capture device is `output.monitor` |

### Client Uplinks

Both are off by default and need a secure context in the browser; see [Usage](../usage.md#microphone-and-webcam).

| Uplink | Selected with | Codec | Delivered to the session as |
|---|---|---|---|
| Microphone | `--microphone-enabled` | Opus (WebRTC) or Opus over the WebSocket | a PulseAudio source, through the same sound server the capture reads |
| Webcam | `--webcam-enabled` | H.264, VP8, VP9, AV1, H.265 or MJPEG, chosen by `--webcam-encoder` (`auto` measures the client) | a V4L2 device: the [V4L2 Interposer](v4l2-interposer.md) socket, a v4l2loopback device, or a PipeWire `Video/Source` node |

### Transport Protocols

| Transport | Selected with | Ports | Notes |
|---|---|---|---|
| WebSockets (default) | `--mode=websockets` | single TCP port (default `8080`) | WebCodecs-based client decode (striped JPEG without WebCodecs); no STUN/TURN required |
| WebRTC (opt-in) | `--mode=webrtc` | signaling over the same port; media over UDP (or TCP) with ICE: ephemeral ports, a port range, or one shared UDP and/or TCP port | Uses a vendored [`aiortc`](https://github.com/aiortc/aiortc) fork; may need STUN/TURN, and offers a port range, UDP/TCP mux, and ICE-lite, see [WebRTC and Firewall Issues](../firewall.md#restricting-the-ports-port-range-udp-mux-tcp-mux-and-ice-lite) |

Use `--enable-dual-mode=true` to let the client switch between the WebSocket and WebRTC transports from the UI.
