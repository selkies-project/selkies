---
title: Development and Contributions
description: Build Selkies locally, customize a container, follow the style guides, and run the tests.
---

**Go to [Knowledge Base](#knowledge-base) for information on customization.**

**We are in need of maintainers and community contributors. Please consider stepping up, as we can never have too much help!**

This project was meant to be built upon community contributions from people without any prior media networking experience.

The project is built almost entirely in Python, with the performance-critical media paths isolated in small, self-contained Rust extensions (`pixelflux` and `pcmflux`). This keeps the orchestration code approachable even without prior experience in multimedia application development, making this project a perfect starting point for anyone who wants to get started.

Please return your developments with a [Pull Request](https://github.com/selkies-project/selkies/pulls) if you made modifications to the code or added new features, especially if you use this project commercially (as per MPL-2.0 license obligations). We will be happy to help or consult if you are stuck.

**NOTE: this project is licensed under the [Mozilla Public License, version 2.0](https://www.mozilla.org/en-US/MPL/2.0/FAQ/), which obliges to share modified code files licensed by MPL-2.0 when distributed externally, but does not apply for any larger work outside this project, which might be open-source or proprietary under any license of choice. Externally originated components outside this project may contain works licensed over more restrictive copyleft/proprietary licenses, as well as other terms of intellectual property, including but not limited to patents, which users or developers are obliged to adhere to.**

Our license prevents proprietary entities from engulfing our code without providing anything back, unlike the Apache License, but does not impede any larger proprietary work embedding our code, unlike the GNU GPL/LGPL/AGPL. Either way, we strongly encourage proprietary entities to provide back your developments in terms of pull requests directly into our code repository.

As the relatively permissive license compared to similar projects is for the benefit of the community, non-profit or profit, please do not take advantage of it. If improvements are not merged into this code repository, it will ultimately lead to the project becoming unsustainable. We need your help to continue maintaining performance and quality, as well as staying competent compared to proprietary applications. We want commercial research and development to thrive together with Selkies.

## Contributions

Please join our [Discord](https://discord.gg/wDNGDeSW5F) server, then start out with the [Issues](https://github.com/selkies-project/selkies/issues) to see if new enhancements that you can make or things that you want solved have been already raised.

**No programming experience:** You can still be a tester or a community helper/moderator at [Discord](https://discord.gg/wDNGDeSW5F)! Do you see anything that feels uncomfortable compared to other projects? Raise an issue and suggest various improvements including to the documentation. Have you used OBS, FFmpeg, or any other live streaming/video editing software before? You can suggest optimized parameters for the video encoders from your experiences. You can experiment with various encoder and streaming parameters, which are exposed in a very accessible way in [`settings.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/settings.py) (the command-line and environment settings listed in the [Settings Reference](settings.md)) and applied in [`media_pipeline.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/media_pipeline.py), improving streaming performance.

**Some Python or HTML/JavaScript frontend experience:** Our codebase and web interface always has room for improvement. Consider helping out on various issues or cleaning up the code otherwise.

**Linux X11/Wayland/Container/Conda experience:** Please report issues with the capture interface and provide improvements for our reference containers, the AppImage, and the `noarch` conda package that every release builds from [`infra/appimage/recipe.yaml`](https://github.com/selkies-project/selkies/tree/main/infra/appimage/recipe.yaml). `pixelflux`, `pcmflux`, and `pulsectl-asyncio` have no conda-forge feedstocks yet; contributing and maintaining them would let Selkies install from conda alone. A protocol and interface can never be great without a great environment it runs in. If you want to bring Selkies to MacOSX or Windows, check our issues!

**C/Rust experience:** Selkies delegates its media encoding to the [`pixelflux`](https://github.com/selkies-project/pixelflux) (screen capture with H.264/JPEG encoding) and [`pcmflux`](https://github.com/selkies-project/pcmflux) (PulseAudio capture with Opus encoding) Rust extensions, whose crate APIs are published at <https://pixelflux.selkies.io> and <https://pcmflux.selkies.io>, and its opt-in WebRTC transport to a vendored fork of `aiortc`. We need you to fix bugs and implement new capabilities in these components or any other upstream dependencies. This will not only benefit Selkies but also help the broader communities around those projects.

**Any type of multimedia networking experience:** While relevant experience is not necessary to contribute, we still feel great to have you as our companions. Please consider stepping up as a maintainer in addition to contributing! Development for commercial purposes are always fine as well as (our weak copyleft) license terms are complied with. Shape Selkies so that it fits your project as a first-class citizen, while keeping it accessible to many other people.

**WebSocket/WebRTC developers or Chromium/Firefox/Safari multimedia contributors:** We always need you, but you are generally very busy people. Even so, you can always provide directions on topics, ideas, specifications, or technologies that we have missed, so that other people including us can implement them. In many occasions, a single paragraph from experts are equal to hundreds of hours of work.

**Funding to improve this project:** If you want new features or improvements but if you are not a developer or lack enough time, please consider offering bounties by contacting us. If you want new features that require upstream work in our dependencies (such as `pixelflux`, `pcmflux`, or `aiortc`), we may need to fund developers capable of implementing them so they can be brought into Selkies as well. Such issues are tagged as requiring upstream development. Even for features or improvements that are ready to be implemented, crowdfunding bounties motivate developers to solve them faster.

Regardless of your experience level, there is always something that you could help. Our code structure enables you to focus on parts of the code that you know best without necessarily understanding the rest.

When contributing, please follow the overall style of the code, and the names of all variables, classes, or functions should be unambiguous and as less generic/confusing as possible.

## Influenced Projects

Currently in collaboration and received influences from: <https://github.com/Xpra-org/xpra>, <https://github.com/m1k1o/neko>

Provided heavy influences to other projects: <https://github.com/nestriness/nestri>, <https://github.com/Steam-Headless/docker-steam-headless>, <https://github.com/ai-dock>

## Contributors

Contact information for contributors currently available for paid consulting tasks is available by request in [Discord](https://discord.gg/wDNGDeSW5F).

### Maintainers

These people make structural decisions for this project and press the `Merge Pull Request` button.

[Seungmin Kim](https://github.com/ehfd): Co-Owner, Head Maintainer (Apr 2022 -), Academia Representative (Yonsei University College of Medicine, San Diego Supercomputer Center)

[Ryan Kuba](https://github.com/thelamer): Co-Owner, Head Maintainer (Jun 2025 -), [LinuxServer.io](https://www.linuxserver.io) Representative.

[Dan Isla](https://github.com/danisla): Project Founder, Co-Owner, Industry Representative (ex-Google, ex-NASA, ex-itopia)

[PMohanJ](https://github.com/PMohanJ): Contributed new features for the X11 input protocol as well as providing various fixes for the project overall and providing various means of analysis, **currently available for paid consulting tasks in tandem with senior maintainers**

### Code Contributors

[Sam Williams](https://github.com/ayunami2000): Provided various fixes for the WebRTC HTML5 web interface, as well as providing various means of analysis, **currently available for paid consulting tasks in tandem with senior maintainers**

[Kristian Ollikainen](https://github.com/DatCaptainHorse): Professional WebRTC and JavaScript frontend engineer, contributed various insights to the WebRTC and web components

### Past Maintainers

[Jan Van Bruggen](https://github.com/kili-ilo): Project Co-Founder, ex-Google, ex-NASA, ex-itopia, current Verily

[Carlos Ruiz](https://github.com/cruizba): [OpenVidu](https://openvidu.io) Team, provided various proposals for fixing the X11 input protocol

[Reisbel Machado](https://github.com/reisbel): itopia

# Knowledge Base

This section is a knowledge base for code contributions and development.

## Communities

- Selkies Discord: <https://discord.gg/wDNGDeSW5F>

- Selkies Matrix Space (Connect with United States HPC Academics, needs Matrix Account from <https://app.element.io>): <https://matrix.to/#/#ue4research:matrix.nrp-nautilus.io>

- Real-Time Streaming Discord (General WebRTC Advice): <https://discord.gg/KFS32mYXPr>


## Resources

- **Our [Documentation](README.md) and [Issues](https://github.com/selkies-project/selkies/issues)/[Pull Requests](https://github.com/selkies-project/selkies/pulls)** (including closed Issues/Pull Requests) and <https://github.com/m1k1o/neko/issues/371>

- Upstream projects behind the current media stack: [`aiortc`](https://github.com/aiortc/aiortc) (the WebRTC transport is a vendored fork), [`pixelflux`](https://github.com/selkies-project/pixelflux) (screen capture with H.264/JPEG encoding), and [`pcmflux`](https://github.com/selkies-project/pcmflux) (PulseAudio capture with Opus encoding)

- The Rust references those two generate: <https://pixelflux.selkies.io> and <https://pcmflux.selkies.io>

- WebRTC for the Curious: <https://webrtcforthecurious.com>

- WebRTC Official Google Groups: <https://groups.google.com/g/discuss-webrtc>

- Mozilla MDN: <https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API>

- WebRTC Hacks: <https://webrtchacks.com>

## Local Builds

[`docker-compose.yml`](https://github.com/selkies-project/selkies/tree/main/docker-compose.yml) builds and runs everything this repository produces, with Compose V2:

```bash
docker compose build dist                    # the wheel, web client included
docker compose build base desktop            # the session, then the desktop on top of it
docker compose up desktop                    # the Desktop Container on http://localhost:8080
docker compose --profile gpu up desktop-gpu  # the same container with a GPU attached
```

Each service names the `Dockerfile` that builds it, and those files are also the reference build procedure for a host without Docker®: the commands run in a shell as they stand.

The images take the distribution's packages as its repositories serve them on the day of the build, and build XLibre's Xvfb and NVIDIA's EGL platform library from release archives pinned by tag and checksum in [`addons/base/Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/base/Dockerfile). Every release attaches, per flavor and architecture, the digests of the images and of the distribution image they started from and the list of packages each holds (see [Core Components](components/index.md#core-components)). Rebuilding an older commit bit for bit would need those repositories as they were that day, which is a downstream qualification build's job: pin the published image by digest, or build against a retained package mirror; the upstream images promise the manifest, not the rebuild.

`desktop` bind-mounts the checkout at `/opt/selkies-src` and sets `SELKIES_DEV_SOURCE` to it, so the server runs from the tree and a change to it takes effect on a restart rather than a rebuild. `SELKIES_DEV_SOURCE` works on any image, including a published one:

```bash
docker run --rm -it --shm-size=2g -p 8080:8080 \
  -v "$PWD:/opt/selkies-src" -e SELKIES_DEV_SOURCE=/opt/selkies-src \
  ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04
```

The web client is a build product and a fresh checkout has none, so the image's own bundle is linked in at the (gitignored) path it is served from; running `scripts/ci/build-web.sh` locally builds a real one into the tree. The base image, the streaming mode, the port, and the TURN credentials come from the environment (`DISTRIB_IMAGE`, `DISTRIB_RELEASE`, `SELKIES_MODE`, `SELKIES_PORT`, `SELKIES_TURN_*`); a `.env` file next to the Compose file is the usual place for them. The images build with BuildKit (`RUN --mount`); the legacy builder cannot build them. To run a wheel built from this tree instead of the newest GitHub release's, copy it out of the `selkies-py-build` image into `addons/base/wheels/` before building.

## Documentation

The pages under [`docs/`](https://github.com/selkies-project/selkies/tree/main/docs) are plain Markdown and are what <https://docs.selkies.io> publishes. Editing one through GitHub's web editor is enough to change the site: either the pencil icon in the repository, or the **Edit on GitHub** button beside every page, opens the file it was built from.

A page begins with front matter naming it:

```yaml
---
title: Getting Started
description: One sentence, shown under the title and in search results.
---
```

[`docs/meta.json`](https://github.com/selkies-project/selkies/tree/main/docs/meta.json) lists the pages in sidebar order, and a new page has to be added to it to appear there.

Links between pages are written the way GitHub resolves them (`start.md`, or `components/index.md#encoders`) and are rewritten to site URLs during the build. Images live in `docs/assets` and are referenced relative to the file.

### Logo Assets

Every image in the repository is the Selkies logo, and all of them are generated from two hand-authored sources: `docs/assets/logo/selkies.svg` (the mark) and `wordmark.svg` (the lettering). Everything else is composed from those — the two lockups that set the mark beside or above the wordmark, the favicons, the PWA and touch icons, the dashboards' copies of the mark, and the PNG lockups. Edit a source and regenerate; do not edit the outputs:

```bash
python3 scripts/build-logo-assets.py           # rewrite every generated image
python3 scripts/build-logo-assets.py --check   # report which ones are stale
```

It needs `rsvg-convert` (librsvg) and Pillow. Only layout lives in the script: the canvas each lockup uses and the transform it places the mark and the wordmark at, so the mark can never end up embedded twice with two different gradients. The app icons are composed the same way — the mark on an opaque white disc, so a launcher that masks icons to a circle crops nothing.

Previewing the site needs nothing but [Node.js](https://nodejs.org):

```bash
cd website
npm install
npm run dev          # http://localhost:3000
```

`npm run build` writes the site to `website/out`, and `npm run check-links` fails on any link or anchor in that output that does not resolve.

The published site carries every version: `npm run build:versions`, which is what the `Docs` workflow uploads, builds one copy per release tag from the first that shipped the site and one for `main`. The newest release is built as `latest`, which its own version segment and the site root redirect to. The sidebar's version dropdown switches between them. Every version is rendered by the current `website/` over that version's own pages and source, so a fix to the site reaches every version the next time it is published.

### Developer Reference

The Developer Reference is generated from the docblocks in the code, never written by hand and never committed: `docs/reference` is gitignored, and `npm run dev` and `npm run build` generate it fresh, so the published site always matches the source that built it. `npm run generate:api` regenerates it on its own, which is also how a docblock or signature change is picked up while the dev server is running.

The Python modules of `src/selkies` come from their Google-style docstrings and type hints through [fumadocs-python](https://www.fumadocs.dev/docs/integrations/python). That half needs `python3`: the generator bootstraps a private venv in `website/.venv-docs` on first use (delete that directory to rebuild it, e.g. after upgrading the `fumadocs-python` npm package).

The web client core and the two dashboards come from their JSDoc blocks and type annotations through [TypeDoc](https://typedoc.org), which reads the plain JavaScript of `addons/selkies-web-core` and `addons/selkies-dashboard` through the TypeScript compiler's JSDoc support and the TypeScript of `addons/selkies-dashboard-wish` natively, one section per addon. That half needs nothing beyond the site's own `npm install`: the addons' third-party types are not installed and render as `any`, and type errors never fail the build. Every function that carries a docblock appears, exported or not — the streaming cores keep their machinery as closures inside one exported function, and the dashboards keep their handlers inside their components — so the reference covers what the docblocks cover, as it does for Python. `website/scripts/generate-web-docs.mjs` lists what is excluded (translation tables, the wish dashboard's vendored shadcn/ui primitives, the built core copied into the dashboard at build time), and `_`-prefixed members are private and left out.

The comment conventions each language follows are in [`AGENTS.md`](https://github.com/selkies-project/selkies/tree/main/AGENTS.md).

### Settings Reference

[`docs/settings.md`](settings.md) is rendered from the setting definitions in `src/selkies/settings.py` by `scripts/generate-settings-doc.py`, which reads the definition list statically and needs nothing installed. Unlike the Developer Reference it is committed, so it reads on GitHub as well: after changing a setting, run `python3 scripts/generate-settings-doc.py` and commit the page with the change. The `settings-doc` pre-commit hook (and the CI lint gate, through it) runs the script with `--check` and fails when the page is stale.

## Container Customization

The reference images use the [s6 supervision suite](https://skarnet.org/software/s6/) as their service supervisor and a PID-agnostic entrypoint, and a desktop of your own is built on them by replacing the entrypoint or adding s6 services alone. [Base Container](components/base-image.md) describes how the images start, how they are laid out, how a checkout is run inside them, and the package-layer bracket their rootless build needs; [KDE Plasma Desktops](components/kde-images.md) describes the two desktop repositories built that way and what they share.

## Style Guide

- Shell scripts and Dockerfiles should use POSIX `sh` syntax as much as possible. Despite the shell scripts being run in `bash`, avoid using syntax only available in `bash` (such as `[[ ]]`), `zsh`, or other types of shells, unless absolutely needed. If non-POSIX syntax is used, prefer using `bash` syntax, but only if there are no equivalent POSIX alternatives.

- For Python, [Ruff](https://github.com/astral-sh/ruff) with Black formatting or [Black](https://github.com/psf/black) formatting are recommended. For JavaScript, HTML, CSS, Markdown, YAML, and other files, [Prettier](https://github.com/prettier/prettier) formatting is recommended. For code that is not already formatted in these formats, use the formatters with your Pull Requests if possible.

- There should be no empty lines with whitespaces, or line endings with whitespaces. Moreover, there should be a line break at the end of each code file unless the specific code file format should not have one. If there is not, it is okay, but include the line break with your Pull Requests if possible.

- Try using [`codespell`](https://github.com/codespell-project/codespell) or any other code spelling checker including the Visual Studio Code [Code Spell Checker](https://marketplace.visualstudio.com/items?itemName=streetsidesoftware.code-spell-checker), that can check spelling errors in the codebase before finalizing your pull request. Note that some fixes may be false positives, so please check the fixes manually (most notable false positives include `/dev/dri/renderD`).

## Code Guide

- **You need to understand the whole codebase fully before contributing developments.** When editing certain parts of the codebase, they are very likely to interact with other components in a very different location, or the same content needs to be edited in multiple different locations. Therefore, Commits or Pull Requests are very likely to corrupt the repository **UNLESS** you use rigorous search capabilities across the whole codebase as often as possible. Check previous commits as a starting point for the files that tend to be edited together.

- Because of this, use the Visual Studio Code (or any other IDE of choice) **Search and Replace** capabilities rigorously (especially with fine-tuning through case-sensitive search and regular expressions). However, the replacement capability, without adequate care, may replace totally unrelated code. Take great care while using this capability, and reviewers must take special attention to detect potentially breaking typos which may arise from Search and Replace.

- **Write or edit code in relevant files and reference them so that the code style is kept consistent.** For instance, many handler methods that start with `on_` are initially unset, then set and referenced in other components or classes during initialization. If you are implementing a new capability on certain methods or handlers that use methods starting with `on_` frequently, you have to create new `on_` methods as well to handle your capability. This assists with keeping the code highly readable, and putting methods or functions in the wrong files will harm the consistency of the code style. **If you are starting to feel that the location you are writing code in does not blend properly into adjacent code, you are probably writing it in the wrong place!**

- For example, assume that we are writing a new component that receives WebRTC Metrics from the web interface and writes them into multiple CSV files in the host ([#141](https://github.com/selkies-project/selkies/pull/141)). Because a data-channel (WebRTC) or WebSocket message carries the metrics, receiving them is handled in [`input_handler.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/input_handler.py). But this does not mean that everything should be implemented in this file. Instead, they should be implemented in the `Metrics` class of [`webrtc_utils.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/webrtc_utils.py), and be initialized in [`webrtc_mode.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/webrtc_mode.py). This way, relevant code stays in appropriate files and is initialized only when the capabilities are needed.


## Tests

[`tests/`](https://github.com/selkies-project/selkies/tree/main/tests) holds the suites, grouped into tiers by what they need to run. Each one is a standalone program printing a `PASS`/`FAIL` line per check, and `pytest` runs the same suites through a marker per tier:

```bash
pytest tests -m unit                    # the source tree only
pytest tests -m integration             # a display, PulseAudio, an installed selkies
pytest tests -m e2e                     # the above plus Playwright browsers
python3 tests/e2e/test_matrix.py wr-wl  # or one suite, one block, on its own
```

The `integration` tier drives the server over a raw WebSocket or the kernel gamepad backend; the `e2e` tier drives real browsers across `{websockets, webrtc} x {X11, Wayland}`, which is where the transport and backend parity is actually held to account. The `perf` and `soak` tiers run on request. [`tests/README.md`](https://github.com/selkies-project/selkies/tree/main/tests/README.md) documents the environment variables, the `/dev/uinput` emulator that lets the kernel gamepad path run on a machine without one, and the packaging simulator that exercises `infra/packaging/*.sh` with no container runtime.

## Agentic Development

Much of this tree is written and reviewed with coding agents, and the repository is laid out so an agent validates its own work rather than describes it. [`AGENTS.md`](https://github.com/selkies-project/selkies/tree/main/AGENTS.md) (`CLAUDE.md` links to it) is the instruction file every agent reads first: the comment and documentation conventions, the engineering priorities, and the cross-cutting invariants no single module reveals. It is the one file an agent cannot derive from the code, so keep it current as you change what it describes.

The three core repositories each carry one, and they are written to be read together:

| Repository | What its `AGENTS.md` holds |
| --- | --- |
| [selkies](https://github.com/selkies-project/selkies/tree/main/AGENTS.md) | The priority order a change is judged by (latency, unrestricted frame rate, zero-copy, CPU, the GIL), the parity that has to hold between X11 and Wayland and between WebSockets and WebRTC, the paired constants that move together (the codec ladder on the server and in the client, the settings and their documentation), the test tiers and the environment they need, and the commit and pull-request conventions |
| [pixelflux](https://github.com/selkies-project/pixelflux/tree/main/AGENTS.md) | The capture backends and encoder engines, the probes a session is built from and the ladder it falls through, the Python API Selkies drives and what a change to it has to keep, and the GPU tests that need the hardware |
| [pcmflux](https://github.com/selkies-project/pcmflux/tree/main/AGENTS.md) | The capture and Opus paths, the microphone and recording sockets, and the same conventions on its side |

A change in one repository often belongs in another, since Selkies pins the capture stack and its suites drive both extensions, so give an agent all three checkouts and have it land a change in each of them, on the same day, in the order the pins are read: `pcmflux` and `pixelflux` first, so that Selkies' own CI builds against the commits it names, then `selkies`.

An agent works in a sandbox it may break, never in the session it is shown on. The devcontainers under [`.devcontainer`](https://github.com/selkies-project/selkies/tree/main/.devcontainer) are that sandbox ready-made: `postcreate.sh` builds the web client, installs the package editable with the pinned capture stack and the test extras, then the Playwright engines, the OpenH264 plugin Firefox negotiates H.264 with, and the C helpers under `tests/tools`. On any other host the same comes from `pip install -e .[test]`, `python3 -m playwright install --with-deps chromium firefox webkit`, `sh tests/tools/fetch-openh264.sh`, `make -C tests/tools`, and the packages the `suites` job of [`tests.yaml`](https://github.com/selkies-project/selkies/tree/main/.github/workflows/tests.yaml) installs; Miniforge serves a host whose package manager is closed, with the system `libgbm.so` kept for GBM on NVIDIA. [`scripts/ci/test-stack.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/test-stack.sh) then starts what the suites stream from, an `Xvfb` on `E2E_DISPLAY` (`:99` unless set) with the canvas the containers use and a PulseAudio null sink, leaving both alone when they are up already; CI starts its display with the same script. That display is never the one a desktop runs on, because the suites inject input and resize the root window, which is why `E2E_DISPLAY` is never taken from `DISPLAY`. The Wayland suites nest their own `labwc`, patched as [`addons/base/build-labwc.sh`](https://github.com/selkies-project/selkies/tree/main/addons/base/build-labwc.sh) builds it; a stock one makes them skip, and CI counts a skip as a failure (`tests/tools/assert_no_skips.py`). GPU paths need the hardware: pixelflux's `cargo test gpu_ -- --ignored --nocapture --test-threads=1` on a machine with the NVIDIA driver, and a render node for its dmabuf checks. A change to pixelflux or pcmflux reaches this sandbox as a wheel (`pip wheel . --no-deps` in that checkout) installed into the same environment, and the suites here then drive it over both transports on X11 and Wayland.

The loop an agent runs is the one CI runs, from the cheap end: `pre-commit run --all-files` for the lint gate, `pytest tests -m unit` on every change, then the `integration` tier and the `e2e` blocks the change touches (`python3 tests/e2e/test_matrix.py wr-wl` and its siblings, each a standalone program), and a measurement wherever a claim is a number, since the suites hold behavior and the numbers are what the priority order in `AGENTS.md` is decided on: a latency, a frame rate, a bitrate, before and after. A parity change runs on both sides it claims to unify, X11 and Wayland, WebSockets and WebRTC, the default and the wish dashboards. A failure the agent cannot reproduce is narrowed until it is fixed or precisely described, never waved through, and a pre-existing defect it finds is in scope.

What an agent hands back is what a reviewer can check: one commit per concern under a one-line `type: Sentence` subject, the contributor's own identity on it, no generated bundles or build artifacts, the translations updated with every user-facing string, docblocks describing the tree as it now is rather than the change, and the suites and measurements it ran named in the pull request.

## Continuous Integration

Every workflow lives under [`.github/workflows`](https://github.com/selkies-project/selkies/tree/main/.github/workflows):

- `ci.yaml` orchestrates pushes to `main` and every pull request. It lints (Ruff, codespell, actionlint, and ESLint plus TypeScript over the dashboards), byte-compiles the package on Python 3.9 through 3.15, then builds the wheel once and hands that single artifact to the image and package builds.
- `build-wheel.yaml` bundles the web client with [`scripts/ci/build-web.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/build-web.sh) and builds the wheel, which every image and package build starts from. `smoke-wheel.yaml` installs it into a clean virtualenv on the newest and the oldest supported interpreter and runs the console script, against the capture stack the run resolved or, when a release asks for it, PyPI's copy of the same versions.
- `build-pixelflux-pcmflux-wheels.yaml` puts pixelflux and pcmflux into the run's artifacts, so images, packages, AppImages, and the suites ride wheels the run holds rather than an index. Its locate job names one release of each project -- the per-commit pre-release of their `main` HEAD, or, for a release of this repository, the release carrying the version `pyproject.toml` pins -- and a fetch job downloads it, one job per artifact the build would otherwise produce, so only what no release covers is built here. Everything that needs the capture stack reads those artifacts, so a run resolves it once.
- `images.yaml` publishes the multi-architecture `base`, `desktop`, `coturn`, and `turn-rest` images to ghcr.io. The base image's own build runs the console script, imports both extension modules and loads the libraries the capture stack opens by name, so an image whose payload cannot start is never pushed; no suite here runs inside an image. Each architecture builds on its own native runner and pushes by digest; a merge job assembles the manifest, so no QEMU is involved. `base` and `desktop` are built in the same job, in that order, so the desktop layer names the base it was just given by digest rather than a tag that does not exist yet. A final job prunes the images no tag points at any more, which is what every push leaves behind when it moves the `main-*` tags onto new manifests. Beside each digest, the build job records what the images were built from and hold: the distribution image by digest, read from the provenance the push attached, and each image's package list, read out of the builder's cache; a release attaches those files.
- `packages.yaml` builds `.deb`, `.rpm`, `.apk`, `.pkg.tar.zst`, and both AppImages, each inside a container of the target distribution. Every native package carries the Input Interposer, which `infra/packaging/interposer.sh` compiles into the package root. The AppImage's conda environment is solved for glibc 2.28, the floor its wheels and interposers are built for, and the build fails on any file it carries that needs a newer glibc. Each AppImage is then run by [`scripts/ci/verify-appimage.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/verify-appimage.sh) with the build's AppDir gone: an entry point or a bundled sound server that names the path the conda prefix was installed to still resolves while that tree is there, and works nowhere else.
- `tests.yaml` runs the suites in [`tests/`](https://github.com/selkies-project/selkies/tree/main/tests). `ci.yaml` calls it for the `unit` and `integration` tiers on every push and pull request, and it runs the browser tier nightly and on demand. A dispatched run takes a `tiers` marker expression and a `suites` name expression (`pytest -k`), so a few suites can be re-run on the runner without the two-hour matrix.
- `release.yaml` is the maintainer entry point described below, and `release-published.yaml` applies its `latest` choice to the badge and its `pypi` choice to the index once the draft is published, and rebuilds this site around the release; `docs.yaml` publishes this site; `devcontainer-feature.yaml` validates and publishes the devcontainer feature.

Ruff's rule selection lives in `pyproject.toml`, codespell's exceptions in `.codespellrc`, and each dashboard's ESLint rules in its own `eslint.config.js`, so `ruff check`, `codespell`, and [`scripts/ci/lint-web.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/lint-web.sh) from the repository root reproduce the CI lint exactly.

Shell is linted by `shellcheck`, which actionlint runs over every workflow `run:` block and the `unit` tier runs over every script the tree ships. Which findings it reports at that severity differs between releases, so both jobs install the pinned build with [`scripts/ci/install-shellcheck.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/install-shellcheck.sh); run it with a writable directory (`scripts/ci/install-shellcheck.sh ~/.local/bin`) to lint against the same one locally.

The same three run as a pre-commit hook, which is the easier way to stay ahead of the lint gate:

```bash
pip install pre-commit
pre-commit install          # once per clone
pre-commit run --all-files  # or check the whole tree on demand
```

# Maintainer Documentation

- New releases are published by going to the [Release](https://github.com/selkies-project/selkies/actions/workflows/release.yaml) GitHub Action Workflow, and triggering `workflow_dispatch` by clicking on `Run workflow` with `Branch: main`, and specifying the release tag. The tag is a PEP 440 version without the leading `v`, either a release such as `1.2.3` or a pre-release such as `2.0.0rc0`; a pre-release is marked as one on the GitHub release. The `latest` input decides whether the release is designated as the latest everywhere, the floating `latest` image tags and the "Latest" badge of the published release together: `auto`, the default, designates a release and not a pre-release, `never` holds both back for a hotfix of an older series, and `always` designates a pre-release as well. GitHub gives the badge to no release marked as a pre-release, so a designated pre-release is marked as a release. The `pypi` input decides whether the wheel and the sdist go to PyPI once the draft is published: `auto`, the default, publishes a release designated as the latest and no pre-release, `never` leaves the index as it is, and `always` publishes a pre-release or a hotfix of an older series as well. The upload is PyPI's trusted publishing, registered on the project for this repository's `release-published.yaml` workflow and its `pypi` environment, so no token lives in the repository; PyPI keeps the first upload of a version, so a version it holds cannot be released there again. The `capture_stack` input decides where the pinned pixelflux and pcmflux come from: `releases`, the default, takes the wheels each project released for the version `pyproject.toml` pins, so a release can be cut with neither project on PyPI, and `pypi` resolves them from there instead, which is what proves the path a user installing the wheel takes. The pin has to name what exists either way, a pre-release included: pip takes no pre-release for a pin that does not name one, so an rc capture stack is pinned as `pixelflux~=2.1.0rc1`. The draft release for the new proposed release will be generated in the [Releases](https://github.com/selkies-project/selkies/releases) page, only visible to the maintainers. After waiting for the release build to finish, editing the release notes, and publishing the release, the `release-published.yaml` workflow puts the badge where the run decided, which the notes carry as an invisible marker, whatever the publish form's checkbox said, publishes the wheel and the sdist the release carries to PyPI when the run decided so, and rebuilds this site, which lists the release and points `latest` at the badge. **If the same release is created multiple times because of certain issues, make sure to delete the previous release and the tag before running the [Release](https://github.com/selkies-project/selkies/actions/workflows/release.yaml) GitHub Action Workflow again.**
