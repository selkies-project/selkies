---
title: Gamepads
description: The Input Interposer and fake-udev, which give a container's applications gamepads with no kernel device, and the kernel gamepads Selkies registers where /dev/uinput is writable.
---

## Input Interposer

The [Input Interposer](https://github.com/selkies-project/selkies/tree/main/addons/input-interposer) is a special library that allows the usage of joysticks or gamepads inside unprivileged containers (most of the occasions with shared Kubernetes clusters or HPC clusters), where host kernel devices required for creating a joystick interface are not available. It uses an `LD_PRELOAD` hack to intercept application calls that open a Linux joystick/gamepad device and pass data through a unix domain socket, translating gamepad events from Selkies into joystick/gamepad events without requiring access to `/dev/input/js0` or kernel modules such as `uinput` (much like how [VirtualGL](https://github.com/VirtualGL/virtualgl) intercepts OpenGL commands). It also serves the other direction: an application preloaded with it that opens `/dev/uinput` to create its own virtual input device (Steam Input's controller, a gamepad remapper) gets a working device backed by a socket, which sibling preloaded applications discover through [fake-udev](#fake-udev) and read as an ordinary `/dev/input/eventN` — the container equivalent of the [kernel devices](#kernel-gamepads) it falls back to where `/dev/uinput` is writable. The same pool can also carry a copy of the session's own keyboard and pointer, for the few applications that enumerate evdev instead of the display server (fullscreen games, input remappers): `publish_input_devices` turns that on and is off by default, and fake-udev derives each device's class from its capability bits. The desktop is driven by the compositor's virtual keyboard or XTEST either way, so these devices duplicate events and never carry them. Where `/dev/uinput` is writable the kernel serves them instead, and every application finds them without a preload. Under host capture neither backend publishes them: that session belongs to another compositor which already reads the kernel's own devices, and the capture injects into it directly. The library was formerly `selkies_joystick_interposer.so`; every install location still carries that name as a symlink, so a deployment that preloads the old path keeps working.

> **Note:** the `LD_PRELOAD` used here (and in [fake-udev](#fake-udev)) is a deliberate, legitimate interposition technique for redirecting device access in unprivileged environments. It is unrelated to — and distinct from — the process-global `LD_PRELOAD` anti-pattern that `pixelflux`'s multi-GPU NVENC support specifically avoids when selecting a GPU for hardware encoding.

On this backend Selkies delivers the gamepads, and the keyboard and pointer copies where `publish_input_devices` asks for them, over the sockets alone, so an application sees them only when it is started with the interposer preloaded; keyboard and pointer input reaches ordinary applications through the display server either way. It is meant for containers: on a host where the kernel is reachable, [Kernel Gamepads](#kernel-gamepads) covers the same ground with no preloading and no shadowed system libraries. The interposer is built from source and wired automatically in the [Desktop Container](desktop-image.md) and the desktop containers, every native Selkies package ships it under `/usr/$LIB` for images built on those, and the AppImage carries it at `usr/lib/selkies_input_interposer.so` (its `AppRun` exports the path as `SELKIES_INTERPOSER` rather than preloading it, since Selkies itself must keep seeing the real device nodes); elsewhere, build and install it (and [fake-udev](#fake-udev)) from the source in this repository:

```bash
git clone https://github.com/selkies-project/selkies.git && cd selkies
apt-get update && apt-get install --no-install-recommends -y build-essential
make -C addons/input-interposer && PREFIX=/usr make -C addons/input-interposer install
cd addons/fake-udev && make && cp libudev.so.1.0.0-fake libudev.so.1 libudev.so /usr/lib/$(gcc -print-multiarch)/
```

The `/dev/input` directory has to exist for the Input Interposer to augment it:

```bash
mkdir -pm1777 /dev/input
```

Each of the four gamepad slots is interposed as both a joydev node (`js0`-`js3`) and an evdev node (`event1000`-`event1003`). Opening either path by name is intercepted whether or not the file exists, and an application that scans `/dev/input` sees the evdev node of every bound slot added to the listing and, if it watches the directory with inotify, sees a slot bound or withdrawn later as that node appearing or vanishing, so no placeholder files are needed either way.

The following environment variables are required to be set in the environment each application is being run in to receive the joystick/gamepad input.

```bash
export SELKIES_INTERPOSER='/usr/$LIB/selkies_input_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
```

You can replace `/usr/$LIB/selkies_input_interposer.so` with any non-root path of your choice for the interposer library.

SDL2 applications discover the four pads through [fake-udev](#fake-udev). Where discovery through `libudev` is unavailable — `SDL_JOYSTICK_DISABLE_UDEV=1`, an SDL sandbox build, or an SDL built without udev — name the evdev nodes instead, which needs no placeholder files. Never name the joydev nodes: with fake-udev active, a `/dev/input/js0` hint is a second, different node for the slot SDL already enumerated as `event1000`, so the pad shows up twice.

```bash
export SDL_JOYSTICK_DEVICE=/dev/input/event1000:/dev/input/event1001:/dev/input/event1002:/dev/input/event1003
```

Check the [Input Interposer README.md](https://github.com/selkies-project/selkies/tree/main/addons/input-interposer/README.md) documentation for usage instruction and compiling information on other platforms.

Check the following links for explanations of similar, but different attempts, for reference:

<https://github.com/Steam-Headless/dumb-udev>

<https://github.com/games-on-whales/inputtino>

<https://github.com/games-on-whales/inputtino/tree/stable/src/uhid>

<https://games-on-whales.github.io/wolf/stable/dev/fake-udev.html>

<https://github.com/games-on-whales/wolf/tree/stable/src/fake-udev>

## fake-udev

The [fake-udev](https://github.com/selkies-project/selkies/tree/main/addons/fake-udev) addon provides a `libudev` shared library (`libudev.so.1`) designed to be used with `LD_PRELOAD`. It intercepts `libudev` calls and adds a fixed set of virtual gamepads to what the system's `libudev` reports, so that applications which discover input devices through `libudev` (for example, via `udev_enumerate_scan_devices`) find the Selkies virtual gamepads; a pad is listed only while the interposer serves it, and one served later arrives as a hotplug add, so a scanner never opens a node that would only time out. A running udev daemon is no substitute on this backend: the pads exist only as interposer sockets, so a real `libudev` query never reports them (the [kernel devices](#kernel-gamepads) are the case where it does). fake-udev covers discovery and the [Input Interposer](#input-interposer) covers the device itself — applications that enumerate through `libudev` need both, and, like the interposer, it uses `LD_PRELOAD` by design.

Everything outside the pads passes through to the real `libudev`, which fake-udev loads by path at first use, so a preloaded application still sees its GPU, webcam, or hidraw devices and the host's own input devices exactly as without the preload: a nested KWin, which discovers its render nodes through `libudev`, keeps hardware acceleration under the preload. The only real devices hidden are input nodes that share a pad's name (`js0`–`js3`, `event1000`–`event1003`), since their `/dev/input` paths are the interposer's. `SELKIES_REAL_LIBUDEV` names the real library when it lives outside the platform library directories, and `SELKIES_REAL_LIBUDEV=none` turns passthrough off, leaving only the pads visible. Without a real `libudev` on the system, that is the behavior by default.

## Kernel Gamepads

Where `/dev/uinput` is available — a desktop host rather than an unprivileged container — Selkies registers each gamepad slot as a real kernel device instead. Applications then enumerate it through the kernel like any USB controller, so neither the [Input Interposer](#input-interposer) nor [fake-udev](#fake-udev) is involved and nothing has to be preloaded. This is what lets Steam, Proton, and browsers running inside the remote desktop find the controller.

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
