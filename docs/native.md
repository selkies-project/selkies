---
title: Native Install
description: Install Selkies as a package or an AppImage and attach it to a display and audio server you already run.
---

Selkies also ships outside a container: native packages for the common distributions, and an AppImage that installs nothing. Neither brings a desktop, a display server, or an audio server — they attach to the ones you run — so [Getting Started](start.md) is the shorter road if a container will do.

None of these needs a Python environment: the web client, the `pixelflux` (screen capture with H.264/JPEG encoding) and `pcmflux` (PulseAudio capture with Opus encoding) extensions, and the interposers all travel inside. Every block below uses the release version, which is the release's tag, so paste this line first (set `SELKIES_VERSION` yourself for a release other than the latest):

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name')"
```

## Packages

Installs a private Python environment at `/opt/selkies`, puts `selkies`, `selkies-resize`, and `selkies-gpu-probe` on `PATH`, carries both interposers, and pulls every system library it needs through your package manager. Pick your distribution's line. Every file is `selkies-<version>-<distribution>-<architecture>.<format>` with the version exactly as the tag spells it, pre-release or final (`selkies-2.0.0rc0-ubuntu26.04-amd64.deb`, `selkies-2.0.0-fc-x86_64.rpm`), the distribution left out where the package is not built per distribution; inside, the package carries the version the way its packager orders it (`2.0.0~rc0-1` for dpkg and rpm, `2.0.0_rc0-r0` for apk), so the final release upgrades over a pre-release:

```bash
# Ubuntu and Debian. The suffix names the distribution the package was built in
# (ubuntu24.04, ubuntu26.04, bookworm, trixie); this reads yours from os-release
. /etc/os-release
DISTRO="$([ "${ID}" = "ubuntu" ] && echo "ubuntu${VERSION_ID}" || echo "${VERSION_CODENAME}")"
PKG="selkies-${SELKIES_VERSION}-${DISTRO}-$(dpkg --print-architecture).deb"
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/${SELKIES_VERSION}/${PKG}"
sudo apt-get install -y "./${PKG}"
```

```bash
# Fedora and Enterprise Linux
. /etc/os-release
PKG="selkies-${SELKIES_VERSION}-$([ "${ID}" = "fedora" ] && echo fc || echo el9)-$(uname -m).rpm"
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/${SELKIES_VERSION}/${PKG}"
sudo dnf install -y "./${PKG}"
```

```bash
# Alpine
PKG="selkies-${SELKIES_VERSION}-$(uname -m).apk"
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/${SELKIES_VERSION}/${PKG}"
sudo apk add --allow-untrusted "./${PKG}"
```

```bash
# Arch Linux, which Arch publishes for x86_64 alone
PKG="selkies-${SELKIES_VERSION}-$(uname -m).pkg.tar.zst"
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/${SELKIES_VERSION}/${PKG}"
sudo pacman -U "./${PKG}"
```

The Arch package also carries a pacman hook that opens the CUPS scheduler's mode whenever `cups` or `selkies` is installed or upgraded: Arch installs `cupsd` readable by root alone, and the print queue runs a copy of it as the session user.

For hardware-accelerated H.264, add your GPU's driver: NVENC comes with the NVIDIA driver (`libnvidia-encode`), and Intel and AMD encode through VA-API (`libva2` plus your vendor's driver — `intel-media-va-driver-non-free` for Intel, or `i965-va-driver-shaders` for older generations, and the AMDGPU driver's own for AMD). `vainfo`, `intel-gpu-tools`, `radeontop`, and `nvtop` are optional monitors.

## The AppImage

Runs from wherever you put it, on any distribution with glibc 2.28 or newer (Enterprise Linux 8, Debian 10, Ubuntu 18.10, and later), without touching the system. Every Python and native dependency is inside; it starts an `Xvfb` when the display it is pointed at is not up, and its own PulseAudio when none is listening:

```bash
APP="selkies-${SELKIES_VERSION}-$(uname -m).AppImage"
curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/${SELKIES_VERSION}/${APP}"
chmod +x "./${APP}"
"./${APP}" --public --port=8080 --basic-auth-user=user --basic-auth-password=mypasswd
```

`--public` accepts connections on every interface, IPv4 and IPv6; without it, Selkies listens on the loopback addresses only (`127.0.0.1,::1`), for a session reached through SSH port forwarding or a reverse proxy on the same machine. `--addr=` names particular addresses to listen on instead, and is not given together with `--public`.

Given `selkies-session` as its first argument, it runs a whole session instead, with its own display and sound server and the host's desktop, as [Jupyter, Coder, and Open OnDemand](platforms.md) describes: `"./${APP}" selkies-session --port=8080 --enable-basic-auth=false`.

What it takes from the host is the graphics stack and the display server: `libgbm`, `libEGL`, and the GPU's own driver have to be the host's for the GPU to be reachable at all, an X11 session needs the host's X server (or `Xvfb`), and the headless Wayland backend needs the host's `libwayland-server`. Everything above them travels with the AppImage.

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
selkies --public --port=8080 --enable-https=false --https-cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https-key=/etc/ssl/private/ssl-cert-snakeoil.key --basic-auth-user=user --basic-auth-password=mypasswd --encoder=h264enc --enable-resize=false
```

