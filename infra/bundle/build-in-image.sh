#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Assemble the Selkies bundle, selkies-<version>-<arch>-linux-<gnu|musl>.tar.gz:
# one directory tree that runs a whole session on a host providing its C
# library, the kernel, and, for the GPU, its driver, unpacked anywhere
# (crates/selkies/src/commands/bundle.rs reads it):
#
#   bin/selkies, bin/selkies-desktop
#   libexec/selkies/  Xvfb, Xwayland, xkbcomp (xservers.sh), pulseaudio, dbus-daemon,
#                     cups/, the print queue's scheduler (cups.sh)
#   lib/selkies/      the libraries those link that are not glibc's or the GPU
#                     driver's, pulse/modules/, and preload/, the interposers
#   share/selkies/    the xkb data, the buses' configuration, the logo, and fonts/,
#                     the shell's UI face
#
# Every program finds its libraries through a run path relative to itself
# ($ORIGIN), a RUNPATH rather than an RPATH, so it binds that program's own
# lookups alone and never the libraries a host's GPU driver loads into the same
# process. What only the host can supply stays the host's: glibc, libstdc++ and
# libgcc_s, which every glibc distribution installs (musl's are carried, since
# Alpine installs neither), and the graphics stack (libgbm is reached through gbm-loader.c, the
# rest is loaded at run time). The X servers carry nothing of their own at all.
#
# Runs inside the manylinux_2_28 or musllinux_1_2 image (infra/bundle/build.sh),
# whose C library is the bundle's floor. PulseAudio and dbus-daemon are the image's distribution's
# builds with their libraries alongside; the server, the shell, and the
# interposers are built from this tree, the codecs from SELKIES_CODECS where
# set (scripts/ci/build-codecs.sh), else built here.
#
# Usage: build-in-image.sh <out-dir>   (dist/web built; SELKIES_VERSION stamps)
set -eux
OUT="$(mkdir -p "${1:?usage: build-in-image.sh <out-dir>}" && readlink -f "$1")"
REPO="$(dirname "$(readlink -f "$0")")/../.."
BUILD="${SELKIES_BUILD_DIR:-/build}"
export SELKIES_BUILD_DIR="${BUILD}"
ARCH="$(uname -m)"
VERSION="${SELKIES_VERSION:-0.0.0}"
if ldd --version 2>&1 | grep -qi musl; then LIBC=musl; else LIBC=gnu; fi
NAME="selkies-${VERSION}-${ARCH}-linux-${LIBC}"
TREE="${BUILD}/bundle/${NAME}"
rm -rf "${BUILD}/bundle"
mkdir -p "${TREE}/bin" "${TREE}/libexec/selkies" "${TREE}/lib/selkies/pulse/modules" \
    "${TREE}/lib/selkies/preload" "${TREE}/share/selkies/dbus-1"

# Installed once into the builder image (infra/bundle/Dockerfile), else here
[ -f /etc/selkies-bundle-deps ] || "${REPO}/infra/bundle/deps.sh"
export PATH="/opt/python/cp312-cp312/bin:${PATH}"
# shellcheck source=infra/packaging/xkbcommon.sh
. "${REPO}/infra/packaging/xkbcommon.sh"

"${REPO}/infra/bundle/xservers.sh" "${TREE}"
"${REPO}/infra/bundle/cups.sh" "${TREE}"


# The server links the libgbm loader and pixman from the archives the X servers
# were built with: -lgbm and -lpixman-1 find a directory holding only the
# archive first, so neither is a library the host has to have
if [ -z "${SELKIES_CODECS:-}" ]; then
    [ -f "${BUILD}/codecs/env.sh" ] \
        || "${REPO}/infra/packaging/codecs.sh" "${BUILD}/codecs" "${REPO}/crates/pixelflux"
    export SELKIES_CODECS="${BUILD}/codecs"
fi
STATIC="${BUILD}/static-link"
mkdir -p "${STATIC}"
cp "${BUILD}/xservers/deps/lib/libgbm.a" "${BUILD}/xservers/deps/lib/libpixman-1.a" "${STATIC}/"
export LIBRARY_PATH="${STATIC}:${LIBRARY_PATH}"
# The build image's own link names would be found first on the search path the
# crates' build scripts add; this image is thrown away with them
rm -f /usr/lib64/libgbm.so /usr/lib64/libpixman-1.so /usr/lib/libgbm.so /usr/lib/libpixman-1.so
# The server's libraries beside it, as a RUNPATH binding its own lookups alone
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C link-arg=-Wl,--enable-new-dtags -C link-arg=-Wl,-rpath,\$ORIGIN/../lib/selkies"
"${REPO}/infra/packaging/server.sh" "${BUILD}/server-root"
install -Dm755 "${BUILD}/server-root/usr/bin/selkies" "${TREE}/bin/selkies"
# The toolchain server.sh installed, and the profile it built with
export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}" CARGO_HOME="${CARGO_HOME:-/opt/rust/cargo}"
export PATH="${CARGO_HOME}/bin:${PATH}"
PROFILE="${SELKIES_CARGO_PROFILE:-release}"
CARGO_TARGET_DIR="${BUILD}/target" cargo build --profile "${PROFILE}" --locked -p selkies-desktop \
    --manifest-path "${BUILD}/server/Cargo.toml"
