---
title: Getting Started
description: Run the Selkies desktop container, with or without a GPU, and open it in a browser.
---

## Quick Start

The container carries a desktop, a browser and an audio stack, so there is nothing to install and nothing to prepare. Selkies streams over plain WebSockets on a **single port (default `8080`)**; WebRTC is an opt-in transport (`--mode=webrtc`). Open <https://localhost:8080> when it is up, and accept the container's self-signed certificate.

> **The default login is `ubuntu` / `mypasswd`.** Change it with `-e PASSWD=...` (or `-e SELKIES_BASIC_AUTH_PASSWORD=`), and do that before putting a session anywhere others can reach it.

Pick the block for the GPU the session should render on, and copy it whole. Not using containers? [Native Install](native.md) has the packages and the AppImage.

### Without a GPU

Everything renders in software, and this is the whole command:

```bash
docker run --name selkies -it -d --rm --shm-size=2g -p 8080:8080 \
    ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

### Intel or AMD

Pass the DRM render node in. `--group-add` covers a host that keeps that node to a group the container's user is not in:

```bash
docker run --name selkies -it -d --rm --shm-size=2g -p 8080:8080 \
    --device /dev/dri --group-add "$(stat -c %g /dev/dri/renderD128)" \
    ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

### NVIDIA

The [NVIDIA Container Toolkit](https://github.com/NVIDIA/nvidia-container-toolkit) (1.20.1 or newer) passes the driver, its Vulkan ICD and the DRM nodes in, so the runtime flags are all it takes:

```bash
docker run --name selkies -it -d --rm --shm-size=2g -p 8080:8080 \
    --runtime nvidia --gpus all \
    ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

Older toolkit releases leave `/dev/nvidia-modeset` and the DRM render node out; add `--device /dev/nvidia-modeset --device /dev/dri` there.

### What the flags are for

`--shm-size=2g` matters because the browsers inside the desktop crash on Docker®'s 64 MB default, and `-p 8080:8080` is the one port the whole session is served on. The image tag names the distribution inside the image — `ubuntu26.04` or `debiantrixie`, a free choice unrelated to your host — and `main` is the newest commit, `latest` the newest release.

The container serves HTTPS by default, on the distribution's snakeoil certificate, so the browser warns once until you trust it or name a real certificate with `-e SELKIES_HTTPS_CERT=` and `-e SELKIES_HTTPS_KEY=`; `-e SELKIES_ENABLE_HTTPS=false` serves plain HTTP where something in front already terminates TLS.

`-e SELKIES_BASIC_AUTH_USER=` and `-e SELKIES_BASIC_AUTH_PASSWORD=` replace the default login, `-e SELKIES_MODE=webrtc` opts into the WebRTC transport, and `-e SELKIES_WAYLAND=true` runs the same desktop on the headless Wayland backend. On a machine with more than one GPU, `-e SELKIES_AUTO_GPU=` picks which one the session renders on. [Desktop Container](component.md#desktop-container) covers each of them, along with the second display, the apps panel and the embedded TURN server.

## Desktop Container

Full desktop containers that can be used out-of-the-box are available in separate repositories. If you can deploy Docker® or Podman containers, this is the easiest way to get started.

[`docker-selkies-egl-desktop`](https://github.com/selkies-project/docker-selkies-egl-desktop) and [`docker-selkies-glx-desktop`](https://github.com/selkies-project/docker-selkies-glx-desktop) are ready-to-go KDE Plasma desktops built on the [Base Container](component.md#desktop-container), with hardware acceleration on NVIDIA, AMD and Intel GPUs: the first reaches the GPU through EGL on the base's own display servers (X11 or Wayland, sharing one GPU between containers), the second runs its own X.Org server on the GPU.

## Minimal Container

The [Desktop Container](https://github.com/selkies-project/selkies/tree/main/addons/desktop) is the reference minimal-functionality container developers can base upon, or test Selkies quickly. The bare minimum LXQt desktop (Openbox window manager) is installed together with Firefox and Google Chrome, as well as an embedded TURN server inside the container for quick WebRTC firewall traversal.

Instructions are available in the [Desktop Container](component.md#desktop-container) section.

**With the default WebSocket transport, a single exposed port is all you need.** A TURN server only becomes relevant if you opt into the WebRTC transport (`--mode=webrtc`) inside a Docker® or Kubernetes container without `--network=host` or `hostNetwork: true`, or in other cases where the HTML5 web interface loads but the WebRTC connection fails. In that case, follow the instructions from [WebRTC and Firewall Issues](firewall.md) to make the container or self-hosted standalone instance use an external TURN server. This is required for all self-hosted WebRTC applications, unlike proprietary services which provide a TURN server for you.

## Without a container

[Native Install](native.md) covers the native packages and the AppImage, how to attach Selkies to a display and audio server you already run, and the full session script for a standalone machine.
