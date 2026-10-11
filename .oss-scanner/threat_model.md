# Threat model

## What this project does and where untrusted input enters
- Selkies streams a Linux desktop to a browser: a Python asyncio server (`src/selkies`) with a bundled JavaScript web
  client (`addons/selkies-web-core`, dashboards in `addons/selkies-dashboard*`). One desktop user runs it; it injects
  keyboard, mouse, touch and gamepad input, syncs the clipboard both ways, serves files under a configured root, and
  plays audio, microphone and webcam through LD_PRELOAD interposers (`addons/*-interposer`, `addons/fake-udev`).
- Network input, weakest principal first: any peer that reaches the port (HTTP, the data WebSocket, WebRTC
  signaling, ICE/STUN/TURN, DTLS, SCTP, RTP/RTCP); a viewer token (watch only); a gamepad slot token; a read-write
  collaborator; the controller (full token or Basic auth user); the orchestrator holding the master token
  (`POST /api/tokens`, the signaling server peer).
- Entry points: `stream_server.py` (auth middleware, upload, download, recording, screenshot, print, apps routes),
  `websockets_mode.py` (data plane, binary opcodes, token table), `webrtc_signaling_server.py`, the vendored WebRTC
  stack (`webrtc/`, `ice/`, edited in tree and in scope), `input_handler.py` (`on_message`), `sessions.py` (roles).
- Local input: other accounts on the same host, through sockets and directories under `/tmp` and
  `$XDG_RUNTIME_DIR` (interposers, CUPS, X11, Wayland, PulseAudio).

## Components that matter most / least
- Most: authentication and role checks (a viewer or slot token must never reach input, files, the clipboard or
  another role); the parsers of remote bytes in `webrtc/` and `ice/` (one packet must not stall the event loop,
  which serves every client); the interposers' C code; file upload and download confinement.
- In scope but lower: the web client (origin checks on `postMessage`, token handling), CI workflows and release
  scripts.
- Out of scope: `website/`, `docs/`, `infra/`, and the prebuilt third-party wheels and patches in `addons/base`.

## How to exercise it
- `python -m pytest tests -m unit` runs offline from the source tree (pixelflux and pcmflux are stubbed), as the
  unprivileged `tester` user the image creates, since some suites check that permission errors are handled;
  `unit/test_webrtc_topology_settings.py` needs a network interface besides loopback for its ICE host candidates and
  fails without one. The suites in `tests/unit` show how to drive the token table, the auth middleware (aiohttp test
  client), the input dispatcher and the interposers (`tests/tools` builds a uinput shim and a V4L2 probe).
- The parsers can be called directly: `selkies.ice.stun.parse_message`, `selkies.webrtc.rtp`,
  `selkies.webrtc.rtcsctptransport`, `selkies.webrtc.sdp`.
- The full server needs a display, PulseAudio and a GPU or software encoder; reproducers that need no display are
  preferred.

## How you rate severity
- Critical: an unauthenticated peer gains input, files, the clipboard or code execution on a deployment with
  authentication on; memory corruption in the C interposers that a remote or other local user controls.
- High: escalation between roles (viewer or slot token to input, files or the clipboard; any token to the master
  token's powers); one message or packet from an unauthenticated or viewer peer that blocks the event loop for
  seconds or more, repeatably; a cross-origin page that injects input or reads the stream or a token.
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
