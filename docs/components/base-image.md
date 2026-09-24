---
title: Base Container
description: The session with no desktop in it, how it starts, how it is laid out, how to run a checkout inside it, and how to build a desktop of your own on it.
---

The [Base Container](https://github.com/selkies-project/selkies/tree/main/addons/base), `ghcr.io/selkies-project/selkies/base:main-${DISTRIB_FLAVOR}`, is the whole session apart from what it looks like: the display servers, the audio stack, the GPU wiring, the gamepad and webcam plumbing, service supervision, an embedded TURN server, and Selkies itself. The [Desktop Container](desktop-image.md) adds LXQt to it, and the [KDE Plasma desktops](kde-images.md) add Plasma; a desktop of your own is built the same way, and everything on this page applies to all of them unchanged.

## What is in the image

| Part | What it is |
| --- | --- |
| X11 backend | [XLibre](https://github.com/X11Libre/xserver)'s `Xvfb`, built from a release archive pinned by checksum with the two patches under `addons/base/patches`: the screen pixmap lives on the GPU so glamor renders and DRI3 presents there, and the server starts with spare outputs Selkies plugs a second display into |
| Wayland backend | Selkies' own headless capture compositor, and a nested [labwc](https://labwc.github.io) session compositor built from source with `addons/base/build-labwc.sh` (window management, decorations, XWayland, and a control socket a second screen is asked over) |
| Audio | PipeWire, WirePlumber, and `pipewire-pulse`, which `pcmflux` captures from and the microphone plays into |
| GPU runtime | NVIDIA's EGL platform libraries for GBM, Wayland, and X11 (`egl-x11`, pinned by checksum), Mesa with Zink, the VA-API and Vulkan loaders, and `selkies-gpu-probe`, which measures what the session can render on |
| Printing | A CUPS scheduler Selkies runs its own print queue on, so a document printed in the session reaches the browser |
| Supervision | The [s6](https://skarnet.org/software/s6/) supervision suite from the distribution's packages, one service directory per daemon under `/etc/service` |
| TURN | An embedded [coTURN](turn.md#coturn) for the WebRTC transport, started only when no external TURN server is configured |
| Selkies | The `selkies` wheel with `pixelflux` and `pcmflux`, the [Input Interposer](input-interposer.md), [fake-udev](input-interposer.md#fake-udev), and the [V4L2 Interposer](v4l2-interposer.md) built from source, and `selkies-privileged-files` |

The image is built for Ubuntu 26.04 and Debian Trixie, on `x86_64` and `aarch64`, and is rootless: every layer runs as uid 1000 through `fakeroot`, so the package database and apt cache belong to the session user and `sudo apt-get install` works inside a running session the same way. `sudo-root` is the real thing, for device nodes and permissions only. The setuid and setgid files (`mount`, `su`, `sudo`, `fusermount3`, the PAM helpers) belong to root, which is what the privileged-file bracket below is for.

## How it starts

`/etc/container-entrypoint.sh` is the image's entrypoint. It is PID-agnostic: it can be PID 1 (add `docker run --init` for zombie reaping) or run below an injected init or launcher, since it ends by launching `s6-svscan /etc/service` itself rather than expecting to be the init. In order, it:

1. Creates `XDG_RUNTIME_DIR` (`/tmp/runtime-ubuntu`) with the `0700` mode dbus and PipeWire require, sets the time zone from `TZ` and the account password from `PASSWD`, and creates the XDG directories a desktop menu watches so the first application installed into the home appears in it.
2. Wires the gamepad and webcam plumbing for the session's applications: `SELKIES_INTERPOSER`, `SELKIES_WEBCAM_INTERPOSER`, and `FAKE_UDEV_LIB` go into `LD_PRELOAD`, and `/dev/input` is created where the container has none of its own.
3. Reads the backend (`SELKIES_WAYLAND`) and the transport (`SELKIES_MODE`) the way `settings.py` reads them, so the service set and Selkies agree.
4. Runs `selkies-gpu-probe`, which resolves the render node from `SELKIES_RENDER_DRI` or the `SELKIES_AUTO_GPU` token exactly as the capture will and brings the compositor's renderer up on it. From that one report the entrypoint settles hardware OpenGL for the session's applications (the vendor's GLX and EGL on NVIDIA, Mesa's own driver elsewhere, Zink on the Vulkan driver where Mesa itself does not reach the GPU, `DISABLE_ZINK=true` to opt out) and whether a Wayland session can keep its backend: a compositor that cannot reach the GPU starts the X11 backend instead unless `SELKIES_WAYLAND_X11_FALLBACK=false`. `docker exec <container> selkies-gpu-probe` asks it the same question later.
5. Sets the display (`DISPLAY=:20` on X11; the nested compositor's XWayland takes the first free number on Wayland), the PipeWire latency and the PulseAudio socket, and the print queue's socket.
6. Computes the session environment, embedded coTURN defaults included: the address a browser outside the container would reach the TURN server on is asked of a public resolver, and the embedded server is only started when no external TURN server is configured well enough to be used (a REST service, or a host and port with credentials or a shared secret).
7. Writes that environment to `${XDG_RUNTIME_DIR}/container-env`, which every service sources at start, since the services are siblings of the entrypoint rather than its children.
8. Derives the service set from the toggles by holding services down (`/etc/service/<name>/down`) rather than deleting them, so a restarted container that keeps its filesystem can still change backend: the X11 framebuffer is parked on Wayland, the nested compositor on X11 or under `SELKIES_WAYLAND_COMPOSITOR=none`, the desktop session under `START_LXQT=false`, and coTURN when an external TURN server is configured.
9. Hands over to `s6-svscan -t5000 /etc/service`.

The services under `/etc/service` are, each a `run` script and a `finish` script:

| Service | Runs |
| --- | --- |
| `dbus` | The system bus, at `${XDG_RUNTIME_DIR}/dbus-system-bus` |
| `pipewire`, `wireplumber`, `pipewire-pulse` | The audio stack; the two clients wait until the daemon answers rather than merely for its lock file |
| `xvfb` | The framebuffer X server on the X11 backend; parked on Wayland |
| `wayland` | The nested session compositor on the Wayland backend, unless an operator's compositor is captured (`SELKIES_WAYLAND_HOST_DISPLAY`) or none is wanted |
| `xsettingsd` | The XSETTINGS manager, so a DPI change from the dashboard reaches running X11 applications live |
| `coturn` | The embedded TURN server, listening on `SELKIES_TURN_PORT` with the `TURN_MIN_PORT`-`TURN_MAX_PORT` relay range |
| `selkies` | `/etc/selkies-entrypoint.sh`, once the environment file exists |

`s6-svc` and `s6-svstat` control and inspect them, the `supervisorctl` equivalents (`s6-svc -r /etc/service/selkies` restarts Selkies). `s6-overlay` is deliberately not used: it insists on being PID 1, while plain `s6-svscan` works both as PID 1 and below any foreign init.

`/etc/selkies-entrypoint.sh` is the Selkies service. It sources the environment file, drops the interposers from its own `LD_PRELOAD` by value (they answer for `/dev/video0` and `/dev/input` in whatever process loads them, which the capture and gamepad backends must keep seeing as the kernel reports them; an operator-supplied preload survives), waits for the X socket on the X11 backend, and starts `selkies`, whose every setting comes from the `SELKIES_*` variables of the environment ([Settings Reference](../settings.md)).

## Layout

| Path | What lives there |
| --- | --- |
| `/etc/container-entrypoint.sh`, `/etc/selkies-entrypoint.sh` | The two scripts above |
| `/etc/service/<name>/` | One directory per s6 service, with its `run` and `finish` scripts |
| `/tmp/runtime-ubuntu` (`XDG_RUNTIME_DIR`) | The session's sockets, the environment file, the print queue's socket, the logs the services write |
| `selkies`, `selkies-gpu-probe`, `selkies-resize` | The console scripts of the wheel: the server, the GPU report, and the resize helper for a session with `--enable-resize=false` |
| `/usr/local/bin/selkies-privileged-files` | The setuid bracket for package layers, and the `sudo-root ... run` path for in-session package management |
| `/usr/$LIB/selkies_input_interposer.so`, `selkies_v4l2_interposer.so`, `libudev.so.1.0.0-fake` | The preloads the session's applications get, exported as `SELKIES_INTERPOSER`, `SELKIES_WEBCAM_INTERPOSER`, and `FAKE_UDEV_LIB` |
| `/home/ubuntu` | The session user's home, uid 1000; mount a volume there to keep settings, downloads, and installed applications |

## Running a checkout inside it

`SELKIES_DEV_SOURCE` names a checkout mounted into the container, and the Selkies service then runs from that tree instead of the package baked into the image, so a change takes effect on a restart of the service rather than a rebuild of the image. The web client is a build product and a fresh checkout has none, so the image's own bundle is linked in at the (gitignored) path it is served from; `scripts/ci/build-web.sh` builds a real one into the tree. It works on any image built on the base, a published one included:

```bash
docker run --rm -it --shm-size=2g -p 8080:8080 \
  -v "$PWD:/opt/selkies-src" -e SELKIES_DEV_SOURCE=/opt/selkies-src \
  ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

The repository's [`docker-compose.yml`](https://github.com/selkies-project/selkies/tree/main/docker-compose.yml) does the same for the `desktop` service, and builds every image here; [Development](../development.md#local-builds) describes it. A change to `pixelflux` or `pcmflux` reaches the container as a wheel installed into the image's Python beside the mounted tree.

## Building a desktop on it

Use the base as the `FROM` image and add only the desktop: its packages, its session defaults, and the s6 service its session needs, one `run` script under `/etc/service/<name>/`. [`addons/desktop/Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/desktop/Dockerfile) is the reference, adding LXQt, the browsers, and the proot-apps runner in that way, and the [KDE Plasma desktops](kde-images.md) are the same pattern in their own repositories. Keep the base's entrypoint and services as they are; a desktop that needs the entrypoint changed replaces the script alone, so it keeps up with the base's updates:

```dockerfile
ARG DISTRIB_FLAVOR=ubuntu26.04
FROM ghcr.io/selkies-project/selkies/desktop:main-${DISTRIB_FLAVOR}
ARG DISTRIB_FLAVOR

USER 0
SHELL ["/bin/sh", "-c"]

# Replace changed files
# Copy scripts and service definitions used to start the container with `--chown=1000:1000`
#COPY --chown=1000:1000 container-entrypoint.sh /etc/container-entrypoint.sh
#RUN chmod -f 755 /etc/container-entrypoint.sh
#COPY --chown=1000:1000 selkies-entrypoint.sh /etc/selkies-entrypoint.sh
#RUN chmod -f 755 /etc/selkies-entrypoint.sh
# Replace or add s6 services (one directory per service under /etc/service)
#COPY --chown=1000:1000 services/ /etc/service/
#RUN find /etc/service -name run -exec chmod -f 755 {} +

USER 1000
ENV SHELL=/bin/bash
ENV USER=ubuntu
ENV HOME=/home/ubuntu
WORKDIR /home/ubuntu

EXPOSE 8080

ENTRYPOINT ["/etc/container-entrypoint.sh"]
```

Use persistent image tags (`${SELKIES_VERSION}-${DISTRIB_FLAVOR}`) to build on a specific release rather than whatever `main` is on the day.

A layer that installs packages needs one more pair. The images are rootless, every layer running as uid 1000 through fakeroot, while their setuid and setgid files belong to root, and dpkg replaces a file by hardlinking the old one aside first, which the kernel denies uid 1000 on a setuid file it does not own. An archive update to one of those packages therefore fails the layer that takes it, so bracket the package work with the pair the image ships for it:

```dockerfile
USER 0
SHELL ["/bin/sh", "-c"]
RUN selkies-privileged-files release

USER 1000
SHELL ["/usr/bin/fakeroot", "--", "/bin/sh", "-c"]
RUN apt-get update && apt-get install --no-install-recommends -y <packages>

USER 0
SHELL ["/bin/sh", "-c"]
RUN selkies-privileged-files restore
USER 1000
```

`restore` gives back every owner and bit `release` recorded. A setuid or setgid helper the new packages bring is in no record and needs its own `chown root:root` and `chmod` beside it, since the kernel honors neither bit on a file uid 1000 owns. Inside a running session the same work goes through `sudo-root selkies-privileged-files run apt-get install -y <packages>`: `sudo` is itself one of the files a release hands over, so one root process holds both ends and runs the command as the session user under fakeroot, the way an in-session `sudo apt-get` does.
