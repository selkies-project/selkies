#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Repackage a glibc Selkies bundle (infra/bundle/build.sh) as the AppImage of
# its architecture, selkies-<version>-<arch>.AppImage: the bundle's tree is the
# AppDir as it stands, with infra/appimage/AppRun at its root and the desktop
# entry and icon the AppImage format reads, compressed into a SquashFS image
# behind AppImage's static type 2 runtime, which mounts it and starts AppRun.
# Nothing is compiled, so the AppImage runs the same binaries as every other
# channel, down to the bundle's glibc floor. Needs python3 (fetch-verified) and
# mksquashfs with zstd (squashfs-tools 4.4 or newer).
#
# Usage: build.sh <bundle.tar.gz> <out-dir>
set -eux
BUNDLE="$(readlink -f "${1:?usage: build.sh <bundle.tar.gz> <out-dir>}")"
OUT="$(mkdir -p "${2:?usage: build.sh <bundle.tar.gz> <out-dir>}" && readlink -f "$2")"
REPO="$(dirname "$(readlink -f "$0")")/../.."
NAME="$(basename "${BUNDLE}" .tar.gz)"
case "${NAME}" in
    selkies-*-linux-gnu) ;;
    *) echo "build.sh: ${NAME} is not a glibc bundle" >&2; exit 2 ;;
esac
ARCH="${NAME%-linux-gnu}"
ARCH="${ARCH##*-}"
VERSION="${NAME#selkies-}"
VERSION="${VERSION%-"${ARCH}"-linux-gnu}"
WORK="${SELKIES_BUILD_DIR:-${REPO}/build}/appimage"

rm -rf "${WORK}"
mkdir -p "${WORK}/AppDir"
APPDIR="${WORK}/AppDir"
tar -xzf "${BUNDLE}" -C "${APPDIR}" --strip-components=1
install -m755 "${REPO}/infra/appimage/AppRun" "${APPDIR}/AppRun"
cat > "${APPDIR}/selkies.desktop" <<'EOF'
[Desktop Entry]
Name=Selkies
Comment=Low-latency HTML5 remote desktop streaming
Exec=selkies
Icon=selkies
Type=Application
Terminal=true
Categories=Network;RemoteAccess;
EOF
cp "${APPDIR}/share/selkies/icon.png" "${APPDIR}/selkies.png"
ln -s selkies.png "${APPDIR}/.DirIcon"

"${REPO}/addons/base/fetch-verified" release AppImage/type2-runtime continuous \
    "runtime-${ARCH}" "${WORK}/runtime"
mksquashfs "${APPDIR}" "${WORK}/payload.squashfs" -root-owned -noappend -comp zstd -quiet
cat "${WORK}/runtime" "${WORK}/payload.squashfs" > "${OUT}/selkies-${VERSION}-${ARCH}.AppImage"
chmod 755 "${OUT}/selkies-${VERSION}-${ARCH}.AppImage"
ls -la "${OUT}/selkies-${VERSION}-${ARCH}.AppImage"
