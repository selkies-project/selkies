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

`tests/requirements.txt` lists what the suites need on top of an installed
`selkies`; the browsers themselves come from `playwright install chromium
firefox webkit`.

## Tiers

| Tier | Needs |
| --- | --- |
| `unit` | The source tree and `gcc` for `tests/tools`. The dashboard translation audit also wants `node`, and reports itself skipped without it. |
| `integration` | An X display (`E2E_DISPLAY`, default `:99`) or the Wayland backend, PulseAudio, and `selkies` importable with `pixelflux`/`pcmflux`. |
| `e2e` | The above plus Playwright browsers, the built web client (`scripts/ci/build-web.sh`), `wl-clipboard` for the Wayland clipboard checks, and `tests/tools/fetch-openh264.sh` for the Firefox WebRTC block. |
| `perf` | A long constrained-link pacer benchmark, plus `xterm` and `xdotool` for the screen-damage load generator. Run on request. |
| `soak` | The whole `pixelflux`/`pcmflux` API surface, including recording and Wayland. Run on request. |

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `E2E_DISPLAY` | `:99` | X display the server streams from. Never point this at a real session: the suites inject input and resize the root window. |
| `E2E_PORT` | `18080` | Server port. Everything the server exposes, `/api/metrics` included, is on it. |
| `E2E_WORKDIR` | `$TMPDIR/selkies-tests` | Server log, shim recordings and other scratch. |
| `SELKIES_TEST_PYTHON` | the interpreter running the tests | Interpreter the server under test runs on. |
| `E2E_CHROME` | unset | System Chrome/Chromium binary. Unset uses Playwright's bundled Chromium. |
| `E2E_FIREFOX_PROFILE` | `$E2E_WORKDIR/firefox-profile` | Persistent Firefox profile; clipboard permission does not survive a fresh one, and `tests/tools/fetch-openh264.sh` seeds the OpenH264 plugin into it. Firefox negotiates no H.264 without that plugin, and the WebRTC block skips. |
| `E2E_TURN_REST_URI` | unset | TURN REST endpoint. WebRTC runs on host candidates alone without it. |

## Tools

`make -C tests/tools` builds the two helpers the suites need:

- **`uinput_shim.so`** — an emulator for `/dev/uinput`, preloaded into the
  server. It decodes the setup ioctls against the real `<linux/uinput.h>` and
  writes the event stream to a file, so the kernel gamepad backend can be
  driven on a host that has no uinput node, which includes CI.
- **`uinput_abi_truth`** — prints the ioctl encodings, struct sizes and offsets
  the kernel headers define. `unit/test_uinput_abi.py` compares them against the
  constants `selkies.input_handler` computes in pure Python.

`make -C tests/tools gamepad` builds the interposer-side inspection tools
(`jsread`, `sdlenum`, `udevscan`), which need SDL2 and libudev. They are for
looking at what an application sees through the Joystick Interposer, and are not
part of any tier.

`tools/wlobs.py` is the pywayland client the Wayland blocks use to prove that
input reached the compositor seat; `tools/tcp2unix.py` is the reverse proxy the
Unix-socket suite puts in front of a server with no TCP listener.

## Packaging

`tests/packaging/simulate.sh` runs `infra/packaging/*.sh` against a genuinely
read-only `/repo` with the root-only tools stubbed, on any host, with no
container runtime. It takes seconds and catches the staging and read-only-mount
mistakes that otherwise only surface in the release job:

```bash
python3 -m build            # or drop a wheel in dist/
tests/packaging/simulate.sh
```

Those scripts also compile the Joystick Interposer into each package, including
a 32-bit variant wherever the compiler can produce one. A host without a
multilib toolchain can unpack one and point `MULTILIB_SYSROOT` at it, and the
simulation covers that branch instead of skipping it:

```bash
mkdir -p /tmp/multilib && cd /tmp/multilib
apt-get download libc6-dev-i386 libc6-i386 lib32gcc-13-dev lib32gcc-s1
for d in *.deb; do dpkg-deb -x "$d" root/; done
mkdir -p root/lib32 root/lib
ln -sf ../usr/lib32/libc.so.6 root/lib32/libc.so.6
ln -sf ../usr/lib32/ld-linux.so.2 root/lib/ld-linux.so.2
MULTILIB_SYSROOT=/tmp/multilib/root tests/packaging/simulate.sh
```
