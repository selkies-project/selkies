<h1 align="center">pixelflux</h1>

<p align="center">Part of <a href="https://www.linuxserver.io"><img src="https://raw.githubusercontent.com/selkies-project/pixelflux/main/assets/linuxserver.svg" alt="LinuxServer.io" title="LinuxServer.io" width="182" align="middle"></a></p>

[![Tests](https://github.com/selkies-project/pixelflux/actions/workflows/tests.yml/badge.svg)](https://github.com/selkies-project/pixelflux/actions/workflows/tests.yml) [![Release](https://img.shields.io/github/v/release/selkies-project/pixelflux?display_name=tag)](https://github.com/selkies-project/pixelflux/releases/latest) [![PyPI](https://img.shields.io/pypi/v/pixelflux)](https://pypi.org/project/pixelflux/) [![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0) [![Docs](https://img.shields.io/badge/docs-pixelflux.selkies.io-blue)](https://pixelflux.selkies.io/) [![Discord](https://img.shields.io/badge/dynamic/json?logo=discord&label=Discord%20Members&query=approximate_member_count&url=https%3A%2F%2Fdiscordapp.com%2Fapi%2Finvites%2FwDNGDeSW5F%3Fwith_counts%3Dtrue)](https://discord.gg/wDNGDeSW5F) [![Ask DeepWiki](https://img.shields.io/badge/Ask-DeepWiki-blue)](https://deepwiki.com/selkies-project/pixelflux)

**A performant pixel delivery pipeline for diverse sources, blending flexible and high-performance modern encoding formats.**

This module provides a Python interface to a high-performance capture library supporting both **X11** and **Wayland** environments. It captures pixel data, detects changes, and encodes modified stripes into JPEG or H.264.

It encodes JPEG, H.264, H.265, VP8, VP9, and AV1. Every video codec runs on NVIDIA's NVENC (H.264, H.265, AV1) or on VA-API for Intel/AMD GPUs (all five) where the GPU carries it, H.264, H.265, and AV1 additionally on a Jetson's Tegra encoder through the vendor V4L2 interface, and otherwise on the software encoder the build resolves for it: x264 or, in a GPL-free build, the BSD-licensed OpenH264 for H.264; x265 or kvazaar for H.265; libvpx for VP8 and VP9; SVT-AV1 for AV1. JPEG and H.264 can be cut into stripes encoded in parallel; the other codecs stream whole frames. **About "zero copy":** the Wayland GPU path is truly zero-copy (dmabuf frames flow GBM → encoder without touching system RAM), and so are the X11 paths that encode on the GPU: DRI3 has the X server blit the screen into GPU buffers the hardware encoder reads in place, on any server drawing on the encode GPU (NVIDIA's included), and on an NVIDIA server DRI3 cannot serve, NvFBC has the driver composite the screen straight into video memory registered with NVENC in place, so a frame is never read, written, or copied by the CPU. Every other X11 session copies **exactly once**: the X server renders each frame into a shared-memory surface (`XShmGetImage`); the encoder threads then read that mapped surface **in place** and pass the encoded bytes to Python through the buffer protocol without any further copies.

## Installation

pixelflux is a single self-contained **Rust** extension compiled during installation. Both the X11 and Wayland backends, all encoders, and the Python API live in it. (`libjpeg-turbo` — and, in a GPL-free build, `openh264` — C sources are vendored and built by their Rust `-sys` crates, so `cmake` and `nasm` are required, but no system copies of those libraries are used.)

### 1. Prerequisites

Ensure you have the Rust toolchain (`cargo`), Python development files, and the development libraries below.

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build dependencies (Debian/Ubuntu)
sudo apt-get update && \
sudo apt-get install -y \
  git \
  curl \
  python3-dev \
  cmake \
  nasm \
  libclang-dev \
  libx264-dev \
  libx265-dev \
  libvpx-dev \
  libsvtav1enc-dev \
  libdav1d-dev \
  libde265-dev \
  libgbm-dev \
  libdrm-dev \
  libwayland-dev \
  libinput-dev \
  libudev-dev \
  libxkbcommon-dev \
  libpixman-1-dev
```

> **Notes:** the software codec libraries are linked directly and bound at build time from the headers of the copies that are linked (`codec-sys`, through `pkg-config`), so any release from the oldest the build takes on builds: libvpx 1.11 for VP8 and VP9, SVT-AV1 1.0 for AV1, x265 3.5 for H.265 (or, in a GPL-free build, kvazaar 2.3.2, the first whose rate control belongs to each encoder rather than to the process: Ubuntu 26.04 packages it, and elsewhere build it from source as the wheel recipe does), and dav1d 1.0 and libde265 1.0.8 as the virtual camera's AV1 and HEVC decoders. Each full-frame encoder is opened once in a forked child, and `pixelflux.SOFTWARE_ENCODERS` reports which encoder each codec has in the build that runs on the machine; `pixelflux.hardware_encoders(encode_node_index, auto_gpu)` reports the hardware half, the codecs the NVENC or VA-API of the node those two capture settings resolve to has an engine for, probed once per node. Software AV1 wants **SVT-AV1 2.3.0 or newer**, where the encoder's packet call became blocking for low delay: below it a session still encodes, but the encoder holds two frames before its first packet and every frame ships that late. `libjpeg-turbo` is vendored and built statically by its crate — **no `libturbojpeg` system package is needed** (only `cmake` + `nasm`). X11 capture uses pure-Rust XCB; colorspace conversion is pure-Rust; the NVENC/CUDA libraries and libva are loaded at runtime (no compile-time NVIDIA or VA-API packages).
>
> **GPL components (`libx264`, `x265`):** software H.264 uses the system `libx264` (GPL-2.0+) and software H.265 the system `x265` (GPL-2.0+), the only GPL-licensed dependencies **of pixelflux itself**. They are enabled **by default**; to build without them, set `PIXELFLUX_ENABLE_GPL=0` (or `=false`) before `pip install`. The build then substitutes the BSD-licensed Cisco OpenH264 (vendored, built from source) as the software H.264 encoder behind the very same API and wire format — striped and full-frame sessions, CRF and CBR, live bitrate/quality changes all keep working, and neither `libx264-dev` nor `libx265-dev` is required. Software H.265 follows the same switch, with the BSD-licensed kvazaar in x265's place. What you lose is 4:4:4 software H.264 and H.265: OpenH264 and kvazaar are 4:2:0-only, so `video_fullcolor` is encoded 4:2:0 on the CPU (NVENC still carries it). JPEG, NVENC, and VA-API are unaffected. `pixelflux.SOFTWARE_ENCODERS` maps each codec to the software encoder the build in use carries (`{"h264": "x264", "h265": "x265", "vp8": "libvpx", "vp9": "libvpx", "av1": "svt-av1"}` for the wheels), and a notice is printed at install time whether GPL components are enabled or not.
>
> **Official wheels are always GPL-enabled** (x264 and x265 as the software H.264 and H.265 encoders); the `PIXELFLUX_ENABLE_GPL=0` path is for verified license-minimal source builds.

### 2. Hardware Acceleration (Optional but Recommended)
*   **NVIDIA (NVENC):** The library detects the NVIDIA driver at runtime. No extra compile-time packages are needed.
*   **Intel/AMD (VA-API):** The library opens the host's `libva` at runtime; nothing to build against. You must have the correct drivers (e.g., `intel-media-va-driver-non-free` or `mesa-va-drivers`).
*   **NVIDIA Jetson (Tegra):** Nothing to install or build against. The L4T libraries the backend needs ship with JetPack and are loaded at runtime.

### 3. Install the Package

**Option A: Install a prebuilt wheel**

Every release on the [GitHub Releases page](https://github.com/selkies-project/pixelflux/releases) carries wheels (`manylinux_2_28` and `musllinux`, x86_64 and aarch64, CPython 3.9 and newer), the pre-releases cut per commit included; take the one for your interpreter and platform:
```bash
pip install ./pixelflux-<version>-cp312-cp312-manylinux_2_28_x86_64.whl
```
A wheel leaves a few libraries to the host, the ones a desktop already carries: `libgbm1`, `libpixman-1-0`, and `libxkbcommon0` on Debian and Ubuntu (`mesa-libgbm`, `pixman`, and `libxkbcommon` on Fedora and RHEL), and `libegl1` for NVENC's zero-copy dmabuf import; a musllinux wheel also takes the C++ runtime from Alpine (`apk add libstdc++ libgcc mesa-gbm pixman libxkbcommon`).

**Option B: Install from local source**
```bash
# From the root of the project repository
pip install .
```

## Usage

### Backend Selection

### X11 GPU capture (NvFBC and DRI3)

On an NVIDIA GPU whose session encodes on NVENC, X11 capture goes through **NvFBC**: the NVIDIA X
driver composites each frame into a buffer it owns in video memory and hands back a CUDA device
pointer, which is registered with the encoder in place. Nothing is copied, the driver generates a
frame when an application damages the screen (push model) rather than on a sampling timer, and a
fullscreen unoccluded application can present straight into the capture buffer, bypassing the X
server. Measured at 1920x1080 on a Tesla V100 against the XShm path on the same display and codec:
2.51 ms per frame instead of 5.24 ms for H.264, and 3.03 ms instead of 5.69 ms for H.265, with the
host CPU spending under 0.2 ms a frame because it touches no pixels.

It is chosen automatically and needs no setting. A session it cannot serve says so once and
streams through XShm instead: a codec NVENC has no engine for, software encoding, a GPU that is
not NVIDIA, a watermark (which is composited into host pixels), or a driver without NvFBC.
`libnvidia-fbc.so.1` is loaded at run time and ships with the driver; containers get it under the
`video` driver capability. There is nothing to turn on or off: what the driver offers decides.

On any other X server whose screen lives on the GPU (XLibre's Xvfb started with `-glamor -dri`,
an Xorg on a DRM driver), X11 capture goes through **DRI3**: pixelflux allocates a small pool of
dmabufs through GBM on the render node the server draws with, hands each to the server as a pixmap
(`PixmapFromBuffers`), and every frame is one `CopyArea` of the root into the next one, a GPU blit
in glamor. The hardware encoder, NVENC or VA-API, imports the dmabuf in place through the same
path the Wayland zero-copy capture uses, so no frame crosses to the CPU. The Damage extension says
whether anything was drawn since the last frame and ends the wait for the next one, so a change is
published as it lands; its region says how much, so a blinking caret stays a small change and the
still-screen cleanup refines around it. The XFixes cursor is composited by the server through Render. It is declined, with one line saying why, for a codec no hardware engine serves,
software encoding, a server without DRI3, Damage, or Render, a server drawing on a GPU other
than the encode node's, a buffer the server will not import, or a first frame the encoder cannot
read; the session then streams through XShm. A watermark is not a reason: the server composites it
through Render like the cursor, so it costs no readback here. A DRI3 1.0 server (the NVIDIA X driver
before 595) takes buffers without a layout, so the pool is linear there.

Each blit is waited for with a one-pixel `GetImage`, which the server answers once it has landed.
Where the server offers `SELKIES-SEMAPHORE` (the Selkies images' Xvfb) and the session encodes on
NVENC, the server signals a GPU semaphore pixelflux made instead (through Vulkan, imported into
CUDA), and the encode is queued at once behind a wait on it. Under an uncapped GPU-bound client a
GPU job submitted only once the CPU has learnt that the one before it finished waits a whole draw
of that client for its turn, so the `GetImage` costs two draws a frame: an uncapped 72 fps client
on a GTX 1080 streams at 35 fps through it, and at 60 fps through the semaphore with grab to
encoded at 10-12 ms instead of 28. The GPU serves its contexts in the order they were made, so
that holds where the encoder's context is older than the client's; a capture started while the
game already runs keeps the two draws, as it would with the `GetImage`.

Whichever path runs, an X11 capture publishes its frame rate on the root window, the fastest
running capture's, as `_FAKE_SCREEN_MILLIHZ` (CARDINAL, millihertz) and rounded to whole frames as
`_FAKE_SCREEN_FPS`, and deletes both when the last one ends. A server that fakes its vblank reads
them: the Selkies build of XLibre's Xvfb runs the vblank at that rate, never below the one it
started with unless the published one is at most 0.1% slower (59.94 under 60), so an application
that waits on Present for vsync presents as fast as it is streamed. Any other server keeps the
properties as inert data.

`pixelflux` supports both an X11 and a **Wayland** backend (the latter built on [Smithay](https://github.com/Smithay/smithay)), selected per capture by the `use_wayland` attribute on `CaptureSettings`:

- `settings.use_wayland = True` — force the Wayland backend
- `settings.use_wayland = False` — force the X11 backend
- `settings.use_wayland = None` (default) — use Wayland when the session exposes a `WAYLAND_DISPLAY`, otherwise X11

To test launching programs into this backend simply add `WAYLAND_DISPLAY=wayland-1` before launching them: 

```bash
WAYLAND_DISPLAY=wayland-1 glmark2-es2-wayland -s 1920x1080
```

Each display renders at its own `target_fps`, and fresh input may pull its next frame forward:
half a frame after the last one, a pointer move or a key press is composited and published at
once, and the cadence then follows the input's phase, so a pointer moving at the client's refresh
rate is captured as it lands instead of up to a frame later. The frames pulled forward come out
of a small budget, which keeps the sustained rate within a few percent of `target_fps`.

### Host capture (external compositors)

Setting `wayland_host_display` to another compositor's socket captures **that** session instead
of the built-in one, with input injected over libei where the portal backend grants an EIS socket,
else through the virtual keyboard and pointer protocols, else through kernel uinput devices where
`/dev/uinput` is writable, and through the portal's own `Notify*` methods last (a socket write to
the compositor lands in about 4 us, a uinput write in about 12 us, a portal call in about 2 ms). Frames
are fetched with `ext-image-copy-capture-v1` when the host offers it (wlroots 0.19+, KWin 6.2+,
COSMIC) and `zwlr-screencopy-v1` (v3) otherwise, so any wlroots-era or KDE compositor works;
`PIXELFLUX_HOST_CAPTURE=zwlr` forces the fallback for triage. Both protocols share the same
buffer plan: with a GPU the host blits into GBM dmabufs that the encoder imports directly (no
CPU copy anywhere), a host that cannot import our dmabufs (other GPU, software renderer) is
captured through shm instead, and both are damage-gated so a static screen costs nothing.

A host that offers neither protocol — GNOME, or KDE before its `ext-image-copy-capture`
support — is captured through **xdg-desktop-portal** instead: one `RemoteDesktop` session on the
session bus (`DBUS_SESSION_BUS_ADDRESS`) hands out a PipeWire stream per monitor, and takes the
keyboard and pointer where the host has no virtual-input protocol for them. A host that grants the
screen but refuses those devices answers the whole request as a refusal, so the session is asked
again for capture alone: declining remote interaction leaves the video working without injection. Each capability
picks its rung from the registry, so a KWin that serves `ext-image-copy-capture` but no
`zwlr-virtual-pointer` gets its frames natively and its pointer from the portal. The compositor
owns the portal's buffers: a dmabuf frame is imported by the encoder where it lies (the stream
offers the modifiers the encoder's display imports) and a memfd frame is read in place, cursor
metadata delivers the host's own cursor sprite to the cursor callback (KWin sends a cursor's image
once per change of cursor, so if the stream's first frame is replaced before it arrives, the sprite
stays missing until the cursor next changes shape), and the stream is asked for at most the
capture's frame rate. Portal input takes the lower-latency **libei** channel where
the backend answers `ConnectToEIS` — one socket for keyboard, pointer, and touch, and the path the
GNOME and KDE backends develop — and the portal's own `Notify*` methods otherwise. On a scaled output
the pointer lands in each path's own space: libei's regions in the compositor's logical space, and
`NotifyPointerMotionAbsolute` in the stream's logical size as the spec has it (Mutter reads it in the
stream's pixels, which only the portal backend can convert to, and xdg-desktop-portal-gnome does
not); a finger's scroll goes in the logical pixels of the output under the pointer. A successful
`ConnectToEIS` makes the session refuse `Notify*`, so libei is taken only once its handshake binds a
device; keys resolve against the compositor's own keymap that libei delivers, with a raw-keycode
fallback, while the `Notify*` path travels keysyms the host applies its layout to. The portal's consent dialog, where a
backend shows one (GNOME; KDE shows none to an unsandboxed process), blocks only the first
start: the restore token the backend hands back is kept in
`$XDG_STATE_HOME/pixelflux/portal-restore-token` (`~/.local/state` by default) and restores the
session without a dialog when it is reopened to change the cursor mode and on every later
start. `libpipewire-0.3` is loaded at run time as for the virtual camera; the bus client is pure
Rust.

Displays map onto host outputs by rank, so multi-display capture needs the host to expose that
many outputs: `ScreenCapture.output_capacity()` reports the host's output count (`-1` when
self-compositing, where outputs are created on demand), and `create_output` refuses ids the
host cannot back rather than minting an output that would never receive a frame.

A capture's size is asked of the host through `wlr-output-management`, but what it captures is
the mode the host ends up running. A host that refuses the request, offers no layout management
(KWin), or acknowledges a mode it never applies (Hyprland) has the capture re-sized to the mode
it announces, which `get_realized_geometry` reports and `capture_state` carries as a caveat, so
an output with a fixed mode list streams at its own resolution instead of never converging.

The built-in compositor also **serves** `ext-image-copy-capture-v1` (with per-output sources),
so standard capture tools — or another pixelflux — can record a pixelflux session: dmabuf
clients are filled by one GPU blit from the composited frame, shm clients by one readback, and
an unchanged screen holds the frame instead of duplicating it.

### Automatic GPU Selection

Set the `auto_gpu` attribute on `CaptureSettings` to let pixelflux pick a render node
automatically instead of supplying one — `"true"` (or any truthy value) picks the first GPU, and
a token (a kernel driver name, PCI **vendor** id, or devicetree prefix) picks the first GPU that
matches. It enumerates `/sys/class/drm`, pairs each `cardN` with its `renderD*` node by PCI
device, and skips non-GPU cards (IPMI/VGA). Selection is **driver-aware**: NVIDIA nodes are routed
to NVENC, while Intel (`i915`) and AMD (`amdgpu`) nodes take the VA-API path. Both the X11 and
Wayland backends honor it. (Selkies fills this from its `--auto-gpu` / `SELKIES_AUTO_GPU` inputs.)

```python
settings.auto_gpu = "true"
```

When auto-selection is off, the encoder device is chosen by `encode_node_index` (default `-2` =
auto): `-1` forces software and `>= 0` selects `/dev/dri/renderD(128 + index)`; an explicit
`encode_node_path` / `render_node_path` takes precedence.

### Capture Settings

The `CaptureSettings` class configures both backends.

```python
from pixelflux import CaptureSettings, ScreenCapture

settings = CaptureSettings()

# --- Core Capture ---
settings.capture_width = 1920
settings.capture_height = 1080
settings.capture_x = 0
settings.capture_y = 0
settings.capture_cursor = True
settings.target_fps = 60.0
settings.scale = 1.0  # Fractional scaling (Wayland only)
settings.wayland_host_display = ""                  # Capture from an EXTERNAL compositor instead of the built-in one (host-capture mode)

# --- Codec ---
# "jpeg" (striped stills), "h264" (striped, or full-frame with video_fullframe), or a
# full-frame video codec: "h265", "vp8", "vp9", "av1"
settings.codec = "h264"
# Force CPU encoding and ignore hardware encoders. The software encoder behind each codec
# is the build's (pixelflux.SOFTWARE_ENCODERS): x264 or OpenH264 for H.264, x265 or kvazaar
# for H.265, libvpx for VP8/VP9, SVT-AV1 for AV1.
settings.use_cpu = False
# Software encoders thread within a frame (x264's and OpenH264's slices, x265's and
# kvazaar's wavefront, libvpx's partitions and rows, SVT-AV1's processors, and the color
# conversion ahead of each), on one less than the host's cores, up to four for H.264 and eight
# otherwise; the striped encoders (H.264 without video_fullframe, JPEG) code their stripes in
# parallel. False codes each frame on one thread, the stripes one after another, to bound what one
# capture takes of a host that runs many; a Turbo session still hashes its frame on a thread
# beside the encode.
settings.cpu_encode_threading = True

# --- Debugging ---
settings.debug_logging = False # Enable/disable the continuous FPS and settings log. SVT-AV1's banner follows SVT_LOG, errors-only unless set.

# --- JPEG Settings ---
settings.jpeg_quality = 75              # Quality for changed stripes (0-100)
settings.paint_over_jpeg_quality = 90   # Quality for static "paint-over" stripes (0-100)

# --- Video Settings ---
settings.video_crf = 25                            # Quality index on the H.264 QP scale (0-51, lower is better quality); mapped onto each codec's own quantizer range
settings.video_paintover_crf = 18                  # Quality index a still screen is cleaned up at. Acts under CRF when lower than video_crf, under CBR when finer than what the rate control reached.
settings.video_paintover_burst_frames = 5          # Frames sent after a cleanup or a key frame on a still screen, so rate control settles.
settings.video_fullcolor = False                   # Use 4:4:4 chroma instead of 4:2:0 where the codec carries it (H.264 and H.265): software x264/x265 and NVENC take it, VA-API negotiates it per device.
settings.video_bit_depth = 8                       # Bits per sample, 8 or 10: 10 where the codec has a 10-bit profile an encoder here codes (H.264 on x264; H.265, VP9, and AV1 on VA-API, x265, libvpx, and SVT-AV1; H.265 and AV1 on NVENC). An engine without it hands the session to the software encoder, as it does a 4:4:4 it lacks; OpenH264, kvazaar, and VP8 stay 8-bit. A software encoder pays for it in CPU, and the source is 8-bit, so it is recommended only with 4:4:4 (`video_fullcolor`).
settings.video_fullframe = True                    # H.264 only: encode full frames instead of changed stripes (every other video codec is full-frame)
settings.video_streaming_mode = False              # Bypass all VNC logic and work like a normal video encoder, higher constant CPU usage for fullscreen gaming/videos
settings.keyframe_interval_s = 0.0                   # Periodic keyframe interval in seconds (0 = keyframes only on demand/paint-over)
settings.video_cbr_mode = False                    # Switches to CBR mode and ignores CRF value. Used in conjunction with video_bitrate_kbps.
settings.video_bitrate_kbps = 4000                 # Target bitrate for CBR mode. Required when video_cbr_mode is enabled.
settings.video_vbv_multiplier = 0.0                # Optional VBV size as a multiple of one frame's bit budget. CBR: 0 = auto (1.5, or 3 with periodic keyframes). CRF: a positive value caps software x264 at a peak of video_bitrate_kbps (capped CRF); 0 = uncapped.
settings.auto_adjust_screen_capture_size = True   # Allow pixelflux to adjust its capture width and height.

# --- Hardware Acceleration ---
# Encoder device selection:
#   -2: Auto-detect (default; combine with auto_gpu — see Automatic GPU Selection)
#   -1: Force software encoding
#   >= 0: Use the GPU at /dev/dri/renderD(128 + index)
settings.encode_node_index = -2
# Explicit encoder device path; takes precedence over the index above. str or bytes,
# e.g. "/dev/dri/renderD128".
settings.encode_node_path = None
# Explicit compositor render node (Wayland); str or bytes.
settings.render_node_path = None

# --- Wire Format / Zero-Copy (X11) ---
# False (default): prepend the per-stripe header to each packet (the WebSocket path): the tag,
# the codec and frame kind, then the frame id, the stripe's top row, its width and height, and
# the id of the frame it predicts from, all big-endian u16.
# True: emit the raw encoded payload with no header (for a WebRTC path that frames itself).
settings.omit_stripe_headers = False

# --- Change Detection & Optimization ---
settings.video_min_qp = 0                          # CBR QP clamps: 0 = encoder default; max bounds the quality floor, min bounds bit waste on easy content
settings.video_max_qp = 0
settings.use_paint_over_quality = True  # Clean up a still screen at the paint-over quality, whatever video_streaming_mode sends (CBR video cleans up without it too, at video_crf)
settings.paint_over_trigger_frames = 15 # Frames without change before a held cleanup, counted at 60 fps (a lower frame rate waits as long; a cleanup through the rate control starts on the first still frame; a screen changing only in small places gets it after four times this)
settings.damage_block_threshold = 10    # Consecutive changes to trigger "damaged" state
settings.damage_block_duration = 30     # Frames a stripe stays "damaged"

# --- Watermarking ---
# Must be a bytes object. The path to your PNG image.
settings.watermark_path = b"/path/to/your/watermark.png" 
settings.cursor_size_cap = 128                     # Cap out-of-band hardware-cursor PNGs to this longest edge (<= 0 = uncapped)
# 0:None, 1:TopLeft, 2:TopRight, 3:BottomLeft, 4:BottomRight, 5:Middle, 6:Animated
settings.watermark_location_enum = 4 
```

### Raw H.264 Annex-B Output (WebCodecs)

Two settings together make an H.264 capture emit bare Annex-B access units, the form a WebCodecs `VideoDecoder` takes:

```python
settings.codec = "h264"
settings.video_fullframe = True
settings.omit_stripe_headers = True
```

- **`omit_stripe_headers`** drops the 12-byte header (`VIDEO_HEADER_LEN`) that otherwise precedes each payload: the tag, the codec and frame kind, the frame id, the stripe's top row, its width and height, and the id of the frame it predicts from. The payload is the encoder's bitstream and nothing else, and the metadata is carried only on the `StripeFrame` attributes (`frame_id`, `stripe_y_start`, `stripe_height`, `data_type`, `reference_frame_id`). Every encoder reads it, software, NVENC, VA-API, V4L2, and Tegra alike, and it applies to JPEG stripes as well, which then arrive as bare JFIF.
- **`video_fullframe`** makes an H.264 capture encode whole frames instead of horizontal stripes. Without it a software H.264 session cuts the picture into stripes, and each stripe is an independent stream that only covers its own rows. The other video codecs are always full-frame, and a hardware H.264 encoder is always full-frame whatever this is set to.
- **Together** each callback delivers one complete access unit of one picture, start-code prefixed (`b_annexb` in libx264), with `stripe_y_start` 0 and the full capture size.

Points to code against:

- The frame kind is not in the bytes. A key frame is told by its IDR NAL unit (type 5). `reference_frame_id` is -1 on a key frame only where the session tracks references; a session that does not (a hardware encoder that cannot invalidate a reference) reports -2 on every frame. The sequence and picture parameter sets are repeated on every key frame, so a decoder configured from any key frame can start there.
- The GOP is infinite unless `keyframe_interval_s` is positive, so a key frame comes only when asked: at the start, from `capture.request_idr_frame()`, or from the cleanup and recovery policies. A client that joins late, reconnects, or resets its decoder must call `request_idr_frame()`, and a decoder must not be fed delta frames before the key frame that follows.
- A software encoder that does not honor the request still labels the frame by what it produced, so decide on the frame that arrives rather than on the request.
- NVENC is configured without access unit delimiters, so a unit begins at its first parameter-set or slice NAL unit.

### Input Injection (Wayland Only)

In Wayland mode, `pixelflux` acts as the compositor. You cannot use external tools like `xdotool`. Instead, use the input injection methods provided by the `ScreenCapture` instance:

```python
capture = ScreenCapture()
capture.start_capture(my_callback, settings)

# Inject Mouse Motion (Absolute coordinates); returns the move's number
motion = capture.inject_mouse_move(x=500.0, y=300.0)

# Where the pointer is, read back without a round trip to the compositor:
# (x, y, scale, last applied move number), or None before its first pass and
# under host capture; the position includes the move once `last applied` reaches its number
x, y, scale, applied = capture.pointer_location()

# Inject Mouse Button (evdev button codes: 272=Left, 273=Right, 274=Middle)
# State: 1 = Pressed, 0 = Released
capture.inject_mouse_button(btn=272, state=1) 

# Inject Scroll (Vertical/Horizontal)
capture.inject_mouse_scroll(x=0.0, y=10.0)

# Inject a touchpad's scroll: a finger's travel in physical pixels, fractional and with no
# wheel steps, then the lift, which a toolkit starts its kinetic scroll on
capture.inject_finger_scroll(dx=0.0, dy=24.5)
capture.inject_finger_scroll_end()

# Inject Keyboard Key
# scancode: Linux raw keycode (e.g., 17 for 'w')
# state: 1 = Pressed, 0 = Released
capture.inject_key(scancode=17, state=1)
```

### Stripe Callback

Your callback receives a single **`StripeFrame`** object (the same type on both the X11 and
Wayland backends). It supports the buffer protocol — `bytes(frame)` / `memoryview(frame)` /
`len(frame)` — and exposes the stripe metadata as attributes:

```python
def my_callback(frame):
    # frame.data_type      (the codec id: 0=JPEG, 1=H.264, 2=VP8, 3=VP9, 4=AV1, 5=H.265)
    # frame.frame_id
    # frame.stripe_y_start
    # frame.stripe_height
    # frame.capture_ns, frame.encode_start_ns, frame.encode_end_ns
    #                      (CLOCK_MONOTONIC nanoseconds, comparable with time.monotonic_ns();
    #                       the stripes of a frame share them)
    # frame.reference_frame_id
    #                      (the frame this one predicts from: -1 when it decodes on its own,
    #                       -2 where the encoder does not track its references)
    encoded_data = bytes(frame)          # copy out, or use memoryview(frame) zero-copy (below)
    # Send encoded_data to the client...
```

### Zero-Copy Frames

`memoryview(frame)` aliases the native encoder buffer with **no copy**, on **every supported
Python version (3.9 and newer)**. The frame object owns its buffer and keeps it alive until every
consumer — including a transport that retained a slice during a partial write — has released its
view, so the hand-off is memory-safe. (The old `deferred_free` / `OwnedFrame` / PEP 688 /
Python-3.12-only path is gone; the native buffer protocol does this on all versions.) Hand the
view straight to an async socket; keep the frame referenced for the duration of the send.

```python
def my_callback(frame):
    if frame.data_type == 0 or len(frame) == 0:   # nothing to send
        return
    # Hand BOTH the view and the frame to your sender (e.g. an asyncio.Queue) so the buffer
    # outlives the send: the view pins the frame, which frees the buffer once the view drops.
    queue.put_nowait({"data": memoryview(frame), "owner": frame})
```

See `example/screen_to_browser.py` for a complete queue-based usage.

### What a Capture Runs On

Which path a capture took is decided by the hardware, the driver, and the display server rather
than by a setting, so the capture says what it settled on instead of leaving it in the log.
`capture.stream_info()` returns a dict, `None` before a start and after a stop:

| Key | Meaning |
| --- | --- |
| `backend` | `x11` or `wayland` |
| `capture`, `zero_copy` | the capture path (`NvFBC`, `DRI3`, `XShm`, `dmabuf`, `readback`) and whether frames reach the encoder without a copy |
| `capture_reason` | why a zero-copy path was declined, each declined path named (`NvFBC: ...; DRI3: ...`); empty where there is nothing to explain |
| `zero_copy_available` | whether the display server offered the encoder a zero-copy path at all: NvFBC on an X server the NVIDIA driver drives, DRI3 on one drawing on the encode node's GPU, a Wayland compositor rendering on that GPU; a readback where it did falls short of one, and one where it did not is how the host is built |
| `encoder`, `hardware` | `NVENC`, `VAAPI`, or the software library, and whether it runs on a GPU |
| `encoder_reason` | why the session does not encode in hardware: the refusal the hardware session answered with, software encoding being selected, or a session given up after repeated errors |
| `codec`, `fullcolor`, `striped` | what the stream is, after any demotion |
| `gpu`, `driver`, `encode_node` | the device a hardware session encodes on |
| `renderer`, `render_node`, `render_gpu`, `renderer_reason` | Wayland only: `gl` or `pixman`, the node and GPU the compositor renders on, and why it renders in software |

The description follows the capture: a session that demotes itself mid-stream (zero-copy to
readback, hardware to software) changes what the next call returns. NVENC names its own device;
for a VA-API session the first call brings a GL context up once on the encode node to name the
GPU, so a caller with an event loop makes the call off it.

`capture.stream_stats()` returns cumulative counters since the start (`frames`, `bytes`,
`encode_ns`, `pipeline_ns` from capture to the end of the encode), which a caller differences
into a rate. They are relaxed atomics tallied once per delivered frame and read only on the
call, so a capture nobody inspects pays nothing for them.

## Zero-Copy Pipeline (Wayland)

The Wayland backend implements a **Zero-Copy** architecture for hardware encoding.

1.  **Rendering:** The compositor renders the desktop to a GPU buffer (GBM).
2.  **Export:** This buffer is exported as a `Dmabuf` (file descriptor).
3.  **Encoding:** The `Dmabuf` is imported directly into the encoder context (NVENC or VA-API) without ever copying pixel data to system RAM (CPU).

**Performance Note:** Software (Pixman) rendering, the absence of a hardware encoder, or utilizing a render node different from the encoding node will force a "Readback" fallback, copying pixels to the CPU and breaking the zero-copy chain (higher latency and CPU load). A watermark does **not** force readback — on the GPU path it is composited into the frame before encoding.

## Built-in MP4 Recorder

For convenience, the extension ships its own fragmented-MP4 muxer (no `avformat` dependency) with the `start_recording(...)`, `stop_recording()`, and `recording_status()` Python functions, controllable through the `PIXELFLUX_RECORD*` environment variables. Recording taps the encoded full-frame H.264 stream, and HTTP endpoints allow remote trigger/stop/status.

`start_recording(path, settings=None, audio_socket="")` adds an Opus audio track when `audio_socket` (or `PIXELFLUX_RECORD_AUDIO`) names a Unix socket serving an Ogg Opus stream, such as [pcmflux](https://github.com/selkies-project/pcmflux)'s `output_socket`: the recorder connects, keeps the packets as they are, and places them by their granule positions against the video clock, so the file is muxed without a decode or re-encode. `recording_status()` counts them as `audio_frames`.

## Screenshots

`screenshot_png(display=0)` returns a PNG of one display, the same image the Computer-Use server serves: the in-process Wayland compositor's output when one runs, else the root of the X server named by `DISPLAY`. It needs no running capture. Wayland preserves the current compositor or host capture cursor policy; the X11 screenshot draws the cursor into the image.

On Wayland, PNG compression runs in the requesting thread outside the Python GIL, after the compositor hands it an owned copy of the rendered output. A GL compositor whose GLES has `GL_EXT_buffer_storage` (NVIDIA's and Mesa's do) only starts the readback, into a persistently mapped pixel-pack buffer, and the requesting thread waits for the GPU and copies the pixels out, so the compositor no longer waits out the transfer; without the extension, or with `PIXELFLUX_SCREENSHOT_SYNC=1`, the compositor reads back synchronously into the caller's buffer. The buffers handed to callers are kept for the next screenshot. Requests are bounded through compression: up to four across all displays and two per display, with at most one queued readback per display. A call that exceeds either limit returns `Screenshot busy`; it does not replace another caller's pending capture. A timed-out caller cancels its pending readback, and removing an output fails its pending capture.

PNGs contain 8-bit RGBA pixels. Unused alpha bytes in opaque host formats become fully opaque; alpha-bearing formats retain their alpha. Losslessness refers to the rendered screenshot, not to the precision of an application's original data.

## Recording Sink

The capture session can output the raw video stream directly to a Unix domain socket for external recording: Annex-B for H.264 and H.265, an OBU stream for AV1, and IVF for VP8 and VP9.

*Note: This feature requires full-frame video encoding and does not work with JPEG or striped H.264 modes.*

```python
# Enable the unix socket (forces IDR frames every 30 frames and on connect)
settings.recording_socket = "/tmp/pixelflux_record"
```

You can then capture the stream using `ffmpeg`:

```bash
# Raw copy
ffmpeg -f h264 -i unix:///tmp/pixelflux_record -c:v copy test.h264

# Re-encode for a clean MP4
ffmpeg -f h264 -framerate 60 -i unix:///tmp/pixelflux_record -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p test.mp4
```

## Virtual Camera

`VirtualCamera` turns a client's webcam uplink into a V4L2 capture device for applications. Encoded frames of any
browser codec — H.264, VP8, VP9, AV1, HEVC (WebCodecs or a WebRTC media track), and MJPEG (the canvas fallback) — are
pushed in; a worker thread decodes them (OpenH264, libvpx, dav1d, libde265, TurboJPEG), fits them into the device's
fixed format (raw I420 by default, NV12, or YUYV; or MJPEG, a compressed device that carries an MJPEG uplink's frames
as received, decoding nothing, and re-encodes only frames that must be fitted), and publishes every frame to the
configured sinks at once:

- a shared-memory ring served over a Unix socket to the Selkies V4L2 interposer (`LD_PRELOAD`, no privileges, no
  kernel module), which presents `/dev/videoN` to the application;
- a v4l2loopback output device (`device_path`) on hosts and privileged containers that have the module, where
  applications need no preload at all;
- a PipeWire `Video/Source` node (`pipewire`) when a daemon is reachable, for PipeWire-native consumers and the
  `pipewire-v4l2` wrapper (`libpipewire-0.3` is loaded at run time; nothing is linked at build time).

```python
from pixelflux import VirtualCamera, VirtualCameraSettings

settings = VirtualCameraSettings()
settings.socket_path = "/run/user/1000/selkies_webcam0.sock"   # what the interposer connects to (default: $XDG_RUNTIME_DIR, else /tmp)
settings.width, settings.height = 1280, 720          # frames are scaled and letterboxed to fit
settings.pixel_format = "I420"                       # "I420", "NV12", "YUYV" or "MJPEG" (an MJPEG uplink passes through)
settings.device_path = "auto"                        # "", "auto", or a /dev/videoN to mirror into
settings.pipewire = True                             # publish the PipeWire node when a daemon is reachable

cam = VirtualCamera()
cam.start(settings)
flags = cam.push(h264_frame, VirtualCamera.CODEC_H264, keyframe=True)   # any buffer object; returns at once
if flags & VirtualCamera.KEYFRAME_WANTED:
    ...  # the decoder lost its reference (dropped frame, late start): ask the client for a keyframe
print(cam.stats())   # pushed/decoded/published/dropped/skipped/errors, geometry, clients, device_path
cam.stop()
```

`push(data, codec, keyframe=False, offset=0)` copies the encoded bytes out of the buffer and returns; decoding never
runs on the caller's thread and no worker ever calls back into Python. A full decoder queue drops the oldest frame
and, for inter-coded codecs, waits for the next keyframe rather than decoding against a missing reference.
`VirtualCamera.shm_layout()` reports the ring's byte layout for the interposer's ABI test.

## Computer Use Interface (X11 and Wayland)

Both backends implement the [Anthropic Computer Use specification](https://github.com/anthropics/claude-quickstarts/tree/main/computer-use-demo), providing an HTTP API for AI agents to control the desktop. On Wayland the compositor injects input natively; on X11 the existing display is driven through XTEST with root-window screenshots. Enable it by setting the `PIXELFLUX_CU` environment variable to the port the server should listen on:

```bash
export PIXELFLUX_CU=5000
export PIXELFLUX_CU_TOKEN="$(openssl rand -hex 32)"
```

A caller drives the desktop with its owner's full authority, so every request carries a bearer token and the server does not start without one: set `PIXELFLUX_CU_TOKEN` beside `PIXELFLUX_CU`, or pass it to `start_computer_use(bind, token)` when a script starts the server itself, and send it as `Authorization: Bearer <token>` (401 without it). A bare port is served on the loopback addresses only (`127.0.0.1,::1`), which every account on the host can reach; name the addresses to listen on as comma-separated `host:port` entries to open it to other hosts, `PIXELFLUX_CU=0.0.0.0:5000,[::]:5000` for every interface. A recording `/record_start` names is a file name inside `PIXELFLUX_RECORD_DIR`, refused when that is unset; without a name it goes to `PIXELFLUX_RECORD`, else a timestamped file in that directory, the session's `XDG_RUNTIME_DIR`, or `/tmp`, readable by its owner alone; a file already at that name is written only if it is the owner's and has no other name.

When using Computer Use, call `ensure_wayland_display()` before starting a capture to bring the compositor socket up early — this lets apps launched alongside your script connect to `WAYLAND_DISPLAY` immediately. GPU auto-selection (`auto_gpu` on `CaptureSettings`) works normally; the screenshot path forces a single-frame CPU readback when the GPU is in zero-copy mode, started on the compositor and finished by the caller where the GPU allows it (see [Screenshots](#screenshots)).

The Computer Use server listens for `POST` requests on `/computer-use` and responds with JSON. Unless otherwise noted, successful actions return:

```json
{"result":"ok"}
```

Coordinates are specified in absolute framebuffer pixels. Any coordinates outside the framebuffer are automatically clamped to the nearest valid pixel.

### Actions

All actions are `POST` requests to `/computer-use` with a JSON body.

**`screenshot`** - Capture the current display as a base64-encoded PNG:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"screenshot"}' | jq -r '.data' | base64 -d > screen.png
```

**`mouse_move`** - Move the cursor to absolute pixel coordinates:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"mouse_move","coordinate":[500,300]}'
```

**`left_click`** / **`right_click`** / **`middle_click`** - Click a mouse button, optionally at a coordinate and/or while holding a keyboard modifier:

```bash
# Simple click
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"left_click"}'

# Right click at a specific position while holding Shift
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"right_click","coordinate":[800,600],"text":"shift"}'
```

**`double_click`** / **`triple_click`** - Perform multiple left mouse clicks, optionally while holding a modifier:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"double_click","coordinate":[400,300]}'

curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"triple_click","text":"ctrl"}'
```

