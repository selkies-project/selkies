# Working on this repository

Selkies is developed together with two sibling repositories, [pixelflux](https://github.com/selkies-project/pixelflux)
(screen capture and video encode) and [pcmflux](https://github.com/selkies-project/pcmflux) (audio capture and
encode). A change in one often belongs in another; coordinate across all three.

Use web search, web fetch, and other available tools as necessary. Make sure that the comments or documentation are
not too verbose (do not add comments more fit for a PR summary than a comment). Do not leave arbitrary numbers (such
as issue or task numbers) in the code or documentation. Do not use inline comments. Do not use comments or
documentation that describe arbitrary code changes of previous states compared to the current code that do not need
explanation. The code commenting should reflect the current state of the codebase and be used to convey information
to an LLM bot or developer.

Python follows a fixed documentation standard: Google-style docstrings plus type hints on signatures, kept as you
touch code. Modules, classes, and any function that is not trivially self-describing carry a docstring (summary
line, then `Args:`/`Returns:`/`Raises:` only where non-obvious; never pad trivial helpers). Rationale that explains
a whole function belongs in its docstring, not in a block comment above it; contrasting with a rejected design
alternative is good rationale, narrating the codebase's own past revisions is forbidden. Type hints must stay
Python-3.9-safe: `Optional`/`Union` from `typing`, no `X | Y`, no new `from __future__ import annotations`, and
conditionally imported types (pixelflux, pcmflux, Xlib) never appear in runtime-evaluated annotations — use `Any`
rather than guess; a wrong hint is worse than none. The website's Developer Reference is generated from these
docstrings at docs build time (fumadocs-python/griffe via `website/scripts/generate-python-docs.mjs`; output is
gitignored, never committed), so docstrings are rendered as MDX: keep anything shaped like `<name>` or containing
braces inside backticks. The vendored forks `src/selkies/Xlib`, `src/selkies/webrtc`, and `src/selkies/ice` are
the exception — they keep upstream documentation style for diffability and are excluded from the reference; only
Selkies-added comments there follow the rules above.

Empirical testing is a very useful way to develop this project, and empirical testing is possible for EVERYTHING,
including implementation, auditing, validation, or verification. A few such options are by utilizing the currently
installed Firefox and Chrome, as well as the WebKit engine provided by Playwright/Selenium/Puppeteer/Cypress in place
of Safari, for end-to-end tests. HOWEVER, ask the user for permission to create a test environment (possibly using
Miniforge; but note that it is likely the system `libgbm.so` should be used for GBM support on NVIDIA and other GPUs)
and receive directives from the user on how the environment should be constructed and constrained.

Note that parity between X11 and Wayland, as well as between WebSockets and WebRTC, or between the default dashboard
and the wish dashboard, is considered a key focus (things that were not wired up correctly on either side, and similar
discrepancies, are subject to fixes or deduplication). I prefer deduplicating code that performs similar purposes
across different modes over keeping duplicate code for no reason and more fragility. Refactor through deduplication if
you are confident there will be no regressions (or able to validate regressions). Screen coroutine usage in both
Python and JavaScript, as well as thread usage in all languages, so that everything is performant and does not lead to
hanging or lagging. Performance preservation or improvements such as zero-copy and latency-reducing measures are
always important. Note that compatibility should be ensured for Python 3.9 to 3.14 or even higher, and CUDA/NVENC 11
to 13 or higher. Gate on capabilities, never on interpreter versions: prefer the API that already encapsulates the
difference (e.g. a library's own runner), else probe the feature itself (`hasattr`, a parameter's presence in
`inspect.signature`, a try/except of the API) — never compare `sys.version_info`. Update the translations as well (and write/update additional entries if necessary) as necessary.
Injection and clipboard mechanisms form fallback ladders that resolve the newest architecture first and degrade rung
by rung: ext- before zwlr-data-control; on Wayland the seat keymap, then the in-process virtual-keyboard client, then
the data-control clipboard paste; on X11 in-process XTEST before an xdotool fork. Cooldowns re-probe the top rung
rather than latching into a fallback. Under a NESTED app compositor the seat rung only carries keysyms the base
layout resolves — the seat overlay never survives the inner compositor's own keymap — so the keyboard worker routes
character-bearing keysyms the base lacks (Cyrillic/Arabic/legacy planes, the Unicode plane) onto the
virtual-keyboard text batch by classification, not by failure. Which socket that batch and the selection are aimed at
is auto-detected: the compositor is the live `wayland-<N>` socket beside the capture compositor's, never a
differently named relay a session listens on, and aiming at the capture compositor instead is what silently kills
every non-ASCII key. A DPI is an output scale on that same compositor (wlr-output-management, in-process), never Xft
resources: XWayland runs in its logical space and is scaled with it, and scaling the CAPTURE output instead shrinks
the session and upscales it. The scale carries the size that screen is about to take in the same configuration,
because a session lays its desktop out once per applied configuration: a scale arriving alone leaves it at the
pre-connect mode under the new scale, a fraction of its final size, which a client that does not lay out again keeps.
Only what that compositor leaves — a KWin session, or no session at all — becomes the capture scale, and only a
changed capture scale restarts a capture. The Wayland path is subprocess-free by design — never reintroduce wtype, wl-copy or similar
forks where the in-process pixelflux harness exists. One X11 exception is deliberate: an X11 desktop hosted by a
ROOTFUL Xwayland that talks to the capture compositor directly (no nested session compositor) bridges no selection
on its own, so on the Wayland backend the X11 XFixes monitor is also built, on that live `$DISPLAY`, and the
clipboard loop waits on both it and the compositor feed; client writes are offered there too. Under a nested
compositor its XWM bridges its Xwayland, so no X monitor is built. Client-requested commands (the apps panel) run in
the session the applications live in (`WebRTCInput.app_session`): the nested compositor's socket and its Xwayland,
a rootful Xwayland's `DISPLAY` with no `WAYLAND_DISPLAY` (a Wayland toolkit would otherwise leave the desktop as a
fullscreen toplevel of the capture compositor), else the capture socket — plus the desktop's session bus and
identity adopted from its processes. The server publishes `app_terminal` (foot for a Wayland session, st for X11,
first installed) and the dashboards build the launch command from it. pixelflux answers the frame callbacks of a
surface-backed cursor and delivers a sprite on its commit: Xwayland and libwayland-cursor clients attach no new
sprite while the previous cursor frame callback is pending, which is what froze X cursors under rootful Xwayland.
A defect that predates the change you are making is still in scope: finding it does not make it someone else's,
and "pre-existing" is not a reason to leave it. Fix it, or say precisely what is broken, what you ruled out, and
what you would do next. The same applies to a failure you cannot reproduce yet -- narrow it until it is either
fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

