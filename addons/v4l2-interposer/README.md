# Selkies V4L2 (Webcam) Interposer

An `LD_PRELOAD` library that presents a virtual V4L2 capture device
(`/dev/video0`) fed by the pixelflux virtual camera, over its Unix domain socket
or from its PipeWire node. It lets Selkies deliver a browser's webcam into a
container — picked up by unmodified consumers such as Chromium, Firefox,
`ffmpeg`, GStreamer, `v4l2-ctl` and libv4l2-based applications — without the
`v4l2loopback` kernel module, any `/dev/video*` device, or elevated privilege.

The browser encodes its camera (H.264/VP8 over the WebRTC media track or
WebCodecs over the WebSocket, MJPEG as the last-resort canvas path). The Selkies
backend hands every encoded frame to `pixelflux.VirtualCamera`, which decodes it,
fits it into the device's fixed raw format (I420 by default), publishes it into a
shared-memory ring and rings a one-byte doorbell on the socket. This library
emulates the observable half of a fixed-function webcam: the `VIDIOC_*` ioctl
surface, MMAP streaming buffers, `read()` I/O and `poll()` readiness.

Where a v4l2loopback device is available (a desktop host, or a privileged
container), pixelflux mirrors the same frames into it (`webcam_device`), and
applications need no preload at all — the kernel-device counterpart of
`/dev/uinput` for the gamepads. Where a PipeWire daemon is reachable, the same
camera is also published as a PipeWire `Video/Source` node (`webcam_pipewire`)
for PipeWire-native applications and the `pipewire-v4l2` wrapper — and this
library can take its frames from that node instead of the socket (see
[Frame sources](#frame-sources)), so everything the interposer covers works
from a PipeWire node alone.

## How it works

- The application-facing fd is the connected socket itself, so `poll`/`select`/
  `epoll`/`dup`/`fork` work natively with no interception; socket readability is
  the frame-ready signal for `VIDIOC_DQBUF`.
- Frame pixels never cross the socket. The backend passes a shared-memory
  staging memfd once per connection via `SCM_RIGHTS`; `mmap()` of the device fd
  is redirected onto a per-handle buffer memfd, so the application maps real
  shared memory and `DQBUF` copies one frame into it.
- Only a fixed format is advertised: the pixel format, resolution and frame
  rate the backend configures (`webcam_pixel_format`, `webcam_width`,
  `webcam_height`). Control ioctls return `EINVAL` per control (terminating
  enumeration loops the way the kernel does for a camera without controls);
  events, cropping and output ioctls return `ENOTTY`, exactly as a minimal real
  webcam does.
- The libc `syscall()` entry point is interposed as well, covering consumers
  built on the libv4l2 wrapper library (OBS, `v4l2-ctl`, distribution
  GStreamer builds), which reaches the kernel through raw syscalls rather than
  `open()`/`ioctl()`.
- Directory scans of exactly `/dev` and `/sys/class/video4linux` get the
  device entry injected into their `readdir()` stream (suppressed when a real
  entry of the same name exists), so applications that enumerate cameras by
  listing find the device with no placeholder node; `stat`/`lstat`/`fstatat`/
  `statx`/`access` report it as a character device (major 81, minor = device
  index).
- The device's sysfs view exists as read-only data served from memory:
  `/sys/class/video4linux/videoN/{name,dev,index,uevent}` and
  `/sys/dev/char/81:N/uevent`, through `open()`/`fopen()` (a memfd holding the
  content) and `stat()`. Tools that identify a node by its major:minor before
  opening it (`v4l2-ctl`, udev-style enumerators) find a video device.
- When the frame source ends (the backend stops, the PipeWire node goes away),
  a blocked or polling reader gets `ENODEV`, never a spin on `EAGAIN`.
- The hot hooks (`read`, `close`, `mmap`, `ioctl`, `fstat`, `fcntl`) decide
  "not ours" from a lock-free fd bitmap, so the rest of the application — and
  PipeWire's own loop threads, which run inside the application through these
  same hooks when frames come from a node — never contend on the handle table.
- `dup`/`dup2`/`dup3` and `fcntl(F_DUPFD*)` results are tracked as aliases of
  the originating handle, and `fcntl(F_SETFL)` updates the handle's
  `O_NONBLOCK` behavior, matching real device-fd semantics.

The shared-memory layout and the on-connect configuration struct are defined
once in `pixelflux/src/webcam/ring.rs` and mirrored byte-for-byte by the
`WC_SHM_*` constants and `webcam_config_t` here; `pixelflux.VirtualCamera.shm_layout()`
reports the writer's view for the ABI test in `tests/unit`.

## Frame sources

`SELKIES_WEBCAM_SOURCE` selects where a device open takes its frames from:

- `socket`: the backend's webcam socket (`SELKIES_WEBCAM_SOCKET_PATH`), the
  shared-memory ring described above.
- `pipewire`: the PipeWire `Video/Source` node named by
  `SELKIES_WEBCAM_PIPEWIRE_NODE` (default `selkies-webcam`, the pixelflux
  node). libpipewire is `dlopen`ed on the first device open, never at library
  load; the stream targets exactly that node (no session-manager fallback onto
  another camera, no reconnect), negotiates I420/NV12/YUY2 or `video/mjpg` (the
  MJPEG device) in plain or memfd memory, and hands the newest frame to
  `DQBUF`/`read()` through the same doorbell mechanism the socket uses. This is the route for consumers the
  `pipewire-v4l2` wrapper cannot serve (raw-syscall libv4l2 applications,
  `read()` I/O, processes that must not enumerate a placeholder node) when only
  the node is available.
- `auto` (default): the socket, then PipeWire.

The PipeWire source is compiled in when the libpipewire headers are found at
build time (`HAVE_PIPEWIRE`; the Makefile and the packaging scripts detect
them); the library itself is a runtime dependency only of that code path.

## Compiling

```bash
make
# 32-bit variant for Wine/Steam and 32-bit browsers (needs gcc-multilib):
make all32
```

## Enabling for an application

Preload the library and point the backend's webcam socket at the same path. The
single quotes on the first line are required so `$LIB` expands per process.

```bash
export SELKIES_WEBCAM_INTERPOSER='/usr/$LIB/selkies_v4l2_interposer.so'
export LD_PRELOAD="${SELKIES_WEBCAM_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
# Optional overrides (must match the backend):
#   SELKIES_WEBCAM_DEVICE=0                     -> /dev/video0
#   SELKIES_WEBCAM_SOCKET_PATH=/tmp             -> /tmp/selkies_webcam0.sock
#   SELKIES_WEBCAM_SOURCE=auto|socket|pipewire  -> where frames come from
#   SELKIES_WEBCAM_PIPEWIRE_NODE=selkies-webcam -> the node the pipewire source uses
#   WEBCAM_LOG=1                                -> stderr diagnostics
chromium
```

Do **not** preload this into the Selkies backend process itself; the capture
side must keep seeing real device nodes.

## Testing without the full stack

`test-server.py` starts a `pixelflux.VirtualCamera` on the webcam socket and
feeds it synthetic MJPEG frames, standing in for a browser. Run it, then run a
consumer under the interposer:

```bash
python3 test-server.py --width 640 --height 480 &
WEBCAM_LOG=1 LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" \
    ffmpeg -f v4l2 -i /dev/video0 -frames:v 10 out.mkv
# or the libc-level probe from tests/tools (MMAP and read() modes):
LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" ../../tests/tools/v4l2probe /dev/video0 30
# the same through the PipeWire node the test server publishes:
SELKIES_WEBCAM_SOURCE=pipewire LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" ../../tests/tools/v4l2probe /dev/video0 30
```