install -Dm755 "${BUILD}/target/${PROFILE}/selkies-desktop" "${TREE}/bin/selkies-desktop"

# PulseAudio with the modules Selkies loads, and the distribution's dbus-daemon
# with its configuration
install -m755 /usr/bin/pulseaudio /usr/bin/dbus-daemon "${TREE}/libexec/selkies/"
modules="$(dirname "$(find /usr/lib64 /usr/lib -name module-null-sink.so 2> /dev/null | head -n 1)")"
for module in module-native-protocol-unix module-always-sink module-null-sink module-virtual-source; do
    install -m755 "${modules}/${module}.so" "${TREE}/lib/selkies/pulse/modules/"
done
# Selkies' logo, the desktop's start button and its application-icon wallpaper
# where the image installs none of its own
install -m644 "${REPO}/docs/assets/logo/selkies.svg" "${TREE}/share/selkies/icon.svg"
install -m644 "${REPO}/docs/assets/logo/mark-512x512.png" "${TREE}/share/selkies/icon.png"
# The face the shell and the window manager draw their text in, Inter (SIL Open
# Font License), which the drawing library adds to the host's fonts: a host can
# have none at all
CACHE="${SELKIES_SOURCE_CACHE:-${BUILD}/sources}"
mkdir -p "${CACHE}" "${TREE}/share/selkies/fonts"
if [ ! -f "${CACHE}/Inter-4.1.zip" ]; then
    curl -fsSL --retry 5 --connect-timeout 30 -o "${CACHE}/Inter-4.1.zip.part" \
        https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip
    mv "${CACHE}/Inter-4.1.zip.part" "${CACHE}/Inter-4.1.zip"
fi
echo "9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e  ${CACHE}/Inter-4.1.zip" \
    | sha256sum -c -
python3 - "${CACHE}/Inter-4.1.zip" "${TREE}/share/selkies/fonts" <<'PY'
import sys, zipfile
names = {"extras/ttf/Inter-Regular.ttf": "Inter-Regular.ttf",
         "extras/ttf/Inter-Bold.ttf": "Inter-Bold.ttf", "LICENSE.txt": "Inter-LICENSE.txt"}
with zipfile.ZipFile(sys.argv[1]) as archive:
    for member, name in names.items():
        with open(f"{sys.argv[2]}/{name}", "wb") as out:
            out.write(archive.read(member))
PY
cp /usr/share/dbus-1/session.conf "${TREE}/share/selkies/dbus-1/"
# The session's system bus: the distribution's configuration names the account
# the daemon drops to, which the daemon looks up even when it never drops, and
# which a host need not have; this one takes what the session user sends, and
# the services' own policies from the host where it has them
cat > "${TREE}/share/selkies/dbus-1/system.conf" <<'EOF'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <auth>EXTERNAL</auth>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow user="*"/>
    <allow own="*"/>
    <allow send_type="method_call"/>
    <allow send_type="signal"/>
    <allow send_requested_reply="true" send_type="method_return"/>
    <allow send_requested_reply="true" send_type="error"/>
    <allow receive_type="method_call"/>
    <allow receive_type="method_return"/>
    <allow receive_type="error"/>
    <allow receive_type="signal"/>
  </policy>
  <includedir>/usr/share/dbus-1/system.d</includedir>
  <includedir>/etc/dbus-1/system.d</includedir>
</busconfig>
EOF
# The Wayland server library the server opens at run time, found through its run path
cp -L "${BUILD}/xservers/deps/lib/libwayland-server.so.0" "${TREE}/lib/selkies/"

# The interposers the session's applications preload, and their 32-bit builds
# where the image has a 32-bit toolchain
build_interposers() {
    suffix="$1"
    shift
    gcc "$@" -O2 -shared -fPIC -o "${TREE}/lib/selkies/preload/selkies_input_interposer${suffix}.so" \
        "${REPO}/addons/input-interposer/input_interposer.c" -ldl -pthread
    gcc "$@" -O2 -shared -fPIC -o "${TREE}/lib/selkies/preload/selkies_v4l2_interposer${suffix}.so" \
        "${REPO}/addons/v4l2-interposer/v4l2_interposer.c" -ldl -pthread
    rm -rf "${BUILD}/fake-udev"
    cp -r "${REPO}/addons/fake-udev" "${BUILD}/fake-udev"
    make -C "${BUILD}/fake-udev" CC="gcc $*" > /dev/null
    install -m755 "${BUILD}/fake-udev/libudev.so.1.0.0-fake" \
        "${TREE}/lib/selkies/preload/libudev.so.1.0.0-fake${suffix}"
}
build_interposers ""
if echo 'int main(void){return 0;}' | gcc -m32 -x c - -o /dev/null 2> /dev/null; then
    build_interposers _32 -m32
