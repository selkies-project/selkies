#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Assemble selkies-<ver>-<arch>.AppImage (run inside the manylinux_2_28
# container scripts/ci/appimage.sh starts, with the checkout at /repo, the
# linuxdeploy AppImage in /work, and /out for the result).
#
# The image is AlmaLinux 8, glibc 2.28, the floor pixelflux's wheels are built
# for, so whatever the AppImage carries runs on any distribution since. The
# server is built here as every native package builds it (infra/packaging/
# server.sh), every codec library linked in from the pinned source pixelflux's
# wheels build, since this distribution carries none at the release pixelflux
# needs. linuxdeploy then bundles the libraries the binary links and the
# Wayland server library pixelflux opens by name, leaving the host the GPU
# stack (libEGL, libgbm, libdrm, the NVIDIA and VA-API drivers) and libva,
# whose drivers sit where the host's own libva looks for them.
#
# Beside the server it carries what it starts where a host has none: the
# PulseAudio daemon with its modules, pactl, the XKB keymaps, and the input and
# V4L2 interposers. No desktop session is bundled: the AppImage streams an
# existing X display or Xvfb or a Wayland compositor, or runs `selkies session`
# with the host's own desktop.
set -eux
ARCH="$(uname -m)"
OUTPUT="selkies-${SELKIES_VERSION:-0.0.0}-${ARCH}.AppImage"
APPDIR=/build/AppDir

# Everything from the image's AlmaLinux BaseOS, AppStream, and PowerTools, as
# pixelflux's wheels are built; the EPEL repository the image carries is left
# out, and a mirror still syncing is waited out with fresh metadata.
dnf_install() {
    for attempt in 1 2 3 4; do
        dnf install -y --disablerepo='epel*' "$@" && return 0
        dnf clean metadata
        sleep $((attempt * 30))
    done
    return 1
}
dnf_install git nasm bison clang clang-devel pixman-devel libxcb-devel libdrm-devel libinput-devel \
    mesa-libgbm-devel mesa-libEGL-devel wayland-devel systemd-devel \
    pulseaudio-libs-devel pulseaudio pulseaudio-utils xkeyboard-config
# dav1d and libxkbcommon build with meson, which the image's own Python is too
# old for; the interpreters manylinux ships carry a current pip
/opt/python/cp311-cp311/bin/pip install --retries 5 meson ninja
export PATH="/opt/python/cp311-cp311/bin:${PATH}"

# The server reads keysyms through API libxkbcommon added in 1.0, past the 0.9
# this release carries, so a pinned release is linked in statically, reading
# the host's keymaps or the copy AppRun points it at
XKB=/build/xkbcommon
rm -rf "${XKB}"
git init -q "${XKB}/src"
git -C "${XKB}/src" fetch -q --depth 1 https://github.com/xkbcommon/libxkbcommon.git \
    refs/tags/xkbcommon-1.7.0:refs/tags/xkbcommon-1.7.0
[ "$(git -C "${XKB}/src" rev-parse 'FETCH_HEAD^{commit}')" = 7a31e3585edf78be281559377e26d15f8c4bc655 ]
git -C "${XKB}/src" checkout -q FETCH_HEAD
meson setup "${XKB}/build" "${XKB}/src" --prefix="${XKB}" --libdir=lib --buildtype=release \
    --default-library=static -Db_staticpic=true -Denable-x11=false -Denable-wayland=false \
    -Denable-docs=false -Denable-tools=false -Denable-xkbregistry=false \
    -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale
ninja -C "${XKB}/build" install
# The crates name the library to the linker rather than through pkg-config, so
# its directory goes ahead of the system's for the compiler driver as well
export LIBRARY_PATH="${XKB}/lib" PKG_CONFIG_PATH="${XKB}/lib/pkgconfig"

rm -rf "${APPDIR}"
/repo/infra/packaging/server.sh "${APPDIR}"

# The interposers, for containers with no reachable /dev/uinput or /dev/video*,
# at the paths AppRun exports. The V4L2 interposer is built without its
# PipeWire frame source, whose headers this release carries too old.
mkdir -p "${APPDIR}/usr/lib"
gcc -shared -fPIC -O2 -o "${APPDIR}/usr/lib/selkies_input_interposer.so" \
    /repo/addons/input-interposer/input_interposer.c -ldl -lpthread
