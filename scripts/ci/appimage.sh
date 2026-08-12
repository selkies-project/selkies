#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build selkies-<ver>-<arch>.AppImage on the runner for this architecture.
# Uses rattler-build to package selkies as a conda package (which also ships as
# a release artifact), then assembles the AppImage with linuxdeploy and the
# conda plugin in infra/appimage.
#
# Usage: scripts/ci/appimage.sh [arch]   (any `uname -m` name; defaults to this host)

set -eux

# linuxdeploy and Miniforge both name their artifacts after `uname -m`
ARCH="${1:-$(uname -m)}"

cd "$(readlink -f "$(dirname "$0")")/../.."
WORK="${PWD}/build/appimage"
rm -rf "${WORK}" AppDir
mkdir -p "${WORK}"

# 1) rattler-build: selkies conda package (noarch) from the repo
export PATH="${HOME}/.pixi/bin:${PATH}"
if ! command -v pixi >/dev/null; then
  curl -fsSL https://pixi.sh/install.sh | sh
fi
pixi global install rattler-build
rattler-build build \
    --recipe infra/appimage/recipe.yaml \
    --output-dir "${WORK}/conda-output" \
    --channel-priority disabled

PKG="$(find "${WORK}/conda-output" \( -name 'selkies-*.tar.bz2' -o -name 'selkies-*.conda' \) | head -n1)"
test -f "${PKG}"

# 2) linuxdeploy (latest published build) and the conda plugin. The plugin is
#    the one in infra/appimage: upstream publishes no release artifacts, and
#    this one installs Miniforge rather than Miniconda3, keeping the AppImage
#    on conda-forge alone.
scripts/ci/fetch.sh \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${ARCH}.AppImage" \
    "${WORK}/linuxdeploy.AppImage"
cp infra/appimage/linuxdeploy-plugin-conda.sh "${WORK}/linuxdeploy-plugin-conda.sh"
# The plugin downloads Miniforge from the same rate-limited host
cp scripts/ci/fetch.sh "${WORK}/fetch.sh"
chmod +x "${WORK}/linuxdeploy.AppImage" "${WORK}/linuxdeploy-plugin-conda.sh" "${WORK}/fetch.sh"
# linuxdeploy resolves `--plugin conda` by searching PATH
export PATH="${WORK}:${PATH}"

# 3) Assemble the AppDir: the conda plugin installs a Miniforge prefix at
#    AppDir/usr/conda, adds the channels and packages named below, and finally
#    pip-installs PIP_REQUIREMENTS into the same prefix.
export OUTPUT="selkies-${SELKIES_VERSION:-0.0.0}-${ARCH}.AppImage"
# conda channels are ';'-separated; the local one is the rattler-build output
# root, the directory holding noarch/
CONDA_CHANNELS="${WORK}/conda-output;conda-forge"
CONDA_PYTHON_VERSION="3.12"
# ffmpeg pinned to the LGPL-only conda-forge variant so pixelflux sees an
# x264-free avcodec stack inside the AppImage
CONDA_PACKAGES="selkies;ffmpeg=*=*lgpl*;libxcb;pulseaudio;libva;libxkbcommon;zlib"
# Runtime dependencies with no conda-forge package. pixelflux and pcmflux come
# from the freshly built master-HEAD wheels when CI supplies them (the AppImage
# env always runs Python 3.12, see CONDA_PYTHON_VERSION above), else from PyPI.
PIP_REQUIREMENTS="pulsectl-asyncio aitop"
for project in pixelflux pcmflux; do
  wheel=""
  if [ -n "${PIXELFLUX_PCMFLUX_WHEELS_DIR:-}" ]; then
    wheel="$(find "${PIXELFLUX_PCMFLUX_WHEELS_DIR}" -maxdepth 1 \
        -name "${project}-*cp312*manylinux*${ARCH}*.whl" | head -n1)"
  fi
  PIP_REQUIREMENTS="${PIP_REQUIREMENTS} ${wheel:-${project}}"
done

# conda and pip take configuration from CONDA_*/PIP_* names of their own, where a
# ';'-separated channel list parses as one channel. These four address the plugin,
# so they reach linuxdeploy alone and the toolchain solve below stays on
# conda-forge.
env CONDA_CHANNELS="${CONDA_CHANNELS}" \
    CONDA_PYTHON_VERSION="${CONDA_PYTHON_VERSION}" \
    CONDA_PACKAGES="${CONDA_PACKAGES}" \
    PIP_REQUIREMENTS="${PIP_REQUIREMENTS}" \
    "${WORK}/linuxdeploy.AppImage" --appimage-extract-and-run \
    --appdir AppDir \
    --plugin conda

# Fails the build rather than shipping an AppImage that cannot start
AppDir/usr/conda/bin/selkies --help > /dev/null

# 3b) Joystick Interposer, for containers with no reachable /dev/uinput. It is
#     compiled with the conda-forge toolchain instead of the runner's gcc: the
#     runner links against a glibc far newer than the rest of the AppImage
#     needs, while the sysroot pinned here keeps the library loadable on every
#     distribution the bundled environment already runs on. The 2.28 sysroot is
#     the oldest that still carries the kernel input headers.
CC_ENV="${WORK}/cc-env"
# conda names its sysroot packages after its own subdir (linux-64,
# linux-aarch64, ...), which no `uname -m` mapping reproduces
CONDA_SUBDIR="$(AppDir/usr/conda/bin/conda info --json \
    | AppDir/usr/conda/bin/python -c 'import json,sys; print(json.load(sys.stdin)["platform"])')"