fi

# The libraries every program and module links, but for the C library's, glibc's
# C++ runtime, and the graphics stack, each copied once under its soname.
# libcrypt is carried: distributions moved it to a compat package with soname 2.
CXX_RUNTIME='|gcc_s|stdc\+\+'
[ "${LIBC}" = gnu ] || CXX_RUNTIME=''
HOST_LIBS="^(ld-linux.*|ld-musl.*|libc\\.musl.*|linux-vdso|lib(c|m|dl|pthread|rt|resolv|util|mvec|anl|nsl|BrokenLocale${CXX_RUNTIME})|lib(GL|GLX|GLdispatch|OpenGL|EGL|GLESv[12]_CM|GLESv2|gbm|drm|vulkan|va|va-drm|va-x11|va-wayland|cuda|nvidia-.*))\\.so"
LIBDIR="${TREE}/lib/selkies"
queue="$(find "${TREE}/bin" "${TREE}/libexec" "${LIBDIR}/pulse/modules" -type f \
    -exec sh -c 'head -c4 "$1" | grep -q ELF' _ {} \; -print)"
while [ -n "${queue}" ]; do
    next=""
    for elf in ${queue}; do
        for lib in $(ldd "${elf}" 2> /dev/null | awk '/=> \// {print $1 "=" $3}'); do
            soname="${lib%%=*}"
            path="${lib#*=}"
            echo "${soname}" | grep -qE "${HOST_LIBS}" && continue
            # A module's helpers stay beside it
            dest="${LIBDIR}/${soname}"
            case "${path}" in */pulseaudio/modules/*) dest="${LIBDIR}/pulse/modules/${soname}" ;; esac
            [ -e "${dest}" ] && continue
            cp -L "${path}" "${dest}"
            chmod 755 "${dest}"
            next="${next} ${dest}"
        done
    done
    queue="${next}"
done
# The run paths: rewritten in place where a library has one (patchelf grows a
# file by its whole alignment to add one), and the daemons' as an RPATH, which
# their libraries' own lookups take too; neither daemon shares a process with a
# GPU driver
for elf in "${LIBDIR}"/*.so*; do
    if [ -n "$(patchelf --print-rpath "${elf}")" ]; then
        patchelf --set-rpath "\$ORIGIN" "${elf}"
    fi
done
for elf in "${LIBDIR}"/pulse/modules/*.so; do
    if [ -n "$(patchelf --print-rpath "${elf}")" ]; then
        patchelf --set-rpath "\$ORIGIN:\$ORIGIN/../.." "${elf}"
    fi
done
for elf in "${TREE}"/libexec/selkies/pulseaudio "${TREE}"/libexec/selkies/dbus-daemon; do
    patchelf --force-rpath --set-rpath "\$ORIGIN/../../lib/selkies" "${elf}"
done
patchelf --force-rpath --set-rpath "\$ORIGIN/../../../../lib/selkies" \
    "${TREE}/libexec/selkies/cups/sbin/cupsd"
patchelf --force-rpath --set-rpath "\$ORIGIN/../../../../../../lib/selkies" \
    "${TREE}/libexec/selkies/cups/lib/cups/daemon/cups-exec"
patchelf --print-rpath "${TREE}/bin/selkies" | grep -qxF "\$ORIGIN/../lib/selkies"

# Every program resolves every library it links from the tree or from what the
# host is expected to have
missing="$(find "${TREE}/bin" "${TREE}/libexec" "${LIBDIR}" -type f ! -path '*/preload/*' \
    -exec sh -c 'head -c4 "$1" | grep -q ELF || exit 0
        ldd "$1" 2>&1 | grep "not found" | sed "s|^|$1: |"' _ {} \;)"
[ -z "${missing}" ] || { echo "${missing}" >&2; exit 1; }
# The server links libxcb in statically and exports none of it, and no libxcb is
# carried, so the one a host's GPU driver loads (NVIDIA's L4T X and EGL stack
# links the distribution's) is the only one it can bind: a bundled copy would
# shadow it through the run path, which takes the capture thread down there
if find "${LIBDIR}" -name 'libxcb*' | grep -q . \
    || nm -D --defined-only "${TREE}/bin/selkies" | grep -q ' xcb_'; then
    echo "the bundle carries or exports libxcb" >&2
    exit 1
fi
"${TREE}/bin/selkies" --version
readelf -d "${TREE}/bin/selkies" | grep -E 'NEEDED|RUNPATH'

tar -C "${BUILD}/bundle" -czf "${OUT}/${NAME}.tar.gz" "${NAME}"
ls -la "${OUT}/${NAME}.tar.gz"
