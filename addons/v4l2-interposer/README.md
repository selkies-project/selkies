# Selkies V4L2 (Webcam) Interposer

An `LD_PRELOAD` library that presents a virtual V4L2 capture device
(`/dev/video0`) backed by a Unix domain socket to the Selkies backend. It lets
Selkies deliver a browser's webcam into a container — picked up by unmodified
consumers such as Chromium, Firefox, `ffmpeg` and GStreamer — without the host
`v4l2loopback` kernel module, any `/dev/video*` device, or elevated privilege.

The browser captures its camera, encodes frames as MJPEG, and sends them over
the existing WebSocket or WebRTC data channel. The Python backend stages each
frame into a shared-memory ring and rings a one-byte doorbell on the socket.
This library emulates the observable half of a fixed-function MJPEG webcam: the
`VIDIOC_*` ioctl surface, MMAP streaming buffers, and `poll()` readiness.

## How it works

- The application-facing fd is the connected socket itself, so `poll`/`select`/
  `epoll`/`dup`/`fork` work natively with no interception; socket readability is
  the frame-ready signal for `VIDIOC_DQBUF`.
- Frame pixels never cross the socket. The backend passes a shared-memory
  staging memfd once per connection via `SCM_RIGHTS`; `mmap()` of the device fd
  is redirected onto a per-handle buffer memfd, so the application maps real
  shared memory and `DQBUF` copies one frame into it.
- Only a fixed format is advertised: MJPEG at the resolution and frame rate the
  backend configures. Control ioctls return `EINVAL` per control (terminating
  enumeration loops the way the kernel does for a camera without controls);
  events, cropping and output ioctls return `ENOTTY`, exactly as a minimal
  real webcam does.
- The libc `syscall()` entry point is interposed as well, covering consumers
  built on the libv4l2 wrapper library, which reaches the kernel through raw
  syscalls rather than `open()`/`ioctl()`.
- Directory scans of exactly `/dev` and `/sys/class/video4linux` get the
  device entry injected into their `readdir()` stream (suppressed when a real
  entry of the same name exists), so applications that enumerate cameras by
  listing find the device with no placeholder node.
- `dup`/`dup2`/`dup3` and `fcntl(F_DUPFD*)` results are tracked as aliases of
  the originating handle, and `fcntl(F_SETFL)` updates the handle's
  `O_NONBLOCK` behavior, matching real device-fd semantics.


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
#   WEBCAM_LOG=1                                -> stderr diagnostics
chromium
```

Do **not** preload this into the Selkies backend process itself; the capture
side must keep seeing real device nodes.

A placeholder device node makes `/dev` directory scans list the camera. In a
container without `CAP_MKNOD` this is a plain file; `stat`/`access` are
interposed to report it as char-major 81:

```bash
touch /dev/video0 2>/dev/null || true
```

## Testing without the full stack

`test-server.py` opens the webcam socket, allocates the staging ring, and
streams synthetic MJPEG frames, mimicking the Python backend. Run it, then run
a consumer under the interposer:

```bash
python3 test-server.py &
WEBCAM_LOG=1 LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" \
    ffmpeg -f v4l2 -i /dev/video0 -frames:v 10 out.mjpeg
# or:
WEBCAM_LOG=1 LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" \
    v4l2-ctl --device /dev/video0 --all
```
