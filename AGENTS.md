# Working on this repository

Selkies is developed together with two sibling repositories, [pixelflux](https://github.com/selkies-project/pixelflux)
(screen capture and video encode) and [pcmflux](https://github.com/selkies-project/pcmflux) (audio capture and
encode). A change in one often belongs in another; coordinate across all three.

## What this file is for

This file holds working conventions and the cross-cutting invariants an agent cannot infer from any single module.
Mechanism and rationale — how a fallback ladder resolves, why a rung is pinned, what a flags byte carries — belong
in the docstring or header comment of the module that implements them, where the developer reference is generated
from and where they are updated with the code. Do not restate them here, and do not grow this file when a
subsystem changes; update the docstring at the site instead.

## Comments and documentation

Keep comments terse and current: no comments that read like a PR summary, no inline comments, no issue or task
numbers, no narration of what the code used to do. A comment describes the present state of the code for a
developer or an LLM.

Python follows a fixed standard: Google-style docstrings plus type hints on signatures, kept as you touch code.
Modules, classes, and any function that is not trivially self-describing carry a docstring (summary line, then
`Args:`/`Returns:`/`Raises:` only where non-obvious; never pad trivial helpers). Rationale that explains a whole
function belongs in its docstring, not in a block comment above it; contrasting with a rejected design alternative
is good rationale, narrating past revisions is forbidden. Type hints must stay Python-3.9-safe: `Optional`/`Union`
from `typing`, no `X | Y`, no new `from __future__ import annotations`, and conditionally imported types (pixelflux,
pcmflux, Xlib) never appear in runtime-evaluated annotations — use `Any` rather than guess; a wrong hint is worse
than none. The website's Developer Reference is generated from these docstrings (fumadocs-python/griffe via
`website/scripts/generate-python-docs.mjs`; output is gitignored, never committed) and rendered as MDX, so keep
anything shaped like `<name>` or containing braces inside backticks. The vendored forks `src/selkies/Xlib`,
`src/selkies/webrtc`, and `src/selkies/ice` keep upstream documentation style for diffability and are excluded
from the reference; only Selkies-added comments there follow these rules.

Update the translations whenever user-facing strings change, adding entries where necessary.

## Testing

Empirical testing is possible for everything — implementation, auditing, validation, verification — using the
installed Firefox and Chrome, and Playwright/Selenium/Puppeteer/Cypress WebKit in place of Safari, for end-to-end
tests. Ask the user for permission before creating a test environment (possibly Miniforge; the system `libgbm.so`
should likely be used for GBM support on NVIDIA and other GPUs) and take their directives on how it is constructed
and constrained.

A defect that predates the change you are making is still in scope: fix it, or say precisely what is broken, what
you ruled out, and what you would do next. The same applies to a failure you cannot reproduce yet — narrow it until
it is fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

## Engineering priorities

- Parity between X11 and Wayland, WebSockets and WebRTC, and the default and wish dashboards is a key focus;
  anything wired up on one side but not the other is a bug. Prefer deduplicating code that serves the same purpose
  across modes over keeping parallel copies; refactor through deduplication when you are confident there is no
  regression or can validate it.
- Screen coroutine usage in Python and JavaScript and thread usage in every language so nothing hangs or lags.
  Zero-copy and latency-reducing measures are always worth preserving or adding.
- Compatibility spans Python 3.9 to 3.14 or higher and CUDA/NVENC 11 to 13 or higher. Gate on capabilities, never
  on interpreter versions: prefer the API that already encapsulates the difference (e.g. a library's own runner),
  else probe the feature itself (`hasattr`, a parameter's presence in `inspect.signature`, a try/except of the
  API) — never compare `sys.version_info`.

## Cross-cutting invariants

Each is documented in full where named; read that before changing the subsystem.

- The Wayland path is subprocess-free: never reintroduce wtype, wl-copy or similar forks where the in-process
  pixelflux harness exists. Injection and clipboard mechanisms are fallback ladders that resolve the newest
  architecture first and whose cooldowns re-probe the top rung rather than latching (`src/selkies/input_handler.py`
  module docstring: ladder order, nested-compositor keysym routing, socket auto-detection; `_x11_session_display`
  for the rootful-Xwayland clipboard exception, `app_session` for where client-requested commands run).
- A DPI is an output scale on the session compositor, never Xft resources; only a changed capture scale restarts a
  capture (`src/selkies/display_utils.py` module docstring).
- Software H.264 is a property of the installed pixelflux build, never a Selkies setting
  (`settings.software_h264_encoder`, `canonical_encoder`; the OpenH264 profile gate in `src/selkies/rtc.py`).
- The sound-server control plane is in-process over pulsectl_asyncio under a never-cancel discipline; `pactl` is
  only the fallback when the bindings are missing (`src/selkies/audio_control.py` module docstring).
- The webcam uplink mirrors the microphone: nothing about a frame is decoded or copied in Python, the device format
  follows the first uplink, and the client's JPEG rungs exist for browsers without WebCodecs
  (`addons/selkies-web-core/lib/webcam-capture.js` header, `src/selkies/webcam.py`,
  `addons/v4l2-interposer/v4l2_interposer.c` header for the interposer's locking rules).
