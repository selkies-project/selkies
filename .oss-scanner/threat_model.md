# Threat model

## What this project does and where untrusted input enters
- Selkies streams a Linux desktop to a browser: a Rust server on the tokio runtime (`crates/selkies`, the `selkies`
  executable, with its WebRTC stack in `crates/selkies-rtc`) with a JavaScript web client compiled into it
  (`web/selkies-web-core`, dashboards in `web/selkies-dashboard*`). One desktop user runs it; it injects
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
  endpoint and table in `tokens.rs`, the operator routes in `routes.rs`, the web client in `static_files.rs`: a
  `web_root` directory no path or link leaves, or the table `embedded.rs` serves, which a request names by key),
  `files` (upload, download, recording, screenshot, print, apps routes), `ws` (data plane, binary opcodes in
  `protocol.rs`), WebRTC signaling (`crates/selkies-rtc/src/signaling`, mounted by `webrtc/routes.rs`), the WebRTC
  stack (`crates/selkies-rtc`: `ice`, `dtls`, `srtp`, `rtp`, `sctp`, `sdp`, and the vendored `rtc-dtls`, `rtc-sctp`, and `rtc-srtp` under `vendor/`, edited in tree and in scope),
  `input` (`Dispatcher::receive`, the input authority in `authority.rs`), `core` (`Role`). Native media entry
  points include browser webcam bitstreams and H.264 SPS input in `crates/pixelflux/src/{webcam,encoders/sps.rs}`,
  microphone Opus and RED in `crates/pcmflux/src/red.rs`, the token-gated Computer-Use HTTP server in
  `crates/pixelflux/src/computer_use.rs`, and untrusted Wayland buffers and local recording, webcam, and Ogg sockets.
- Under `--wm` (`crates/selkies-wm`, installed in the compositor through `crates/pixelflux/src/wm.rs`): the
  openbox-format `rc.xml` and `menu.xml` the session user's configuration directory or `/etc/xdg` carries are
  parsed (`selkies-wm/src/config`), as is the user's key bind override file the desktop shell's shortcut editor
  writes (`~/.config/selkies-wm/keybinds`: a bind moved to another chord, disabled, or a command added on one,
  read again within a second of a change, `Config::apply_overrides`), the `autostart` file there is run as the
  session's application (`selkies-wm/src/session.rs`, `start_autostart`) holding a lock on
  `$XDG_RUNTIME_DIR/selkies-autostart.lock` that what it starts inherits, and run again when that lock frees under
  `--app-restart`, the session's mode is read from `~/.config/selkies-wm/session` (the one word `desktop` or
  `application`; anything else is the image's mode) and written when the root menu or the window menu switches
  it, which they offer unless `--session-switch` is off or `--desktop` is locked; a locked mode only keeps the
  window manager from starting the shell, since the session user can start any program; `--wm-menus` or
  `--wm-keybinds` turned off take the menu, close, kill, and run bindings away and have the window manager read
  only the system's `rc.xml`, `menu.xml`, and no key bind overrides, which narrows what a session user reaches
  through the window manager and nothing else, since the session user can still run programs and write files, a pipe menu runs the command the menu names and parses what it prints, the
  key and pointer bindings start the commands they name in the session user's environment, the Xwayland socket
  the compositor starts for its X11 clients (`crates/pixelflux/src/wayland/xwm.rs`) is one more local socket in
  `/tmp/.X11-unix`, and `zwlr_foreign_toplevel_manager_v1` lets any Wayland client of the capture socket list,
  activate, minimize, and close windows (`crates/pixelflux/src/wayland/foreign_toplevel.rs`), and
  `ext_workspace_manager_v1` lets any such client read the virtual desktops and ask the window policy for a switch
  (`crates/pixelflux/src/wayland/workspace.rs`), and `zwp_keyboard_shortcuts_inhibit_manager_v1` lets the client
  whose window holds the keyboard have the policy's binds stand aside while it reads keys (the desktop shell's
  shortcut editor; an inhibitor on any other window is left inactive, `KeyboardShortcutsInhibitHandler` in
  `crates/pixelflux/src/wayland/frontend.rs`); all three carry no bytes to parse beyond the protocol's typed
  arguments.
  Everything there runs as the session user, who may already do each of these; the boundary it keeps is the
  stream's: a panic in the window manager is caught at the policy boundary and the compositor carries on without it.
- Under `--desktop`, the `selkies-desktop` shell serves the desktop Settings portal
  (`crates/selkies-desktop/src/portal.rs`): `org.freedesktop.portal.Settings` on the session bus under
  `org.freedesktop.portal.Desktop`, taken only while no other process owns that name. Any process of the session
  user's bus may call it; it answers `ReadAll`, `ReadOne`, `Read`, and its `version` property from the shell's own
  theme decision, takes no writes, and runs nothing a caller names, so a caller learns only the scheme, accent, and
  theme names every window of the session is drawn with.
- Under `--desktop` the shell is also the session's notification server (`crates/selkies-desktop/src/notify.rs`):
  `org.freedesktop.Notifications` on the session bus, taken only while no other process owns that name. Any
  process of the session user's bus may call `Notify`, `CloseNotification`, `GetCapabilities`, and
  `GetServerInformation`, and every one hears the `NotificationClosed` and `ActionInvoked` signals. What a caller
  sends is bounded on the server's own thread before the shell draws it: texts are cut to a length (application
  name 64 characters, summary 256, body 2048 after its markup is stripped to text), actions to eight, the hints
  are read as typed fields with every other key skipped, an `image-data` hint is dropped unless its sides are
  1 to 1024, its samples 8 bits, its channels 3 or 4, and its rows fit the bytes sent, an `image-path` or icon
  path is a file the session user can read of at most 16 MiB, decoded under the image crate's allocation limits,
  a `desktop-entry` hint naming a path is ignored, every image is shrunk to 128 pixels, and the shell keeps at
  most 64 notifications, closing the oldest. A caller can show text and an image on the user's screen and learn
  when the user invoked one of its actions; nothing it sends is run, opened, or written.
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
  proot-apps release and the FerrisPad package checked against the digests GitHub records for them.
- In scope but lower, too: the `selkies-desktop` shell (`crates/selkies-desktop`), its own process the server
  starts under `--desktop`, whose control socket sits in the session's `$XDG_RUNTIME_DIR` and whose file manager
  decodes images and scans directories under the session user's home; it is kept out of the stream's process for
  that reason. Besides the commands a session user's `selkies-desktop --menu` and the like send it, the window
  manager writes one there itself, `bar-attached`, a list of display corners each with a flag (`ipc.rs`,
  `parse_bar_edges`): a word that does not read is skipped, and what reads only moves the bars of the displays it
  names.
- Out of scope: `website/`, `docs/`, `infra/`, and the prebuilt third-party wheels and patches in `addons/base`.

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
