# Threat model

## What this project does and where untrusted input enters
- Selkies streams a Linux desktop to a browser: a Rust server on the tokio runtime (`crates/selkies`, the `selkies`
  executable, with its WebRTC stack in `crates/selkies-rtc`) with a bundled JavaScript web client
  (`addons/selkies-web-core`, dashboards in `addons/selkies-dashboard*`). One desktop user runs it; it injects
  keyboard, mouse, touch and gamepad input, syncs the clipboard both ways, serves files under a configured root, and
  plays audio, microphone and webcam through LD_PRELOAD interposers (`addons/*-interposer`, `addons/fake-udev`).
  Screen capture, video encode, virtual-webcam decode, and Computer Use live in `crates/pixelflux`; audio capture,
  Opus encode/decode, RFC 2198 RED, and Ogg output live in `crates/pcmflux`. Both are Rust libraries by default,
  with optional PyO3 bridges used only by their standalone wheels.
- Network input, weakest principal first: any peer that reaches the port (HTTP, the data WebSocket, WebRTC
  signaling, ICE/STUN/TURN, DTLS, SCTP, RTP/RTCP); a viewer token (watch only); a gamepad slot token; a read-write
  collaborator; the controller (full token or Basic auth user); the orchestrator holding the master token
  (`POST /api/tokens`, the signaling server peer).
- Entry points, under `crates/selkies/src` unless named otherwise: `http` (the auth gate in `auth.rs`, the token
  endpoint and table in `tokens.rs`, the operator routes in `routes.rs`), `files` (upload, download, recording,
  screenshot, print, apps routes), `ws` (data plane, binary opcodes in `protocol.rs`), WebRTC signaling
  (`crates/selkies-rtc/src/signaling`, mounted by `webrtc/routes.rs`), the WebRTC stack (`crates/selkies-rtc`: `ice`,
  `dtls`, `srtp`, `rtp`, `sctp`, `sdp`, and the vendored `rtc-sctp` under `vendor/`, edited in tree and in scope),
  `input` (`Dispatcher::receive`, the input authority in `authority.rs`), `core` (`Role`). Native media entry
  points include browser webcam bitstreams and H.264 SPS input in `crates/pixelflux/src/{webcam,encoders/sps.rs}`,
  microphone Opus and RED in `crates/pcmflux/src/red.rs`, the token-gated Computer-Use HTTP server in
  `crates/pixelflux/src/computer_use.rs`, and untrusted Wayland buffers and local recording, webcam, and Ogg sockets.
- Local input: other accounts on the same host, through sockets and directories under `/tmp` and
  `$XDG_RUNTIME_DIR` (interposers, CUPS, X11, Wayland, PulseAudio), and through a file-manager directory they can
  write (planted names, FIFOs, set-ID files). The screen is no account's to map: Xvfb starts without `-shmem`, whose
  segment any local account could read and write, and shares its framebuffer (`SELKIES-FRAMEBUFFER`) only with a
  local X client that may read the root, read-only.

## Components that matter most / least
- Most: authentication and role checks (a viewer or slot token must never reach input, files, the clipboard or
  another role); the parsers of remote bytes in `crates/selkies-rtc` (one packet must not stall a tokio worker or a
  task every client depends on); native decoder and FFI memory safety, RED bounds, Computer-Use token checks, and
  peer-controlled sizes in pixelflux and pcmflux; the interposers' C code; file upload and download confinement.
- In scope but lower: the web client (origin checks on `postMessage`, token handling), CI workflows and release
  scripts, and what the images download at build time outside a signed apt repository: the desktop image takes the
  `selkies-desktop` binary at the release tag `SELKIES_DESKTOP_VERSION` pins, checked against the `.sha256` that
  release publishes beside it, and proot-apps checked against the digest GitHub records for it.
- Out of scope: `website/`, `docs/`, `infra/`, the prebuilt third-party wheels and patches in `addons/base`, and the
  `selkies-desktop` program itself, a separate project, whose control socket sits in the session's
  `$XDG_RUNTIME_DIR`.

## How to exercise it
- `cargo test --workspace` runs the server's own tests in process, and they show how to drive the token table
  (`http/tokens.rs`), the auth gate and the routes over a real loopback listener (`http/route_tests.rs`), the file
  routes' confinement (`files/route_tests.rs`), and the input dispatcher and its authority (`input/tests.rs`). It
  builds pixelflux and pcmflux, so it needs the native libraries they build against (the packages the `server` stage
  of `addons/base/Dockerfile` installs); the ICE topology tests in `crates/selkies-rtc/src/ice/tests.rs` need a
  network interface besides loopback for their host candidates and fail without one.
- `python -m pytest tests -m unit` runs offline from the source tree, as the unprivileged `tester` user the image
  creates, since some suites check that permission errors are handled; the suites in `tests/unit` drive the web
  client's modules and the interposers (`tests/tools` builds a uinput shim and a V4L2 probe).
- The parsers of what a WebRTC peer sends can be called directly from `crates/selkies-rtc`: `ice::stun::parse`,
  `rtp::RtpPacket::parse` and `rtp::RtcpPacket::parse`, `sctp::Sctp::handle_packet`, `sdp::SessionDescription::parse`.
  `crates/selkies-rtc/fuzz` drives each the way the server does (`cargo fuzz run rtp|sctp|sdp|stun` on a nightly
  toolchain, seeds in `fuzz/seeds/<target>`).
- The scanner also runs pixelflux with `--features python` and with `--no-default-features --features openh264`,
  and pcmflux with `--features python`. The pixelflux suites cover SPS/webcam decoding, Computer-Use requests, and
  VA-API through a pure-Rust mock driver; pcmflux covers RED parsing and Ogg page construction and round trips.
  GPU, render-node, encoder-device, and physical sound-server tests remain `#[ignore]` without their hardware.
  `crates/pixelflux/fuzz` drives the SPS reads and the frame-kind reads the webcam uplink runs on a client's
  bitstream (`sps`, `keyframe`), and `crates/pcmflux/fuzz` drives RED, beside the WebRTC targets. DTLS and SRTP
  are parsed by the pinned `rtc-dtls` and `rtc-srtp` crates, not here, so they have no target of their own.
- The full server needs a display, PulseAudio and a GPU or software encoder; reproducers that need no display are
  preferred.

## How you rate severity
- Critical: an unauthenticated peer gains input, files, the clipboard or code execution on a deployment with
  authentication on; memory corruption in the C interposers that a remote or other local user controls.
- High: escalation between roles (viewer or slot token to input, files or the clipboard; any token to the master
  token's powers); one message or packet from an unauthenticated or viewer peer that blocks a tokio worker or a task
  every client depends on for seconds or more, repeatably; a cross-origin page that injects input or reads the stream or a token.
- Medium: another local account reaching the session through shared sockets or directories; resource exhaustion
  that needs an authenticated session or is bounded; secrets in logs at INFO or above; mutable unverified fetches in
  release builds.
- Low: secrets in DEBUG logs; hardening without a demonstrated crossing.

## Anything to leave alone
- A controller can do whatever the desktop user can: that is the product. Shell commands from the client
  (`command_enabled`, off by default) are a feature when the operator turns them on.
- With authentication turned off (no password, no tokens), any connector controls the desktop by design; operators
  front such deployments with their own proxy.
- The documented placeholder container password (`mypasswd`) and the turn-rest addon's public openrelay secret are
  intentional.
- Reports should name the file and line, give a reproducer runnable against the source tree, and propose a patch.
