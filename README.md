![Selkies](https://raw.githubusercontent.com/selkies-project/selkies/main/docs/assets/logo/horizontal.svg)

[![Build](https://github.com/selkies-project/selkies/actions/workflows/ci.yaml/badge.svg)](https://github.com/selkies-project/selkies/actions/workflows/ci.yaml)
[![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)
[![Docs](https://img.shields.io/badge/docs-docs.selkies.io-blue)](https://docs.selkies.io/)
[![Discord](https://img.shields.io/badge/dynamic/json?logo=discord&label=Discord%20Members&query=approximate_member_count&url=https%3A%2F%2Fdiscordapp.com%2Fapi%2Finvites%2FwDNGDeSW5F%3Fwith_counts%3Dtrue)](https://discord.gg/wDNGDeSW5F)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/selkies-project/selkies)

**Moonlight, Google Stadia, or GeForce NOW in noVNC form factor for Linux X11 and Wayland, in any HTML5 web interface you wish to embed inside, with at least 60 frames per second on Full HD resolution.**

**We are in need of maintainers and community contributors. Please consider stepping up, as we can never have too much help!**

![The desktop container open in Chrome: LXQt with Firefox on the documentation, and the Selkies sidebar on the left](https://raw.githubusercontent.com/selkies-project/selkies/main/docs/assets/screenshot.webp)

Selkies is an open-source low-latency high-performance Linux-native GPU/CPU-accelerated HTML5 remote desktop streaming platform, for self-hosting, containers, Kubernetes, or Cloud/HPC platforms, [started out first by Google engineers](https://web.archive.org/web/20210310083658/https://cloud.google.com/solutions/gpu-accelerated-streaming-using-webrtc), then open-sourced and developed by academic researchers, [LinuxServer.io](https://www.linuxserver.io), and community contributors. It streams over plain WebSockets by default, with WebRTC available as an opt-in transport.

Selkies is designed for researchers studying Agentic AI, Graphical AI, Robotics, Autonomous Driving, Drug Discovery technologies, SLURM supercomputer or HPC system administrators, Jupyter, Kubernetes, Docker®, Coder infrastructure administrators, and Linux cloud gaming enthusiasts.

While designed for clustered or unprivileged containerized environments, Selkies can also be deployed in desktop computers, and any performance issue that would be problematic in cloud gaming platforms is also considered a bug.

The HTML5 client runs on Chromium, Firefox, and Safari, with two-way clipboard (text and images), low-latency zero-copy video rendering, automatic GPU selection, resilient keyboard, mouse, and gamepad input, and microphone and webcam forwarding into the session.

## Quick Start

The desktop container carries a desktop, a browser, and an audio stack, so without a GPU this is the whole command:

```bash
docker run --name selkies -it -d --rm --shm-size=2g -p 8080:8080 \
    ghcr.io/selkies-project/selkies/desktop:latest-ubuntu26.04
```

Open <https://localhost:8080>, accept the container's self-signed certificate, and log in as `ubuntu` with the password `mypasswd`; `-e PASSWD=` sets another, which it needs before anyone else can reach the session. [Getting Started](https://docs.selkies.io/start) has the commands for Intel, AMD, and NVIDIA GPUs and what each flag is for. The tag names the distribution inside the image, `ubuntu26.04` or `debiantrixie`, after `latest` for the newest release or `main` for the newest commit.

Other ways to run it:

- `pip install selkies` (Python 3.9 or newer) brings the server, its web client, and the capture extensions, and `selkies-session` then runs the desktop the host has installed, bringing up what it lacks for a session (a display and a sound server), which is how [Jupyter, Coder, and Open OnDemand](https://docs.selkies.io/platforms) start it.
- [Native Install](https://docs.selkies.io/native) has the `.deb`, `.rpm`, `.apk`, and Arch packages, which attach Selkies to a display and sound server you run, and the AppImage, which installs nothing and starts a display and a sound server where none is running.
- The [Base Container](https://docs.selkies.io/components/base-image) is the whole session without a desktop, the image to build your own `FROM`, and [`docker-selkies-egl-desktop`](https://github.com/selkies-project/docker-selkies-egl-desktop) and [`docker-selkies-glx-desktop`](https://github.com/selkies-project/docker-selkies-glx-desktop) are KDE Plasma desktops built on it.

**[Read the Documentation](https://docs.selkies.io/) for the rest.** The [Settings Reference](https://docs.selkies.io/settings) lists every current setting. [Licensing](https://docs.selkies.io/licensing) inventories the third-party components of an installation, with their licenses and where the GPL pieces come from.
