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
#                     driver's, pulse/modules/, preload/, the interposers, and on
#                     glibc egl-x11/, NVIDIA's X11 EGL platform, for the images
#   share/selkies/    the xkb data, the buses' configuration, the logo, fonts/,
#                     the shell's UI face, and on glibc egl/, that platform's
#                     configuration
#
# Every program finds its libraries through a run path relative to itself
# ($ORIGIN), a RUNPATH rather than an RPATH, so it binds that program's own
# lookups alone and never the libraries a host's GPU driver loads into the same
# process. What only the host can supply stays the host's: glibc, libstdc++ and
# libgcc_s, which every glibc distribution installs (musl's are carried, since
# Alpine installs neither), and the graphics stack (libgbm is reached through gbm-loader.c, the
# rest is loaded at run time). The X servers carry nothing of their own at all.
#
# Runs inside the bundle's toolchain image (infra/bundle/Dockerfile, which
# infra/bundle/build.sh picks), whose C library is the bundle's floor, and takes
# everything this tree does not compile itself from where that image built it,
# SELKIES_TOOLCHAIN, once the key the image records is this tree's
# (infra/bundle/toolchain.sh); in a bare manylinux_2_28 or musllinux_1_2 image,
# or one built from other files, it builds those under toolchain/ in the build
# directory first, kept there for the next build of the same files. A target
# directory kept from a build against another toolchain is emptied, since
# codec-sys binds the codecs' headers without watching them. PulseAudio
# (crates/pcmflux/scripts/build-libpulse.sh, with libltdl), dbus-daemon
# (dbus.sh), and the scheduler's libcrypt (cups.sh) are built from the releases
# their scripts pin, as everything else the toolchain holds, and a library that
# belongs to a package of the image's distribution, or that the image built
# itself under /usr/local, fails the build, so nothing a distribution released
# is carried but musl's C++ runtime, which comes with its compiler; the server,
# the shell, and the interposers are built from this tree, the codecs taken
# from SELKIES_CODECS where set, else from the toolchain.
#
# The bundle's wheel, `cargo xtask wheel`'s repackaging of it, is written beside it.
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

KEY="$(sh "${REPO}/infra/bundle/toolchain.sh" key)"
TOOLCHAIN="${SELKIES_TOOLCHAIN:-}"
if [ -z "${TOOLCHAIN}" ] || [ "$(cat "${TOOLCHAIN}/key" 2> /dev/null)" != "${KEY}" ]; then
    [ -z "${TOOLCHAIN}" ] || echo "the toolchain image was built from other files than this tree's; building its components" >&2
    TOOLCHAIN="${BUILD}/toolchain"
    [ "$(cat "${TOOLCHAIN}/key" 2> /dev/null)" = "${KEY}" ] || rm -rf "${TOOLCHAIN}"
    mkdir -p "${TOOLCHAIN}"
    "${REPO}/infra/bundle/deps.sh"
fi
export SELKIES_TOOLCHAIN="${TOOLCHAIN}"
export PATH="/opt/python/cp312-cp312/bin:${PATH}"
# shellcheck source=infra/packaging/xkbcommon.sh
. "${REPO}/infra/packaging/xkbcommon.sh"

"${REPO}/infra/bundle/xservers.sh" "${TREE}"
"${REPO}/infra/bundle/cups.sh" "${TREE}"
"${REPO}/infra/bundle/dbus.sh" "${TOOLCHAIN}/dbus"


# The server links the libgbm loader and pixman from the archives the X servers
# were built with: -lgbm and -lpixman-1 find a directory holding only the
# archive first, so neither is a library the host has to have
if [ -z "${SELKIES_CODECS:-}" ]; then
    [ -f "${TOOLCHAIN}/codecs/env.sh" ] \
        || "${REPO}/infra/packaging/codecs.sh" "${TOOLCHAIN}/codecs" "${REPO}/crates/pixelflux"
    export SELKIES_CODECS="${TOOLCHAIN}/codecs"
fi
STATIC="${BUILD}/static-link"
mkdir -p "${STATIC}"
cp "${TOOLCHAIN}/xservers/deps/lib/libgbm.a" "${TOOLCHAIN}/xservers/deps/lib/libpixman-1.a" "${STATIC}/"
export LIBRARY_PATH="${STATIC}:${LIBRARY_PATH}"
# The build image's own link names would be found first on the search path the
# crates' build scripts add; this image is thrown away with them
rm -f /usr/lib64/libgbm.so /usr/lib64/libpixman-1.so /usr/lib/libgbm.so /usr/lib/libpixman-1.so
# The server's libraries beside it, as a RUNPATH binding its own lookups alone
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C link-arg=-Wl,--enable-new-dtags -C link-arg=-Wl,-rpath,\$ORIGIN/../lib/selkies"
if [ "$(cat "${BUILD}/target/.toolchain-key" 2> /dev/null)" != "${KEY}" ]; then
    mkdir -p "${BUILD}/target"
    find "${BUILD}/target" -mindepth 1 -delete
    echo "${KEY}" > "${BUILD}/target/.toolchain-key"
fi
"${REPO}/infra/packaging/server.sh" "${BUILD}/server-root"
install -Dm755 "${BUILD}/server-root/usr/bin/selkies" "${TREE}/bin/selkies"
install -Dm755 "${BUILD}/server-root/usr/bin/selkies-desktop" "${TREE}/bin/selkies-desktop"

