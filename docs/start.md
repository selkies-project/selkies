---
title: Getting Started
description: Run Selkies from a container, a native package, or the AppImage, and bring up a session around it.
---

## Quick Start

Every release publishes the same build several ways. Pick the one that fits, copy the whole block, run it. None of them needs a Python environment: the web client, the `pixelflux` (screen capture with H.264/JPEG encoding) and `pcmflux` (PulseAudio capture with Opus encoding) extensions, and the interposers all travel inside. Selkies streams over plain WebSockets on a **single port (default `8080`)**; WebRTC is an opt-in transport (`--mode=webrtc`).

Every block below prints the version into `SELKIES_VERSION` first, so paste that line whichever route you take:

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name' | sed 's/^v//')"
```

### A container, with a desktop already in it

Nothing to install, and the only route that brings its own desktop, browser and audio stack. Open <http://localhost:8080> afterwards and log in as `ubuntu` / `mypasswd`:

```bash
docker run --name selkies -it -d --rm --shm-size=2g -p 8080:8080 \
    ghcr.io/selkies-project/selkies/example:main-ubuntu26.04
```

`debiantrixie` is the other flavor, and the flavor only names the distribution inside the image — it is a free choice, not a property of your host. Add `--gpus 1 --runtime nvidia` for an NVIDIA GPU, or `--device /dev/dri` for Intel and AMD. [Example Container](component.md#example-container) covers the rest, including the Wayland backend and the embedded TURN server.

### A native package, on a machine you already have a desktop on

Installs a private Python environment at `/opt/selkies`, puts `selkies`, `selkies-resize` and `selkies-gpu-probe` on `PATH`, carries both interposers, and pulls every system library it needs through your package manager. Pick your distribution's line:

```bash
# Ubuntu and Debian: the suffix names the distribution the package was built in
# (ubuntu24.04, ubuntu26.04, bookworm, trixie), and amd64 or arm64
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies_${SELKIES_VERSION}-1~trixie_amd64.deb"
sudo apt-get install -y "./selkies_${SELKIES_VERSION}-1~trixie_amd64.deb"
```

```bash
# Fedora and Enterprise Linux: fc or el9, and x86_64 or aarch64
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-${SELKIES_VERSION}-1.fc.x86_64.rpm"
sudo dnf install -y "./selkies-${SELKIES_VERSION}-1.fc.x86_64.rpm"
```

```bash
# Alpine
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-${SELKIES_VERSION}-r0-x86_64.apk"
sudo apk add --allow-untrusted "./selkies-${SELKIES_VERSION}-r0-x86_64.apk"
```

```bash
# Arch Linux, published for x86_64 alone
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-${SELKIES_VERSION}-1-x86_64.pkg.tar.zst"
sudo pacman -U "./selkies-${SELKIES_VERSION}-1-x86_64.pkg.tar.zst"
```

For hardware-accelerated H.264, add your GPU's driver: NVENC comes with the NVIDIA driver (`libnvidia-encode`), and Intel and AMD encode through VA-API (`libva2` plus your vendor's driver — `intel-media-va-driver-non-free` for Intel, or `i965-va-driver-shaders` for older generations, and the AMDGPU driver's own for AMD). `vainfo`, `intel-gpu-tools`, `radeontop` and `nvtop` are optional monitors.

Then go to [Run a session](#run-a-session).

### An AppImage, installing nothing at all

Runs from wherever you put it, on any distribution, without touching the system. Every Python and native dependency is inside; it starts an `Xvfb` when the display it is pointed at is not up, and its own PulseAudio when none is listening:

```bash
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-${SELKIES_VERSION}-x86_64.AppImage"
chmod +x "./selkies-${SELKIES_VERSION}-x86_64.AppImage"
"./selkies-${SELKIES_VERSION}-x86_64.AppImage" --addr=0.0.0.0 --port=8080 --basic-auth-user=user --basic-auth-password=mypasswd
```

What it takes from the host is the graphics stack and the display server: `libgbm`, `libEGL` and the GPU's own driver have to be the host's for the GPU to be reachable at all, an X11 session needs the host's X server (or `Xvfb`), and the headless Wayland backend needs the host's `libwayland-server`. Everything above them travels with the AppImage.

## Run a session

A native package installs Selkies but starts nothing: it attaches to a display and an audio server you provide. The container and the AppImage both bring their own, so skip this if you took one of those.

**1. Point Selkies at your display and audio server.**

**Selkies attaches to an existing X.Org X11 display and an already-running PulseAudio (or PipeWire-Pulse) server.** [Run a full session](#run-a-full-session-on-a-standalone-machine-cloud-instance-or-virtual-machine) has a script that starts both for you.

**Check that you are using X.Org instead of Wayland (which is the default in many distributions) when attaching to an existing display -- an already-running Wayland session cannot be captured. A separate headless Wayland mode (started and owned by Selkies itself) is available with `--wayland=true` / `SELKIES_WAYLAND=true`, but when attaching to an existing graphical session that session must be X.Org. You also need to be logged in from the login screen or autologin should be enabled.**

```bash
export DISPLAY="${DISPLAY:-:0}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"
```

**The same variables have to be set for the desktop session and its applications**, or they will have no audio.

**2. Run Selkies:**

```bash
selkies --addr=0.0.0.0 --port=8080 --enable-https=false --https-cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https-key=/etc/ssl/private/ssl-cert-snakeoil.key --basic-auth-user=user --basic-auth-password=mypasswd --encoder=h264enc --enable-resize=false
```

In the default WebSocket mode, `--encoder=` accepts `h264enc` (default; hardware NVENC or VA-API when a supported GPU is available, otherwise the software encoder `pixelflux` was built with — `x264`, or OpenH264 in a GPL-free build), `h264enc-striped` (striped software H.264 on that same encoder), or `jpeg`. Add `--use-cpu=true` to force software encoding. To use the opt-in WebRTC transport instead, add `--mode=webrtc`; the same `--encoder=` knob applies, filtered to what WebRTC can produce (`h264enc`, the hardware-first default) — any other value falls back to it with a logged warning.

`--enable-https=false` above leaves the web interface on plain HTTP, which browsers accept as a secure context only on `localhost`; the clipboard, gamepads, pointer lock, and the microphone and webcam need one. Setting `--enable-https=true` is the whole switch: the `--https-cert=` and `--https-key=` paths are the `ssl-cert-snakeoil` pair Debian and Ubuntu install, and when they are absent Selkies writes a self-signed pair itself, so nothing has to be prepared. Browsers warn once on a self-signed certificate; a certificate from an authority at those paths, or a reverse proxy terminating TLS in front, avoids the warning.

The default username (set with `--basic-auth-user=` or `SELKIES_BASIC_AUTH_USER`), when not specified, is taken from the `CUSTOM_USER`, then `USERNAME`, then `USER` environment variable, and is `ubuntu` when none of them is set. The password has no default: set it with `--basic-auth-password=`, `SELKIES_BASIC_AUTH_PASSWORD`, `PASSWORD`, or `PASSWD`, or pass `--enable-basic-auth=false` to serve without a login. Selkies refuses to start with basic authentication enabled and no password, so a login is never served that nobody chose a password for.

Dynamic resizing (`--enable-resize`, **on by default**) fits the remote resolution to the client window; the command above turns it off because it **must NOT** be enabled when streaming a physical monitor. Leave it on when streaming a virtual display (`Xvfb` or the Wayland backend) and skip the next step.

**3. Resize to your intended resolution (DO NOT resize when streaming a physical monitor):**

```bash
selkies-resize 1920x1080
```

**4. Check the [**Joystick Interposer**](component.md#joystick-interposer) section if you need to use joystick/gamepad devices from your web browser client, and the [**V4L2 Interposer**](component.md#v4l2-interposer) section for the webcam.**

You can install `selkies_joystick_interposer.so` and `selkies_v4l2_interposer.so` to any non-root path of your choice and point `SELKIES_INTERPOSER` and `SELKIES_WEBCAM_INTERPOSER` at them.

**5. (WebRTC mode only) If you switched to `--mode=webrtc` and the HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start:**

**This step is only relevant to the opt-in WebRTC transport. The default WebSocket transport uses a single TCP port and needs no STUN/TURN server. In WebRTC mode, when there is very high latency or stutter and the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, this section is very important.**

Please read [**WebRTC and Firewall Issues**](firewall.md).

**6. Read [**Troubleshooting and FAQs**](faq.md) if something is not as intended and [**Usage**](usage.md) for more information on customizing.**

## Desktop Container

Full desktop containers that can be used out-of-the-box are available in separate repositories. If you can deploy Docker® or Podman containers, this is the easiest way to get started.

[`docker-selkies-glx-desktop`](https://github.com/selkies-project/docker-selkies-glx-desktop) and [`docker-selkies-egl-desktop`](https://github.com/selkies-project/docker-selkies-egl-desktop) are expandable ready-to-go out-of-the-box containerized remote desktop implementations of Selkies supporting hardware acceleration on NVIDIA and other GPUs.

## Minimal Container

The [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) is the reference minimal-functionality container developers can base upon, or test Selkies quickly. The bare minimum LXQt desktop (Openbox window manager) is installed together with Firefox and Google Chrome, as well as an embedded TURN server inside the container for quick WebRTC firewall traversal.

Instructions are available in the [Example Container](component.md#example-container) section.

**With the default WebSocket transport, a single exposed port is all you need.** A TURN server only becomes relevant if you opt into the WebRTC transport (`--mode=webrtc`) inside a Docker® or Kubernetes container without `--network=host` or `hostNetwork: true`, or in other cases where the HTML5 web interface loads but the WebRTC connection fails. In that case, follow the instructions from [WebRTC and Firewall Issues](firewall.md) to make the container or self-hosted standalone instance use an external TURN server. This is required for all self-hosted WebRTC applications, unlike proprietary services which provide a TURN server for you.

## Advanced Install

**Choose between [Quick Start](#quick-start) and this section.**

[Quick Start](#quick-start) gets a session up. This section is what is inside one, and a full run script that starts the display and audio servers itself rather than attaching to yours. It matches the reference `Dockerfile` build procedure.

### Backgrounds

Selkies has a modularized architecture, but at runtime it is a **single Python application**, the one every package and image above carries, that:

- serves the HTML5 web client, which is bundled inside it (at `src/selkies/selkies_web`) and served from the same single port;
- captures and encodes the screen through the `pixelflux` extension (hardware H.264 via NVENC or VA-API, software H.264 via `x264` — or OpenH264 in a GPL-free `pixelflux` build — or JPEG);
- captures and encodes audio through the `pcmflux` extension (Opus);
- injects keyboard, mouse, and gamepad input through a vendored `python-xlib` (XTEST/XFixes);
- and, only for the opt-in WebRTC transport, uses a vendored fork of `aiortc`.

`pixelflux`, `pcmflux` and the web client all travel inside whichever medium you installed. There is **no separate multimedia-framework build or web-interface package to install**.

For more information, check the [Components](component.md) section.

The [All-In-One Desktop Containers](#desktop-container) support unprivileged self-hosted Kubernetes clusters and Docker®/Podman.

### Run a full session on a standalone machine, cloud instance, or virtual machine

**NOTE: STUN/TURN is only relevant to the opt-in WebRTC transport (`--mode=webrtc`). The default WebSocket transport uses a single TCP port. If you use WebRTC mode and both your server and client have closed ports or a restrictive firewall, you will need an external STUN/TURN server capable of `srflx` or `relay` type ICE connections; either open the UDP and TCP port ranges 49152-65535 of your server, or follow the instructions from [WebRTC and Firewall Issues](firewall.md).**

While this instruction assumes that you are installing this project systemwide, it is possible to install and run all components completely within the userspace.

**1. Install Selkies** by any route in [Quick Start](#quick-start); the native package is the one this script assumes, since it puts `selkies` on `PATH` and pulls in the system libraries through your package manager.

The steps below use the release version, so put it in the environment first:

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name' | sed 's/^v//')"
```

