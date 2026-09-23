---
title: KDE Plasma Desktops
description: The docker-selkies-egl-desktop and docker-selkies-glx-desktop images, what each draws the desktop on, how they are laid out, and how they are developed and kept in step.
---

[`docker-selkies-egl-desktop`](https://github.com/selkies-project/docker-selkies-egl-desktop) and [`docker-selkies-glx-desktop`](https://github.com/selkies-project/docker-selkies-glx-desktop) are ready-to-go KDE Plasma desktops in separate repositories, built `FROM` the [Base Container](base-image.md) the way [`addons/desktop`](desktop-image.md) builds the LXQt one. Each README carries the run commands for Docker, Kubernetes and Apptainer, the variables the image adds, and its troubleshooting; this page is what the two are and how they relate to what this repository provides.

## What each one is

| | `selkies-egl-desktop` | `selkies-glx-desktop` |
| --- | --- | --- |
| Display server | The base's own: XLibre's Xvfb with glamor and DRI3 on X11, and the headless Wayland backend with a nested `kwin_wayland` under `SELKIES_WAYLAND=true` | An X.Org server of its own on the GPU (`services/xorg`), configured at every start by `selkies-xorg-config` for NVIDIA's driver or the modesetting driver; X11 only |
| How applications reach the GPU | EGL and DRI3: NVIDIA's GLX and EGL client libraries present through the framebuffer server's DRI3, Mesa's drivers on every other vendor, Zink on the NVIDIA Vulkan driver where Mesa itself does not reach the GPU | The vendor's own X driver with nothing translating in between: the proprietary NVIDIA driver, or modesetting on any GPU with a KMS driver |
| GPUs per container | One GPU serves as many containers as it has memory for; runs without a GPU at all | One GPU per container, since the X server owns it |
| Second display | A kwin virtual output on Wayland (the image rebuilds `kwin-wayland` with the patch under `patches/kwin`), a RandR monitor on X11 (`kwin_x11` rebuilt with `patches/kwin-x11` to take its screens from those monitors) | A RandR monitor on the X server's one output, with the same `kwin_x11` rebuild |
| Tags | `26.04`, `26.04-<build>`, `latest` | the same |

Both add the same desktop to the base: `plasma-desktop` with Dolphin, Konsole, KWrite, Gwenview, Ark and System Settings, Firefox and Google Chrome, the [proot-apps](https://github.com/linuxserver/proot-apps) runner behind the dashboards' apps panel, and on `x86_64` Steam behind [proot-bwrap](https://github.com/selkies-project/proot-bwrap) and Wine Staging with `winetricks`. Both build on the Ubuntu 26.04 base alone, since the rebuilt kwin packages are the archive's exact version, and publish for `amd64` and `arm64`. Plasma's own compositing is off in the system defaults, since every desktop animation is bandwidth for nothing on a stream.

## How they start

Neither carries an entrypoint, a supervisor configuration or a web server of its own. The base's `container-entrypoint.sh`, its `selkies` service and every other service are used as they are ([How it starts](base-image.md#how-it-starts)), and each repository adds only the s6 services its session needs under `services/`:

| Service | Image | Runs |
| --- | --- | --- |
| `dbus-session` | both | The session bus Plasma's components find each other on |
| `plasma` | both | The Plasma session on X11, `startplasma-x11` on the display server, or `kwin_x11` alone under `START_PLASMA=false`, so a single application from the apps panel is managed, resized and maximized without a desktop around it; on the EGL image's Wayland backend it parks, since the Plasma session is then the nested compositor the base's `wayland` service starts |
| `xorg` | GLX only | The X.Org server, on the GPU the base resolved for the session, sharing a virtual terminal it never switches to; its log is at `/tmp/runtime-ubuntu/Xorg.log` |

The EGL image sets `SELKIES_WAYLAND_COMPOSITOR` to the Plasma session, so the base's `wayland` service starts `kwin_wayland` rather than labwc. The GLX image removes the base's `xvfb` and `wayland` services and presets `DISABLE_ZINK=true`, since OpenGL goes through the X server's own GLX vendor; where another display server already holds the GPU's DRM master, a host session on the same card, its `xorg` service hands over to the base's framebuffer server, which it keeps as `selkies-xvfb-server`, rather than fail the session.

On X11 the Plasma shell lays its panels and wallpaper out for the DPI it started at, so both images replace the shell when the dashboard's UI scaling changes: the desktop lays out afresh at the new size within a few seconds, and open windows stay where they are.

## Configuration

Everything Selkies reads is a variable of the [Settings Reference](../settings.md); each README's configuration table lists the ones the image adds (`PASSWD`, `TZ`, `START_PLASMA`, and for the GLX image the X server's initial mode `DISPLAY_SIZEW`, `DISPLAY_SIZEH`, `DISPLAY_REFRESH`, `DISPLAY_CDEPTH`, the NVIDIA `VIDEO_PORT`, and `NVIDIA_DRIVER_VERSION` where the host's cannot be read). The video encoder, the bitrates, the frame rate and the UI scaling are chosen from the web interface and are not set in the environment; a single value in `SELKIES_ENCODER` or `SELKIES_SCALING_DPI` locks that choice.

The GLX image's NVIDIA X server modules (`nvidia_drv.so` and the GLX server module) come in with the driver's libraries from the NVIDIA Container Toolkit v1.20.1 or higher; under a runtime that injects the libraries alone, the first start lifts the two out of the driver installer matching the host's version, and a container that keeps its filesystem keeps them across restarts.

## Developing them

`SELKIES_DEV_SOURCE` works on these images as on the base ([Running a checkout inside it](base-image.md#running-a-checkout-inside-it)), so a change to Selkies is tried in a Plasma session without rebuilding the image. Each repository's `docker-compose.yml` builds and runs its image, and its `Dockerfile` is the reference build procedure: the kwin rebuild stages first, then the desktop layers on the base, under the same privileged-file bracket the base documents.

A change to a shared component is two Pull Requests, one per repository; a change to what the base provides is one here, and both pick it up at their next build. What is shared, and how closely:

| Component | Shared between | When updating |
| --- | --- | --- |
| `LICENSE`, the Plasma package set, the session defaults under `/etc/xdg`, the browsers and proot-apps layers, `services/dbus-session` | both repositories | identical; copy between them |
| `services/plasma` | both repositories | identical but for the Wayland branch, which only the EGL image has |
| The browsers and proot-apps layers, the privileged-file bracket | both repositories and `addons/desktop` here | identical, with the helper scripts fetched from this repository by `SELKIES_REF` rather than copied |
| `patches/kwin`, `selkies-kwin`, the kwin-wayland rebuild stage | EGL image only | assess against the kwin the archive ships |
| `patches/kwin-x11`, the kwin-x11 rebuild stage | both repositories | identical; copy between them |
| `services/xorg`, `selkies-xorg-config` | GLX image only | assess by hand |
| `README.md`, `docker-compose.yml`, `egl.yml`/`xgl.yml`, the publish workflow | both repositories | similar but not identical; assess by hand |

Each repository's `container-publish.yml` builds both architectures on native runners, pushes by digest and merges the manifest under the release, timestamped and `latest` tags on a push to `main`; a pull request builds both architectures and pushes nothing.