# PulseAudio with the modules Selkies loads, and dbus-daemon
install -m755 /usr/bin/pulseaudio "${TOOLCHAIN}/dbus/bin/dbus-daemon" "${TREE}/libexec/selkies/"
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
cp "${TOOLCHAIN}/dbus/share/dbus-1/session.conf" "${TREE}/share/selkies/dbus-1/"
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
cp -L "${TOOLCHAIN}/xservers/deps/lib/libwayland-server.so.0" "${TREE}/lib/selkies/"

# NVIDIA's X11 EGL platform, which the images install where a driver's EGL looks
# (addons/base): nothing of a bundle on a host loads it, whose driver brings its own
if [ "${LIBC}" = gnu ]; then
    "${REPO}/infra/bundle/egl-x11.sh" "${TOOLCHAIN}/egl-x11"
    mkdir -p "${TREE}/lib/selkies/egl-x11" "${TREE}/share/selkies/egl"
    cp "${TOOLCHAIN}"/egl-x11/lib/libnvidia-egl-*.so.1 "${TREE}/lib/selkies/egl-x11/"
    cp -r "${TOOLCHAIN}/egl-x11/share/egl/egl_external_platform.d" "${TREE}/share/selkies/egl/"
fi

# The interposers the session's applications preload, and their 32-bit builds
# where the image has a 32-bit toolchain. The webcam interposer's PipeWire frame
# source builds against the toolchain's PipeWire headers, its library opened at
# run time.
"${REPO}/infra/bundle/pipewire.sh" "${TOOLCHAIN}/pipewire"
PW_CFLAGS="-I${TOOLCHAIN}/pipewire/include/pipewire-0.3 -I${TOOLCHAIN}/pipewire/include/spa-0.2"
build_interposers() {
    suffix="$1"
    shift
    gcc "$@" -O2 -shared -fPIC -o "${TREE}/lib/selkies/preload/selkies_input_interposer${suffix}.so" \
        "${REPO}/addons/input-interposer/input_interposer.c" -ldl -pthread
    rm -rf "${BUILD}/v4l2-interposer"
    cp -r "${REPO}/addons/v4l2-interposer" "${BUILD}/v4l2-interposer"
    if [ -z "${suffix}" ]; then v4l2=selkies_v4l2_interposer.so; else v4l2=all32; fi
    make -C "${BUILD}/v4l2-interposer" PW_CFLAGS="${PW_CFLAGS}" "${v4l2}" > /dev/null
    built="$(find "${BUILD}/v4l2-interposer" -maxdepth 1 -name 'selkies_v4l2_interposer*.so')"
    grep -qF libpipewire-0.3.so.0 "${built}"
    install -m755 "${built}" "${TREE}/lib/selkies/preload/selkies_v4l2_interposer${suffix}.so"
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
# C++ runtime, and the graphics stack, each copied once under its soname: the
# ones this build and the toolchain made (libcrypt.so.1 is cups.sh's libxcrypt,
# in the tree already), never one a package of the image's distribution owns
# or the image built itself under /usr/local (manylinux2014's libcrypt), but
# for musl's C++ runtime, which comes with the distribution's compiler.
provider() {
    case "$1" in
        /usr/local/*) echo "the builder image's own build" ;;
        *)
            if command -v rpm > /dev/null; then
                rpm -qf "$1" > /dev/null 2>&1 && echo "the distribution's $(rpm -qf "$1")"
            elif command -v apk > /dev/null; then
                apk info -W "$1" 2> /dev/null | sed -n "s/.* is owned by /the distribution's /p"
            fi
            ;;
    esac
    return 0
}
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
            from="$(provider "$(readlink -f "${path}")")"
            if [ -n "${from}" ] && ! echo "${soname}" | grep -qE '^lib(stdc\+\+|gcc_s)\.so'; then
                echo "${elf} links ${path}, from ${from}" >&2
                exit 1
            fi
            cp -L "${path}" "${dest}"
            chmod 755 "${dest}"
            next="${next} ${dest}"
        done
    done
    queue="${next}"
done
# The run paths: rewritten in place where a library has one (patchelf grows a
# file by its whole alignment to add one), and the sound server's as an RPATH,
# which its libraries' own lookups take too; it shares no process with a GPU
# driver. dbus-daemon links the C library alone.
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
patchelf --force-rpath --set-rpath "\$ORIGIN/../../lib/selkies" "${TREE}/libexec/selkies/pulseaudio"
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

echo "${KEY}" > "${TOOLCHAIN}/key"
tar -C "${BUILD}/bundle" -czf "${OUT}/${NAME}.tar.gz" "${NAME}"
# The wheel of this platform, the bundle repackaged (crates/xtask/src/wheel.rs)
# by a tool built from the tree in place, without the server's link flags; on
# musl with the C runtime linked dynamically, as server.sh builds there
XTASK_RUSTFLAGS=""
[ "${LIBC}" = gnu ] || XTASK_RUSTFLAGS="-C target-feature=-crt-static"
RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}" RUSTFLAGS="${XTASK_RUSTFLAGS}" CARGO_TARGET_DIR="${BUILD}/target" \
    "${CARGO_HOME:-/opt/rust/cargo}/bin/cargo" run --quiet --locked --manifest-path "${REPO}/Cargo.toml" -p xtask -- wheel "${OUT}/${NAME}.tar.gz" "${OUT}"
ls -la "${OUT}/${NAME}.tar.gz" "${OUT}"/*.whl