**2. Build the Joystick Interposer to process gamepad input**, if you need to use joystick/gamepad devices from your web browser client in an environment without `/dev/uinput` — typically an unprivileged container. Where `/dev/uinput` is writable, Selkies registers gamepads as [kernel devices](component.md#kernel-gamepads) instead and this step, along with the `LD_PRELOAD` exports below, is unnecessary. Otherwise applications receive gamepad input only when they are started with the interposer preloaded, and `fake-udev` is additionally required for applications that discover devices through `libudev`. Both are built and wired automatically in the [Example Container](component.md#example-container) and the desktop containers. Elsewhere, build them from source (they are small, dependency-free `LD_PRELOAD` libraries):

```bash
git clone https://github.com/selkies-project/selkies.git && cd selkies
apt-get update && apt-get install --no-install-recommends -y build-essential
make -C addons/js-interposer && PREFIX=/usr make -C addons/js-interposer install
cd addons/fake-udev && make && cp libudev.so.1.0.0-fake libudev.so.1 libudev.so /usr/lib/$(gcc -print-multiarch)/
```

On `x86_64`, add `apt-get install -y gcc-multilib && make -C addons/js-interposer install32` (and `make all32` in `addons/fake-udev`) for 32-bit applications such as most of the Steam and Wine catalog, since `/usr/$LIB` resolves per process bitness. Container images built on the `.deb`, `.rpm`, `.apk` or `.pkg.tar.zst` package need neither step: each of them already carries the interposer, and the `.deb` and `.rpm` carry the 32-bit variant too.

More information can be found in [Joystick Interposer](component.md#joystick-interposer).

You can install `selkies_joystick_interposer.so` to any non-root path of your choice and point `SELKIES_INTERPOSER` at it. The webcam uplink has a matching library built the same way, `make -C addons/v4l2-interposer && PREFIX=/usr make -C addons/v4l2-interposer install`; see [V4L2 Interposer](component.md#v4l2-interposer).

SDL2 applications discover the four pads through `fake-udev`. Where discovery through `libudev` is unavailable — `SDL_JOYSTICK_DISABLE_UDEV=1`, an SDL sandbox build, or an SDL built without udev — export `SDL_JOYSTICK_DEVICE=/dev/input/event1000:/dev/input/event1001:/dev/input/event1002:/dev/input/event1003` instead, which needs no placeholder files. Never name the joydev nodes there: with `fake-udev` active, a `/dev/input/js0` hint is a second, different node for the slot SDL already enumerated as `event1000`, so the pad shows up twice.

**3. Run Selkies after changing the below script appropriately** (install `xvfb` and uncomment relevant sections if there is no real display, **DO NOT resize when streaming a physical monitor**)**:**

**Check that you are using X.Org instead of Wayland (which is the default in many distributions) when attaching to an existing display -- an already-running Wayland session cannot be captured. A separate headless Wayland mode (started and owned by Selkies itself) is available with `--wayland=true` / `SELKIES_WAYLAND=true`, but when attaching to an existing graphical session that session must be X.Org. You also need to be logged in from the login screen or autologin should be enabled.**

```bash
export DISPLAY="${DISPLAY:-:0}"
# Configure the interposers: gamepads for the session's applications, and the
# webcam if the client's camera is forwarded into it
export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
export SELKIES_WEBCAM_INTERPOSER='/usr/$LIB/selkies_v4l2_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}:${SELKIES_WEBCAM_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
sudo mkdir -pm1777 /dev/input

# Commented sections are optional but may be mandatory based on setup

# Start a virtual X11 server if not already running, skip this line if an X server already exists or you are already using a display
# (-s 0 -dpms keeps the server's own screen saver and DPMS from ever blanking the framebuffer, as the example container and the AppImage do; see the FAQ on screen locking)
# Xvfb "${DISPLAY}" -screen 0 8192x4096x24 -s 0 -dpms +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" +extension "XFIXES" +extension "XTEST" +iglx +render -nolisten "tcp" -ac -noreset -shmem >/tmp/Xvfb_selkies.log 2>&1 &

# Wait for X server to start
# echo 'Waiting for X Socket' && until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done && echo 'X Server is ready'

# Choose one between PulseAudio and PipeWire if not already running, either one must be installed

# Initialize PulseAudio (set PULSE_SERVER to unix:/run/pulse/native if your user is in the pulse-access group and pulseaudio is triggered with sudo/root), omit the below lines if a PulseAudio server is already running
# export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
# export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
# export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"
# /usr/bin/pulseaudio -k >/dev/null 2>&1 || true
# /usr/bin/pulseaudio --verbose --log-target=file:/tmp/pulseaudio_selkies.log --disallow-exit &

# Initialize PipeWire
# export PIPEWIRE_LATENCY="256/48000"
# export DISABLE_RTKIT="y"
# export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
# export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-/tmp}}"
# export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
# export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"
# pipewire &
# wireplumber &
# pipewire-pulse &

# Replace this line with your desktop environment session or skip this line if already running; on an NVIDIA GPU, export `MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_KOPPER_DRI2=1` beforehand to run OpenGL through the NVIDIA Vulkan driver
# lxqt-session &

# Replace with your wanted resolution if using without resize, DO NOT USE if there is a physical display
# selkies-resize 1920x1080

# Starts the remote desktop process with the interposers unloaded: they answer
# for /dev/input and /dev/video0 in the session's applications, which the
# gamepad and capture sides must keep seeing as the kernel reports them, and
# their process-wide hooks can stop the asyncio loop answering. The two
# variables stay set, which is what tells Selkies applications reach the
# devices through the preload.
# In the default WebSocket mode, change `--encoder=` to `h264enc-striped` or `jpeg` for a different encoder; add `--use-cpu=true` to force software encoding
# For the WebRTC transport instead, add `--mode=webrtc` (`--encoder=h264enc` is the only WebRTC encoder)
# DO NOT set `--enable-resize=true` if there is a physical display
env -u LD_PRELOAD selkies --addr=0.0.0.0 --port=8080 --enable-https=false --https-cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https-key=/etc/ssl/private/ssl-cert-snakeoil.key --basic-auth-user=user --basic-auth-password=mypasswd --encoder=h264enc --enable-resize=false &
```

The login, the encoder choice, and the HTTPS options behave as in [Quick Start](#quick-start) step 4.

**4. (WebRTC mode only) If you switched to `--mode=webrtc` and the HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start:**

**This step is only relevant to the opt-in WebRTC transport. In WebRTC mode, when there is very high latency or stutter and the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, this section is very important.**

Please read [**WebRTC and Firewall Issues**](firewall.md).

**5. Read [**Troubleshooting and FAQs**](faq.md) if something is not as intended and [**Usage**](usage.md) for more information on customizing.**

### Install an unreleased build

Every push to `main` builds the same media a release does, so an unreleased commit installs exactly like the released one above. **Nothing here needs Docker®.** Log in to GitHub, open that commit's `CI` run in [Actions](https://github.com/selkies-project/selkies/actions), and take its Build Artifacts: the `selkies-wheel` artifact holds the wheel, and the package jobs attach the `.deb`, `.rpm`, `.apk`, `.pkg.tar.zst` and the AppImage. [`gh run download`](https://cli.github.com/manual/gh_run_download) fetches them from a shell instead.

The container images are published to `ghcr.io` rather than attached to the run, as `ghcr.io/selkies-project/selkies/base:main-ubuntu26.04` and `example:main-ubuntu26.04` (and the `debiantrixie` flavor of each), which every push moves onto the new build. Run one as the [Example Container](component.md#example-container) shows, or name it in a `FROM` line to build your own desktop on it — [Container Customization](development.md#container-customization) covers that. Replace `main` with `latest` in any of these tags for the newest release instead of the newest commit.