**`left_click_drag`** - Press the left mouse button at `start_coordinate`, drag to `coordinate`, then release:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"left_click_drag","start_coordinate":[100,100],"coordinate":[500,300]}'
```

**`left_mouse_down`** / **`left_mouse_up`** - Press or release the left mouse button without moving the pointer:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"left_mouse_down"}'
```

**`type`** - Type a string of text:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"type","text":"Hello, world!"}'
```

**`key`** - Press a key or key combination. Key combinations are specified using `+` separators:

```bash
# Single key
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"key","text":"Return"}'

# Key combination
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"key","text":"ctrl+s"}'

curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"key","text":"ctrl+alt+Delete"}'
```

**`hold_key`** - Hold a key for the specified duration (seconds). Durations are capped at 100 seconds.

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"hold_key","text":"ctrl","duration":2.0}'
```

**`scroll`** - Scroll vertically or horizontally, optionally at a coordinate and/or while holding a keyboard modifier:

```bash
# Scroll down 3 clicks
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"scroll","scroll_direction":"down","scroll_amount":3}'

# Scroll at a position while holding Shift
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"scroll","coordinate":[500,400],"scroll_direction":"up","scroll_amount":5,"text":"shift"}'
```

**`cursor_position`** - Return the current cursor position:

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"cursor_position"}' | jq -r '.text'
# → X=500,Y=300
```

**`wait`** - Pause execution for the specified duration (seconds). Durations are capped at 100 seconds.

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"wait","duration":0.5}'
```

