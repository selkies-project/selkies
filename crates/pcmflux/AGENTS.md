# Working on this repository

pcmflux is the audio-capture and encode library behind
[selkies](https://github.com/selkies-project/selkies), and is developed together with it and with
[pixelflux](https://github.com/selkies-project/pixelflux) (screen capture and video encode). A change in one often
belongs in another; coordinate across all three.

Use web search, web fetch, and other available tools as necessary. Make sure that the comments or documentation are
not too verbose (do not add comments more fit for a PR summary than a comment). Do not leave arbitrary numbers (such
as issue or task numbers) in the code or documentation. Do not use inline comments. Do not use comments or
documentation that describe arbitrary code changes of previous states compared to the current code that do not need
explanation. The code commenting should reflect the current state of the codebase and be used to convey information
to an LLM bot or developer. Write American English -- color, behavior, center, initialize, canceled -- except
where a name belongs to something upstream, such as a Wayland `Cancelled` event or an NVENC `colourMatrix` field.

Empirical testing is possible for everything here, including implementation, auditing, validation, and verification,
and every change is validated before it is reported. The crate under `pcmflux/` is a Rust library (`rlib` and
`cdylib`) whose public API (`AudioCapture`, `AudioPlayback`, the settings, `AudioFrame`, `AudioSink`, `Error`,
`State`, `stop_all`) is everything in `src/lib.rs`; its `python` feature adds `src/python.rs`, the `pcmflux`
extension module as thin wrappers over that API, and nothing in `lib.rs` names PyO3. Both configurations are tested on
every change: `cargo test --lib` and `cargo clippy --all-targets -- -D warnings` without features (no interpreter is
linked), the same two with `--features python`, and `pre-commit run --all-files` (rustfmt, both clippy configurations,
ruff, and codespell) are the floor, and `cargo test --release bench_emit_assembly -- --ignored --nocapture` prints the
assembly measurement to quote rather than assert. The test binary with `--features python` links the interpreter
because pyo3's `extension-module` is a crate feature the Python build alone asks for (`features` on the `RustExtension`
in `setup.py`); folding it into `python` would leave that test binary unable to link. Behavior is decided by the
Python bridge: a default, a validation message, a timing, a buffer size, or a framing is the same through both
bridges, and the Python side is the one the end-to-end suites measure. The crate's `Cargo.toml` is the one place the version lives: `setup.py` reads it, spelling a semver
pre-release the PEP 440 way (`2.1.0-rc.1` is `2.1.0rc1` to pip), and the release workflow stamps the tag into the
manifest and the lock, so a build ahead of a release carries the series version and a release the tag's. End to end, a change is a wheel (`pip wheel . --no-deps`) installed into a
selkies sandbox as the Agentic Development section of that repository's `docs/development.md` describes, driven by its audio suites over
both transports with the installed Firefox and Chrome and Playwright/Selenium/Puppeteer/Cypress WebKit in place of
Safari. Ask before building an environment on a machine that was not set up for one (Miniforge serves a host with a
closed package manager) and take the operator's directives on how it is constructed and constrained. Say which checks
could not run where the hardware for them was not available.

`.oss-scanner/` serves Anthropic's OSS Scanner. `threat_model.md` states what pcmflux does, where untrusted bytes
enter from the weakest principal up, what matters most, how to exercise it, and how severity is rated; `Dockerfile`
builds the crate and runs its tests offline the way CI's cargo job does. The threat model is the rule the code is
held to, not a description of it: where the code lets an input reach what the model keeps from it, the code is the
defect even when its documentation and a test agree with it, and the fix corrects all three. Anyone, an agent
included, may change the rule to harden it (a principal reaching less, a boundary it did not state yet); loosening
it is a maintainer's call. Keep the model current in the commit that moves it: a new socket, file written, or parser
of bytes pcmflux does not write itself joins its entry points. Carry a new system package the build or the tests
need into `Dockerfile`.

`.clusterfuzzlite/` builds `pcmflux/fuzz` (`cargo fuzz`) in an OSS-Fuzz builder for ClusterFuzzLite
(`.github/workflows/fuzz.yml`: the code a pull request changes for five minutes, every target for an hour a day)
and OSS-Fuzz. Code that parses bytes pcmflux does not write itself (a microphone packet, a RED payload, a socket
peer's request) gets a target in `fuzz/fuzz_targets` that compiles the modules it fuzzes through `#[path]`, so it
needs none of the crate's system libraries, with seeds in `fuzz/seeds/<target>/` and its source among the
workflow's `pull_request` paths. A crash the fuzzer finds is fixed at its cause, and its input joins the seeds.

CodeQL's code scanning and the other checkers (clippy, rustfmt, ruff and codespell through pre-commit; cargo-deny's
advisories, licenses and bans; Dependabot's alerts; the OSS Scanner's and the fuzzers' findings) are at least
stylistic recommendations: a finding is fixed in the code and its alert closed by the next scan or update, never
dismissed or closed by hand, and a lint is not silenced (`#[allow]`, `noqa`) where the code can be written its way.
Code that is safe through a check a checker cannot follow (a validator in another function, a bound set elsewhere) is
rewritten so the check sits where it is recognized (CodeQL's `*Customizations.qll` in github/codeql lists the forms),
so the checker keeps watching that path. A protocol requirement alone is dismissed, with the standard that fixes it
named (a hash, port or file format the protocol or its tools prescribe).

Priority: End-to-End Processing Latency > Resource Usage or Unrestricted Frame Rate >= Quality (non-perceptible or
statistically insignificant latency fluctuations of up to 5% are acceptable in exchange for quality or bandwidth
consistency, and slightly more GPU or CPU usage is also acceptable if it has no latency impact and brings a
substantial quality improvement) >> Overall Bandwidth Efficiency (since encoded frames are only used once, unlike
.mp4/.mkv)

Note that parity between X11 and Wayland, as well as between WebSockets and WebRTC, between the default dashboard
and the wish dashboard, or across encoders and codecs (the hardware encoders, NVENC, VA-API, V4L2 M2M and Tegra, and
the software ones, x264, OpenH264, x265, kvazaar, libvpx, SVT-AV1 and libjpeg-turbo; H.264, H.265, AV1, VP8, VP9 and
JPEG), is considered a key focus (things that were not wired up correctly on either side, and similar discrepancies,
are subject to fixes or deduplication; a fix or a feature made for one encoder or codec is checked against every
other its mechanism applies to and carried there, or the reason one cannot take it is stated). I prefer
deduplicating code that performs similar purposes across different modes over keeping duplicate code for no reason
and more fragility. Refactor through deduplication if you are confident there will be no regressions (or able to
validate regressions). Screen coroutine usage in both
Python and JavaScript, as well as thread usage in all languages, so that everything is performant and does not lead to
hanging or lagging. Performance preservation or improvements such as zero-copy and latency-reducing measures are
always important, and the GIL is held no longer than the work needs. End-to-end latency and an unrestricted frame
rate are separate goals rather than two ends of one dial: neither is spent to buy the other. A change never drops a
capability or falls back to an older implementation to make itself simpler; where one seems to be in the way, say
what it is rather than removing it. Note that compatibility should be ensured for Python 3.9 to 3.15 or even higher.
A defect that predates the change you are making is still in scope: finding it does not make it someone else's,
and "pre-existing" is not a reason to leave it. Fix it, or say precisely what is broken, what you ruled out, and
what you would do next. The same applies to a failure you cannot reproduce yet -- narrow it until it is either
fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

A change is ready when four questions have answers, and the commit or pull request gives them to the reviewer:
was the defect, or the missing behavior, reproduced on the code before the change (a failing check or a measurement
on the old tree, not an argument from the source); is it gone, or present, on the exact code being committed, through
the path a user takes rather than a switch a user would never flip (a developer toggle, a debug key, a knob of the
rig); can the change affect behavior it was not aimed at, and what was run to know; and is the change stripped to what
makes it work, since every line the first two answers do not need is noise the maintainers have to sift. A change in an
area a maintainer has said they are working on goes to a branch and a pull request carrying those answers, never
straight to `main`, whatever standing permission to push `main` exists. An issue is closed by a maintainer,
never by you. A pull request's `Closes` keyword is not you closing it; the maintainer's merge is.
An optional path another component may offer
(a protocol a compositor advertises, a driver feature, a device) is taken only when its presence is detected and never
as the default: that it is exposed is not proof it works, and a reviewer has to be able to tell what runs where.

`LICENSES.md` inventories the crates, the linked libpulse (LGPL-2.1-or-later), and the statically linked libopus (BSD-3-Clause);
`pcmflux/deny.toml` keeps the crate graph permissive (the `Licenses` workflow runs it). A new crate that links
native code gets a row there.

Update this file when certain details change.