In the default WebSocket mode, `--encoder=` accepts `h264enc` (default), `h265enc`, `vp8enc`, `vp9enc`, and `av1enc` (each hardware NVENC or VA-API where the GPU carries the codec, otherwise the software encoder `pixelflux` was built with — `x264` (or OpenH264 in a GPL-free build), `x265` (or kvazaar in a GPL-free build), libvpx, SVT-AV1), `h264enc-striped` (striped software H.264), or `jpeg`. Add `--use-cpu=true` to force software encoding. To use the opt-in WebRTC transport instead, add `--mode=webrtc`; the same `--encoder=` knob applies to the full-frame encoders (`h264enc`, `h265enc`, `vp8enc`, `vp9enc`, `av1enc`), a browser that declines the codec is answered with H.264, and the striped `h264enc-striped` and `jpeg` fall back to the default with a logged warning.

`--public` above accepts connections on every interface, IPv4 and IPv6; without it, Selkies listens on the loopback addresses only (`127.0.0.1,::1`), and `--addr=` names particular addresses instead. `--enable-https=false` leaves the web interface on plain HTTP, which browsers accept as a secure context only on `localhost`; the clipboard, gamepads, pointer lock, and the microphone and webcam need one. Setting `--enable-https=true` is the whole switch: the `--https-cert=` and `--https-key=` paths are the `ssl-cert-snakeoil` pair Debian and Ubuntu install, and when they are absent Selkies writes a self-signed pair itself, so nothing has to be prepared. Browsers warn once on a self-signed certificate; a certificate from an authority at those paths, or a reverse proxy terminating TLS in front, avoids the warning.

The default username (set with `--basic-auth-user=` or `SELKIES_BASIC_AUTH_USER`), when not specified, is taken from the `CUSTOM_USER`, then `USERNAME`, then `USER` environment variable, and is `ubuntu` when none of them is set. The password has no default: set it with `--basic-auth-password=`, `SELKIES_BASIC_AUTH_PASSWORD`, `PASSWORD`, or `PASSWD`, or pass `--enable-basic-auth=false` to serve without a login. Selkies refuses to start with basic authentication enabled and no password, so a login is never served that nobody chose a password for.

Dynamic resizing (`--enable-resize`, **on by default**) fits the remote resolution to the client window; the command above turns it off because it **must NOT** be enabled when streaming a physical monitor. Leave it on when streaming a virtual display (`Xvfb` or the Wayland backend) and skip the next step.

**3. Resize to your intended resolution (DO NOT resize when streaming a physical monitor):**

```bash
selkies-resize 1920x1080
```

**4. Check the [**Input Interposer**](components/input-interposer.md) section if you need to use joystick/gamepad devices from your web browser client, and the [**V4L2 Interposer**](components/v4l2-interposer.md) section for the webcam.**

You can install `selkies_input_interposer.so` and `selkies_v4l2_interposer.so` to any non-root path of your choice and point `SELKIES_INTERPOSER` and `SELKIES_WEBCAM_INTERPOSER` at them.

**5. (WebRTC mode only) If you switched to `--mode=webrtc` and the HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start:**

**This step is only relevant to the opt-in WebRTC transport. The default WebSocket transport uses a single TCP port and needs no STUN/TURN server. In WebRTC mode, when there is very high latency or stutter and the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, this section is very important.**

Please read [**WebRTC and Firewall Issues**](firewall.md).

**6. Read [**Troubleshooting and FAQs**](faq.md) if something is not as intended and [**Usage**](usage.md) for more information on customizing.**


## Advanced Install