**`zoom`** - Capture and return a cropped base64-encoded PNG of the specified framebuffer region (`[left, top, right, bottom]`):

```bash
curl -s -X POST http://localhost:5000/computer-use \
  -H "Authorization: Bearer $PIXELFLUX_CU_TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"zoom","region":[100,200,400,350]}' | jq -r '.data' | base64 -d > zoomed.png
```

## NVIDIA Jetson (Tegra)

A Jetson has a hardware video encoder and none of the usual ways to reach it: L4T carries no
`libnvidia-encode`, has no VA-API driver, and its `/dev/v4l2-nvenc` node is a placeholder that
`h264_v4l2m2m` cannot drive. The encoder is only reachable through the vendor's own libraries,
so the Tegra session loads `libnvv4l2.so` for the encoder node and, for the surfaces, whichever
library the board carries: `libnvbuf_utils.so` on JetPack 4, or `libnvbufsurface.so` with
`libnvbufsurftransform.so` on JetPack 5 and 6, where the older one is gone. Either way the
captured BGRA is converted to NV12 on the VIC block and the encoder is handed DMABUF surfaces,
so no frame is converted on a CPU core.

*   **Selection:** the ladder consults this backend before it probes render nodes, because a
    Jetson has no render node to probe. `hardware_encoders()` reports every codec the vendor
    encoder takes a capture format for there, and a session logs its backend as `TEGRA`.
