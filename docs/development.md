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

**No programming experience:** You can still be a tester or a community helper/moderator at [Discord](https://discord.gg/wDNGDeSW5F)! Do you see anything that feels uncomfortable compared to other projects? Raise an issue and suggest various improvements including to the documentation. Have you used OBS, FFmpeg, or any other live streaming/video editing software before? You can suggest optimized parameters for the video encoders from your experiences. You can experiment with various encoder and streaming parameters, which are exposed in a very accessible way in [`settings.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/settings.py) (the roughly 120 command-line and environment settings) and applied in [`media_pipeline.py`](https://github.com/selkies-project/selkies/tree/main/src/selkies/media_pipeline.py), improving streaming performance.

**Some Python or HTML/JavaScript frontend experience:** Our codebase and web interface always has room for improvement. Consider helping out on various issues or cleaning up the code otherwise.

**Linux X11/Wayland/Container/Conda experience:** Please report issues with the capture interface and provide improvements for our reference containers. If you have the capacity to maintain conda-forge feedstocks, please add yourself as a maintainer and contribute new feedstocks. A protocol and interface can never be great without a great environment it runs in. If you want to bring Selkies to MacOSX or Windows, check our issues!

**C/Rust experience:** Selkies delegates its media encoding to the `pixelflux` (screen capture with H.264/JPEG encoding) and `pcmflux` (PulseAudio capture with Opus encoding) Rust extensions, and its opt-in WebRTC transport to a vendored fork of `aiortc`. We need you to fix bugs and implement new capabilities in these components or any other upstream dependencies. This will not only benefit Selkies but also help the broader communities around those projects.

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

[Jan Van Bruggen](https://github.com/JanCVanB): Project Co-Founder, ex-Google, ex-NASA, ex-itopia, current Verily

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

- Upstream projects behind the current media stack: [`aiortc`](https://github.com/aiortc/aiortc) (the WebRTC transport is a vendored fork), `pixelflux` (screen capture with H.264/JPEG encoding), and `pcmflux` (PulseAudio capture with Opus encoding)

- WebRTC for the Curious: <https://webrtcforthecurious.com>

- WebRTC Official Google Groups: <https://groups.google.com/g/discuss-webrtc>

- Mozilla MDN: <https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API>

- WebRTC Hacks: <https://webrtchacks.com>

## Local Builds

[`docker-compose.yml`](https://github.com/selkies-project/selkies/tree/main/docker-compose.yml) builds and runs everything this repository produces, with Compose V2:

```bash
docker compose build dist                    # the wheel, web client included
docker compose up example                    # the Example Container on http://localhost:8080
docker compose --profile gpu up example-gpu  # the same container with a GPU attached
```

`example` bind-mounts `src/selkies` over the installed package, so server-side edits take effect on a restart rather than a rebuild. The base image, the streaming mode, the port, and the TURN credentials come from the environment (`DISTRIB_IMAGE`, `DISTRIB_RELEASE`, `SELKIES_MODE`, `SELKIES_PORT`, `SELKIES_TURN_*`); a `.env` file next to the Compose file is the usual place for them. To run a wheel built from this tree instead of the latest PyPI release, copy it out of the `selkies-py-build` image into `addons/example/wheels/` before building.

## Documentation

The pages under [`docs/`](https://github.com/selkies-project/selkies/tree/main/docs) are plain Markdown and are what <https://selkies-project.github.io/selkies> publishes. Editing one through GitHub's web editor is enough to change the site: either the pencil icon in the repository, or the **Edit on GitHub** button beside every page, opens the file it was built from.

A page begins with front matter naming it:

```yaml
---
title: Getting Started
description: One sentence, shown under the title and in search results.
---
```

[`docs/meta.json`](https://github.com/selkies-project/selkies/tree/main/docs/meta.json) lists the pages in sidebar order, and a new page has to be added to it to appear there.

Links between pages are written the way GitHub resolves them (`start.md`, or `component.md#encoders`) and are rewritten to site URLs during the build. Images live in `docs/assets` and are referenced relative to the file.

### Logo assets

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

`npm run build` writes the site to `website/out`, which is what the `Docs` workflow uploads, and `npm run check-links` fails on any link or anchor in that output that does not resolve.

## Container Customization

The reference container images (the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) and the LXQt desktop images built by CI) use the [s6 supervision suite](https://skarnet.org/software/s6/) as their service supervisor, installed from the distribution's package registry (`s6` package): `s6-svscan /etc/service` starts one `s6-supervise` per service directory — the X11 display server (or the headless Wayland compositor), the desktop session, the audio stack (`pipewire`/`wireplumber`/`pipewire-pulse`), `dbus`, and `selkies`, restarting any that crash. Services are controlled with `s6-svc` and inspected with `s6-svstat`, the `supervisord`/`supervisorctl` equivalents, without any Python dependency (`s6-overlay` is deliberately NOT used: it insists on being PID 1, while plain `s6-svscan` works both as PID 1 and below any foreign init or launcher).

**If you want to change the image behavior, use the original container as a base image and only replace the entrypoint script(s) and/or the s6 service files. This will keep you up to date with the latest updates. Use persistent container tags (such as `v1.0.0-ubuntu26.04` for the [Example Container](component.md#example-container)) to preserve a specific container build.**

Start with the below sample `Dockerfile` example and place your modified `container-entrypoint.sh` and s6 service files within the same directory or Git repository (switch the `FROM` line to `ghcr.io/selkies-project/selkies/example:main-${DISTRIB_RELEASE}` for the [Example Container](component.md#example-container), and `ghcr.io/selkies-project/nvidia-glx-desktop:${DISTRIB_RELEASE}` or `ghcr.io/selkies-project/nvidia-egl-desktop:${DISTRIB_RELEASE}` for the desktop containers):

```dockerfile
ARG DISTRIB_RELEASE=ubuntu26.04
FROM ghcr.io/selkies-project/selkies/example:main-${DISTRIB_RELEASE}
ARG DISTRIB_RELEASE

USER 0
SHELL ["/bin/sh", "-c"]

# Replace changed files
# Copy scripts and service definitions used to start the container with `--chown=1000:1000`
#COPY --chown=1000:1000 container-entrypoint.sh /etc/container-entrypoint.sh
#RUN chmod -f 755 /etc/container-entrypoint.sh
#COPY --chown=1000:1000 selkies-entrypoint.sh /etc/selkies-entrypoint.sh
#RUN chmod -f 755 /etc/selkies-entrypoint.sh
# Replace or add s6 services (one directory per service under /etc/service)
#COPY --chown=1000:1000 services/ /etc/service/
#RUN find /etc/service -name run -exec chmod -f 755 {} +

USER 1000
ENV SHELL=/bin/bash
ENV USER=ubuntu
ENV HOME=/home/ubuntu
WORKDIR /home/ubuntu

EXPOSE 8080

ENTRYPOINT ["/etc/container-entrypoint.sh"]
```

The entrypoint script of the base images launches `s6-svscan /etc/service` itself, so it does not need to be PID 1 and the image keeps working when another init or launcher is injected above it.

## Container Guide

The [`docker-nvidia-glx-desktop`](https://github.com/selkies-project/docker-nvidia-glx-desktop)/[`docker-nvidia-egl-desktop`](https://github.com/selkies-project/docker-nvidia-egl-desktop) desktop container repositories (referenced as Desktop Containers here), and the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) share various components between each other:

`LICENSE`, the entrypoint scripts (`entrypoint.sh` / `container-entrypoint.sh`, `selkies-entrypoint.sh`), and the s6 service definitions under `/etc/service` are always identical in both Desktop Containers (copy and paste between each container). As these components are also very similar to the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example), **you need to do three Pull Requests including the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) if relevant lines changed in the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example), and at least two Pull Requests for both Desktop Containers.**

The `Dockerfile` is always identical below and above the lines that say `Anything above/below this line should always be kept the same...` in both Desktop Containers. This component is not shared with the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example), and installation procedures for Selkies should be updated to the desktop containers on every release, so **you need to do three Pull Requests including the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) if relevant lines changed in the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example), and at least two Pull Requests for both Desktop Containers.**

The `entrypoint.sh` components are always identical from the start until the line containing `export PULSE_SERVER=..."` in both Desktop Containers. The script for installing NVIDIA userspace driver components are always identical except for the outermost `if` condition. Other script sections require manual assessment when updating, so **you need to do three Pull Requests including the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example) if relevant lines changed in both Desktop Containers and the [Example Container](https://github.com/selkies-project/selkies/tree/main/addons/example).**

`README.md` and `egl.yml`/`xgl.yml` files in both Desktop Containers are similar but have different components, thus requiring manual assessment for both Desktop Containers when updating.

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

- Some code components have `CAPITALIZED_COMMENT:` comment sections such as `OPUS_FRAME:`. These sections indicate that locations with the `CAPITALIZED_COMMENT:` must be edited or added simultaneously.

## Tests

[`tests/`](https://github.com/selkies-project/selkies/tree/main/tests) holds the suites, grouped into tiers by what they need to run. Each one is a standalone program printing a `PASS`/`FAIL` line per check, and `pytest` runs the same suites through a marker per tier:

```bash
pytest tests -m unit                    # the source tree only
pytest tests -m integration             # a display, PulseAudio, an installed selkies
pytest tests -m e2e                     # the above plus Playwright browsers
python3 tests/e2e/test_matrix.py wr-wl  # or one suite, one block, on its own
```

The `integration` tier drives the server over a raw WebSocket or the kernel gamepad backend; the `e2e` tier drives real browsers across `{websockets, webrtc} x {X11, Wayland}`, which is where the transport and backend parity is actually held to account. The `perf` and `soak` tiers run on request. [`tests/README.md`](https://github.com/selkies-project/selkies/tree/main/tests/README.md) documents the environment variables, the `/dev/uinput` emulator that lets the kernel gamepad path run on a machine without one, and the packaging simulator that exercises `infra/packaging/*.sh` with no container runtime.

## Continuous Integration

Every workflow lives under [`.github/workflows`](https://github.com/selkies-project/selkies/tree/main/.github/workflows):

- `ci.yaml` orchestrates pushes to `main` and every pull request. It lints (Ruff, codespell, actionlint, and ESLint plus TypeScript over the dashboards), byte-compiles the package on Python 3.9 through 3.14, then builds the wheel once and hands that single artifact to the image and package builds.
- `build-wheel.yaml` bundles the web client with [`scripts/ci/build-web.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/build-web.sh), builds the wheel, and smoke-tests it in a clean virtual environment.
- `build-pixelflux-pcmflux-wheels.yaml` builds pixelflux and pcmflux from their upstream `master` so images, packages, and AppImages ride the latest capture and audio code instead of the last PyPI release.
- `images.yaml` publishes the multi-architecture `example`, `coturn`, and `turn-rest` images to ghcr.io. Each architecture builds on its own native runner and pushes by digest; a merge job assembles the manifest, so no QEMU is involved.
- `packages.yaml` builds `.deb`, `.rpm`, `.apk`, `.pkg.tar.zst`, and both AppImages, each inside a container of the target distribution. Every native package carries the Joystick Interposer, which `infra/packaging/interposer.sh` compiles into the package root.
- `tests.yaml` runs the suites in [`tests/`](https://github.com/selkies-project/selkies/tree/main/tests). `ci.yaml` calls it for the `unit` and `integration` tiers on every push and pull request, and it runs the browser tier nightly and on demand.
- `release.yaml` is the maintainer entry point described below; `docs.yaml` publishes this site; `devcontainer-feature.yaml` validates and publishes the devcontainer feature.

Ruff's rule selection lives in `pyproject.toml`, codespell's exceptions in `.codespellrc`, and each dashboard's ESLint rules in its own `eslint.config.js`, so `ruff check`, `codespell`, and [`scripts/ci/lint-web.sh`](https://github.com/selkies-project/selkies/tree/main/scripts/ci/lint-web.sh) from the repository root reproduce the CI lint exactly.

The same three run as a pre-commit hook, which is the easier way to stay ahead of the lint gate:

```bash
pip install pre-commit
pre-commit install          # once per clone
pre-commit run --all-files  # or check the whole tree on demand
```

# Maintainer Documentation

- New releases are published by going to the [Release](https://github.com/selkies-project/selkies/actions/workflows/release.yaml) GitHub Action Workflow, and triggering `workflow_dispatch` by clicking on `Run workflow` with `Branch: main`, and specifying the release tag. The tag is a PEP 440 version without the leading `v`, either a release such as `1.2.3` or a pre-release such as `2.0.0rc0`; a pre-release is marked as one on the GitHub release and leaves the floating `latest` image tags on the last full release. The `latest_tags` input decides that on its own when a run needs it to: `always` and `never` move or hold the `latest` tags whatever the tag says, and `auto` is the behavior just described. The draft release for the new proposed release will be generated in the [Releases](https://github.com/selkies-project/selkies/releases) page, only visible to the maintainers. After waiting for the release build to finish, editing the release notes, and publishing the release, the release will be visible as the latest release. **If the same release is created multiple times because of certain issues, make sure to delete the previous release and the tag before running the [Release](https://github.com/selkies-project/selkies/actions/workflows/release.yaml) GitHub Action Workflow again.**
