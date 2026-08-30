# Working on this repository

Selkies is developed together with two sibling repositories, [pixelflux](https://github.com/selkies-project/pixelflux)
(screen capture and video encode) and [pcmflux](https://github.com/selkies-project/pcmflux) (audio capture and
encode). A change in one often belongs in another; coordinate across all three.

This file holds working conventions and the cross-cutting invariants no single module reveals. Mechanism and
rationale live in the docstring of the module that implements them, never here.

## Comments and documentation

The developer reference is generated from the docblocks, so explanation that lives in an inline comment is lost to
it. Put rationale in the docblock of the function or module it explains, and prefer a clearer name or a small
helper over a comment. An inline comment is for the line that stays surprising after that — a workaround for a
specific bug, an ordering or value that looks wrong but is required — and says why the line is that way, not what
it does. Comments are terse and current: no PR summaries, no issue or task numbers, no narration of what the code
used to do. The prose under `docs/` follows the same rule: it describes what the tree does now, not what an
earlier revision did or what a change replaced.

Every language follows one shape: a Google-style docblock on the module, on every class, and on every function
that is not trivially self-describing, with the types on the signature. A docblock opens with a summary line, then
parameters, return value and exceptions only where non-obvious; never pad trivial helpers. Contrasting with a
rejected design alternative is good rationale; narrating past revisions is forbidden. A module's docblock carries
the mechanism the module implements — a fallback ladder, a wire framing, the `window` contract a streaming core
publishes for the dashboards. Docblocks render as Markdown, so keep anything shaped like `<name>` or containing
braces inside backticks.

- Python: Google-style docstrings (`Args:`/`Returns:`/`Raises:`) plus type hints on signatures, kept as you touch
  code. Hints must stay Python-3.9-safe: `Optional`/`Union` from `typing`, no `X | Y`, no new
  `from __future__ import annotations`, and conditionally imported types (pixelflux, pcmflux, Xlib) never appear in
  runtime-evaluated annotations — use `Any` rather than guess.
- TypeScript (`.ts`/`.tsx`): JSDoc blocks with `@param name description`, `@returns`, `@throws`, and no types in
  the tags — the signature is the type. Props are an `interface` or `type` with a line per field.
- JavaScript (`.js`/`.jsx`): the same JSDoc blocks, but the tags carry the types (`@param {Type} name`,
  `@returns {Type}`, `@typedef`/`@callback` for option bags and callbacks). The wish dashboard's `tsc` compiles
  `selkies-web-core/lib` through `allowJs`, so these types are checked contracts: a `@type` on an exported
  constant replaces its inferred type for every TypeScript consumer.
- Both: a `/** ... @module */` block after the license header is the module docstring; `_`-prefixed members are
  private and hidden from the reference; a React component documents what it renders and which core messages or
  `window` state it consumes. A tag TypeDoc does not know (`@constructor`) is a build warning. Every docblock is
  published, exported or not, so a closure's docblock is reference material, not a private note.

Vendored code keeps upstream documentation style and is excluded from the reference: the Python forks
`src/selkies/Xlib`, `src/selkies/webrtc` and `src/selkies/ice`, and the shadcn/ui primitives under
`addons/selkies-dashboard-wish/src/components/ui`; only Selkies-added comments there follow these rules. The
translation tables and the build helpers (`copy-*.js`, `gendb.js`, the vite and eslint configs) are excluded too.

Update the translations whenever user-facing strings change, adding entries where necessary.

## Testing

End-to-end testing is possible with the installed Firefox and Chrome, and Playwright/Selenium/Puppeteer/Cypress
WebKit in place of Safari. Ask the user for permission before creating a test environment (possibly Miniforge; the
system `libgbm.so` should likely be used for GBM support on NVIDIA and other GPUs) and take their directives on how
it is constructed and constrained.

A defect that predates the change you are making is still in scope: fix it, or say precisely what is broken, what
you ruled out, and what you would do next. The same applies to a failure you cannot reproduce yet — narrow it until
it is fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

## Engineering priorities

- Parity between X11 and Wayland, WebSockets and WebRTC, and the default and wish dashboards: anything wired up on
  one side but not the other is a bug. Prefer deduplicating code that serves the same purpose across modes over
  keeping parallel copies, when you are confident there is no regression or can validate it.
- Screen coroutine usage in Python and JavaScript and thread usage in every language so nothing hangs or lags.
  Zero-copy and latency-reducing measures are always worth preserving or adding.
- Compatibility spans Python 3.9 to 3.14 or higher and CUDA/NVENC 11 to 13 or higher. Gate on capabilities, never
  on interpreter versions: prefer the API that already encapsulates the difference (e.g. a library's own runner),
  else probe the feature itself (`hasattr`, a parameter's presence in `inspect.signature`, a try/except of the
  API) — never compare `sys.version_info`.

## Cross-cutting invariants

Each is documented in full where named; read that before changing the subsystem.

- The Wayland path is subprocess-free: never reintroduce wtype, wl-copy or similar forks where the in-process
  pixelflux harness exists. Injection and clipboard are fallback ladders whose cooldowns re-probe the top rung
  rather than latching (`src/selkies/input_handler.py` module docstring).
- A DPI is an output scale on the session compositor, never Xft resources; only a changed capture scale restarts a
  capture (`src/selkies/display_utils.py` module docstring).
- Software H.264 is a property of the installed pixelflux build, never a Selkies setting
  (`settings.software_h264_encoder`, `canonical_encoder`; the OpenH264 profile gate in `src/selkies/rtc.py`).
- The sound-server control plane is in-process over pulsectl_asyncio under a never-cancel discipline; `pactl` is
  only the fallback when the bindings are missing (`src/selkies/audio_control.py` module docstring).
- Bulk traffic sharing the session connection is paced by an end-to-end gauge, never by the local send
  queue alone: a proxy or a receive window in front absorbs writes, so that queue reads empty on the very
  transfer burying the stream (`selkies._bulk_pace`, `stream_server.UplinkGauge`; WebRTC instead rides
  SCTP's own congestion control, which no such hop can hide).
- A modifier's role comes from the keysym the client resolved for it, never from the engine's flags:
  browsers name the same physical key differently (macOS Option is `AltGraph` to Gecko, `Alt` to Blink, a
  Meta key to WebKit). That decides both text-versus-shortcut and when a held modifier is stale
  (`Input._composesText`, `Input._releaseDesyncedModifiers`; `tests/tools/keyboard_chord_audit.mjs` holds
  every engine to one answer).
- One remote pointer is driven by an `Input` per display page, so a pointer message carries the buttons the
  event reports held, never the transitions one page happened to witness: a held drag crosses between pages,
  reaching one that never saw the press and leaving one that never sees the release
  (`Input._mouseButtonMovement`).
- The webcam uplink mirrors the microphone: nothing about a frame is decoded or copied in Python
  (`addons/selkies-web-core/lib/webcam-capture.js` header, `src/selkies/webcam.py`,
  `addons/v4l2-interposer/v4l2_interposer.c` header for the interposer's locking rules).
