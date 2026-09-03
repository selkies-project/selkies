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
    --gpus 1 --runtime nvidia \
    ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

Toolkit releases older than that leave `/dev/nvidia-modeset` and the DRM render node out; only there, add `--device /dev/nvidia-modeset --device /dev/dri`.

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

## Apptainer

The same images run under [Apptainer](https://apptainer.org) as an ordinary user, which is how a job on a shared GPU node gets a desktop: no daemon, no root, the image pulled straight from the registry. A script that starts the desktop and keeps it for the job's lifetime, with a display number and a port that no other session on the node uses (a job id from the scheduler is a good seed for both):

```bash
#!/bin/bash
N=$((RANDOM % 900 + 100))
PORT=$((RANDOM % 10000 + 20000))
mkdir -p "$HOME/selkies/home" "$HOME/selkies/tmp"
apptainer run --nv --writable-tmpfs --contain --cleanenv \
    --home "$HOME/selkies/home:/home/ubuntu" -B "$HOME/selkies/tmp:/tmp" -B /dev/dri \
    --env "DISPLAY=:$N,SELKIES_PORT=$PORT,PASSWD=mypasswd" \
    docker://ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

Reach it with `ssh -L 8080:<node>:$PORT <login-node>` and open `https://localhost:8080`. What each flag is for, measured rather than assumed:

- **`apptainer run`**, not `apptainer instance start`: a Docker image has no start script, so an instance runs nothing. `run` executes the image's entrypoint, and the script keeps it in the foreground for the job's lifetime.
- **A display number and a private `/tmp` per session.** The container shares the node's network namespace, and an X server's abstract socket lives there: two sessions on one node with the default display `:20` collide, and the second one's X server never comes up. The base's display server honours `DISPLAY`. `-B <dir>:/tmp` with `--contain` gives the session its own runtime directory, sockets and locks instead of the node's shared `/tmp`.
- **Ports are the node's ports.** There is no port mapping; `SELKIES_PORT` has to be free on the node, and is reached through whichever host can see it.
- **`--writable-tmpfs`**: the image is read-only under Apptainer and the service supervisor writes into its service directories at start. `--home <dir>:/home/ubuntu` keeps the session's settings and downloads across jobs.
- **`--nv`** binds the NVIDIA driver in. On the X11 backend OpenGL then runs through Zink on the Vulkan driver, and NVENC encodes. The Wayland backend (`SELKIES_WAYLAND=true`) also needs the driver's GBM backend, which Apptainer's library list does not carry; bind it from the host, with the driver's version in the file names:

  ```bash
  L=/usr/lib/x86_64-linux-gnu
  apptainer run --nv ... --env "SELKIES_WAYLAND=true,..." \
      -B "$L/gbm/nvidia-drm_gbm.so:$L/gbm/nvidia-drm_gbm.so" \
      -B "$L/libnvidia-egl-gbm.so.1.1.3:$L/libnvidia-egl-gbm.so.1" \
      -B "$L/libnvidia-allocator.so.580.178.04:$L/libnvidia-allocator.so.1" \
      -B /usr/share/egl/egl_external_platform.d/15_nvidia_gbm.json ...
  ```

  `--nvccli` (setup through nvidia-container-cli) is not a way around this: it binds the device nodes and the driver libraries but no GBM backend either, and it needs `NVIDIA_VISIBLE_DEVICES=all` and a spelled-out `NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics,display,video` in the launch environment, refusing the `all` the images set. Neither is a CDI specification (`--device nvidia.com/gpu=all`), although the toolkit's `nvidia-ctk cdi generate` lists every file the Docker runtime injects: Apptainer applies a specification's mounts but runs none of its hooks, and those hooks are what create the links the driver is loaded through, so on its own the specification leaves applications without the driver's GLX and EGL, and combined with `--nv` its X module mounts are dropped.
- **Intel and AMD** need only `-B /dev/dri` and the render node's group, which a job normally has; `--rocm` for AMD.
- Device nodes for the gamepad slots are not made (the container cannot, and the interposer needs none), and the `sudo` fallbacks the entrypoint tries print errors and carry on: the session is unaffected.

The [docker-selkies-egl-desktop](https://github.com/selkies-project/docker-selkies-egl-desktop) image takes exactly the same command with its own image reference. The [docker-selkies-glx-desktop](https://github.com/selkies-project/docker-selkies-glx-desktop) image runs its own X.Org server on the GPU, which needs the host's NVIDIA X modules bound to the paths the server loads them from; its README has the command.

## Without a container

[Native Install](native.md) covers the native packages and the AppImage, how to attach Selkies to a display and audio server you already run, and the full session script for a standalone machine.
