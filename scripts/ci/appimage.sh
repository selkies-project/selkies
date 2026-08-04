#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build Selkies-<ver>-<arch>.AppImage on the runner for this architecture.
# Uses rattler-build to package selkies as a conda package (which also ships as
# a release artifact), then assembles the AppImage with linuxdeploy and the
# linuxdeploy conda plugin (https://github.com/linuxdeploy/linuxdeploy-plugin-conda).
#
# Usage: scripts/ci/appimage.sh [x86_64|aarch64]

set -eux

ARCH="${1:-$(uname -m)}"
case "${ARCH}" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "unsupported architecture: ${ARCH}" >&2; exit 1 ;;
esac

cd "$(readlink -f "$(dirname "$0")")/../.."
WORK="${PWD}/build/appimage"
mkdir -p "${WORK}"

# 1) rattler-build: selkies conda package (noarch) from the repo
export PATH="${HOME}/.pixi/bin:${PATH}"
if ! command -v pixi >/dev/null; then
  curl -fsSL https://pixi.sh/install.sh | sh
fi
pixi global install rattler-build micromamba || true
rattler-build build \
    --recipe infra/appimage/recipe.yaml \
    --output-dir "${WORK}/conda-output" \
    --channel-priority disabled

PKG="$(find "${WORK}/conda-output" -name 'selkies-*.tar.bz2' -o -name 'selkies-*.conda' | head -n1)"
test -f "${PKG}"

# 2) linuxdeploy + conda plugin (latest published builds)
case "${ARCH}" in
  x86_64)
    LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
    PLUGIN_URL="https://github.com/linuxdeploy/linuxdeploy-plugin-conda/releases/download/continuous/linuxdeploy-plugin-conda-x86_64.AppImage"
    ;;
  aarch64)
    LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-aarch64.AppImage"
    PLUGIN_URL="https://github.com/linuxdeploy/linuxdeploy-plugin-conda/releases/download/continuous/linuxdeploy-plugin-conda-aarch64.AppImage"
    ;;
esac
curl -fsSL -o "${WORK}/linuxdeploy.AppImage" "${LINUXDEPLOY_URL}"
curl -fsSL -o "${WORK}/linuxdeploy-plugin-conda.AppImage" "${PLUGIN_URL}"
chmod +x "${WORK}/linuxdeploy.AppImage" "${WORK}/linuxdeploy-plugin-conda.AppImage"

# 3) Assemble the AppDir: the conda plugin creates an env with the selkies
#    package (from the local rattler-build channel) plus the runtime native
#    libraries; pixelflux and pcmflux arrive as pip packages in the same env.
export OUTPUT="selkies-${SELKIES_VERSION:-0.0.0}-${ARCH}.AppImage"
export CONDA_CHANNELS="$(dirname "$(readlink -f "${PKG}")/../");conda-forge"
# ffmpeg pinned to the LGPL-only conda-forge variant so pixelflux sees an
# x264-free avcodec stack inside the AppImage (gpl variant exists but is wrong here)
export CONDA_PACKAGES="selkies;python=3.12;ffmpeg=*=*lgpl*;libxcb;pulseaudio;libva;libxkbcommon;zlib"
export CONDA_CHANNEL_PRIORITY="flexible"
export PIP_REQUIREMENTS="pixelflux pcmflux"
export DEPLOY_GLIBC_VERSION="0"

rm -rf AppDir
"${WORK}/linuxdeploy.AppImage" --appimage-extract-and-run \
    --appdir AppDir \
    --plugin conda

# 4) Custom AppRun + desktop integration (graphics/minimal: no desktop session
#    is bundled; the AppImage streams an existing X display/Xvfb or Wayland)
ENV_BIN="$(dirname "$(find AppDir/usr/conda/envs -name selkies -type f | head -n1)")"
test -x "${ENV_BIN}/selkies" || test -x "${ENV_BIN}/python"
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
ENV_BIN="$(cd "${HERE}" && dirname "$(find usr/conda/envs -name selkies -type f | head -n1)")"
export PATH="${HERE}/${ENV_BIN}:${PATH}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${XDG_RUNTIME_DIR}/pulse/native}"
export PIPEWIRE_LATENCY="128/48000"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR}/pulse}"

# Auto-start a virtual display when none is available (salvaged from the legacy
# run-wrappers, transport-agnostic). Wayland mode starts its own compositor and
# skips this; the host's Xvfb is used when present.
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
    if [ -x "${HERE}/${ENV_BIN}/pulseaudio" ]; then
        "${HERE}/${ENV_BIN}/pulseaudio" --verbose --log-target=file:/tmp/pulseaudio_selkies.log --disallow-exit --exit-idle-time="-1" &
    elif command -v pulseaudio >/dev/null 2>&1; then
        pulseaudio --verbose --log-target=file:/tmp/pulseaudio_selkies.log --disallow-exit --exit-idle-time="-1" &
    fi
fi

exec "${HERE}/${ENV_BIN}/selkies" "$@"
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
# Keep the conda package alongside the AppImage (portable conda distribution)
cp "${PKG}" out/
ls -la out/