*   **Codecs:** H.264, H.265, and AV1, 4:2:0, Main profile. The capture queue is set to the
    session's codec and the rest of the path is the same whichever it is. The board decides what
    it actually has an engine for: one it does not serve refuses the format, and the session
    falls back to software with the refusal in `stream_info`. The VIC does the color conversion,
    so `video_fullcolor` has no effect on this path.
*   **Live changes:** the CBR target bitrate can be changed on a running session; the frame
    rate and the resolution rebuild it, as elsewhere.
*   **Measured on a Jetson Nano** (L4T R32.6.1, four Cortex-A57 cores) in a live Selkies
    session, against the striped software encoder on the same board: 0.17 cores at 1080p30 and
    0.30 at 1080p60, against 2.26 cores for x264 at 1080p30. At 4K30 it holds 29 fps on
    0.44 cores. The encode itself is 0.10 ms a frame at 1080p and 0.21 ms at 4K; what the path
    actually spends is the copy of the captured frame into the staging surface (2.8 ms at
    1080p, 9.6 ms at 4K) and the VIC conversion (1.7 ms and 5.9 ms).
*   **Measured on an AGX Orin** (L4T R36.4.3, twelve Cortex-A78AE cores), isolated encode path
    over 200 frames: 0.05 cores at 1080p30, 0.10 at 1080p60, 0.20 at 1080p120, and 0.15 at 4K30,
    against 0.49 cores for the striped software encoder in a live session at 1080p30. 4K60 is
    not reachable on that board either: the VIC conversion alone is 12 ms a frame there and the
    path holds 46 fps. The conversion is pinned to the VIC rather than left at the API's
    default; both cost 4.9 ms a frame at 1080p there and the GPU 0.9 ms, and that GPU is what a
    robot runs its perception on.