# Back-compat name for deployments that preload the pre-rename path.
ln -sf selkies_input_interposer.so "${APPDIR}/usr/lib/selkies_joystick_interposer.so"
gcc -shared -fPIC -O2 -o "${APPDIR}/usr/lib/selkies_v4l2_interposer.so" \
    /repo/addons/v4l2-interposer/v4l2_interposer.c -ldl -lpthread

# The sound server AppRun and `selkies session` start: its modules, loaded by
# the module directory AppRun names, and its startup script
mkdir -p "${APPDIR}/usr/lib/pulseaudio" "${APPDIR}/usr/share/selkies/pulse" \
    "${APPDIR}/usr/libexec/selkies-session" "${APPDIR}/usr/share/X11"
cp -r /usr/lib64/pulse-*/modules "${APPDIR}/usr/lib/pulseaudio/modules"
cp /etc/pulse/default.pa "${APPDIR}/usr/share/selkies/pulse/default.pa"
install -m755 /repo/infra/appimage/session-pulseaudio "${APPDIR}/usr/libexec/selkies-session/pulseaudio"
cp -rL /usr/share/X11/xkb "${APPDIR}/usr/share/X11/xkb"

mkdir -p "${APPDIR}/usr/share/applications" "${APPDIR}/usr/share/icons/hicolor/512x512/apps"
cat > "${APPDIR}/usr/share/applications/selkies.desktop" <<'DESKTOP'
[Desktop Entry]
Name=Selkies
Comment=Low-latency HTML5 remote desktop streaming
Exec=selkies
Icon=selkies
Type=Application
Categories=Network;RemoteAccess;
Terminal=true
DESKTOP
cp /repo/docs/assets/logo/icon-512x512.png "${APPDIR}/usr/share/icons/hicolor/512x512/apps/selkies.png"

# linuxdeploy and the AppImage plugin it runs are AppImages themselves, and a
# container has no FUSE to mount them with
export APPIMAGE_EXTRACT_AND_RUN=1 OUTPUT LINUXDEPLOY_OUTPUT_VERSION="${SELKIES_VERSION:-0.0.0}"
cd /build
/work/linuxdeploy.AppImage --appdir "${APPDIR}" \
    --executable "${APPDIR}/usr/bin/selkies" \
    --executable /usr/bin/pulseaudio \
    --executable /usr/bin/pactl \
    --library "$(readlink -f /usr/lib64/libwayland-server.so.0)" \
    --deploy-deps-only "${APPDIR}/usr/lib/pulseaudio/modules" \
    --desktop-file "${APPDIR}/usr/share/applications/selkies.desktop" \
    --icon-file "${APPDIR}/usr/share/icons/hicolor/512x512/apps/selkies.png" \
    --custom-apprun /repo/infra/appimage/AppRun
# linuxdeploy leaves out what its excludelist expects every desktop to carry,
# which a headless host does not: the X11 client libraries PulseAudio links
# and ALSA's, which its modules do
for lib in libX11.so.6 libX11-xcb.so.1 libICE.so.6 libSM.so.6 libasound.so.2; do
    cp -L "/usr/lib64/${lib}" "${APPDIR}/usr/lib/${lib}"
    # shellcheck disable=SC2016  # the loader's $ORIGIN, literal
    patchelf --set-rpath '$ORIGIN' "${APPDIR}/usr/lib/${lib}"
done
/work/linuxdeploy.AppImage --appdir "${APPDIR}" --output appimage

# The floor is the point of building here, and one library linked from a newer
# toolchain raises it silently, so every ELF file is checked rather than assumed
find "${APPDIR}" -type f | while read -r path; do
    [ "$(head -c 4 "${path}" | od -An -c | tr -d ' ')" = '177ELF' ] || continue
    need="$(objdump -T "${path}" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
    [ -z "${need}" ] || [ "$(printf '%s\n2.28\n' "${need}" | sort -V | tail -1)" = 2.28 ] \
        || echo "GLIBC_${need} ${path#"${APPDIR}"/}"
done > /build/glibc-over
[ ! -s /build/glibc-over ] || { echo "these need a glibc newer than 2.28:" >&2; cat /build/glibc-over >&2; exit 1; }
echo "every ELF file in the AppDir needs at most GLIBC_2.28"

cp "/build/${OUTPUT}" /out/
chown "${HOST_UID:-0}:${HOST_GID:-0}" "/out/${OUTPUT}"
