# Tests

Every suite here is a standalone program. It prints one `PASS`/`FAIL` line per
check, a count at the end, and exits non-zero if anything failed:

```bash
python3 tests/e2e/test_matrix.py ws-x11
```

`tests/test_suites.py` runs each of them as a pytest case, so the same suites
also work as:

```bash
pytest tests -m unit                      # source tree only
pytest tests -m "integration or e2e"      # needs a display and browsers
pytest tests -m image                     # a published image, see Image tier
pytest tests -k gamepad -v
```

`tests/suites.py` is the registry both entry points read: it lists every suite,
its tier, its selectors, and its timeout. Add a suite there and it appears in
both.

The suites' extra dependencies are the `test` optional-dependency group in
`pyproject.toml`: `pip install .[test]` (or `selkies[test]`) on top of an
installed `selkies`; the browsers themselves come from `playwright install
chromium firefox webkit`. The unit tier alone runs without the capture stack:
`pip install pytest -r <(python scripts/ci/unit-deps.py)` installs the
runtime dependencies minus pixelflux and pcmflux, which is what CI does on
every interpreter the package supports.

## Tiers

| Tier | Needs |
| --- | --- |
| `unit` | The source tree and `gcc` for `tests/tools`. The audits of the web client (translations, typing, pointer lock, relative motion) also want `node`, and report themselves skipped without it. The example session scripts are parsed with `bash -n` (their Python helpers byte-compiled), and every shell script the tree ships is linted at `shellcheck`'s lowest severity where it is installed. Which findings that severity reports differs between `shellcheck` releases, so CI pins the version rather than taking the runner image's; a distro build may disagree with it in either direction. The rate-control and paint-over defaults, the one-shot NVML probe, the clipboard-paste typing route (for compositors without `zwp_virtual_keyboard`, such as KWin), the clipboard ladder's back-off around a dead X display, and the input interposer's hooks under a signal handler that calls them (built off the tree with `gcc`) are covered here too. |
| `integration` | An X display named by `E2E_DISPLAY` or the Wayland backend, PulseAudio, and `selkies` importable with `pixelflux`/`pcmflux`. Suites needing a server of their own (the keymap, connection-leak, and session-DPI checks) start a throwaway `Xvfb` on a free display number instead, and need nothing set. The output-layout suite wants the Xvfb the images ship, XLibre's patched to offer pluggable outputs, which `addons/base/build-xvfb.sh` builds (the CI job does exactly that); on a stock one it skips. The XWayland typing check nests `labwc` (which brings `Xwayland`) on a private socket and reads the keys back out of `xev` from `x11-utils`, and skips without them. The Wayland seam and session-screen suites want the labwc the images ship — patched with the control socket and the seam cursor path — which `addons/base/build-labwc.sh` builds (the CI job does exactly that); on a stock labwc the seam suite skips its held-grab checks and the session-screen suite skips whole. The packaging simulation needs neither, only the source tree and a `python3` that can build a virtualenv. |
| `e2e` | The above plus Playwright browsers (WebKit on Linux also needs GStreamer's `opengl` plugin, `gstreamer1.0-gl`, which `playwright install --with-deps webkit` brings; without it WebKit paints every `<video>` through a software fallback sink, so the harness refuses to launch it), the built web client (`scripts/ci/build-web.sh`), `wl-clipboard` for the Wayland clipboard checks, `wmctrl` for the two-display desktop-window check (skipped with a notice when absent), `tkinter` for the sender-lag recovery suite's scene painter (skipped without it), `tests/tools/fetch-openh264.sh` for the Firefox WebRTC blocks, and a coturn `turnserver` on `PATH` (or named by `E2E_TURNSERVER`) for the TURN relay suite, which skips with the install command when there is none. The pointer-motion suite drives the *installed* Chrome and Firefox on the test display with XTEST, and skips when neither is on `PATH`. The Wayland typed-text suite types into Chrome and Firefox running on pixelflux's compositor and, through pixelflux's virtual keyboard, on a headless `labwc`, whose half it skips without one. The pointer-lock suite's game-view block needs `libsdl2-2.0-0` (skipped without it), and its desktop selectors need `openbox`, `kwin-x11`, and `kwin-wayland` plus `dbus-run-session`, and the `labwc` the Wayland suites already use (each skipped with the missing binary named). The gaming-mode suite drives the installed Chrome, windowed under `openbox` on a private X server, and skips without either. |
| `perf` | A long constrained-link pacer benchmark, plus `xterm` and `xdotool` for the screen-damage load generator. Run on request. |
| `image` | A published desktop image and somewhere to run it (Kubernetes, Docker, or a session already up), installed Chrome, release Firefox, Playwright's WebKit, `xdotool` and `xclip` on the client display. Run on request; see [Image tier](#image-tier). |
| `soak` | The whole `pixelflux`/`pcmflux` API surface, including recording and Wayland. Run on request. |

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `E2E_DISPLAY` | none; required | X display the server streams from. Deliberately not defaulted and never inherited from `DISPLAY`: the suites inject input and resize the root window, so pointing them at a real session damages it. Provision a throwaway server (`Xvfb :N -screen 0 8192x4096x24 -noreset`) and name it here. |
| `E2E_PORT` | a free port | Server port; everything the server exposes, `/api/metrics` included, is on it. Left unset each suite process takes its own, so runs do not have to be serialized. Set it when something in front of the server needs a fixed one. |
| `E2E_WORKDIR` | `$TMPDIR/selkies-tests` | Server log, shim recordings, and other scratch. Run through `pytest`, each suite's logs are also copied into `suite-logs/<suite>/` under it, which CI uploads. Its `run/` is the `XDG_RUNTIME_DIR` every compositor, observer, and client the suites start is given, so their sockets never land in a desktop session's own. |
| `SELKIES_TEST_PYTHON` | the interpreter running the tests | Interpreter the server under test runs on. |
| `E2E_PAGE_CALL_TIMEOUT` | `60` | Seconds a browser is given to answer a call that carries no deadline of its own (`evaluate`, a close). An engine whose renderer wedges would otherwise hold a suite to the runner's kill; past the bound the call raises and the suite reports, printing the page's last console lines and every WebKit thread's stack (`gdb`, through `sudo -n` where Yama needs it; kept in `browser-stall-<pid>.log` in the work directory too). 0 turns it off. |
| `E2E_CHROME` | unset | System Chrome/Chromium binary. Unset uses Playwright's bundled Chromium. |
| `E2E_FIREFOX_PROFILE` | `$E2E_WORKDIR/firefox-profile` | Persistent Firefox profile; clipboard permission does not survive a fresh one, and `tests/tools/fetch-openh264.sh` seeds the OpenH264 plugin into it. Firefox negotiates no H.264 without that plugin, and the WebRTC block skips. |
| `E2E_TURN_REST_URI` | unset | TURN REST endpoint. WebRTC runs on host candidates alone without it. |
| `E2E_TURNSERVER` | `turnserver` on `PATH` | coturn binary the TURN relay suite starts on loopback (`apt install coturn` provides it). Without one that suite is skipped. |

## Tools

`make -C tests/tools` builds the helpers the suites need:

- **`v4l2probe`** — a libc-only V4L2 capture client (format enumeration,
  MMAP or `read()` streaming, pixel samples) that the webcam suites run under
  the V4L2 interposer or against a kernel device to see what an application
  sees.

- **`uinput_shim.so`** — an emulator for `/dev/uinput`, preloaded into the
  server. It decodes the setup ioctls against the real `<linux/uinput.h>` and
  writes the event stream to a file, so the kernel gamepad backend can be
  driven on a host that has no uinput node, which includes CI.
- **`uinput_abi_truth`** — prints the ioctl encodings, struct sizes, and offsets
  the kernel headers define. `unit/test_uinput_abi.py` compares them against the
  constants `selkies.input_handler` computes in pure Python.

`make -C tests/tools gamepad` builds the interposer-side inspection tools
(`jsread`, `sdlenum`, `sdlread`, `udevscan`), which need SDL2 and libudev. They
are for looking at what an application sees through the Input Interposer, and
are not part of any tier. `jsread` reads the joydev node directly, `sdlenum`
lists what SDL2 enumerates, `sdlread` opens one of those pads and prints the
button, axis, and hat events SDL2 delivers for it, and `udevscan` shows the
device list `libudev` reports. `tools/gamepad/gpserver.py` is the matching
server: it serves one pad on `$SELKIES_JS_SOCKET_PATH` and drives events into
it, so the readers have something to show without a browser.

`tools/wlobs.py` is the pywayland client the Wayland blocks use to prove that
input reached the compositor seat; `tools/sdl_relative_probe.py` is the SDL2
window in relative mouse mode that stands in for a game, reading locked motion
the way one does (XInput2 raw motion, the Wayland relative pointer) so a warp
cannot pass for a delta; `tools/tcp2unix.py` is the reverse proxy the
Unix-socket suite puts in front of a server with no TCP listener; and
`tools/ws_tap.py` is the one the ack-heartbeat suite puts in front of a server
to log what the client's socket worker sends, which a page-side hook never sees.

## Packaging

`tests/packaging/simulate.sh` runs `infra/packaging/*.sh` against a genuinely
read-only `/repo` with the root-only tools stubbed, on any host, with no
container runtime. It catches the staging, read-only-mount, and package-version
mistakes that otherwise only surface in the release job:

```bash
python3 -m build            # or drop a wheel in dist/
tests/packaging/simulate.sh
```

`packaging/test_packaging.py` is that script as a suite, in the `integration`
tier. It reports one check per packager and needs nothing prepared: it packages
a wheel from `WHEEL_DIR` or `dist/` when one is there and builds one into
`$E2E_WORKDIR/packaging-wheel` when not, reusing it afterwards. The packaging
scripts build a virtualenv, so it hands them the interpreter running the tests
rather than a distro `python3` that may lack `ensurepip`.

Those scripts also compile the Input Interposer into each package, including
a 32-bit variant wherever the compiler can produce one. A host without a
multilib toolchain can unpack one and point `MULTILIB_SYSROOT` at it, and the
simulation covers that branch instead of skipping it:

```bash
mkdir -p /tmp/multilib && cd /tmp/multilib
# The gcc runtime package is named for the distribution's compiler generation,
# so it is resolved rather than spelled out.
apt-get download libc6-dev-i386 libc6-i386 lib32gcc-s1 \
  "$(apt-cache search --names-only '^lib32gcc-[0-9]+-dev$' | sort -V | tail -1 | cut -d' ' -f1)"
for d in *.deb; do dpkg-deb -x "$d" root/; done
mkdir -p root/lib32 root/lib
ln -sf ../usr/lib32/libc.so.6 root/lib32/libc.so.6
ln -sf ../usr/lib32/ld-linux.so.2 root/lib/ld-linux.so.2
MULTILIB_SYSROOT=/tmp/multilib/root tests/packaging/simulate.sh
```

## Image tier

The `image` tier runs the maintainer's human-testing checklist (below) against
a published desktop image rather than this tree. `tests/image/run.py` brings
the image up by tag or digest once per capture backend (`SELKIES_WAYLAND`),
switches the streaming mode the way the dashboard's own switch does
(`POST /api/switch`), and runs every item in every engine: each cell is one
image x transport x backend x engine.

```bash
E2E_DISPLAY=:99 E2E_IMAGE_NAMESPACE=selkies \
E2E_IMAGES="trixie=ghcr.io/selkies-project/selkies/desktop:main-debiantrixie,resolute=ghcr.io/selkies-project/selkies/desktop:main-ubuntu26.04" \
  python3 tests/image/run.py
python3 tests/image/run.py --items 1,4 --engines chromium --backends x11 \
  egl=ghcr.io/selkies-project/selkies-egl-desktop@sha256:...
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `E2E_IMAGES` | none; the suite skips | `label=ref` pairs, comma-separated; positional arguments win. A tag is fine: the digest that ran is recorded. |
| `E2E_IMAGE_WHERE` | `kube` | `kube`: a pod in `E2E_IMAGE_NAMESPACE`, reached at its own address from inside the cluster (a node the runner cannot route to is left out and the pod tried elsewhere), or through `kubectl port-forward` with `E2E_IMAGE_REACH=forward` for a runner outside it, where WebRTC needs `SELKIES_TURN_*` in `E2E_IMAGE_ENV` (`E2E_IMAGE_KUBECTL` replaces the `kubectl` command). `docker` or `ssh:HOST`: a container here or on that host (`E2E_IMAGE_DOCKER_GPUS` is passed to `--gpus`). `url:https://HOST:PORT`: a session already running, with `E2E_IMAGE_EXEC` the prefix that runs `bash -c` inside it and `E2E_IMAGE_LOGS` the command printing its log. |
| `E2E_IMAGE_GPU` | `none` | `nvidia` (a GPU with NVENC, AV1-capable preferred), `nvidia-av1` (Ada or newer only), or `amd` (`amd.com/gpu`). The NVIDIA classes pick nodes by `nvidia.com/gpu.product`; `E2E_IMAGE_GPU_PRODUCTS` (comma-separated) replaces the list for a cluster that names its GPUs differently. |
| `E2E_IMAGE_PRIORITY_CLASS` | none | The pods' `priorityClassName`. A preempted pod is brought up again before the next cell. |
| `E2E_IMAGE_TRANSPORTS`, `E2E_IMAGE_BACKENDS`, `E2E_IMAGE_ENGINES`, `E2E_IMAGE_ITEMS` | all | `websockets,webrtc`; `x11,wayland`; `chromium,firefox,webkit`; `1-10`. |
| `E2E_IMAGE_ENV`, `E2E_IMAGE_LABELS`, `E2E_IMAGE_NAME` | none, none, `selkies-imagetest` | Extra container environment and pod labels (`K=V,...`), and the pod or container name prefix. |
| `E2E_IMAGE_DRIFT_SECONDS` | `120` | How long item 8 watches the shared view of a still screen for quality drift; tens of minutes for the long check. |
| `E2E_IMAGE_KEEP` | unset | `1` leaves each target running after its run. |

Targets are started with `SELKIES_ENABLE_BASIC_AUTH=false` and `CAP_SYS_PTRACE`
(the apps panel's runner needs it), each from a fresh pod or container: one of
the same name, a killed run's, is removed first. The client browsers run headed on
`E2E_DISPLAY`, so focus, the X clipboard, and pointer lock are real there:
Chrome's focus emulation is off on every page and the Firefox profile drops the
clipboard testing pref. Inside the session the tier serves a small tester site
on loopback (`tests/image/site`) and opens it in the session's own Chrome and
Firefox: a known pattern for the decoded-picture checks, a seeded texture for
quality, and testers for the microphone and webcam, gamepads, playback, and
text input, each reporting what it saw back through the target's shell. The
run ends with `$E2E_WORKDIR/image-tier/<run>/table.md` (a row per cell, a
column per item, `passed/checked`), `results.json` with every check and its
detail, the screenshots, and each target's server log.

| Item | Module | Checked |
| --- | --- | --- |
| 1 | `i01_encoders` | Every encoder the dashboard's menu offers, under CBR and CRF, on the decoded picture and the stream description; Turbo and paint-over on a still screen at the sliders' floor (frames after the stop, quality against no paint-over); 4:4:4 on one-pixel chroma; CPU encoding where a GPU encodes. |
| 2 | `i02_media_devices` | The top bar's microphone and webcam toggles with fake client devices, heard and seen by a tester in the session's Chrome and Firefox, and the server's virtual devices up. |
| 3 | `i03_displays` | `#display2` as a second monitor and a window moved onto it; force-aligned resolution, native cursor, antialiasing, presets with HiDPI turning itself off, manual size, UI scaling (Xft.dpi), and reset, on xrandr and the window's own size. |
| 4 | `i04_clipboard` | 2.5 MB of text and multi-MB PNGs both ways between the session's and the client's X clipboards, with real focus; Upload Image through the panel's file chooser. |
| 5 | `i05_transfers` | 150 MiB up through Upload Files and down through the Download Files link, byte for byte, timed against a plain TCP stream over the same link. |
| 6 | `i06_apps` | `SELKIES_COMMAND_ENABLED` first; then an app installed and removed through the Apps panel. |
| 7 | `i07_gamepad` | A virtual pad and the touch gamepad: every button and four diagonals per stick, read by testers in the session's Chrome and Firefox. |
| 8 | `i08_shared` | `#shared` gets the screen and sound and nothing else (pointer, keys, clipboard, uploads, gamepad); its picture holds its quality; `#player2`-`#player4` drive pads. |
| 9 | `i09_shortcuts` | Ctrl+Shift+M, G, F, X and Left Click, pointer lock alone and in gaming mode's fullscreen, and shortcuts off. |
| 10 | `i10_media_ime` | A clip playing in the session's Chrome and Firefox, its sound and motion reaching the client; Korean composed through an IME on the client into a session text field. |

What WebKit cannot stand in for: Playwright's WebKit is WebKitGTK on Linux, not
Safari. Its media stack is GStreamer rather than VideoToolbox, so what decodes
and how fast says nothing about Safari's hardware decoders; it sends Safari's
user agent but is not Safari's WebRTC, autoplay, or audio-unlock policy, and
WebRTC VP8, which it decodes in libwebrtc and its player then drops as late, is
checked on the stream alone. Here it
also runs headless (headed WebKitGTK's web process dies on an Xvfb display), so
it has no real focus, no clipboard of its own (Safari's paste confirmation is
not modeled at all), no fullscreen or pointer lock, no touch moves (an iPad's or
iPhone's touch is not covered), no IME composition, and only mock camera and
microphone sources. Those checks report themselves skipped there; a Mac or an
iPad is still the only way to cover Safari.

The checklist, verbatim:

> Images: https://github.com/selkies-project/selkies/pkgs/container/selkies%2Fdesktop,
> https://github.com/selkies-project/docker-selkies-glx-desktop, https://github.com/selkies-project/docker-selkies-egl-desktop
>
> Repeat (WebSockets, WebRTC) x (X11, Wayland (`SELKIES_WAYLAND=true`)) - four times total. Trixie and Resolute should
> both be tested for the example desktop.
>
> Moreover, the same tests on Chrome, Firefox, and Safari are appreciated.
>
> 1. Check all encoders, CRF/CBR, paintover, video streaming mode, 4:4:4 colors, and CPU encoding working properly.
> 2. Check that the microphone/webcam works (use a web tester in both Firefox and Chrome). The toggle for the
>    microphone/webcam is up at the top.
> 3. Make sure the second display works properly by moving a window to the right. Then, toggle force aligned resolution,
>    native cursor style, and antialiasing. Try changing resolutions (both preset and manual, UI scaling, and HiDPI (it's
>    supposed to be disabled automatically when a resolution is set), and the reset button as well. See if the desktops
>    or windows respond.
> 4. Use very large text clipboard payloads (>2MB) and image payloads. Check if the Image Upload (clipboard) button works
>    as well.
> 5. Test file upload and download, preferably for large files over 100MB. Make sure it's fast enough and uses up the
>    available bandwidth well.
> 6. Check app install, but it would only work when `SELKIES_COMMAND_ENABLED` is enabled, so check if it's enabled.
> 7. Test the gamepad (first, the touch gamepad, and your real gamepad if you happen to have it), both on Firefox and
>    Chrome (they each use different joypad interfaces) with https://hardwaretester.com/gamepad, or any other native
>    client. You need to press all buttons and test four diagonal directions for each thumbstick.
> 8. Test `#shared` and `#player*`. For the shared window, only the screen/audio should work and nothing else. Check no
>    long-term screen quality degradations. For controllers, the gamepad should also work.
> 9. Test all of the shortcuts, and especially the pointer lock when `Ctrl + Shift + Left Click` or full screen is used.
> 10. Test audio/videos or anything else on the web browsers. Moreover, you should test IME in your own language.