AppDir/usr/conda/bin/conda create -y -p "${CC_ENV}" -c conda-forge \
    c-compiler "sysroot_${CONDA_SUBDIR}=2.28" \
  || AppDir/usr/conda/bin/conda create -y -p "${CC_ENV}" -c conda-forge c-compiler
CONDA_CC="$(find "${CC_ENV}/bin" -name '*-linux-gnu-gcc' | head -n1)"
CONDA_SYSROOT="$(find "${CC_ENV}" -maxdepth 2 -type d -name sysroot | head -n1)"
mkdir -p AppDir/usr/lib
"${CONDA_CC}" --sysroot="${CONDA_SYSROOT}" -shared -fPIC -O2 \
    -o AppDir/usr/lib/selkies_joystick_interposer.so \
    addons/js-interposer/joystick_interposer.c -ldl -lpthread
rm -rf "${CC_ENV}"

# The floor is the whole point of compiling this with conda, and the fallback
# above would raise it silently, so the result is checked rather than assumed
INTERPOSER_FLOOR="$(objdump -T AppDir/usr/lib/selkies_joystick_interposer.so \
    | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n1)"
echo "interposer requires at most ${INTERPOSER_FLOOR}"
if [ "$(printf '%s\n%s\n' "${INTERPOSER_FLOOR}" "GLIBC_2.28" | sort -uV | tail -n1)" != "GLIBC_2.28" ]; then
    echo "interposer needs ${INTERPOSER_FLOOR}, newer than the pinned sysroot provides" >&2
    exit 1
fi

# 4) Custom AppRun + desktop integration. No desktop session is bundled: the
#    AppImage streams an existing X display/Xvfb or a Wayland compositor.
mkdir -p AppDir/usr/share/applications AppDir/usr/share/icons/hicolor/512x512/apps
cat > AppDir/usr/share/applications/selkies.desktop <<'DESKTOP'
[Desktop Entry]
Name=Selkies
Comment=Low-latency HTML5 remote desktop streaming
Exec=selkies
Icon=selkies
Type=Application
Categories=Network;RemoteAccess;
Terminal=true
DESKTOP
cp docs/assets/logo/icon-512x512.png AppDir/usr/share/icons/hicolor/512x512/apps/selkies.png
cat > AppDir/AppRun <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "${0}")")"
ENV_BIN="${HERE}/usr/conda/bin"
export PATH="${ENV_BIN}:${PATH}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${XDG_RUNTIME_DIR}/pulse/native}"
export PIPEWIRE_LATENCY="128/48000"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR}/pulse}"
# Path to the bundled Joystick Interposer, for LD_PRELOADing into an application
# that needs gamepads where /dev/uinput is unreachable. Deliberately not added to
# LD_PRELOAD here: selkies itself must keep seeing the real device nodes.
export SELKIES_INTERPOSER="${HERE}/usr/lib/selkies_joystick_interposer.so"

# A help query prints and exits, so it starts no display or audio server
for arg in "$@"; do
    case "${arg}" in
        -h|--help) exec "${ENV_BIN}/selkies" "$@" ;;
    esac
done

# X11 mode streams an existing display; start a virtual one when none is up.
# Wayland mode starts its own compositor and needs nothing here.
if [ "${SELKIES_WAYLAND:-false}" != "true" ]; then
    export DISPLAY="${DISPLAY:-:20}"
    if [ ! -S "/tmp/.X11-unix/X${DISPLAY#*:}" ] && command -v Xvfb >/dev/null 2>&1; then
        Xvfb "${DISPLAY}" -screen 0 8192x4096x24 -s 0 -dpms +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" +extension "XFIXES" +extension "XTEST" +iglx +render -nolisten "tcp" -ac -noreset -shmem >/tmp/Xvfb_selkies.log 2>&1 &
        echo 'Waiting for X Socket'
        until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done
        echo 'X Server is ready'
    fi
fi

# Start a PulseAudio server when none is listening yet (the conda env bundles
# its own pulseaudio, falling back to a host binary otherwise)
if [ ! -e "${PULSE_SERVER#unix:}" ] && [ ! -S "${PULSE_SERVER#unix:}" ]; then
    if [ -x "${ENV_BIN}/pulseaudio" ]; then
        "${ENV_BIN}/pulseaudio" --verbose --log-target=file:/tmp/pulseaudio_selkies.log --disallow-exit --exit-idle-time="-1" &
    elif command -v pulseaudio >/dev/null 2>&1; then
        pulseaudio --verbose --log-target=file:/tmp/pulseaudio_selkies.log --disallow-exit --exit-idle-time="-1" &
    fi
fi

exec "${ENV_BIN}/selkies" "$@"
APPRUN
chmod +x AppDir/AppRun
ln -sf usr/share/icons/hicolor/512x512/apps/selkies.png AppDir/selkies.png
ln -sf usr/share/applications/selkies.desktop AppDir/selkies.desktop

# 5) Final AppImage
"${WORK}/linuxdeploy.AppImage" --appimage-extract-and-run \
    --appdir AppDir \
    --output appimage

mkdir -p out
mv "${OUTPUT}" out/
# The conda package is noarch, so exactly one architecture's job publishes it
if [ "${ARCH}" = "${CONDA_PACKAGE_ARCH:-x86_64}" ]; then
  cp "${PKG}" out/
fi
ls -la out/
