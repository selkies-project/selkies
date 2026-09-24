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
used to do. Everything is written in American English -- color, behavior, center, initialize, canceled, and the
serial comma in a list of three or more -- except a name something upstream owns, such as GitHub Actions'
`cancelled()`, a Wayland `Cancelled` event, Python's `CancelledError`, the Web Audio `AnalyserNode`, or an NVENC
`colourMatrix` field. The prose under `docs/` follows the same rule: it describes what the tree does now, not what an
earlier revision did or what a change replaced.

Every language follows one shape: a Google-style docblock on the module, on every class, and on every function
that is not trivially self-describing, with the types on the signature. A docblock opens with a summary line, then
parameters, return value, and exceptions only where non-obvious; never pad trivial helpers. Contrasting with a
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
`src/selkies/Xlib`, `src/selkies/webrtc`, and `src/selkies/ice`, and the shadcn/ui primitives under
`addons/selkies-dashboard-wish/src/components/ui`; only Selkies-added comments there follow these rules. The three
Python forks are vendored so that they can be changed here rather than worked around, so editing them is the
expected way to fix what they do -- a change belongs upstream as well where upstream would take it. The
translation tables and the build helpers (`copy-*.js`, `gendb.js`, the vite and eslint configs) are excluded too.

Update the translations whenever user-facing strings change, adding entries where necessary.

## Logging

