# Components

## Component Structure

Selkies is composed of a small number of core components plus several optional addons.

**Refer to [Getting Started](start.md) on how you can get on board.**

Use the following commands to retrieve the latest `SELKIES_VERSION` release, the current Ubuntu `DISTRIB_RELEASE`, and the current architecture `ARCH` in the next sections:

```bash
export SELKIES_VERSION="$(curl -fsSL "https://api.github.com/repos/selkies-project/selkies/releases/latest" | jq -r '.tag_name' | sed 's/[^0-9\.\-]*//g')"
export DISTRIB_RELEASE="$(grep '^VERSION_ID=' /etc/os-release | cut -d= -f2 | tr -d '\"')"
export ARCH="$(dpkg --print-architecture)"
```

When instructed to install [binfmt](https://github.com/tonistiigi/binfmt), use the following command with Docker/Podman:

```bash
docker run --rm --privileged tonistiigi/binfmt:latest --install all
```

### Core Components

At runtime, Selkies is a **single Python application** — the `selkies` wheel. Unlike earlier releases, there is no separate GStreamer build and no separate web-interface package: the HTML5 web client is bundled into the wheel, and screen/audio capture and encoding are provided by the `pixelflux` and `pcmflux` extensions, which are installed automatically as dependencies of the wheel.

All release components are available for download from the [Releases](https://github.com/selkies-project/selkies/releases) for the latest stable version.

For the most recent unreleased commit, download from the [GitHub Actions Workflow Runs](https://github.com/selkies-project/selkies/actions) `Build & publish all images` Build Artifacts (under `Artifacts (Produced during runtime)`) for each commit from the `main` branch. Build Artifacts can also be downloaded using the [GitHub CLI](https://cli.github.com) command [`gh run download`](https://cli.github.com/manual/gh_run_download).

#### Python Application

The term `host` or `server` refers to the [Python components](https://github.com/selkies-project/selkies/tree/main/src/selkies) across this documentation.

The Python components are responsible for the host server backend: capturing and encoding the host screen (via `pixelflux`) and audio (via `pcmflux`), injecting keyboard/mouse/gamepad input into the X11 display (via a vendored `python-xlib` using XTEST/XFixes), receiving input signals and communicating other data (including the clipboard) between the client and the host, serving the HTML5 web client, and — only in WebRTC mode — establishing the WebRTC connection to the client. Everything is served by a single [`aiohttp`](https://docs.aiohttp.org) server on a **single port (default `8081`)**.

In the default WebSocket mode, encoded screen frames, audio, input, and other data are multiplexed over WebSocket connections to a WebCodecs-based web client. In the opt-in WebRTC mode (`--mode=webrtc`), host screen video and audio are transported using the WebRTC `MediaStream` interface (through a vendored fork of [`aiortc`](https://github.com/aiortc/aiortc) under `src/selkies/webrtc/`), and other data are transported using the WebRTC `DataChannel` interface.

The architecture-independent wheel is available with the name **`selkies-${SELKIES_VERSION}-py3-none-any.whl`** for download in the [Releases](https://github.com/selkies-project/selkies/releases) for the latest stable version.

**Instructions from [Advanced Install](start.md#advanced-install) still apply below.**

For the most recent unreleased commit, download from the [GitHub Actions Workflow Runs](https://github.com/selkies-project/selkies/actions) `Build & publish all images` **`py-build_linux-amd64`** Build Artifact (under `Artifacts (Produced during runtime)`) for each commit from the `main` branch.

Alternatively, copy the Python Wheel file from the build container image (DO NOT change the platform in non-`x86_64` architectures, install [binfmt](https://github.com/tonistiigi/binfmt) instead, and change `main` with `latest` for the latest stable release):

```bash
docker create --platform="linux/amd64" --name selkies-py ghcr.io/selkies-project/selkies/py-build:main
docker cp selkies-py:/opt/pypi/dist/selkies-0.0.0.dev0-py3-none-any.whl /tmp/selkies-0.0.0.dev0-py3-none-any.whl
docker rm selkies-py
sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall /tmp/selkies-0.0.0.dev0-py3-none-any.whl
rm -f /tmp/selkies-0.0.0.dev0-py3-none-any.whl
# Run the Selkies Python executable after all components are installed
selkies --addr=0.0.0.0 --port=8081 --enable_https=false --https_cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https_key=/etc/ssl/private/ssl-cert-snakeoil.key --basic_auth_user=user --basic_auth_password=mypasswd --encoder=h264enc --enable_resize=false
```

One other alternative way to install the Python application components from the most recent unreleased commit:

```bash
git clone https://github.com/selkies-project/selkies.git
cd selkies
sudo PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall .
# Run the Selkies Python executable after all components are installed
selkies --addr=0.0.0.0 --port=8081 --enable_https=false --https_cert=/etc/ssl/certs/ssl-cert-snakeoil.pem --https_key=/etc/ssl/private/ssl-cert-snakeoil.key --basic_auth_user=user --basic_auth_password=mypasswd --encoder=h264enc --enable_resize=false
```

Installing the wheel also installs the `selkies` and `selkies-resize` console commands.

#### Web Client

The term `client` refers to the [web components](https://github.com/selkies-project/selkies/tree/main/addons/selkies-web-core) across this documentation.

The web client is a WebCodecs-based HTML5 application (with the core `selkies-core.js`, the WebSocket transport core `selkies-ws-core.js`, the WebRTC transport core `selkies-wr-core.js`, and the input library `lib/input.js`). It is responsible for the web browser interface that you see when you use Selkies.

It decodes the incoming H.264 or JPEG stream using the browser [WebCodecs](https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API) API with a low-latency zero-copy rendering path, plays Opus audio, and detects keyboard, mouse, gamepad, and clipboard input from the user, then sends them to the host server backend. It also handles remote cursors with the Pointer Lock API so that you can correctly control interactive applications and games.

The web client source lives at [`addons/selkies-web-core`](https://github.com/selkies-project/selkies/tree/main/addons/selkies-web-core) and is built and bundled into the Python wheel automatically (installed at `src/selkies/selkies_web`), so **there is no separate web package to download or install**. To serve your own copy of the web files, point `--web_root=` (or the `SELKIES_WEB_ROOT` environment variable) at a directory containing an `index.html`. Note that you should change `manifest.json` and `cacheName` in `sw.js` to rebrand the web interface to a different name.

#### Media Capture and Encoding (`pixelflux` and `pcmflux`)

Screen capture and video encoding are performed by [`pixelflux`](https://pypi.org/project/pixelflux/), a Rust (PyO3) extension. It encodes H.264 with hardware NVENC (NVIDIA) or VA-API (Intel/AMD) when a supported GPU is available, and otherwise falls back to software H.264 (`x264` or the BSD-licensed OpenH264), or encodes Motion JPEG. H.265 and AV1 in the capture path are planned but not yet implemented.

Audio capture and encoding are performed by [`pcmflux`](https://pypi.org/project/pcmflux/), a companion Rust (PyO3) extension that captures from PulseAudio (or PipeWire-Pulse) and encodes to Opus.

Both are pulled in automatically as dependencies of the `selkies` wheel, so you normally do not install them separately.

### Optional Components

These components are not required for the base Selkies runtime, but may be needed for specific deployments or preferences. These sections are nonetheless recommended to be read carefully.

#### Joystick Interposer

The [Joystick Interposer](https://github.com/selkies-project/selkies/tree/main/addons/js-interposer) is a special library that allows the usage of joysticks or gamepads inside unprivileged containers (most of the occasions with shared Kubernetes clusters or HPC clusters), where host kernel devices required for creating a joystick interface are not available. It uses an `LD_PRELOAD` hack to intercept application calls that open a Linux joystick/gamepad device and pass data through a unix domain socket, translating gamepad events from Selkies into joystick/gamepad events without requiring access to `/dev/input/js0` or kernel modules such as `uinput` (much like how [VirtualGL](https://github.com/VirtualGL/virtualgl) intercepts OpenGL commands).

> **Note:** the `LD_PRELOAD` used here (and in [fake-udev](#fake-udev)) is a deliberate, legitimate interposition technique for redirecting device access in unprivileged environments. It is unrelated to — and distinct from — the process-global `LD_PRELOAD` anti-pattern that `pixelflux`'s multi-GPU NVENC support specifically avoids when selecting a GPU for hardware encoding.

Pre-built `x86_64` and `aarch64` joystick interposer components for Ubuntu are available with the name (fill in the OS version `DISTRIB_RELEASE` such as `24.04`, `22.04`, Ubuntu-style architecture `ARCH` such as `amd64` and `arm64`) **`selkies-js-interposer_v${SELKIES_VERSION}_ubuntu${DISTRIB_RELEASE}_${ARCH}.tar.gz`** or **`selkies-js-interposer_v${SELKIES_VERSION}_ubuntu${DISTRIB_RELEASE}_${ARCH}.deb`** for download in the [Releases](https://github.com/selkies-project/selkies/releases) for the latest stable version.

**Instructions from [Advanced Install](start.md#advanced-install) still apply below.**

For the most recent unreleased commit, download from the [GitHub Actions Workflow Runs](https://github.com/selkies-project/selkies/actions) `Build & publish all images` **`js-interposer-ubuntu${DISTRIB_RELEASE}-tar.gz_linux-${ARCH}`** or **`js-interposer-ubuntu${DISTRIB_RELEASE}-deb_linux-${ARCH}`** Build Artifact (under `Artifacts (Produced during runtime)`) for each commit from the `main` branch.

Alternatively, copy and install the pre-built Joystick Interposer build (change `--platform=` to `linux/arm64` for `aarch64`, and change `main` with `latest` and `0.0.0` to the release version for the latest stable release):

```bash
docker create --platform="linux/amd64" --name js-interposer ghcr.io/selkies-project/selkies/js-interposer:main-ubuntu${DISTRIB_RELEASE}
docker cp js-interposer:/opt/selkies-js-interposer_0.0.0.deb /tmp/selkies-js-interposer.deb
docker rm js-interposer
sudo apt-get update && sudo apt-get install --no-install-recommends --allow-downgrades -y /tmp/selkies-js-interposer.deb
rm -f /tmp/selkies-js-interposer.deb
```

To retrieve the `.tar.gz` tarball instead of the `.deb` installer:

```bash
docker create --platform="linux/amd64" --name js-interposer ghcr.io/selkies-project/selkies/js-interposer:main-ubuntu${DISTRIB_RELEASE}
docker cp js-interposer:/opt/selkies-js-interposer_0.0.0.tar.gz /tmp/selkies-js-interposer_0.0.0.tar.gz
docker rm js-interposer
```

After extracting the `.tar.gz` tarball, move the `.so` library files to the library path (such as `/usr/lib/x86_64-linux-gnu` and `/usr/lib/i386-linux-gnu`) of your Linux distribution.

The following paths are required to exist for the Joystick Interposer to pass the joystick/gamepad input to various applications:

```bash
sudo mkdir -pm1777 /dev/input
sudo touch /dev/input/js0 /dev/input/js1 /dev/input/js2 /dev/input/js3
sudo chmod 777 /dev/input/js*
```

The following environment variables are required to be set in the environment each application is being run in to receive the joystick/gamepad input.

```bash
export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
export SDL_JOYSTICK_DEVICE=/dev/input/js0
```

You can replace `/usr/$LIB/selkies_joystick_interposer.so` with any non-root path of your choice if using the `.tar.gz` tarball.

Check the [Joystick Interposer README.md](https://github.com/selkies-project/selkies/tree/main/addons/js-interposer/README.md) documentation for usage instruction and compiling information on other platforms.

Check the following links for explanations of similar, but different attempts, for reference:

<https://github.com/Steam-Headless/dumb-udev>

<https://github.com/games-on-whales/inputtino>

<https://github.com/games-on-whales/inputtino/tree/stable/src/uhid>

<https://games-on-whales.github.io/wolf/stable/dev/fake-udev.html>

<https://github.com/games-on-whales/wolf/tree/stable/src/fake-udev>

#### fake-udev

The [fake-udev](https://github.com/selkies-project/selkies/tree/main/addons/fake-udev) addon provides a fake `libudev` shared library (`libudev.so.1`) designed to be used with `LD_PRELOAD`. It intercepts `libudev` calls and simulates the presence of a fixed set of virtual gamepads, so that applications which discover input devices through `libudev` (for example, via `udev_enumerate_scan_devices`) find the Selkies virtual gamepads even in containerized environments where a full udev daemon is not available. It complements the [Joystick Interposer](#joystick-interposer) and, like it, uses `LD_PRELOAD` by design.

#### Universal Touch Gamepad

The [Universal Touch Gamepad](https://github.com/selkies-project/selkies/tree/main/addons/universal-touch-gamepad) is a JavaScript library that adds a customizable on-screen touch gamepad overlay to the web interface. It intercepts `navigator.getGamepads()` to inject a virtual gamepad, making touch devices compatible with applications and games that expect the browser Gamepad API.

#### Selkies Dashboard

The [Selkies Dashboard](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard) and the modern TypeScript variant [Selkies Dashboard (Wish)](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard-wish) are reference React dashboards that demonstrate how to build and brand your own sidebar/control UI on top of `selkies-core` using `window` messaging. They are provided as examples/starting points, not as a required component.

#### Example Container

The [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) is the reference minimal-functionality container developers can base upon, or test Selkies quickly. The bare minimum Xfce4 desktop environment is installed together with Firefox, as well as an embedded TURN server inside the container for quick WebRTC firewall traversal.

Read the [Development](development.md) section for customizing this container for your own usage.

Run the Docker®/Podman container built from the [`Example Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/example/Dockerfile), then connect to port **8081** of your Docker®/Podman host to access the web interface (Username: **`ubuntu`**, Password: **`mypasswd`**, **change `DISTRIB_RELEASE` to `24.04`, `22.04`, or `20.04`, and replace `main` to `latest` for the latest stable release**):

```bash
docker run --name selkies -it -d --rm -e SELKIES_TURN_PROTOCOL=udp -e SELKIES_TURN_PORT=3478 -e TURN_MIN_PORT=65532 -e TURN_MAX_PORT=65535 -p 8081:8081 -p 3478:3478 -p 3478:3478/udp -p 65532-65535:65532-65535 -p 65532-65535:65532-65535/udp ghcr.io/selkies-project/selkies/gst-py-example:main-ubuntu${DISTRIB_RELEASE}
```

Add `--gpus 1 --runtime nvidia` to `docker run` when using NVIDIA GPUs.

Port 3478 and 65532-65535 (change the ports accordingly) are the ports for the internal TURN server, which is **only needed when using the opt-in WebRTC transport (`--mode=webrtc`)** to route WebRTC through restrictive networks. With the default WebSocket transport, you only need to expose the single web port (`8081`). When deploying multiple containers, the TURN ports must be changed (together with the environment variables `TURN_MIN_PORT`/`TURN_MAX_PORT` with at least two ports in the range plus the environment variable `SELKIES_TURN_PORT`) and cannot be used by any other host process or container.

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

Using the `selkies --turn_rest_uri=` option or `SELKIES_TURN_REST_URI` environment variable, the Selkies host periodically queries a URI such as `https://turn-rest.myinfrastructure.io/myturnrest` or `http://192.168.0.10/myturnrest`.

This URI is ideally behind a local area network (LAN) inaccessible from the outside and only accessible to the Python hosts inside the LAN, or alternatively behind authentication using any web server or reverse proxy, if accessible from the outside. This information is periodically sent to the web client (that is also preferably behind authentication with HTTP Basic Authentication or a web server/reverse proxy) through HTTP(S), thus the TURN server information and credentials being propagated to both the Python host and the web client without exposing the TURN server information outside.

Because the time-limited TURN credentials automatically expire after some time, they are not useful even if they are leaked outside, as long as the pathway to the air-gapped or authenticated TURN-REST Container REST HTTP endpoint is not exposed plainly to the internet. [app.py](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/app.py) may also be hosted standalone without a container using the same startup command in the [Dockerfile](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/Dockerfile).

Other authentication methods such as TURN-REST over various types of REST API authentication (but adding support for TURN-REST behind Basic Authentication is trivial, so reach out with some funding) or TURN oAuth authentication are not supported as of now, and likely requires funding.

The TURN-REST Container (or similarly, Kubernetes Pod) should be triggered with the Docker®/Podman options `-e TURN_SHARED_SECRET=`, `-e TURN_HOST=`, `-e TURN_PORT=`, `-e TURN_PROTOCOL=`, `-e TURN_TLS=`, `-e STUN_HOST=`, `-e STUN_PORT=`, where the options are dependent on the TURN server configuration of [coTURN](#coturn) or other TURN server implementations.

Run the Docker®/Podman container built from the [`TURN-REST Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/Dockerfile) (replace `main` to `latest` for the latest stable release**):

```bash
docker run --name turn-rest -it -d --rm -e TURN_SHARED_SECRET=n0TaRealCoTURNAuthSecretThatIsSixtyFourLengthsLongPlaceholdPlace -e TURN_HOST=turn.myinfrastructure.io -e TURN_PORT=3478 -e TURN_PROTOCOL=udp -e TURN_TLS=false -p 8008:8008 ghcr.io/selkies-project/selkies/turn-rest:main
```

From Selkies, it is sufficient to use the `selkies --turn_rest_uri=` option or `export SELKIES_TURN_REST_URI=` environment variable, pointing to the HTTP(S) URI to the TURN REST API server.

Consult the [WebRTC and Firewall Issues: TURN Server Authentication Methods](firewall.md#turn-server-authentication-methods) section for more information on TURN authentication methods.

### Legacy Components

The following components are kept in the repository for now but are **not used by the current runtime**. They are remnants of the previous GStreamer-based architecture and may be phased out.

- [`addons/gstreamer`](https://github.com/selkies-project/selkies/tree/main/addons/gstreamer): the old standalone GStreamer build. The GStreamer runtime has been fully removed from Selkies; this component is no longer required to run the project.
- [`addons/conda`](https://github.com/selkies-project/selkies/tree/main/addons/conda): the old Conda-based portable distribution toolchain that packaged GStreamer and its dependencies into a tarball. Selkies is now installed as a standard Python wheel, so this is no longer part of the install path.

Contributions to remove or repurpose these components are welcome.

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
| X.Org / X11 (via `pixelflux`) | `DISPLAY` environment | vendored [`python-xlib`](https://github.com/python-xlib/python-xlib) (XTEST/XFixes), under `src/selkies/Xlib/` | Linux | Wayland, Mac, and Windows support are planned |

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
| WebSockets (default) | `--mode=websockets` | single TCP port (default `8081`) | WebCodecs-based client decode; no STUN/TURN required |
| WebRTC (opt-in) | `--mode=webrtc` | signaling over the same port; media over UDP (or TCP) with ICE | Uses a vendored [`aiortc`](https://github.com/aiortc/aiortc) fork; may need STUN/TURN, see [WebRTC and Firewall Issues](firewall.md) |

Use `--enable_dual_mode=true` to let the client switch between the WebSocket and WebRTC transports from the UI.
