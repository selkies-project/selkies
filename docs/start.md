# Getting Started

## Quick Start

**Choose between this section and [Advanced Install](#advanced-install) if you need to self-host on a standalone instance or use with HPC clusters. This section is recommended for starters.**

This **Quick Start** installs the Selkies Python package directly with `pip`. The package bundles the HTML5 web client and pulls in the `pixelflux` (screen capture with H.264/JPEG encoding) and `pcmflux` (PulseAudio capture with Opus encoding) extensions as dependencies. Selkies streams over plain WebSockets by default, serving the web interface, signaling, and media on a **single port (default `8081`)**; WebRTC is available as an opt-in transport (`--mode=webrtc`).

Read [Python Application](component.md#python-application) for more details of this step and procedures for installing from the latest commit in the `main` branch.

**1. Install required dependencies, for Ubuntu or Debian-based distributions, run this command:**

```bash
sudo apt-get update && sudo apt-get install --no-install-recommends -y python3 python3-pip python3-dev jq ca-certificates curl xserver-xorg-core xvfb x11-utils x11-xkb-utils x11-xserver-utils libx11-xcb1 libxcb-dri3-0 libxkbcommon0 libxdamage1 libxfixes3 libxtst6 libxext6 libpulse0 pulseaudio
```

For hardware-accelerated H.264 encoding, additionally install the relevant GPU drivers: NVENC is provided by the NVIDIA GPU driver (`libnvidia-encode`), while Intel and AMD GPUs use VA-API (install `libva2` and your vendor's VA-API driver, such as `intel-media-va-driver-non-free` for Intel or the AMDGPU driver for AMD). Optionally install `vainfo`, `intel-gpu-tools`, `radeontop`, or `nvtop` for GPU monitoring.

**2. Install the Selkies Python package**, which bundles the HTML5 web client (fill in `SELKIES_VERSION`):

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name' | sed 's/[^0-9\.\-]*//g')"
cd /tmp && curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-${SELKIES_VERSION}-py3-none-any.whl" && sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall "selkies-${SELKIES_VERSION}-py3-none-any.whl" && rm -f "selkies-${SELKIES_VERSION}-py3-none-any.whl"
```

Alternatively, install directly from the source tree (this also builds and installs the bundled web client):

```bash
git clone https://github.com/selkies-project/selkies.git
cd selkies && sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall .
```

Either method installs the `selkies` and `selkies-resize` console commands.

**3. Set your `DISPLAY` and `PULSE_SERVER` environment variables for the X.Org X11 display server or PulseAudio audio server.**

**Selkies attaches to an existing X.Org X11 display and an already-running PulseAudio (or PipeWire-Pulse) server; it does not start them for you.** See [Advanced Install](#advanced-install) for commands to start a virtual `Xvfb` display and a PulseAudio/PipeWire server yourself.

**Check that you are using X.Org instead of Wayland (which is the default in many distributions but not supported) when using an existing display. You also need to be logged in from the login screen or autologin should be enabled.**

**The environment variables that are set here should also be set with the host application or desktop environment, else you will likely not have audio or be shown an error.**

```bash
export DISPLAY="${DISPLAY:-:0}"
export PIPEWIRE_LATENCY="128/48000"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-/tmp}}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"
```

**4. Run Selkies:**

```bash
selkies --addr=0.0.0.0 --port=8081 --enable_https=false --https_cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https_key=/etc/ssl/private/ssl-cert-snakeoil.key --basic_auth_user=user --basic_auth_password=mypasswd --encoder=h264enc --enable_resize=false
```

In the default WebSocket mode, `--encoder=` accepts `h264enc` (default; hardware NVENC or VA-API when a supported GPU is available, otherwise software `x264`), `h264enc-striped`, `openh264enc` (software OpenH264), or `jpeg`. Add `--use-cpu=true` to force software encoding. To use the opt-in WebRTC transport instead, add `--mode=webrtc` and select the encoder with `--encoder-rtc=` (`h264enc`, the hardware-first default, or `openh264enc`).

The default username (set with `--basic_auth_user=` or `SELKIES_BASIC_AUTH_USER`), when not specified, is the current user environment variable `$USER` (empty username if nonexistent), and the default password (set with `--basic_auth_password=` or `SELKIES_BASIC_AUTH_PASSWORD`), when not specified, is `mypasswd`.

Use `--enable_resize=true` if you want to fit the remote resolution to the client window and skip the next step. You **must NOT** enable this option when streaming a physical monitor.

**5. Resize to your intended resolution (DO NOT resize when streaming a physical monitor):**

```bash
selkies-resize 1920x1080
```

**6. Check the [**Joystick Interposer**](component.md#joystick-interposer) section if you need to use joystick/gamepad devices from your web browser client.**

You can replace `/usr/$LIB/selkies_joystick_interposer.so` with any non-root path of your choice if using the `.tar.gz` tarball.

**7. (WebRTC mode only) If you switched to `--mode=webrtc` and the HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start:**

**This step is only relevant to the opt-in WebRTC transport. The default WebSocket transport uses a single TCP port and needs no STUN/TURN server. In WebRTC mode, when there is very high latency or stutter and the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, this section is very important.**

Please read [**WebRTC and Firewall Issues**](firewall.md).

**8. Read [**Troubleshooting and FAQs**](faq.md) if something is not as intended and [**Usage**](usage.md) for more information on customizing.**

## Desktop Container

Full desktop containers that can be used out-of-the-box are available in separate repositories. If you can deploy Docker® or Podman containers, this is the easiest way to get started.

[`docker-nvidia-glx-desktop`](https://github.com/selkies-project/docker-nvidia-glx-desktop) and [`docker-nvidia-egl-desktop`](https://github.com/selkies-project/docker-nvidia-egl-desktop) are expandable ready-to-go out-of-the-box containerized remote desktop implementations of Selkies supporting hardware acceleration on NVIDIA and other GPUs.

The [`selkies-vdi`](https://github.com/selkies-project/selkies-vdi) or [`selkies-examples`](https://github.com/selkies-project/selkies-examples) repositories from the [Selkies Project](https://github.com/selkies-project) provide containerized virtual desktop infrastructure (VDI) templates, but are outdated. Contributions to sync the projects with the current release are welcome.

## Minimal Container

The [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) is the reference minimal-functionality container developers can base upon, or test Selkies quickly. The bare minimum Xfce4 desktop environment is installed together with Firefox, as well as an embedded TURN server inside the container for quick WebRTC firewall traversal.

Instructions are available in the [Example Container](component.md#example-container) section.

**With the default WebSocket transport, a single exposed port is all you need.** A TURN server only becomes relevant if you opt into the WebRTC transport (`--mode=webrtc`) inside a Docker® or Kubernetes container without `--network=host` or `hostNetwork: true`, or in other cases where the HTML5 web interface loads but the WebRTC connection fails. In that case, follow the instructions from [WebRTC and Firewall Issues](firewall.md) to make the container or self-hosted standalone instance use an external TURN server. This is required for all self-hosted WebRTC applications, unlike proprietary services which provide a TURN server for you.

## Advanced Install

**Choose between [Quick Start](#quick-start) and this section.**

This section installs from Ubuntu packages and shows a full run script, including how to start a virtual display and audio server yourself. It matches the reference `Dockerfile` build procedure.

### Backgrounds

Selkies has a modularized architecture, but at runtime it is a **single Python application**, packaged as the `selkies` wheel, that:

- serves the HTML5 web client, which is bundled inside the wheel (at `src/selkies/selkies_web`) and served from the same single port;
- captures and encodes the screen through the `pixelflux` extension (hardware H.264 via NVENC or VA-API, software H.264 via `x264` or OpenH264, or JPEG);
- captures and encodes audio through the `pcmflux` extension (Opus);
- injects keyboard, mouse, and gamepad input through a vendored `python-xlib` (XTEST/XFixes);
- and, only for the opt-in WebRTC transport, uses a vendored fork of `aiortc`.

`pixelflux` and `pcmflux` are installed automatically as dependencies of the wheel. There is **no separate GStreamer build or web-interface package to install** — earlier releases shipped GStreamer, a Python wheel, and a `gst-web` interface as three separate components, but the GStreamer runtime has been fully removed and the web client is now bundled into the wheel.

For more information, check the [Components](component.md#components) section.

The [All-In-One Desktop Containers](#desktop-container) support unprivileged self-hosted Kubernetes clusters and Docker®/Podman.

### Install the packaged version on self-hosted standalone machines, cloud instances, or virtual machines

**NOTE: STUN/TURN is only relevant to the opt-in WebRTC transport (`--mode=webrtc`). The default WebSocket transport uses a single TCP port. If you use WebRTC mode and both your server and client have closed ports or a restrictive firewall, you will need an external STUN/TURN server capable of `srflx` or `relay` type ICE connections; either open the UDP and TCP port ranges 49152-65535 of your server, or follow the instructions from [WebRTC and Firewall Issues](firewall.md).**

While this instruction assumes that you are installing this project systemwide, it is possible to install and run all components completely within the userspace.

**1. Install the dependencies, for Ubuntu or Debian-based distributions, run this command:**

```bash
sudo apt-get update && sudo apt-get install --no-install-recommends -y python3 python3-pip python3-dev jq ca-certificates curl xserver-xorg-core xvfb wmctrl x11-utils x11-xkb-utils x11-xserver-utils libx11-xcb1 libxcb-dri3-0 libxkbcommon0 libxdamage1 libxfixes3 libxtst6 libxext6 libpulse0 pulseaudio
```

If using supported NVIDIA GPUs, NVENC is bundled with the GPU driver (`libnvidia-encode`). If using AMD or Intel GPUs, install its graphics and VA-API drivers, as well as `libva2`. The `intel-media-va-driver-non-free` package (or `i965-va-driver-shaders` depending on your Intel GPU generation) is recommended for Intel GPUs, and the bundled VA-API driver in the AMDGPU driver is recommended for AMD GPUs. Optionally install `vainfo`, `intel-gpu-tools`, `radeontop`, or `nvtop` for GPU monitoring.

Use the following commands to retrieve the latest `SELKIES_VERSION` release, the current Ubuntu `DISTRIB_RELEASE`, and the current architecture `ARCH` in the following steps:

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name' | sed 's/[^0-9\.\-]*//g')"
export DISTRIB_RELEASE="$(grep '^VERSION_ID=' /etc/os-release | cut -d= -f2 | tr -d '\"')"
export ARCH="$(dpkg --print-architecture)"
```

**2. Install the Selkies Python package** (this component is pure Python, bundles the HTML5 web client, and any operating system is compatible, fill in `SELKIES_VERSION`)**:**

Read [Python Application](component.md#python-application) for more details of this step and procedures for installing from the latest commit in the `main` branch.

```bash
cd /tmp && curl -O -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-${SELKIES_VERSION}-py3-none-any.whl" && sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall "selkies-${SELKIES_VERSION}-py3-none-any.whl" && rm -f "selkies-${SELKIES_VERSION}-py3-none-any.whl"
```

**3. Install the Joystick Interposer to process gamepad input**, if you need to use joystick/gamepad devices from your web browser client (fill in `SELKIES_VERSION`, `DISTRIB_RELEASE`, and `ARCH` of either `amd64` for `x86_64`, and `arm64` for `aarch64`)**:**

Read [Joystick Interposer](component.md#joystick-interposer) for more details of this step and procedures for installing from the latest commit in the `main` branch.

```bash
cd /tmp && curl -o selkies-js-interposer.deb -fsSL "https://github.com/selkies-project/selkies/releases/download/v${SELKIES_VERSION}/selkies-js-interposer_v${SELKIES_VERSION}_ubuntu${DISTRIB_RELEASE}_${ARCH}.deb" && sudo apt-get update && sudo apt-get install --no-install-recommends -y ./selkies-js-interposer.deb && rm -f selkies-js-interposer.deb
```

Alternatively, users may directly place the Joystick Interposer libraries from the `selkies-js-interposer_v${SELKIES_VERSION}_ubuntu${DISTRIB_RELEASE}_${ARCH}.tar.gz` tarball into the library path, for instance into `/usr/lib/x86_64-linux-gnu` and `/usr/lib/i386-linux-gnu`. More information can be found in [Joystick Interposer](component.md#joystick-interposer).

You can replace `/usr/$LIB/selkies_joystick_interposer.so` with any non-root path of your choice if using the `.tar.gz` tarball.

**4. Run Selkies after changing the below script appropriately** (install `xvfb` and uncomment relevant sections if there is no real display, **DO NOT resize when streaming a physical monitor**)**:**

**Check that you are using X.Org instead of Wayland (which is the default in many distributions but not supported) when using an existing display. You also need to be logged in from the login screen or autologin should be enabled.**

```bash
export DISPLAY="${DISPLAY:-:0}"
# Configure the Joystick Interposer
export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
export SDL_JOYSTICK_DEVICE=/dev/input/js0
sudo mkdir -pm1777 /dev/input
sudo touch /dev/input/js0 /dev/input/js1 /dev/input/js2 /dev/input/js3
sudo chmod 777 /dev/input/js*

# Commented sections are optional but may be mandatory based on setup

# Start a virtual X11 server if not already running, skip this line if an X server already exists or you are already using a display
# Xvfb "${DISPLAY}" -screen 0 8192x4096x24 +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" +extension "XFIXES" +extension "XTEST" +iglx +render -nolisten "tcp" -ac -noreset -shmem >/tmp/Xvfb_selkies.log 2>&1 &

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
# export PIPEWIRE_LATENCY="128/48000"
# export DISABLE_RTKIT="y"
# export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
# export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-/tmp}}"
# export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
# export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"
# pipewire &
# wireplumber &
# pipewire-pulse &

# Replace this line with your desktop environment session or skip this line if already running, use VirtualGL `vglrun +wm xfce4-session` here if needed
# [ "${START_XFCE4:-true}" = "true" ] && rm -rf ~/.config/xfce4 && xfce4-session &

# Replace with your wanted resolution if using without resize, DO NOT USE if there is a physical display
# selkies-resize 1920x1080

# Starts the remote desktop process
# In the default WebSocket mode, change `--encoder=` to `h264enc-striped`, `openh264enc`, or `jpeg` for a different encoder; add `--use-cpu=true` to force software encoding
# For the WebRTC transport instead, add `--mode=webrtc` and set `--encoder-rtc=` to `h264enc` or `openh264enc`
# DO NOT set `--enable_resize=true` if there is a physical display
selkies --addr=0.0.0.0 --port=8081 --enable_https=false --https_cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https_key=/etc/ssl/private/ssl-cert-snakeoil.key --basic_auth_user=user --basic_auth_password=mypasswd --encoder=h264enc --enable_resize=false &
```

The default username (set with `--basic_auth_user=` or `SELKIES_BASIC_AUTH_USER`), when not specified, is the current user environment variable `$USER` (empty username if nonexistent), and the default password (set with `--basic_auth_password=` or `SELKIES_BASIC_AUTH_PASSWORD`), when not specified, is `mypasswd`.

**5. (WebRTC mode only) If you switched to `--mode=webrtc` and the HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start:**

**This step is only relevant to the opt-in WebRTC transport. In WebRTC mode, when there is very high latency or stutter and the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, this section is very important.**

Please read [**WebRTC and Firewall Issues**](firewall.md).

**6. Read [**Troubleshooting and FAQs**](faq.md) if something is not as intended and [**Usage**](usage.md) for more information on customizing.**

### Install the latest build on self-hosted standalone machines, cloud instances, or virtual machines

**Build artifacts for every `main` branch commit are available after logging into GitHub in [Actions](https://github.com/selkies-project/selkies/actions), and you do not need Docker® to download them.**

Otherwise, Docker®/Podman (or any equivalent) may be used if you want to use builds from the latest commit. Refer to [Components](component.md) for more information.

This method can be also used when building a new container image with the `FROM [--platform=<platform>] <image> [AS <name>]` and `COPY [--from=<name>] <src_path> <dest_path>` instruction instead of using the `docker` CLI. Change `main` to `latest` if you want the latest release version instead of the latest development version.
