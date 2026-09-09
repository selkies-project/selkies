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
pytest tests -k gamepad -v
```

`tests/suites.py` is the registry both entry points read: it lists every suite,
its tier, its selectors and its timeout. Add a suite there and it appears in
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
| `unit` | The source tree and `gcc` for `tests/tools`. The audits of the web client (translations, typing, pointer lock, relative motion) also want `node`, and report themselves skipped without it. The example session scripts are parsed with `bash -n` (their Python helpers byte-compiled), and every shell script the tree ships is linted at `shellcheck`'s lowest severity where it is installed. Which findings that severity reports differs between `shellcheck` releases, so CI pins the version rather than taking the runner image's; a distro build may disagree with it in either direction. The transport-dependent rate-control defaults, the one-shot NVML probe, the clipboard-paste typing route (for compositors without `zwp_virtual_keyboard`, such as KWin), and the clipboard ladder's back-off around a dead X display are covered here too. |
| `integration` | An X display named by `E2E_DISPLAY` or the Wayland backend, PulseAudio, and `selkies` importable with `pixelflux`/`pcmflux`. Suites needing a server of their own (the keymap, connection-leak and session-DPI checks) start a throwaway `Xvfb` on a free display number instead, and need nothing set. The XWayland typing check nests `labwc` (which brings `Xwayland`) on a private socket and reads the keys back out of `xev` from `x11-utils`, and skips without them. The Wayland seam and session-screen suites want the labwc the images ship — patched with the control socket and the seam cursor path — which `addons/base/build-labwc.sh` builds (the CI job does exactly that); on a stock labwc the seam suite skips its held-grab checks and the session-screen suite skips whole. The packaging simulation needs neither, only the source tree and a `python3` that can build a virtualenv. |
| `e2e` | The above plus Playwright browsers (WebKit on Linux also needs GStreamer's `opengl` plugin, `gstreamer1.0-gl`, which `playwright install --with-deps webkit` brings; without it WebKit paints every `<video>` through a software fallback sink, so the harness refuses to launch it), the built web client (`scripts/ci/build-web.sh`), `wl-clipboard` for the Wayland clipboard checks, `wmctrl` for the two-display desktop-window check (skipped with a notice when absent), `tkinter` for the sender-lag recovery suite's scene painter (skipped without it), `tests/tools/fetch-openh264.sh` for the Firefox WebRTC blocks, and a coturn `turnserver` on `PATH` (or named by `E2E_TURNSERVER`) for the TURN relay suite, which skips with the install command when there is none. The pointer-motion suite drives the *installed* Chrome and Firefox on the test display with XTEST, and skips when neither is on `PATH`. The pointer-lock suite's game-view block needs `libsdl2-2.0-0` (skipped without it), and its desktop selectors need `openbox`, `kwin-x11` and `kwin-wayland` plus `dbus-run-session`, and the `labwc` the Wayland suites already use (each skipped with the missing binary named). The gaming-mode suite drives the installed Chrome, windowed under `openbox` on a private X server, and skips without either. |
| `perf` | A long constrained-link pacer benchmark, plus `xterm` and `xdotool` for the screen-damage load generator. Run on request. |
| `soak` | The whole `pixelflux`/`pcmflux` API surface, including recording and Wayland. Run on request. |

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `E2E_DISPLAY` | none; required | X display the server streams from. Deliberately not defaulted and never inherited from `DISPLAY`: the suites inject input and resize the root window, so pointing them at a real session damages it. Provision a throwaway server (`Xvfb :N -screen 0 8192x4096x24 -noreset`) and name it here. |
| `E2E_PORT` | a free port | Server port; everything the server exposes, `/api/metrics` included, is on it. Left unset each suite process takes its own, so runs do not have to be serialised. Set it when something in front of the server needs a fixed one. |
| `E2E_WORKDIR` | `$TMPDIR/selkies-tests` | Server log, shim recordings and other scratch. Run through `pytest`, each suite's logs are also copied into `suite-logs/<suite>/` under it, which CI uploads. Its `run/` is the `XDG_RUNTIME_DIR` every compositor, observer and client the suites start is given, so their sockets never land in a desktop session's own. |
| `SELKIES_TEST_PYTHON` | the interpreter running the tests | Interpreter the server under test runs on. |
| `E2E_PAGE_CALL_TIMEOUT` | `60` | Seconds a browser is given to answer a call that carries no deadline of its own (`evaluate`, a close). An engine whose renderer wedges would otherwise hold a suite to the runner's kill; past the bound the call raises and the suite reports. 0 turns it off. |
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
- **`uinput_abi_truth`** — prints the ioctl encodings, struct sizes and offsets
  the kernel headers define. `unit/test_uinput_abi.py` compares them against the
  constants `selkies.input_handler` computes in pure Python.

`make -C tests/tools gamepad` builds the interposer-side inspection tools
(`jsread`, `sdlenum`, `sdlread`, `udevscan`), which need SDL2 and libudev. They
are for looking at what an application sees through the Joystick Interposer, and
are not part of any tier. `jsread` reads the joydev node directly, `sdlenum`
lists what SDL2 enumerates, `sdlread` opens one of those pads and prints the
button, axis and hat events SDL2 delivers for it, and `udevscan` shows the
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
container runtime. It catches the staging, read-only-mount and package-version
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

Those scripts also compile the Joystick Interposer into each package, including
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
