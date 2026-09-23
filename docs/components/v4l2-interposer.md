---
title: Webcam
description: The V4L2 Interposer and the other sinks that hand the browser's camera to the session's applications as a V4L2 device.
---

## V4L2 Interposer

The [V4L2 Interposer](https://github.com/selkies-project/selkies/tree/main/addons/v4l2-interposer) is the webcam counterpart of the [Input Interposer](input-interposer.md): an `LD_PRELOAD` library that presents the client's camera to applications as a V4L2 capture device (`/dev/video0`), with no `v4l2loopback` kernel module, no `/dev/video*` node, and no elevated privilege. Unmodified consumers pick it up — Chromium, Firefox, `ffmpeg`, GStreamer, `v4l2-ctl` and libv4l2-based applications. Turn the uplink on with `--webcam-enabled=true` (`SELKIES_WEBCAM_ENABLED`); it is off by default.

The browser encodes its camera (over WebSockets through the measured WebCodecs ladder of H.264, VP8, VP9, AV1 and H.265 where the engine encodes them, JPEG when none keeps up, or the one codec `webcam_encoder` names; over WebRTC the codec `webcam_encoder` names among the ones its answer negotiated, else the first of them, an AV1 frame reassembled from the RTP packets one OBU's fragments span) and Selkies hands each encoded frame to `pixelflux`'s virtual camera, which decodes it, fits it to the device format and publishes it to every sink on its own thread. One camera is shared by every client and lives as long as the server, so an application that opened the device keeps it across transport switches and browser reconnects.

Applications reach the camera through whichever sink the deployment can offer, and the interposer socket is always served:

| Sink | Reached by | Requires |
| --- | --- | --- |
| Interposer socket | applications started with the library preloaded | nothing beyond the library |
| v4l2loopback device (`--webcam-device`, `auto` by default) | every application, with nothing preloaded | the `v4l2loopback` module and a writable output device: a desktop host or a privileged container |
| PipeWire node | PipeWire-native applications and the `pipewire-v4l2` wrapper | a reachable PipeWire daemon |

The device advertises one fixed format, as a fixed-function webcam does: `--webcam-width` and `--webcam-height` size it (client frames are scaled and letterboxed to fit), and `--webcam-pixel-format` pins the format or, left at `auto`, follows the first uplink — a browser sending JPEG gets an MJPEG device its frames pass through untouched, any other uplink an I420 one; under `--webcam-on-start=demand` the device exists before any uplink, so `auto` resolves to I420 and an MJPEG uplink is decoded for it unless the format is pinned. `--webcam-encoder` chooses what clients encode with.

The [Desktop Container](desktop-image.md) and the desktop containers build and wire the library automatically, every native Selkies package ships it under `/usr/$LIB` (the `.deb` and `.rpm` carry the 32-bit variant too), and the AppImage carries it at `usr/lib/selkies_v4l2_interposer.so`, whose path its `AppRun` exports as `SELKIES_WEBCAM_INTERPOSER`. Elsewhere, build it from the source in this repository and preload it in the environment each application runs in:

```bash
git clone https://github.com/selkies-project/selkies.git && cd selkies
apt-get update && apt-get install --no-install-recommends -y build-essential
make -C addons/v4l2-interposer && PREFIX=/usr make -C addons/v4l2-interposer install
```

```bash
export SELKIES_WEBCAM_INTERPOSER='/usr/$LIB/selkies_v4l2_interposer.so'
export LD_PRELOAD="${SELKIES_WEBCAM_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
```

On `x86_64`, `make -C addons/v4l2-interposer all32 install32` (with `gcc-multilib`) adds the 32-bit variant for 32-bit applications, since `/usr/$LIB` resolves per process bitness.

**Never preload the interposer into the Selkies process itself.** It answers for `/dev/video0` in whatever process it is loaded into, so the capture side would stop seeing the real device nodes; the container entrypoints drop every Selkies preload before starting the backend for the same reason.

Check the [V4L2 Interposer README.md](https://github.com/selkies-project/selkies/tree/main/addons/v4l2-interposer/README.md) for the device surface it emulates, the `SELKIES_WEBCAM_SOURCE` frame-source selector, and a test server that stands in for a browser.