The log is what a remote user pastes back, so INFO tells the story of a session and nothing else: the one
startup line saying what the server came up as (`__main__._startup_summary`), what each client connected as
and what its display streams as, the capture and encoder path each display took, and every later change or
failure. A step of the mechanism (a reconfiguration phase, a broadcast, a task starting, a message received,
a value re-seeded) is DEBUG; something the operator has to act on is WARNING or ERROR. Every module logs
through a short logger name that says which part of the server spoke (`main`, `server`, `ws`,
`webrtc`, `signaling`, `display`, `input`, `gamepad`, `audio`, and the module's own name elsewhere), the same
tag in both transports and on both backends, and never the root logger. A line names its display and its
client, states values as `1920x1080`, `60 fps`, `crf 25`, and says what was decided rather than which
function ran; the same event is one line, not a "starting" and a "done". Levels are configured in one place
(`src/selkies/logs.py`): `--debug` opens DEBUG on every Selkies logger and reaches pixelflux as
`debug_logging`, third-party loggers stay at WARNING, and the vendored WebRTC and ICE stacks are paced to one
line per template per five seconds. A debug line that fires per frame, per packet or per cursor change goes
behind that pacing or stays out. The `[X11]` and `[Wayland]` lines pixelflux prints beside these follow the
same rule; the exact strings the suites wait on (`Capture started for`, `Stream settings active`,
`Selkies server running on`, `Socket listening on:`) are contracts, so a change to one changes the test with it.

## Testing

Validate in a sandbox, never in the session you are shown: the devcontainer, or a host set up as the Agentic
Development section of `docs/development.md` describes, where `scripts/ci/test-stack.sh` provides the display and
audio the suites stream from on `E2E_DISPLAY`, apart from any desktop's `DISPLAY`. Run the cheapest tier that covers
a change on every change (`pre-commit run --all-files`, `pytest tests -m unit`), the `integration` tier and the `e2e`
blocks the change touches before reporting it, and a measurement for every claim that is a number. End-to-end testing
is possible with the installed Firefox and Chrome, and Playwright/Selenium/Puppeteer/Cypress WebKit in place of
Safari. Ask before building an environment on a machine that was not set up for one (Miniforge serves a host with a
closed package manager; keep the system `libgbm.so` for GBM on NVIDIA and other GPUs) and take the operator's
directives on how it is constructed and constrained. A suite that skips is a failure in CI; say which checks could
not run where the hardware for them was not available.

A defect that predates the change you are making is still in scope: fix it, or say precisely what is broken, what
you ruled out, and what you would do next. The same applies to a failure you cannot reproduce yet — narrow it until
it is fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

Nothing is pushed before `pre-commit run --all-files` passes on the exact tree being pushed: CI's Lint job runs the
same hooks (`ruff-check`, `codespell`, `settings-doc`, `file-index`, and `web-lint` for the dashboards) and fails the
run on what they find. Before a suite failure is called a regression, run it against the unchanged tree as well: the
suites' servers come from the editable install, so a worktree needs `SELKIES_TEST_PYTHON` pointed at a wrapper that
sets `PYTHONPATH` to its `src`. Check the sandbox's own health the same way. The sound server must answer
`pactl info` within a few seconds, else every websockets handshake pays the control plane's bounded timeouts and
the log-polling suites fail on their windows: point `PULSE_SERVER` at a private PulseAudio instead (the harness
passes it on). Nothing the shell running the suites exports reaches their servers and browsers by accident: a
compositor that needs `LD_LIBRARY_PATH` gets it from a wrapper script on `PATH`, never from the environment. A
process is never ended by name or pattern; attribute it by `/proc/<pid>/environ` (`E2E_WORKDIR`, `XDG_RUNTIME_DIR`,
`DISPLAY`) and end that pid or its group, since the desktop session's own compositor and sound server share the
host.

## Landing a change

A change is ready when four questions have answers, and the commit or pull request gives them to the reviewer:

1. Was the defect, or the missing behavior, reproduced on the code before the change? A failing check or a
   measurement on the old tree is that answer; an argument from the source is not.
2. Is it gone, or present, on the exact code being committed, through the path a user takes? A test that reaches the
   result only through a switch a user would never flip (a developer toggle, a debug key, a knob of the rig) has
   confirmed nothing.
3. Can the change affect behavior it was not aimed at, and what was run to know? Name the suites and measurements
   that ran and the paths they did not cover.
4. Is the change stripped to what makes it work? Every line the first two answers do not need is noise the maintainers
   have to sift; drop it, or say why it stays.

A change in an area a maintainer has said they are working on goes to a branch and a pull request carrying those
answers, never straight to `main`, whatever standing permission to push `main` exists. An issue is closed by a
maintainer, never by you. A pull request's `Closes` keyword is not you closing it; the maintainer's merge
is. An optional path another
component may offer (a protocol a compositor advertises, a driver feature, a device) is taken only when its presence
is detected and never as the default: that it is exposed is not proof it works, and a reviewer has to be able to tell
what runs where.

## Engineering priorities

- Parity between X11 and Wayland, WebSockets and WebRTC, and the default and wish dashboards: anything wired up on
  one side but not the other is a bug. Prefer deduplicating code that serves the same purpose across modes over
  keeping parallel copies, when you are confident there is no regression or can validate it.
- Screen coroutine usage in Python and JavaScript and thread usage in every language so nothing hangs or lags,
  holding the GIL no longer than the work needs. Zero-copy and latency-reducing measures are always worth
  preserving or adding.
- End-to-end latency and an unrestricted frame rate are separate goals, not two ends of one dial: neither is
  spent to buy the other, and a change that trades one away has not improved the other.
- A change never drops a capability or falls back to an older implementation to make itself simpler. Where one
  seems to be in the way, say what it is rather than removing it.
- Compatibility spans Python 3.9 to 3.15 or higher and CUDA/NVENC 11 to 13 or higher. Gate on capabilities, never
  on interpreter versions: prefer the API that already encapsulates the difference (e.g. a library's own runner),
  else probe the feature itself (`hasattr`, a parameter's presence in `inspect.signature`, a try/except of the
  API) — never compare `sys.version_info`.
- The capture stack is pinned, not detected: every Selkies build and release names the pixelflux and pcmflux
  build or release it goes with (`[project.dependencies]`), which CI resolves for the branch under test, so the
  two are never mixed across versions. Backward compatibility measures against a sibling are unwanted for that
  reason: a `hasattr` probe for a method a newer one adds, or a suite that skips because the installed wheel
  lacks an API, hedges a combination that never ships and hides the path it was meant to measure. Call the
  sibling API directly and land its commit first, so the run here builds against it. What a pinned build was
  compiled with (software H.264, below) is still read, as is the host's Python and CUDA range above.

## Cross-cutting invariants

Each is documented in full where named; read that before changing the subsystem.

- The Wayland path is subprocess-free: never reintroduce wtype, wl-copy, or similar forks where the in-process
  pixelflux harness exists. Injection and clipboard are fallback ladders whose cooldowns re-probe the top rung
  rather than latching (`src/selkies/input_handler.py` module docstring).
- A DPI is an output scale on the session compositor, never Xft resources; only a changed capture scale restarts a
  capture (`src/selkies/display_utils.py` module docstring).
- Software H.264 is a property of the installed pixelflux build, never a Selkies setting
  (`settings.software_encoders`, `canonical_encoder`; the OpenH264 profile gate in `src/selkies/webrtc_engine.py`).
- The sound-server control plane is in-process over pulsectl_asyncio under a never-cancel discipline; `pactl` is
  only the fallback when the bindings are missing (`src/selkies/audio_control.py` module docstring).
- Bulk traffic sharing the session connection is paced by an end-to-end gauge, never by the local send
  queue alone: a proxy or a receive window in front absorbs writes, so that queue reads empty on the very
  transfer burying the stream (`websockets_mode._bulk_pace`, `stream_server.UplinkGauge`; WebRTC instead rides
  SCTP's own congestion control, which no such hop can hide).
- A modifier's role comes from the keysym the client resolved for it, never from the engine's flags:
  browsers name the same physical key differently (macOS Option is `AltGraph` to Gecko, `Alt` to Blink, a
  Meta key to WebKit). That decides both text-versus-shortcut and when a held modifier is stale
  (`Input._composesText`, `Input._releaseDesyncedModifiers`).
- A display of an extended desktop is a RandR output with a CRTC of its own wherever the X server offers
  pluggable outputs (spare outputs carrying a `Connected` property, which the Xvfb the images build has): it is
  plugged in, given its exact mode and position, and unplugged when its client leaves, so window managers and
  toolkits meet what hardware would show them and none has to be taught about a framebuffer. The capability is
  detected, never assumed, and a layout that both adds a display and moves the primary publishes the move first,
  because a desktop takes in a new screen before a moved one (`display_utils` module docstring,
  `_sync_apply_output_layout`, `output_layout_stage`). Everything a server without such outputs gets instead
  lives in `display_utils_xrandr` and runs nowhere else: every display a RandR logical monitor listing the
  physical output, because a toolkit realizes a monitor only where one is listed; whether the server lets
  several monitors share the output read back from the reply rather than assumed, since RandR 1.5 gives an
  output to one monitor and servers before 21.1 enforce that (`_sync_set_selkies_layout`). A server whose driver
  reports no display device has no output to list and no screen of its own: there the monitors are the only
  screens the toolkits find, so they are published without one and clearing the layout leaves one covering the
  framebuffer.
- One remote pointer is driven by an `Input` per display page, so a pointer message carries the buttons the
  event reports held, never the transitions one page happened to witness: a held drag crosses between pages,
  reaching one that never saw the press and leaving one that never sees the release
  (`Input._mouseButtonMovement`).
- That drag is placed through the stream box the page it crossed onto published in desktop coordinates (the
  `vp` verb), never from the grabbed page's own coordinates: two viewports share no origin, chrome height, or
  device pixel ratio. Two events have to agree on the offset between a page's client frame and the desktop's
  before it publishes a box, because page zoom scales one frame and not the other, and short of that agreement
  the crossing keeps to the scaled overshoot. So does a crossing between two boxes that overlap on the desktop:
  two windows cannot both be under the pointer, and an engine reporting screen coordinates relative to its own
  window publishes exactly such boxes (`Input._noteScreenAnchor`, `Input._mapToLayout`).
- A client asks the server only for what its own decoder will take, measured rather than assumed: engines
  differ on H.264 4:4:4 and change with every release, and one whose decoder lacks the profile shows no picture
  at all rather than a worse one (`util.canDecodeFullColor`, `canDecodeEncoder`). A full color the server's
  own unlocked default announces is turned off by such a client the same way, so the stream stays on its
  codec at 4:2:0; a WebRTC client says in its hello which 4:4:4 it decodes (`fullcolor_codecs`), so the
  server settles that before the first offer (`RTCApp._settle_fullcolor`) and never switches the profile
  under a decoder mid-stream; only a locked full color goes to the refusal ladder (a report over WebRTC). That
  ladder is one order on both transports (`ENCODER_LADDER` and `encoder_rung` in settings.py, `LADDER_ORDER`
  and `nextRung` in the core): the full-frame codecs the host encodes in hardware, most efficient first, then those it
  encodes in software by their encoders' measured time per frame, then striped H.264, and JPEG last. Over WebSockets the client walks it
  through the encoders it decodes; over WebRTC the offer lists it behind the display's codec
  (`RTCApp.prefer_codec`) and the display follows the codec the answer took.
- A picture a client could not decode is repaired by taking it out of the encoder's references, not by a key
  frame: the client names the frame it lost, the server asks that display's capture to forget it, and the
  stream keeps predicting past it while the other clients see nothing. Over WebSockets the client's decode
  gate names it (`addons/selkies-web-core/lib/decode-gate.js` module docstring, the `LOST_FRAME` verb); over
  WebRTC a second NACK for a packet the sender still holds does (`RTCRtpSender._retransmit`, the `lost_frame`
  event, `RTCApp.on_lost_frame`). A stream whose encoder names no reference -- a stripe, a session that cannot
  invalidate -- gets the key frame instead, and so does a run of drops the encoder never predicts past, or an H.264 loss covering the frame at the
  encoder's `frame_num` wrap, which FFmpeg's decoder cannot be predicted past.
- The webcam uplink mirrors the microphone: nothing about a frame is decoded or copied in Python
  (`addons/selkies-web-core/lib/webcam-capture.js` header, `src/selkies/webcam.py`,
  `addons/v4l2-interposer/v4l2_interposer.c` header for the interposer's locking rules).
- What a session starts with is one rule both transports read (`settings.pipeline_starts_on`, the
  `*_on_start` settings): the server captures only what a page receives and the page requests only what
  the policy or the user turned on, so nothing is started only to be stopped and a capture nobody receives
  never runs (`webrtc_media_pipeline` module docstring, `DataStreamingServer._video_start_state`, each core's
  `applyStartPolicy`). A camera or microphone policy of `demand` starts nothing at connect: what reads the
  virtual device decides, once for both transports, and only one page is ever asked (`capture_demand`
  module docstring).
- What a session runs on is reported, never inferred from settings, and what moves is sent only to a page
  looking at it: pixelflux records each capture and encoder decision where it makes it
  (`ScreenCapture.stream_info`), the server relays that once and on every change as `stream_info`, and every
  periodic figure (`stream_stats`: the host's load, the encode's rate and cost, the link) is sampled and sent
  only while a controller's dashboard has its stats on screen (the `_stats` verb; `stream_stats` module
  docstring, `addons/selkies-web-core/lib/stream-stats.js`). A shared viewer is sent neither, and both
  dashboards draw one reading of it (`lib/stream-stats-view.js`), where a row warns for a session that fell
  short of what it asked for, never for a choice and never for a server exposed no GPU, which is an ordinary
  deployment (`stream_stats.gpu_present`).
- The WebRTC ICE topology is decided once, at startup: `RTCApp.open_ice_muxes` binds the shared UDP and
  TCP ports the settings name (failing the service on a port in use), and every peer's gatherer reads the
  muxes and the ICE-lite choice from its `RTCConfiguration`, never from the settings directly; sessions on
  a shared socket are told apart by their ICE username fragment alone, which is why it is eight characters
  (`ice.mux` module docstring, `ice.Connection`).
