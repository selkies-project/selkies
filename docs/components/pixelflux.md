---
title: pixelflux
description: The screen capture and video encoding extension behind every Selkies session, and where its own documentation lives.
---

Screen capture and video encoding are performed by [`pixelflux`](https://github.com/selkies-project/pixelflux), a Rust (PyO3) extension the `selkies` wheel installs as a dependency. Its Rust reference is published at <https://pixelflux.selkies.io>, and its README carries the Python API a session is driven through; this page is what a Selkies deployment needs to know of it.

## Codecs and Engines

`pixelflux` encodes H.264, H.265, VP8, VP9, and AV1 as whole frames, and H.264 and Motion JPEG as stripes. A codec is encoded on the host's engine where it carries it — NVENC on NVIDIA (H.264, H.265, and AV1 from Ada on), VA-API on Intel and AMD (all five), the vendor encoder of a Jetson (H.264, H.265, and AV1 on Orin) and a V4L2 memory-to-memory encoder on other boards — and otherwise on the software encoder the build carries for it: `x264` or the BSD-licensed OpenH264 for H.264, `x265` or kvazaar for H.265, libvpx for VP8 and VP9, SVT-AV1 for AV1.

At startup Selkies asks `pixelflux.hardware_encoders()` which codecs the encode node's engine serves and `pixelflux.SOFTWARE_ENCODERS` which software encoders the build carries, once, and offers clients only the encoders whose codec one of the two sides serves; the dashboards show the software encoding switch only where a codec has both, since it moves the session between them and is a no-op otherwise. The same probe reports which of them take 4:4:4 (`pixelflux.SOFTWARE_FULLCOLOR`, `pixelflux.hardware_fullcolor()`), which is what lets a full-color session be declined before a frame is sent. A software encoder that cannot run on the host's CPU is probed in a child process and left out rather than allowed to take the session down.

A request the host cannot serve falls through the codecs it does, in the order [Encoders](index.md#encoders) gives, and the session says which encoder came up in its log line.

## Zero-Copy Paths

Frames reach the encoder without a copy wherever the hardware allows it. On Wayland the compositor's dmabuf goes to the encoder as it is. On X11 the same holds on an NVIDIA GPU encoding with NVENC: `pixelflux` captures through NvFBC, so the NVIDIA X driver composites the screen straight into video memory and the encoder reads that buffer in place, which measures 2.52 ms per frame at 1920x1080 against 5.30 ms for the shared-memory path on the same GPU. Every other X11 session copies once, into a shared-memory surface the encoder then reads in place. Nothing needs configuring for any of this; the session says which path it took in its log.

## Capture Backends

The X11 backend captures the display `DISPLAY` names and injects input through XTEST. The Wayland backend is a headless compositor `pixelflux` owns, which the session's applications connect to directly or through a nested compositor, and which hands the encoder its buffers as they are; an external compositor can be captured instead through `ext-image-copy-capture`, `wlr-screencopy` or the desktop portal, with input through the virtual keyboard and pointer protocols or the portal's remote desktop session. [Desktop Container](desktop-image.md) describes how the images choose between them.

`pixelflux` also serves the [virtual webcam](v4l2-interposer.md) the browser's camera is published as, the out-of-band recording tap (`--recording-socket`), the uinput devices of the [kernel gamepads](input-interposer.md#kernel-gamepads), and the Computer-Use HTTP server (`--computer-use-bind`) an agent drives a session through.

## Licensing

The software encoders of `pixelflux` are chosen when it is built, never by a Selkies setting: the default build uses GPL-2.0+ `libx264` and x265 (with an install-time notice), and a build made with `PIXELFLUX_ENABLE_GPL=0` excludes every GPL-licensed component and uses the BSD-licensed OpenH264 and kvazaar instead, behind the same `h264enc` / `h264enc-striped` / `h265enc` encoders; VP8, VP9 (libvpx) and AV1 (SVT-AV1) are BSD-licensed in every build. Selkies reads `pixelflux.SOFTWARE_ENCODERS` to name the encoders in its logs and to default a session known to run on OpenH264 to CBR rate control (OpenH264 targets a bandwidth rather than a quality level); OpenH264 and kvazaar encode 4:2:0 only, so `--video-fullcolor` has no effect on their software paths. [Licensing](../licensing.md) lists every third-party component of an installation with its license and where the GPL pieces come from.

## Installing a Development Build

The wheels of every commit to `pixelflux`'s `main` are attached to a pre-release named by the commit's short hash, and Selkies' own CI builds against the newest of them. A change tried locally is a wheel built with `pip wheel . --no-deps` in the `pixelflux` checkout and installed into the same environment as `selkies`; [Development](../development.md#agentic-development) describes the loop.