**Choose between [Run a session](#run-a-session) and this section.**

[Run a session](#run-a-session) gets a session up. This section is what is inside one, and a full run script that starts the display and audio servers itself rather than attaching to yours. It matches the reference `Dockerfile` build procedure.

### Backgrounds

Selkies has a modularized architecture, but at runtime it is a **single Python application**, the one every package and image carries, that:

- serves the HTML5 web client, which is bundled inside it (at `src/selkies/selkies_web`) and served from the same single port;
- captures and encodes the screen through the `pixelflux` extension (hardware H.264 via NVENC or VA-API, software H.264 via `x264` — or OpenH264 in a GPL-free `pixelflux` build — or JPEG);
- captures and encodes audio through the `pcmflux` extension (Opus);
- injects keyboard, mouse, and gamepad input through a vendored `python-xlib` (XTEST/XFixes);
- and, only for the opt-in WebRTC transport, uses a vendored fork of `aiortc`.

`pixelflux`, `pcmflux`, and the web client all travel inside whichever medium you installed. There is **no separate multimedia-framework build or web-interface package to install**.

For more information, check the [Components](components/index.md) section.

The [All-In-One Desktop Containers](start.md#desktop-container) support unprivileged self-hosted Kubernetes clusters and Docker®/Podman.

### Run a full session on a standalone machine, cloud instance, or virtual machine

**NOTE: STUN/TURN is only relevant to the opt-in WebRTC transport (`--mode=webrtc`). The default WebSocket transport uses a single TCP port. If you use WebRTC mode and both your server and client have closed ports or a restrictive firewall, you will need an external STUN/TURN server capable of `srflx` or `relay` type ICE connections; either open the UDP and TCP port ranges 49152-65535 of your server, or follow the instructions from [WebRTC and Firewall Issues](firewall.md).**

`selkies-session` does all of this from one command where no desktop is already on the screen: it starts a sound server unless one answers, an Xvfb of its own (or, with `SELKIES_WAYLAND=true`, Selkies' compositor), the machine's default desktop, and Selkies with every argument it is given, and it preloads the interposers `SELKIES_INTERPOSER` and `SELKIES_WEBCAM_INTERPOSER` name into that desktop. [Jupyter, Coder, and Open OnDemand](platforms.md) describes it in full:

```bash
selkies-session --session=xfce --public --port=8080 --basic-auth-password=mypasswd
```

The steps below do the same by hand, for a machine whose display, sound server, or desktop is set up on its own or has to outlive Selkies.

While this instruction assumes that you are installing this project systemwide, it is possible to install and run all components completely within the userspace.

**1. Install Selkies** by either route above; the native package is the one this script assumes, since it puts `selkies` on `PATH` and pulls in the system libraries through your package manager.

**2. Build the Input Interposer to process gamepad input**, if you need to use joystick/gamepad devices from your web browser client in an environment without `/dev/uinput` — typically an unprivileged container. Where `/dev/uinput` is writable, Selkies registers gamepads as [kernel devices](components/input-interposer.md#kernel-gamepads) instead and this step, along with the `LD_PRELOAD` exports below, is unnecessary. Otherwise applications receive gamepad input only when they are started with the interposer preloaded, and `fake-udev` is additionally required for applications that discover devices through `libudev`. Both are built and wired automatically in the [Desktop Container](components/desktop-image.md) and the desktop containers. Elsewhere, build them from source (they are small `LD_PRELOAD` libraries needing only libc; `fake-udev` passes everything but the pads through to the system `libudev` it finds at runtime):

```bash
git clone https://github.com/selkies-project/selkies.git && cd selkies
apt-get update && apt-get install --no-install-recommends -y build-essential
make -C addons/input-interposer && PREFIX=/usr make -C addons/input-interposer install
cd addons/fake-udev && make && cp libudev.so.1.0.0-fake libudev.so.1 libudev.so /usr/lib/$(gcc -print-multiarch)/
```

On `x86_64`, add `apt-get install -y gcc-multilib && make -C addons/input-interposer install32` (and `make all32` in `addons/fake-udev`) for 32-bit applications such as most of the Steam and Wine catalog, since `/usr/$LIB` resolves per process bitness. Container images built on the `.deb`, `.rpm`, `.apk` or `.pkg.tar.zst` package need neither step: each of them already carries the interposer, and the `.deb` and `.rpm` carry the 32-bit variant too.

More information can be found in [Input Interposer](components/input-interposer.md).

You can install `selkies_input_interposer.so` to any non-root path of your choice and point `SELKIES_INTERPOSER` at it. The webcam uplink has a matching library built the same way, `make -C addons/v4l2-interposer && PREFIX=/usr make -C addons/v4l2-interposer install`; see [V4L2 Interposer](components/v4l2-interposer.md).

SDL2 applications discover the four pads through `fake-udev`. Where discovery through `libudev` is unavailable — `SDL_JOYSTICK_DISABLE_UDEV=1`, an SDL sandbox build, or an SDL built without udev — export `SDL_JOYSTICK_DEVICE=/dev/input/event1000:/dev/input/event1001:/dev/input/event1002:/dev/input/event1003` instead, which needs no placeholder files. Never name the joydev nodes there: with `fake-udev` active, a `/dev/input/js0` hint is a second, different node for the slot SDL already enumerated as `event1000`, so the pad shows up twice.

**3. Run Selkies after changing the below script appropriately** (install `xvfb` and uncomment relevant sections if there is no real display, **DO NOT resize when streaming a physical monitor**)**:**

**Check that you are using X.Org instead of Wayland (which is the default in many distributions) when attaching to an existing display -- an already-running Wayland session cannot be captured. A separate headless Wayland mode (started and owned by Selkies itself) is available with `--wayland=true` / `SELKIES_WAYLAND=true`, but when attaching to an existing graphical session that session must be X.Org. You also need to be logged in from the login screen or autologin should be enabled.**

```bash
export DISPLAY="${DISPLAY:-:0}"
# Configure the interposers: gamepads for the session's applications, and the
# webcam if the client's camera is forwarded into it
export SELKIES_INTERPOSER='/usr/$LIB/selkies_input_interposer.so'
export SELKIES_WEBCAM_INTERPOSER='/usr/$LIB/selkies_v4l2_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}:${SELKIES_WEBCAM_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
sudo mkdir -pm1777 /dev/input

# Commented sections are optional but may be mandatory based on setup

# Start a virtual X11 server if not already running, skip this line if an X server already exists or you are already using a display
# (-s 0 -dpms keeps the server's own screen saver and DPMS from ever blanking the framebuffer, as the desktop container and the AppImage do; see the FAQ on screen locking)
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
# In the default WebSocket mode, change `--encoder=` to `h265enc`, `vp8enc`, `vp9enc`, `av1enc`, `h264enc-striped` or `jpeg` for a different encoder; add `--use-cpu=true` to force software encoding
# For the WebRTC transport instead, add `--mode=webrtc` (the full-frame encoders stream over WebRTC; `h264enc-striped` and `jpeg` are WebSocket-only)
# DO NOT set `--enable-resize=true` if there is a physical display
env -u LD_PRELOAD selkies --public --port=8080 --enable-https=false --https-cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https-key=/etc/ssl/private/ssl-cert-snakeoil.key --basic-auth-user=user --basic-auth-password=mypasswd --encoder=h264enc --enable-resize=false &
```

The login, the encoder choice, and the HTTPS options behave as in [Run a session](#run-a-session) step 2.

**4. (WebRTC mode only) If you switched to `--mode=webrtc` and the HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start:**

**This step is only relevant to the opt-in WebRTC transport. In WebRTC mode, when there is very high latency or stutter and the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, this section is very important.**

Please read [**WebRTC and Firewall Issues**](firewall.md).

**5. Read [**Troubleshooting and FAQs**](faq.md) if something is not as intended and [**Usage**](usage.md) for more information on customizing.**

### Install an unreleased build

Every push to `main` builds the same media a release does, so an unreleased commit installs exactly like the released one above. **Nothing here needs Docker®.** Log in to GitHub, open that commit's `CI` run in [Actions](https://github.com/selkies-project/selkies/actions), and take its Build Artifacts: the `selkies-wheel` artifact holds the wheel, and the package jobs attach the `.deb`, `.rpm`, `.apk`, `.pkg.tar.zst`, and the AppImage. [`gh run download`](https://cli.github.com/manual/gh_run_download) fetches them from a shell instead.

The container images are published to `ghcr.io` rather than attached to the run, as `ghcr.io/selkies-project/selkies/base:main-ubuntu26.04` and `desktop:main-ubuntu26.04` (and the `debiantrixie` flavor of each), which every push moves onto the new build. Run one as the [Desktop Container](components/desktop-image.md) shows, or name it in a `FROM` line to build your own desktop on it — [Container Customization](development.md#container-customization) covers that. Replace `main` with `latest` in any of these tags for the newest release instead of the newest commit.
