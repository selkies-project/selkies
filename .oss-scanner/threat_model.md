# Threat model

## What this project does and where untrusted input enters
- pixelflux is the native media path of Selkies, a Rust/PyO3 extension (`pixelflux/src`) that captures the screen
  (X11, an in-process Smithay Wayland compositor, PipeWire), encodes it (NVENC, VA-API, V4L2 M2M, x264, x265,
  libvpx, SVT-AV1, OpenH264, JPEG), records it, and decodes the browser's webcam uplink into a virtual camera.
- Untrusted input, weakest principal first:
  - a remote browser's webcam bitstream (H.264, VP8, VP9, AV1, HEVC, MJPEG), passed in by Selkies and decoded in
    process (`webcam/decode.rs`), and the SPS reader and rewriter (`encoders/sps.rs`);
  - the token-gated Computer-Use HTTP server (`computer_use.rs`): screenshots and input injection, loopback by
    default, reachable remotely when bound to a host;
  - other local accounts, through the webcam interposer and recording tap Unix sockets (`webcam/server.rs`,
    `recording_sink.rs`), whose directory falls back to `/tmp`;
  - Wayland clients of the compositor (`wayland/`): buffers, dmabufs, keymaps, clipboard;
  - values the X server and GPU drivers report (geometry, plane lengths, strides), trusted after `dlopen`, and
    the framebuffer and watch memfds a server sharing its framebuffer hands over (`x11/framebuffer.rs`), mapped
    only once sealed against shrinking and agreeing with the reply's geometry.
- The Python embedding (Selkies) has full authority by design.

## Components that matter most / least
- Most: memory safety in `unsafe` and FFI code (decoders, V4L2 M2M, VA-API, Vulkan, NVENC, the webcam ring), the
  Computer-Use token check and request parsing, the socket peer checks, and integer arithmetic on sizes that a
  peer or client controls.
- In scope but lower: values only the X server or a GPU driver can set, and the CI workflows.
- Out of scope: `example/`, the vendored protocol XMLs, and the third-party codec libraries themselves (report a
  misuse of their API here, not their own bugs).

## How to exercise it
- `cargo test --manifest-path pixelflux/Cargo.toml` runs offline: software encoder round trips, a pure-Rust libva
  mock driver for the VA-API sessions, the Computer-Use server on loopback, and the webcam ring. Tests that need a
  GPU, a render node or an encoder device are `#[ignore]`.
- The SPS reader, the webcam decoders and the Computer-Use request handling can be driven from unit tests without
  a display.

## How you rate severity
- Critical: memory corruption reachable from a remote webcam bitstream or the Computer-Use server; input injection
  or screenshots without the token.
- High: memory corruption from a Wayland client or another local account; a remote input that hangs or crashes the
  process (it is the streaming server's).
- Medium: another local account replacing or reading a socket; resource exhaustion bounded by authentication;
  memory safety that only a malformed X server or driver reply reaches.
- Low: hardening without a demonstrated crossing.

## Anything to leave alone
- With the token, the Computer-Use server drives the desktop with the user's authority: that is its purpose.
- Environment variables and Python arguments come from the operator and are trusted.
- Reports should name the file and line, give a reproducer (a unit test is ideal), and propose a patch.
