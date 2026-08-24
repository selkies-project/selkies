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

Keep comments terse and current: no comments that read like a PR summary, no issue or task numbers, no narration
of what the code used to do, and an inline comment only for a line-level invariant a docblock cannot carry. A
comment describes the present state of the code for a developer or an LLM.

Every language follows one shape: a Google-style docblock on the module, on every class, and on every function
that is not trivially self-describing, with the types on the signature. A docblock opens with a summary line, then
parameters, return value and exceptions only where non-obvious; never pad trivial helpers. Rationale that explains
a whole function belongs in its docblock, not in a block comment above it; contrasting with a rejected design
alternative is good rationale, narrating past revisions is forbidden. A module's docblock carries the mechanism
and rationale the module implements — a fallback ladder, a wire framing, the `window` contract a streaming core
publishes for the dashboards. The website's Developer Reference is generated from these docblocks on every site
build and never committed (`docs/reference` is gitignored; `website/scripts/generate-python-docs.mjs` and
`website/scripts/generate-web-docs.mjs`), rendered from Markdown, so keep anything shaped like `<name>` or
containing braces inside backticks.

- Python: Google-style docstrings (`Args:`/`Returns:`/`Raises:`) plus type hints on signatures, kept as you touch
  code. Hints must stay Python-3.9-safe: `Optional`/`Union` from `typing`, no `X | Y`, no new
  `from __future__ import annotations`, and conditionally imported types (pixelflux, pcmflux, Xlib) never appear in
  runtime-evaluated annotations — use `Any` rather than guess; a wrong hint is worse than none. Extracted by
  fumadocs-python (griffe).
- TypeScript (`.ts`/`.tsx`): JSDoc blocks with `@param name description`, `@returns`, `@throws`, and no types in
  the tags — the signature is the type hint, and a type repeated in a tag is a second copy to drift. Props are an
  `interface` or `type` with a line per field.
- JavaScript (`.js`/`.jsx`): the same JSDoc blocks, but the tags carry the types (`@param {Type} name`,
  `@returns {Type}`, `@typedef`/`@callback` for option bags and callbacks) because nothing else does. The wish
  dashboard's `tsc` compiles `selkies-web-core/lib` through `allowJs`, so these types are checked contracts, not
  prose: a `@type` on an exported constant replaces its inferred type for every TypeScript consumer.
- Both: a `/** ... @module */` block after the license header is the module docstring; `_`-prefixed members are
  private and hidden from the reference; a React component documents what it renders and which core messages or
  `window` state it consumes. Extracted by TypeDoc, which reads JavaScript through the TypeScript compiler's JSDoc
  support, so a tag it does not know (`@constructor`) is a warning at build time. Every function with a docblock
  is documented, exported or not (`website/scripts/web-reference-plugin.mjs`), so a closure's docblock is
  reference material, not a private note.

Vendored code keeps upstream documentation style for diffability and is excluded from the reference: the Python
forks `src/selkies/Xlib`, `src/selkies/webrtc` and `src/selkies/ice`, and the shadcn/ui primitives under
`addons/selkies-dashboard-wish/src/components/ui`; only Selkies-added comments there follow these rules. The
translation tables and the build helpers (`copy-*.js`, `gendb.js`, the vite and eslint configs) are excluded too.
The remaining addons (interposers, fake-udev, universal-touch-gamepad, the containers) are covered by their
READMEs and `docs/component.md` rather than the reference.

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
