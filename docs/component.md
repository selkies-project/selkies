---
title: Components
description: The core components and optional addons Selkies is built from, and the encoders and interfaces each one supports.
---

## Component Structure

Selkies is composed of a small number of core components plus several optional addons.

**Refer to [Getting Started](start.md) on how you can get on board.**

Retrieve the latest `SELKIES_VERSION` release, and pick the `DISTRIB_RELEASE` flavor of the
container images below (`ubuntu26.04` or `trixie`). The flavor names the distribution
inside the image, so it is a free choice and not a property of the host:

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name' | sed 's/^v//')"
export DISTRIB_RELEASE="ubuntu26.04"
```

When instructed to install [binfmt](https://github.com/tonistiigi/binfmt), use the following command with Docker/Podman:

```bash
docker run --rm --privileged tonistiigi/binfmt:latest --install all
```

### Core Components

At runtime, Selkies is a **single Python application** — the `selkies` wheel. The HTML5 web client is bundled into it, and screen/audio capture and encoding are provided by the `pixelflux` and `pcmflux` extensions, which are installed automatically as dependencies of the wheel.

Every release carries the same build in each medium below. The [Releases](https://github.com/selkies-project/selkies/releases) page holds the architecture-independent wheel, a `.deb` for Ubuntu 24.04 and 26.04 and for Debian bookworm and trixie, an `.rpm` for Fedora and Enterprise Linux 9, an Alpine `.apk`, an Arch `.pkg.tar.zst`, a self-contained AppImage, and the `noarch` conda package the AppImage environment is built from (`pixelflux`, `pcmflux`, `pulsectl-asyncio`, and `aitop` have no conda-forge builds, so a conda install of it still needs those from pip). Each of those is built for both `x86_64` and `aarch64`, except the Arch package, which Arch Linux publishes for `x86_64` alone. The container images below are published to `ghcr.io` instead: the example image as `v${SELKIES_VERSION}-${DISTRIB_RELEASE}`, the coTURN and TURN-REST addons as `v${SELKIES_VERSION}`, each beside its floating `latest` tag.

A pre-release ships the same media under a tag such as `2.0.0rc0` and is marked as a pre-release: the floating `latest` image tags stay on the last full release by default, the `releases/latest` API keeps pointing at it, and `pip` resolves the pre-release only when asked with `--pre` or an exact version.

For the most recent unreleased commit, download the Build Artifacts of the `CI`, `Images`, or `Packages` workflows for that commit from the [GitHub Actions Workflow Runs](https://github.com/selkies-project/selkies/actions). Build Artifacts can also be downloaded using the [GitHub CLI](https://cli.github.com) command [`gh run download`](https://cli.github.com/manual/gh_run_download).

#### Python Application

The term `host` or `server` refers to the [Python components](https://github.com/selkies-project/selkies/tree/main/src/selkies) across this documentation.

The Python components are responsible for the host server backend: capturing and encoding the host screen (via `pixelflux`) and audio (via `pcmflux`), injecting keyboard/mouse/gamepad input into the X11 display (via a vendored `python-xlib` using XTEST/XFixes), receiving input signals and communicating other data (including the clipboard) between the client and the host, serving the HTML5 web client, and — only in WebRTC mode — establishing the WebRTC connection to the client. Everything is served by a single [`aiohttp`](https://docs.aiohttp.org) server on a **single port (default `8080`)**.

In the default WebSocket mode, encoded screen frames, audio, input, and other data are multiplexed over WebSocket connections to a WebCodecs-based web client. In the opt-in WebRTC mode (`--mode=webrtc`), host screen video and audio are transported using the WebRTC `MediaStream` interface (through a vendored fork of [`aiortc`](https://github.com/aiortc/aiortc) under `src/selkies/webrtc/`), and other data are transported using the WebRTC `DataChannel` interface.

The architecture-independent wheel is available with the name **`selkies-${SELKIES_VERSION}-py3-none-any.whl`** for download in the [Releases](https://github.com/selkies-project/selkies/releases) for the latest stable version.

**Instructions from [Advanced Install](start.md#advanced-install) still apply below.**

For the most recent unreleased commit, download the **`selkies-wheel`** artifact from the `CI` workflow run of that commit in the [GitHub Actions Workflow Runs](https://github.com/selkies-project/selkies/actions), then install it with pip (Build Artifacts can also be downloaded using the [GitHub CLI](https://cli.github.com) command [`gh run download`](https://cli.github.com/manual/gh_run_download)):

```bash
sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall selkies-0.0.0.dev0-py3-none-any.whl
# Run the Selkies Python executable after all components are installed
selkies --addr=0.0.0.0 --port=8080 --enable-https=false --https-cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https-key=/etc/ssl/private/ssl-cert-snakeoil.key --basic-auth-user=user --basic-auth-password=mypasswd --encoder=h264enc --enable-resize=false
```

One other alternative way to install the Python application components from the most recent unreleased commit:

```bash
git clone https://github.com/selkies-project/selkies.git
cd selkies
sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall .
# Run the Selkies Python executable after all components are installed
selkies --addr=0.0.0.0 --port=8080 --enable-https=false --https-cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https-key=/etc/ssl/private/ssl-cert-snakeoil.key --basic-auth-user=user --basic-auth-password=mypasswd --encoder=h264enc --enable-resize=false
```

Installing the wheel also installs the `selkies`, `selkies-resize`, and `selkies-gpu-probe` console commands.

#### Web Client

The term `client` refers to the [web components](https://github.com/selkies-project/selkies/tree/main/addons/selkies-web-core) across this documentation.

The web client is a WebCodecs-based HTML5 application (with the core `selkies-core.js`, the WebSocket transport core `selkies-ws-core.js`, the WebRTC transport core `selkies-wr-core.js`, and the input library `lib/input.js`). It is responsible for the web browser interface that you see when you use Selkies.

It decodes the incoming H.264 or JPEG stream using the browser [WebCodecs](https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API) API with a low-latency zero-copy rendering path, plays Opus audio, and detects keyboard, mouse, gamepad, and clipboard input from the user, then sends them to the host server backend. It also handles remote cursors with the Pointer Lock API so that you can correctly control interactive applications and games.

The web client source lives at [`addons/selkies-web-core`](https://github.com/selkies-project/selkies/tree/main/addons/selkies-web-core) and is built and bundled into the Python wheel automatically (installed at `src/selkies/selkies_web`), so **there is no separate web package to download or install**. To serve your own copy of the web files, point `--web-root=` (or the `SELKIES_WEB_ROOT` environment variable) at a built web directory containing an `index.html`. Rebranding (name, icons, manifest) is done at build time in the `addons/selkies-web-core` source tree, not by editing the shipped artifacts.

#### Media Capture and Encoding (`pixelflux` and `pcmflux`)

Screen capture and video encoding are performed by [`pixelflux`](https://pypi.org/project/pixelflux/), a Rust (PyO3) extension. It encodes H.264 with hardware NVENC (NVIDIA) or VA-API (Intel/AMD) when a supported GPU is available, and otherwise falls back to software H.264 (`x264` or the BSD-licensed OpenH264), or encodes Motion JPEG. H.265 and AV1 in the capture path are planned but not yet implemented.

Audio capture and encoding are performed by [`pcmflux`](https://pypi.org/project/pcmflux/), a companion Rust (PyO3) extension that captures from PulseAudio (or PipeWire-Pulse) and encodes to Opus.

Both are pulled in automatically as dependencies of the `selkies` wheel, so you normally do not install them separately.

**Licensing note (GPL toggle):** the default software H.264 encoder of `pixelflux` uses GPL-2.0+ `libx264`, enabled by default with an install-time notice. Build `pixelflux` from source with `PIXELFLUX_ENABLE_GPL=0` to exclude every GPL-licensed component (the BSD-licensed OpenH264 encoder then substitutes for software H.264).

### Optional Components

These components are not required for the base Selkies runtime, but may be needed for specific deployments or preferences. These sections are nonetheless recommended to be read carefully.

#### Joystick Interposer

The [Joystick Interposer](https://github.com/selkies-project/selkies/tree/main/addons/js-interposer) is a special library that allows the usage of joysticks or gamepads inside unprivileged containers (most of the occasions with shared Kubernetes clusters or HPC clusters), where host kernel devices required for creating a joystick interface are not available. It uses an `LD_PRELOAD` hack to intercept application calls that open a Linux joystick/gamepad device and pass data through a unix domain socket, translating gamepad events from Selkies into joystick/gamepad events without requiring access to `/dev/input/js0` or kernel modules such as `uinput` (much like how [VirtualGL](https://github.com/VirtualGL/virtualgl) intercepts OpenGL commands).

> **Note:** the `LD_PRELOAD` used here (and in [fake-udev](#fake-udev)) is a deliberate, legitimate interposition technique for redirecting device access in unprivileged environments. It is unrelated to — and distinct from — the process-global `LD_PRELOAD` anti-pattern that `pixelflux`'s multi-GPU NVENC support specifically avoids when selecting a GPU for hardware encoding.

On this backend Selkies delivers gamepad input over the sockets alone, so an application sees a controller only when it is started with the interposer preloaded. It is meant for containers: on a host where the kernel is reachable, [Kernel Gamepads](#kernel-gamepads) covers the same ground with no preloading and no shadowed system libraries. The interposer is built from source and wired automatically in the [Example Container](#example-container) and the desktop containers, every native Selkies package ships it under `/usr/$LIB` for images built on those, and the AppImage carries it at `usr/lib/selkies_joystick_interposer.so` (its `AppRun` exports the path as `SELKIES_INTERPOSER` rather than preloading it, since Selkies itself must keep seeing the real device nodes); elsewhere, build and install it (and [fake-udev](#fake-udev)) from the source in this repository:

```bash
git clone https://github.com/selkies-project/selkies.git && cd selkies
apt-get update && apt-get install --no-install-recommends -y build-essential
make -C addons/js-interposer && PREFIX=/usr make -C addons/js-interposer install
cd addons/fake-udev && make && cp libudev.so.1.0.0-fake libudev.so.1 libudev.so /usr/lib/$(gcc -print-multiarch)/
```

The following paths are required to exist for the Joystick Interposer to pass the joystick/gamepad input to various applications:

```bash
mkdir -pm1777 /dev/input
touch /dev/input/js0 /dev/input/js1 /dev/input/js2 /dev/input/js3
chmod 777 /dev/input/js*
```

The following environment variables are required to be set in the environment each application is being run in to receive the joystick/gamepad input.

```bash
export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
export SDL_JOYSTICK_DEVICE=/dev/input/js0
```

You can replace `/usr/$LIB/selkies_joystick_interposer.so` with any non-root path of your choice for the interposer library.

Check the [Joystick Interposer README.md](https://github.com/selkies-project/selkies/tree/main/addons/js-interposer/README.md) documentation for usage instruction and compiling information on other platforms.

Check the following links for explanations of similar, but different attempts, for reference:

<https://github.com/Steam-Headless/dumb-udev>

<https://github.com/games-on-whales/inputtino>

<https://github.com/games-on-whales/inputtino/tree/stable/src/uhid>

<https://games-on-whales.github.io/wolf/stable/dev/fake-udev.html>

<https://github.com/games-on-whales/wolf/tree/stable/src/fake-udev>

#### fake-udev

The [fake-udev](https://github.com/selkies-project/selkies/tree/main/addons/fake-udev) addon provides a fake `libudev` shared library (`libudev.so.1`) designed to be used with `LD_PRELOAD`. It intercepts `libudev` calls and simulates the presence of a fixed set of virtual gamepads, so that applications which discover input devices through `libudev` (for example, via `udev_enumerate_scan_devices`) find the Selkies virtual gamepads. A running udev daemon is no substitute on this backend: the pads exist only as interposer sockets, so a real `libudev` query never reports them (the [kernel devices](#kernel-gamepads) are the case where it does). fake-udev covers discovery and the [Joystick Interposer](#joystick-interposer) covers the device itself — applications that enumerate through `libudev` need both, and, like the interposer, it uses `LD_PRELOAD` by design.

#### Kernel Gamepads

Where `/dev/uinput` is available — a desktop host rather than an unprivileged container — Selkies registers each gamepad slot as a real kernel device instead. Applications then enumerate it through the kernel like any USB controller, so neither the [Joystick Interposer](#joystick-interposer) nor [fake-udev](#fake-udev) is involved and nothing has to be preloaded. This is what lets Steam, Proton, and browsers running inside the remote desktop find the controller.

`SELKIES_UINPUT_GAMEPAD` (`--uinput-gamepad`) selects the behavior:

| Value | Behavior |
| --- | --- |
| `auto` (default) | Kernel devices when `/dev/uinput` is writable and the interposer is not configured for the session (`SELKIES_INTERPOSER` or `LD_PRELOAD`); the interposer sockets otherwise. |
| `true` | Always register kernel devices. |
| `false` | Never register kernel devices. |

The kernel device is the same Xbox pad the interposer presents, with the same axis ranges, and it is created when a client's controller is associated with the slot, so an idle slot is not a phantom controller. The interposer sockets stay bound either way; avoid running an application against both backends at once, or it will see the pad twice.

This needs the `uinput` module and write access to `/dev/uinput` for the account running Selkies, and read access to the created `/dev/input/event*` node for the applications:

```bash
sudo modprobe uinput
sudo usermod -aG input "$(whoami)"
```

#### Universal Touch Gamepad

The [Universal Touch Gamepad](https://github.com/selkies-project/selkies/tree/main/addons/universal-touch-gamepad) is a JavaScript library that adds a customizable on-screen touch gamepad overlay to the web interface. It intercepts `navigator.getGamepads()` to inject a virtual gamepad, making touch devices compatible with applications and games that expect the browser Gamepad API.

#### Selkies Dashboard

The [Selkies Dashboard](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard) and the modern TypeScript variant [Selkies Dashboard (Wish)](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard-wish) are reference React dashboards that demonstrate how to build and brand your own sidebar/control UI on top of `selkies-core` using `window` messaging. They are provided as examples/starting points, not as a required component.

#### Example Container

The [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) is the reference minimal-functionality container developers can base upon, or test Selkies quickly. The bare minimum LXQt desktop (Openbox window manager) is installed together with Firefox, as well as an embedded TURN server inside the container for quick WebRTC firewall traversal. The container defaults to an X11 (Xvfb) session; set `SELKIES_WAYLAND=true` to switch it to the headless Wayland backend instead. Under the Wayland backend the LXQt session runs natively on the nested compositor, anchoring its panel and desktop through layer-shell and controlling its windows through wlr-foreign-toplevel.

The same LXQt desktop runs on either backend. On Wayland the capture compositor Selkies owns serves Wayland clients and manages no windows, so the container nests [labwc](https://labwc.github.io) inside it. That compositor supplies window management, the titlebar controls every window carries, and an XWayland server, so X11-only applications keep working; Selkies detects its socket and aims input, clipboard and display scaling at it. `-e SELKIES_WAYLAND_COMPOSITOR=<name>` runs another compositor there instead, and `-e SELKIES_WAYLAND_COMPOSITOR=none` skips the nested compositor entirely: applications then connect to the capture compositor directly, which is leaner for a single Wayland-native application but leaves no window management, no XWayland, and no desktop session.

A second display needs no configuration. A nested compositor cannot gain or lose a screen while it runs, so the session opens both at startup and Selkies holds the second at a token size until a client connects for it — the desktop stays the shape of what is being shown, and the screen grows to the second display's size the moment it is opened and shrinks back when it closes. `-e SELKIES_WAYLAND_OUTPUTS=N` fixes a different count.

Neither applies with `SELKIES_WAYLAND_COMPOSITOR=none`: applications sit on the capture compositor itself and simply see outputs appear and disappear as Selkies creates them.

A Wayland session asked for on a GPU it cannot reach starts as X11 instead. The compositor needs a working GBM/EGL stack on a DRM render node; where there is none — no `/dev/dri` in the container, an NVIDIA runtime without the `graphics` driver capability, a node with no allocator behind it — it composites in software and hands its clients no dmabuf either, while the same container under Xvfb still reaches the GPU. Because device paths do not answer whether that stack works, the container runs the compositor's own renderer bring-up at startup and switches backend on what it finds. With no GPU present at all both backends render in software, so Wayland stays. The check ships as `selkies-gpu-probe`, which prints the backend it recommends and why, so any container can make the same decision (and you can ask it yourself with `docker exec <container> selkies-gpu-probe`).

`-e SELKIES_WAYLAND_X11_FALLBACK=false` keeps Wayland regardless. The session then renders in software throughout: a compositor without a GPU shares no dmabuf, so applications pointed at the GPU's driver produce buffers it cannot accept and draw nothing at all.

Read the [Development](development.md) section for customizing this container for your own usage.

Run the Docker®/Podman container built from the [`Example Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/example/Dockerfile), then connect to port **8080** of your Docker®/Podman host to access the web interface (Username: **`ubuntu`**, Password: **`mypasswd`**, **set `DISTRIB_RELEASE` to `ubuntu26.04` or `trixie`, and replace `main` to `latest` for the latest stable release**):