## NVIDIA NVENC (X11)

*   **Multi-GPU containers:** When several GPUs are exposed to a container, NVENC is filtered
    in-process to the GPU you selected (no separate `LD_PRELOAD` shim is required). Verified on
    NVIDIA drivers 570–595.
*   **4:4:4 (High 4:4:4):** Set `video_fullcolor = True` to encode full-chroma H.264 via NVENC
    (`video_fullcolor` codec), in addition to the software path. A GPU whose NVENC has no 4:4:4
    (Kepler) hands the request to the software path, as VA-API does; see
    [VA-API 4:4:4](#va-api-444) for how the same request is resolved there.
*   **Force a keyframe on demand:** `capture.request_idr_frame()` forces an IDR frame, e.g. when
    a client reconnects or its decoder is reset. It routes to whichever encoder is active
    (NVENC, VA-API, or software) and is a no-op while no capture is running.
*   **Predict past a frame a client lost:** `capture.invalidate_reference(frame_id)` leaves that
    frame and everything encoded after it out of the predictions, so the next frame decodes for a
    client that never received it and the stream costs no keyframe. Each frame says what it
    predicts from (`StripeFrame.reference_frame_id`), which is what a consumer holds the frames
    behind a loss back by. Every session whose encoder lets it name its references tracks them:
    libx264 and libvpx always, NVENC on the devices whose driver offers reference invalidation,
    VA-API where the driver takes the session's slice headers (H.264, H.265) or addresses
    reference slots (VP8, VP9, AV1), Tegra for H.265 through the vendor's external reference set
    (L4T R32 and R36), and SVT-AV1 at a constant rate where the release takes reference commands
    (4.2 on); a session that does not (x265, kvazaar, SVT-AV1 at a constant quantizer or before
    4.2, Tegra's H.264 and AV1, a stateful V4L2 device) reports `-2` and answers this with a
    keyframe instead, as does an H.264 session for a loss covering the frame at its `frame_num`
    wrap, past which FFmpeg's decoder (Chromium's, Firefox's, and WebKit's on Linux) drops about
    a `frame_num` range of pictures. libx264 counts sixteen values, which the stream carries a
    byte wider, 4096, so that frame comes once in 68 s at 60 fps.
*   **Share one stream among clients that each recover on their own:** set
    `acknowledge_references = True` and call `capture.acknowledge_reference(frame_id)` for each
    frame every client holds, and `capture.acknowledge_reference(frame_id, held=False)` for each
    every client was sent but may yet lose (a datagram transport's). An NVENC session then keeps
    two long-term references (anchors) wherever the device has room for them, AV1 included: the
    newest anchor every client holds stays pinned while every twelfth frame is marked into the
    other, and an anchor within an anchor period of the newest frame every client was sent
    predicts from it and carries `0x08` in the low nibble of its header's type byte, so a
    consumer sends it to every client, as it would a keyframe, and each decodes it. A client
    left behind resyncs at the next such anchor, and one further behind on its report
    (`invalidate_reference`), which is predicted past from the pinned anchor however deep the
    loss (eight seconds of frames at 60 fps are remembered) rather than with a keyframe. H.264
    keeps one anchor, every forty-eighth frame, predicted and flagged the same way, so a loss
    reaching past both the anchor and the recent frames costs a keyframe: OpenH264's decoder
    (Firefox's for WebRTC H.264 where no FFmpeg decodes it) reaches only the first long-term
    index of a CABAC stream, and decodes a frame naming a second against another picture without
    an error. In H.264 the frame at the `frame_num` wrap is an anchor too, reaching back however
    far.

### Color conversion

The desktop source is sRGB, which shares BT.709's primaries and transfer function, so every
session converts with the **BT.709 matrix** and declares it: the software encoders' host
conversion, the VA-API convert (`scale_vaapi`), and NVENC's GPU kernel, at limited range for
4:2:0 and full range for the software 4:4:4 sessions (x264, x265). Chroma sits at the center of
each 2x2 block — the average of all four pixels — so the color a subpixel-antialiased glyph
edge carries cancels instead of tinting the chroma plane.

NVENC takes the captured ARGB, so there is **no CUDA Toolkit / NVRTC requirement** — only the
NVIDIA driver runtime (`libnvidia-encode`, `libcuda`), which is loaded at runtime. Its
fixed-function ARGB→YUV conversion follows the matrix the session declares, measured on Volta
and Pascal, but weights the two columns of a 4:2:0 block 3:1 rather than averaging them. A
4:2:0 session therefore converts on the GPU itself, with a small kernel shipped as PTX that
`libcuda` JIT-compiles (still no toolkit and no runtime compiler), reading the frame where it
already lies — the packed surface, a pitch-linear dmabuf import, or a texture over an
array-typed one — and writing the NV12 the encoder takes. 4:4:4 subsamples nothing, so it keeps
the hardware conversion, and so does a driver that refuses the kernel: the matrix is right
either way and only the siting differs.

VP8 is the one codec that cannot carry this: its keyframe header holds a single color-space bit
whose only defined value is BT.601. Told BT.709 out of band instead — in the decoder
configuration a client passes and in the RTP color-space header extension — Chromium and WebKit
paint it correctly, while Firefox reads neither and inverts BT.601, shifting saturated color by
20 levels on both transports, so VP8 converts and declares BT.601 (WebKit's GStreamer ports
invert BT.709 there whatever it carries, that port dropping the color space the client declares).
JPEG stripes are JFIF, which is BT.601 at full range by definition. Nothing extra to install at
build or runtime beyond the driver.

Two receiver-side caveats, measured rather than inferred. Chromium and Firefox both convert YUV
through libyuv, whose default build clamps the BT.709 Cb→B coefficient to 2.0 because the true
2.112 does not fit its fixed-point constant, so saturated blue arrives up to 12 levels short of
the source wherever that conversion runs on the CPU; BT.601's coefficient is clamped the same way
and loses 2. And Firefox drops the color description of an **AV1** stream it receives over
WebRTC, painting it as BT.601 — its WebCodecs path and every other codec on both transports
honor what the stream declares.

## VA-API 4:4:4

`video_fullcolor = True` is carried into the VA-API session rather than ruled out in advance. The
session asks the device which 4:4:4 surface formats it allocates and which of those its video
processor renders, since every frame reaches the codec through the device's own convert (Intel's
iHD allocates planar `444P` but renders 4:4:4 only as packed `XYUV`, so it encodes from `vuyx`),
and tries each format on both lists in turn, planar `yuv444p` ahead of packed `vuyx`, until one
survives the whole bring-up: the surface pool, the convert, and the codec open under the profile
that carries 4:4:4 (HEVC Main 4:4:4, VP9 profile 1). A driver can still report a surface its
encoder entry point does not take, and that shows only at one of those steps, so a format
refused there hands over to the next rather than failing the session. The session builds the
surface pool and the convert around the format that survived and names it in its init line.

Three layers can refuse, and each says so in the log line that precedes the fallback: the driver
rendering no 4:4:4 surface format, the driver refusing to allocate one, and the codec having no
4:4:4 profile in libva. **For H.264 and AV1 the third is what answers**: libva defines no such
profile for either, so a 4:4:4 request for them never reaches the device. A refusal falls back to
the software path, where x264 does carry 4:4:4 — the request is honored, on the CPU, rather than
silently downgraded to 4:2:0 (a GPL-free build's OpenH264 is 4:2:0-only and says so in the log).
`Colorspace:` in the stream log always reports what the session settled on rather than what was
asked for.

## Features

*   **Dual Backend (one Rust extension):**
    *   **X11:** zero-copy capture through NvFBC on the NVIDIA X driver or through DRI3 on any server whose screen lives on the GPU, else XShm capture via pure-Rust XCB with XFixes cursor and watermark compositing.
    *   **Wayland:** Modern, secure, headless compositor based on [Smithay](https://github.com/Smithay/smithay).
*   **Flexible Encoding:**
    *   **Software:** H.264 through x264 (incl. 4:4:4 — GPL, the default) or, in a GPL-free build, the BSD-licensed OpenH264 (4:2:0), and JPEG — both with multi-threaded striping; full-frame H.265 through x265 (incl. 4:4:4) or kvazaar, VP8 and VP9 through libvpx, AV1 through SVT-AV1, each linked directly; `pixelflux.SOFTWARE_ENCODERS` names the build's encoder per codec, and `pixelflux.hardware_encoders(encode_node_index, auto_gpu)` the codecs a render node's NVENC or VA-API serves, the node resolved as a capture resolves it, probed once per node at first call. `pixelflux.SOFTWARE_FULLCOLOR` and `pixelflux.hardware_fullcolor(encode_node_index, auto_gpu)` name, of those, the codecs each side encodes 4:4:4 when `video_fullcolor` asks for it, so a caller knows the chroma a session will carry before it opens one. `pixelflux.hardware_formats(encode_node_index, auto_gpu)` lists every format the node's engine encodes per codec (`"420-8"`, `"444-8"`, `"420-10"`, `"444-10"`), and `pixelflux.SOFTWARE_FORMATS` the same for each software encoder of the build, which is what `video_bit_depth` is answered from, and `ScreenCapture.stream_info` reports the `bit_depth` a session came up with.
    *   **Hardware:** NVIDIA NVENC (H.264, H.265, and AV1; incl. 4:4:4 for H.264 and H.265, ARGB-direct with matched VUI color signaling, multi-GPU containers, API-version negotiation) and VA-API (Intel/AMD; H.264, H.265, VP8, VP9, and AV1 through libva directly, loaded at runtime, the session writing the headers the driver takes packed; VA-VPP convert, per-device 4:4:4 negotiation, low-power entry points) with Zero-Copy support.
    *   **Driver-aware GPU auto-selection** via the `auto_gpu` setting.
*   **Zero-Copy Frames (X11 & Wayland):** the native frame object (buffer protocol) hands the encoded buffer to Python with no copy, on every supported Python version (3.9 and newer).
*   **Smart Bandwidth Management:**
    *   **Change Detection:** Encodes only changed stripes (Software/JPEG mode).
    *   **Paint-Over:** Cleans up a still screen once its content stops changing, even while
        `video_streaming_mode` encodes every frame: a refresh at the paint-over quality, and, at a
        constant rate on an encoder that holds a quantizer, after a large change a key frame at it
        once the screen holds still. Under CRF the session moves to the paint-over quality for the
        refresh on every encoder that takes a quality from the caller. A constant-rate video
        stream is cleaned up the same with the paint-over off: a rate control alone stops short
        of a clean picture where the rate is low for the frame rate, and the paint-over quality
        is the finest a still screen is refined to, reached as fast as its path carries
        (`video_crf` plays no part under CBR). Under CBR, NVENC and
        software H.264 are cleaned up through their rate control: the frames keep flowing, each
        within its budget, until it codes the screen at the paint-over quality, with no key frame,
        and only where NVENC's rate control stops refining short of it (a low rate for the
        resolution) does NVENC refresh the screen at that quality, a band of it a frame. Those
        bands run at the rate the path carries, which `update_link_rate(kbps)` says (four times the
        stream's rate at most, and the stream's rate where it is not said), and no cleanup frame
        comes to more than 200 ms of it: NVENC codes one that does again, smaller. libvpx's VP8, whose rate control leaves a still screen as it is,
        refreshes it the same way from the start of the cleanup where the rate leaves a band room
        in a frame (a bit a macroblock: 0.5 Mbit/s at 1080p60, 0.22 at 720p60), else in one held
        frame. Other encoders that hold a quantizer hold the cleanup frames at it; one that does
        not sends the refresh at the rate control's own quality, never a key frame. A screen that
        keeps changing only a little (a blinking caret) is cleaned up all the same.
    *   **Damage Throttling:** Limits processing during high-motion scenes.
    *   **On-demand keyframes:** `request_idr_frame()` forces an IDR for reconnecting clients.
    *   **Reference invalidation:** `invalidate_reference(frame_id)` has the encoder predict past a
        frame a client lost, so recovery costs no keyframe; `acknowledge_reference(frame_id)` pins
        the long-term references every client holds, so clients sharing a stream each recover
        on their own (NVENC).
*   **Self-description:** `stream_info()` reports the capture path, the encoder, the GPU, and why a
    faster path was declined, and `stream_stats()` the encode's counters, so a caller shows its
    user what a session runs on instead of pointing them at a log.
*   **Input Handling:** Built-in input injection for mouse and keyboard (Wayland; XTEST on X11 via Computer Use).
*   **Cursor Compositing:** Hardware cursor planes or software rendering options.
*   **Dynamic Watermarking:** Overlay PNGs with static positioning or DVD-screensaver style animation.
*   **Recording Sink:** Direct Unix socket output of full-frame video streams (Annex-B, OBU, or IVF by codec) for local capture.
*   **Virtual Camera:** A client's webcam uplink (H.264/VP8/VP9/AV1/HEVC/MJPEG) decoded off the GIL into a V4L2 capture device (or passed through as an MJPEG device when the browser sends JPEG), served to the Selkies V4L2 interposer (no privileges) and mirrored into v4l2loopback and a PipeWire node where available.
*   **Built-in MP4 Recorder:** Crash-safe fragmented-MP4 recording through a muxer of its own, no media framework linked.
*   **AI Agent Control:** Computer Use API to dump screenshots and drive all facets of a desktop environment.

## Development

`AGENTS.md` carries the conventions and the invariants of this tree, for contributors and coding agents alike. The crate builds in two configurations, `cargo test --lib` (the default `gpl` feature, libx264) and `cargo test --lib --no-default-features --features openh264`, and both are expected to pass. The `#[ignore]`d `gpu_` tests run on an NVIDIA GPU with `cargo test gpu_ -- --ignored --nocapture --test-threads=1`, the `gpu_dmabuf_` ones need a render node, and the `gpu_bench_` ones print measurements. `pip wheel . --no-deps` builds the extension the way the released wheels are built (`PIXELFLUX_ENABLE_GPL=0` for the OpenH264 build), and that wheel installed into a [selkies](https://github.com/selkies-project/selkies) checkout set up as its development documentation describes puts the change under the end-to-end suites. The `.devcontainer` installs the native dependencies the build needs.

Releases are cut by running the [Release](https://github.com/selkies-project/pixelflux/actions/workflows/release.yml) workflow from the Actions tab, **Run workflow** with **Use workflow from** on `main` and the tag, or by pushing the tag. A run releases the commit it started from: a new tag is created on it once the checks pass, and a tag that already exists has to be on it, so an earlier commit's tag is released by a run started from that tag. The `latest` input decides the "Latest" badge: `auto` gives it to a release and not to a pre-release, `never` holds it back for a fix to an older series, and `always` gives it to a pre-release as well. The tag is the version the wheels carry, a PEP 440 one such as `2.1.0` or `2.1.0rc1`, and its notes are `release-notes/<tag>.md`, or for a pre-release the notes of the release it leads to. The `pypi` input decides whether the wheels go to PyPI as well: `auto` publishes a release designated as the latest and no pre-release.

## License

This project is licensed under the **Mozilla Public License Version 2.0**.
A copy of the MPL 2.0 can be found at https://mozilla.org/MPL/2.0/.

Note that the default build links the GPL-2.0+ `libx264` as its software H.264 encoder and the GPL-2.0+ x265 for H.265; build with `PIXELFLUX_ENABLE_GPL=0` to exclude every GPL-licensed component (the BSD-licensed OpenH264 and kvazaar then take their places).

[LICENSES.md](LICENSES.md) inventories every third-party component of both builds (crates, linked and vendored native libraries, what is loaded at run time) with its license, and describes the check (`scripts/check-licenses.py`, `pixelflux/deny.toml`, the `Licenses` workflow) that keeps the non-GPL build free of copyleft code.
