#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the native package in /out and prove what it lays down works with
# nothing but the package's own declared dependencies.
#
# The build container carries a whole toolchain, so a runtime library the
# package forgot to declare is satisfied there and missing on a user's machine.
# This runs in a pristine image of the same distribution instead, and checks
# what a bare `--help` does not: that every library the server and the
# interposers link resolves, that the libraries pixelflux and pcmflux open by name
# at runtime (no ELF dependency names them) are present, and that the server
# serves the web client compiled into it.
#
# $INSTALL is the distribution's install command for /out/*, from the packages
# workflow matrix.

set -eu

sh -c "${INSTALL}" > /dev/null

selkies --version
selkies --help > /dev/null
echo "selkies --version and --help run"

selkies --version | grep -F "web client: embedded (" || {
    echo "::error::the server embeds no web client"
    exit 1
}

interposers="$(find /usr/lib /usr/lib64 -name 'selkies_*_interposer.so' 2>/dev/null || true)"
[ -n "${interposers}" ] || {
    echo "::error::the package installed no interposers"
    exit 1
}
missing="$(for f in /usr/bin/selkies ${interposers}; do
    ldd "${f}" 2>/dev/null | grep 'not found' | sed "s|^|${f}: |"
done | sort -u)"
if [ -n "${missing}" ]; then
    echo "::error::installed files with unresolved dependencies:"
    echo "${missing}"
    exit 1
fi
echo "the server and the interposers resolve every library they link"

# Opened by name at runtime: pixelflux reaches for the Wayland server library,
# VA-API, and the GPU stack, so no ELF dependency names them and only a
# declared package dependency puts them on the system.
NEEDED="libwayland-server.so.0 libEGL.so.1 libgbm.so.1 libpulse.so.0 libva.so.2 libva-drm.so.2 libdrm.so.2 libxkbcommon.so.0 libpixman-1.so.0"
absent=""
for name in ${NEEDED}; do
    found=""
    for dir in /lib /lib64 /usr/lib /usr/lib64 /usr/lib/*-linux-gnu /lib/*-linux-gnu; do
        [ -e "${dir}/${name}" ] && found=1 && break
    done
    [ -n "${found}" ] || absent="${absent} ${name}"
done
if [ -n "${absent}" ]; then
    echo "::error::runtime-loaded libraries the package does not pull in:${absent}"
    exit 1
fi
echo "every runtime-loaded library is present"

# One GET of $2 from the server below with Accept-Encoding $3, printing the
# response's head where $1 is "head" and its body where it is "body": through
# curl, else BusyBox wget, else bash's /dev/tcp, whichever the pristine image
# carries.
PORT=18080
get() {
    url="http://127.0.0.1:${PORT}$2"
    if command -v curl > /dev/null; then
        if [ "$1" = head ]; then
            curl -sS -D - -o /dev/null -H "Accept-Encoding: $3" "${url}"
        else
            curl -sS -H "Accept-Encoding: $3" "${url}"
        fi
    elif command -v wget > /dev/null; then
        if [ "$1" = head ]; then
            wget -q -S -O /dev/null --header "Accept-Encoding: $3" "${url}" 2>&1
        else
            wget -q -O - --header "Accept-Encoding: $3" "${url}"
        fi
    else
        bash -c 'exec 3<>"/dev/tcp/127.0.0.1/$0"
            printf "GET %s HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept-Encoding: %s\r\n\r\n" "$1" "$2" >&3
            cat <&3' "${PORT}" "$2" "$3" | if [ "$1" = head ]; then sed '/^\r*$/q'; else sed '1,/^\r*$/d'; fi
    fi
}
# The server on the Wayland backend, which needs no X server, in a runtime
# directory of its own and without a login, which it otherwise refuses to start
# without a password for: its entry page, and a content-hashed script the page
# loads sent gzip-encoded with its entity tag to a client that takes gzip
RUNTIME="$(mktemp -d)"
chmod 700 "${RUNTIME}"
env XDG_RUNTIME_DIR="${RUNTIME}" SELKIES_WAYLAND=true SELKIES_PORT="${PORT}" \
    selkies --enable-basic-auth=false > "${RUNTIME}/selkies.log" 2>&1 &
server="$!"
i=0
until get body / identity 2> /dev/null | grep -q "selkies-core.js"; do
    i=$((i + 1))
    if [ "${i}" -ge 60 ] || ! kill -0 "${server}" 2> /dev/null; then
        echo "::error::the server never served its entry page; its log:"
        tail -n 20 "${RUNTIME}/selkies.log"
        kill "${server}" 2> /dev/null || true
        exit 1
    fi
    sleep 1
done
asset="$(get body / identity | grep -o 'assets/[^"]*\.js' | head -n 1)"
head="$(get head "/${asset}" gzip | tr -d '\r')"
kill "${server}" 2> /dev/null || true
wait "${server}" 2> /dev/null || true
if [ -z "${asset}" ] || ! printf '%s\n' "${head}" | grep -qi '^ *content-encoding: *gzip$' \
        || ! printf '%s\n' "${head}" | grep -qi '^ *etag: *"'; then
    echo "::error::the server did not send its client's ${asset:-scripts} gzip-encoded with a tag:"
    printf '%s\n' "${head}"
    exit 1
fi
echo "the server serves its entry page and ${asset}, gzip-encoded, from the client it embeds"

# Arch installs the CUPS scheduler readable by root alone; the package's pacman
# hook opens it on the cups install, which is what lets the print queue run a
# copy of it as the session user.
if command -v pacman > /dev/null; then
    pacman -S --noconfirm --needed cups > /dev/null
    mode="$(stat -c %a /usr/bin/cupsd)"
    [ "${mode}" = "755" ] || {
        echo "::error::the pacman hook left /usr/bin/cupsd at mode ${mode}"
        exit 1
    }
    echo "the pacman hook opened the CUPS scheduler"
fi