Software H.264 is a property of the installed pixelflux build, never a Selkies setting: the default build encodes
with libx264 and a `PIXELFLUX_ENABLE_GPL=0` build with OpenH264, behind the same `h264enc` (full-frame) and
`h264enc-striped` encoders. `settings.software_h264_encoder()` reads `pixelflux.SOFTWARE_H264_ENCODER`
(`"x264"` | `"openh264"`); it is published to clients as `software_h264_encoder`, named in the capture-start log,
and drives the one decision that differs between the two: a session known to be on the software path (the striped
encoder, or `h264enc` with software encoding forced) defaults to CBR rate control on OpenH264, in
`resolve_rate_control_default` and in the dashboards' `softwareH264RcDefault` alike. OpenH264 is 4:2:0-only, so a
WebRTC offer for such a session never advertises the 4:4:4 profile. The retired `openh264enc` encoder name is an
alias of `h264enc` (`canonical_encoder`), like the historical `x264enc`. `tests/unit/test_rate_control_defaults.py`
stubs the build both ways; `tests/e2e/test_software_h264.py` streams the software path against whichever build the
server interpreter carries (`SELKIES_TEST_PYTHON`).

The webcam uplink mirrors the microphone: the browser encodes its camera (a sendonly video transceiver the server
reserved recvonly in the bundled SDP on WebRTC; WebCodecs H.264/VP8 or a JPEG canvas fallback as `0x06` frames on the
websocket, `lib/webcam-capture.js`), `src/selkies/webcam.py` gates and hands every frame to the process-wide
`pixelflux.VirtualCamera`, and applications see a V4L2 device through the Joystick-Interposer-style
`addons/v4l2-interposer` (`LD_PRELOAD`, unprivileged), a v4l2loopback device (`webcam_device`, the uinput-like
privileged/bare-metal path) or a PipeWire node (`webcam_pipewire`). The interposer takes frames from the backend
socket or from that PipeWire node (`SELKIES_WEBCAM_SOURCE`), so a node alone serves every consumer it covers; because
PipeWire's loops then run inside the application through the interposer's own hooks, no hook may hold
`handles_mutex` across a wait or a source release, and "not our fd" is decided from a lock-free bitmap. Nothing about
a frame is decoded or copied in Python. The device format follows the first uplink by default
(`webcam_pixel_format=auto`): a browser without WebCodecs sends JPEG and gets an MJPEG device its frames pass
through untouched, every other uplink an I420 device. Without WebCodecs the screen stream likewise degrades to the
striped-JPEG encoder (the WS pre-flight pins `encoder=jpeg` instead of failing; both dashboards offer only
decodable encoders) — the one case the client's JPEG rungs exist for, in both directions. `tests/unit/test_webcam_abi.py` pins the ring ABI the interposer shares with pixelflux;
`tests/integration/test_webcam_device.py` and `tests/e2e/test_webcam.py` cover the device and the browsers.

The sound-server control plane both transports share -- the capture null sink, capture-source resolution, the
SelkiesVirtualMic (``input`` null sink plus module-virtual-source), system defaults, and moving a strayed pcmflux
record stream -- is `src/selkies/audio_control.py` (`AudioControl`), in-process over pulsectl_asyncio: every
operation runs in a task that is never cancelled (a timeout abandons the connection instead, because libpulse holds
raw pointers to per-operation ctypes callbacks that die with the awaiting frame), clients bind to one loop and are
closed through `aclose`, and `pactl` subprocesses are the fallback only when the bindings are missing or cannot
connect (announced in one log line). `tests/unit/test_audio_control.py` drives it against a stub;
`tests/e2e/test_microphone.py` proves the microphone uplink and the absence of any pactl fork on both transports.

Update this file when certain details change.