```bash
docker run --name selkies -it -d --rm -e SELKIES_TURN_PROTOCOL=udp -e SELKIES_TURN_PORT=3478 -e TURN_MIN_PORT=65532 -e TURN_MAX_PORT=65535 -p 8080:8080 -p 3478:3478 -p 3478:3478/udp -p 65532-65535:65532-65535 -p 65532-65535:65532-65535/udp ghcr.io/selkies-project/selkies/example:main-${DISTRIB_RELEASE}
```

Add `--gpus 1 --runtime nvidia` to `docker run` when using NVIDIA GPUs, or `--device /dev/dri` for Intel and AMD.

Hardware OpenGL is set up automatically for whichever GPU is passed in. On NVIDIA, GL runs through [Zink](https://docs.mesa3d.org/drivers/zink.html) on the NVIDIA Vulkan driver, which is what replaces VirtualGL here; other vendors render through the X server's DRI3 render node, for which the image carries an [Xvfb with DRI3](https://github.com/linuxserver/docker-xvfb) in place of the distribution's. `-e DISABLE_ZINK=true` opts out of Zink, and `-e SELKIES_RENDER_DRI=/dev/dri/renderD###` (or the legacy `DRINODE`) names the render node, the same setting the Wayland backend renders on; without a GPU, Mesa falls back to software rendering either way.

Port 3478 and 65532-65535 (change the ports accordingly) are the ports for the internal TURN server, which is **only needed when using the opt-in WebRTC transport (`--mode=webrtc`)** to route WebRTC through restrictive networks. With the default WebSocket transport, you only need to expose the single web port (`8080`). When deploying multiple containers, the TURN ports must be changed (together with the environment variables `TURN_MIN_PORT`/`TURN_MAX_PORT` with at least two ports in the range plus the environment variable `SELKIES_TURN_PORT`) and cannot be used by any other host process or container.

If UDP cannot be used, at the cost of higher latency and lower performance, omit the ports containing `/udp` and use the environment variable `-e SELKIES_TURN_PROTOCOL=tcp`.

All these ports must be exposed to the internet if you need WebRTC access over the internet. If you need to use TURN within a local network, add `-e SELKIES_TURN_HOST={YOUR_INTERNAL_IP}` with `{YOUR_INTERNAL_IP}` set to the internal hostname or IP of the local network. IPv6 addresses must be enclosed with square brackets such as `[::1]`.

Otherwise, to enable host networking, add `--network=host` to the Docker® command to work around this requirement if your server is not behind a firewall. Note that running multiple desktop containers in one host under this configuration may be problematic and is not recommended. You must also pass new environment variables such as `-e DISPLAY=:22` and `-e SELKIES_PORT=8082` into the container, all not overlapping with any other X11 server or container in the same host. Selkies serves everything on this single port; access the container using the specified `SELKIES_PORT`.

If you are behind a reverse proxy or can only expose one HTTP port and you use WebRTC mode, you will need to use an external STUN/TURN server capable of `srflx` or `relay` type ICE connections if you use this in a container WITHOUT host networking.

**Follow the instructions from [coTURN](#coturn) and [WebRTC and Firewall Issues](firewall.md) in order to make the container work using an external TURN server (WebRTC mode only).**

#### coTURN

> Check the [WebRTC and Firewall Issues: coTURN](firewall.md#coturn) section for installing and running coTURN on self-hosted standalone machines, cloud instances, or virtual machines. STUN/TURN is only relevant to the opt-in WebRTC transport.
>
> [Pion TURN](https://github.com/pion/turn)'s `turn-server-simple` executable or [eturnal](https://eturnal.net) are recommended alternative TURN server implementations that support Windows as well as Linux or MacOS. [STUNner](https://github.com/l7mp/stunner) is a Kubernetes native STUN and TURN deployment if Helm is possible to be used.

The [coTURN Container](https://github.com/selkies-project/selkies/tree/main/addons/coturn) is a reference container which provides the [coTURN](https://github.com/coturn/coturn) TURN server. Other than options including `-e TURN_SHARED_SECRET=`, `-e TURN_REALM=`, `-e TURN_PORT=`, `-e TURN_MIN_PORT=` (at least `49152`), and `-e TURN_MAX_PORT=` (at most `65535`), add more command-line options in `-e TURN_EXTRA_ARGS=`.

Run the Docker®/Podman container built from the [`coTURN Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/coturn/Dockerfile) (**replace `main` to `latest` for the latest stable release**):

```bash
docker run --name coturn -it -d --rm -e TURN_SHARED_SECRET=n0TaRealCoTURNAuthSecretThatIsSixtyFourLengthsLongPlaceholdPlace -e TURN_REALM=example.com -e TURN_PORT=3478 -e TURN_MIN_PORT=65500 -e TURN_MAX_PORT=65535 -p 3478:3478 -p 3478:3478/udp -p 65500-65535:65500-65535 -p 65500-65535:65500-65535/udp ghcr.io/selkies-project/selkies/coturn:main
```

**The relay ports and the listening port must all be open to the internet.**

If the TURN relay port range is wide, it may take a very long time for the containers to start up. Simply using `--network=host` instead of specifying `-p 65500-65535:65500-65535` and `-p 65500-65535:65500-65535/udp` can also be plausible.

Modify the relay ports `-p 65500-65535:65500-65535` and `-p 65500-65535:65500-65535/udp` combined with `-e TURN_MIN_PORT=65500 -e TURN_MAX_PORT=65535` as appropriate (at least two relay ports are required per connection).

In addition, use the option `-e TURN_EXTRA_ARGS="--no-udp-relay"` if you cannot open the UDP `min-port=` to `max-port=` port ranges, or `-e TURN_EXTRA_ARGS="--no-tcp-relay"` if you cannot open the TCP `min-port=` to `max-port=` port ranges. Note that the `--no-udp-relay` option may not be supported with web browsers and may lead to the TURN server not working.

Consult the [WebRTC and Firewall Issues: TURN Server Authentication Methods](firewall.md#turn-server-authentication-methods) and [TURN-REST](#turn-rest) sections for the difference between static auth secret/TURN REST API authentication and traditional long-term credential authentication.

#### TURN-REST

**The below is an advanced concept likely required for multi-user WebRTC-mode environments.**

A TURN server is required with WebRTC when both the host and the client are under Symmetric NAT or are each under Port Restricted Cone NAT and Symmetric NAT.

In easier words, if both the host and client are behind restrictive firewalls, the web interface and signaling connection (delivered using HTTP(S) and WebSocket) are delivered and established, but the WebRTC video and audio stream does not establish. In this case, the TURN server relays the WebRTC stream so that the host and client can send the video and audio stream, as well as other data.

![TURN-REST.svg](assets/TURN-REST.svg)

The recommended multi-user TURN server authentication mechanism is the [time-limited short-term credential/TURN REST API mechanism](https://datatracker.ietf.org/doc/html/draft-uberti-behave-turn-rest-00), where there is a single [shared secret](https://github.com/coturn/coturn/blob/master/README.turnserver) that is never exposed externally (only the TURN-REST Container and the coTURN TURN server know), but instead authenticates WebRTC clients (which are Selkies hosts and clients) based on generated credentials which are valid for only a short time (typically 24 hours).

The [TURN-REST Container](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest) is an easy way to distribute short-term TURN server authentication credentials and the information of the TURN server based on the REST API to many Selkies host instances, particularly when behind a local area network (LAN), which may or may not have restricted firewalls.

Using the `selkies --turn-rest-uri=` option or `SELKIES_TURN_REST_URI` environment variable, the Selkies host periodically queries a URI such as `https://turn-rest.myinfrastructure.io/myturnrest` or `http://192.168.0.10/myturnrest`.

This URI is ideally behind a local area network (LAN) inaccessible from the outside and only accessible to the Python hosts inside the LAN, or alternatively behind authentication using any web server or reverse proxy, if accessible from the outside. This information is periodically sent to the web client (that is also preferably behind authentication with HTTP Basic Authentication or a web server/reverse proxy) through HTTP(S), thus the TURN server information and credentials being propagated to both the Python host and the web client without exposing the TURN server information outside.

Because the time-limited TURN credentials automatically expire after some time, they are not useful even if they are leaked outside, as long as the pathway to the air-gapped or authenticated TURN-REST Container REST HTTP endpoint is not exposed plainly to the internet. [app.py](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/app.py) may also be hosted standalone without a container using the same startup command in the [Dockerfile](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/Dockerfile).

Other authentication methods such as TURN-REST over various types of REST API authentication (but adding support for TURN-REST behind Basic Authentication is trivial, so reach out with some funding) or TURN oAuth authentication are not supported as of now, and likely requires funding.

The TURN-REST Container (or similarly, Kubernetes Pod) should be triggered with the Docker®/Podman options `-e TURN_SHARED_SECRET=`, `-e TURN_HOST=`, `-e TURN_PORT=`, `-e TURN_PROTOCOL=`, `-e TURN_TLS=`, `-e STUN_HOST=`, `-e STUN_PORT=`, where the options are dependent on the TURN server configuration of [coTURN](#coturn) or other TURN server implementations.

Run the Docker®/Podman container built from the [`TURN-REST Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/Dockerfile) (replace `main` to `latest` for the latest stable release**):

```bash
docker run --name turn-rest -it -d --rm -e TURN_SHARED_SECRET=n0TaRealCoTURNAuthSecretThatIsSixtyFourLengthsLongPlaceholdPlace -e TURN_HOST=turn.myinfrastructure.io -e TURN_PORT=3478 -e TURN_PROTOCOL=udp -e TURN_TLS=false -p 8008:8008 ghcr.io/selkies-project/selkies/turn-rest:main
```

From Selkies, it is sufficient to use the `selkies --turn-rest-uri=` option or `export SELKIES_TURN_REST_URI=` environment variable, pointing to the HTTP(S) URI to the TURN REST API server.

Consult the [WebRTC and Firewall Issues: TURN Server Authentication Methods](firewall.md#turn-server-authentication-methods) section for more information on TURN authentication methods.


## Encoders and Interfaces

This section lists the encoders and interfaces that are actually implemented in the current runtime. The set of available video encoders depends on the transport mode.

### Encoders

Video is encoded by the `pixelflux` extension.

**WebSocket mode (default)** — select with the `SELKIES_ENCODER` environment variable or the `--encoder=` command-line option:

| Encoder (`--encoder=`) | Codec | Acceleration | Notes |
|---|---|---|---|
| `h264enc` (default) | H.264 AVC | NVIDIA NVENC / Intel & AMD VA-API, software `x264` fallback | Uses hardware encoding when a supported GPU is available; add `--use-cpu=true` to force software |
| `h264enc-striped` | H.264 AVC | Software (`x264`) | Striped/parallel software H.264 |
| `openh264enc` | H.264 AVC | Software (OpenH264) | BSD-licensed software H.264 |
| `jpeg` | Motion JPEG | Software | Maximum-compatibility fallback |

**WebRTC mode (`--mode=webrtc`)** — select with the `SELKIES_ENCODER_RTC` environment variable or the `--encoder-rtc=` command-line option:

| Encoder (`--encoder-rtc=`) | Codec | Acceleration | Browsers |
|---|---|---|---|
| `h264enc` (default) | H.264 AVC | Hardware-first (NVENC/VA-API), else software x264, via `pixelflux` | All major |
| `openh264enc` | H.264 AVC | Software (Cisco OpenH264) | All major |

Additional codecs (H.265/HEVC, AV1, VP8/VP9) are planned for `pixelflux` in the mid-term
future; the vendored WebRTC stack already carries the RTP-side code for them.

### Display Capture

| Interface | Device Selector | Input Injection | Operating Systems | Notes |
|---|---|---|---|---|
| X.Org / X11 (via `pixelflux`) | `DISPLAY` environment | vendored [`python-xlib`](https://github.com/python-xlib/python-xlib) (XTEST/XFixes), under `src/selkies/Xlib/` | Linux | Default backend |
| Wayland (via `pixelflux`) | headless compositor started by Selkies (`--wayland=true` / `SELKIES_WAYLAND=true`) | input injection through the `pixelflux` Wayland backend | Linux | Native Wayland mode; Mac and Windows support is planned |

### Audio Encoder

Opus is currently the only adequate full-band audio codec supported in web browsers by specification.

| Encoder | Codec | Operating Systems | Browsers | Notes |
|---|---|---|---|---|
| `pcmflux` | Opus | Linux | All major | Bitrate via `--audio-bitrate`; Opus RED (RFC 2198) redundancy via `--audio-redundancy` |

### Audio Capture

| Interface | Device Selector | Operating Systems | Notes |
|---|---|---|---|
| PulseAudio or PipeWire-Pulse (via `pcmflux`) | `PULSE_SERVER` or `PULSE_RUNTIME_PATH` environment, `--audio-device-name` | Linux | Default capture device is `output.monitor` |

### Transport Protocols

| Transport | Selected with | Ports | Notes |
|---|---|---|---|
| WebSockets (default) | `--mode=websockets` | single TCP port (default `8080`) | WebCodecs-based client decode; no STUN/TURN required |
| WebRTC (opt-in) | `--mode=webrtc` | signaling over the same port; media over UDP (or TCP) with ICE | Uses a vendored [`aiortc`](https://github.com/aiortc/aiortc) fork; may need STUN/TURN, see [WebRTC and Firewall Issues](firewall.md) |

Use `--enable-dual-mode=true` to let the client switch between the WebSocket and WebRTC transports from the UI.
