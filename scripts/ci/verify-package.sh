#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the native package in /out and prove the bundle it lays down runs a
# session with nothing but what the package itself declares.
#
# This runs in a pristine image of a distribution the package targets, where
# nothing the bundle carries is installed: every program and library under
# /opt/selkies has to resolve what it links, the interposers have to be where
# `/usr/$LIB` finds them, and `selkies session`, run as an ordinary user, has to
# bring up its own buses, sound server, and X server and serve the web client
# the server embeds.
#
# $INSTALL is the distribution's install command for /out/*, from the packages
# workflow matrix.

set -eu

sh -c "${INSTALL}" > /dev/null

selkies --version
selkies --version | grep -F "web client: embedded (" || {
    echo "::error::the server embeds no web client"
    exit 1
}

# The programs, each with the closure its run path finds: a library the bundle
# carries resolves its own dependencies through the program that loads it
missing="$(find /opt/selkies/bin /opt/selkies/libexec -type f | while read -r f; do
    head -c4 "${f}" | grep -q ELF || continue
    ldd "${f}" 2>/dev/null | grep 'not found' | sed "s|^|${f}: |"
done | sort -u)"
if [ -n "${missing}" ]; then
    echo "::error::installed files with unresolved dependencies:"
    echo "${missing}"
    exit 1
fi
echo "every program the bundle carries resolves what it links"

interposer="$(find /usr/lib /usr/lib64 -maxdepth 2 -name selkies_input_interposer.so 2>/dev/null | head -n 1)"
[ -n "${interposer}" ] && [ -f "${interposer}" ] || {
    echo "::error::the package linked no interposer under the loader's library directory"
    exit 1
}
echo "the interposer preloads from ${interposer}"

# One GET of $2 from the session below with Accept-Encoding $3, printing the
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

# A session as an ordinary user, without a login, which the server otherwise
# refuses to start without a password for: its entry page, and a
# content-hashed script the page loads sent gzip-encoded with its entity tag to
# a client that takes gzip
useradd -m selkies-check 2> /dev/null || adduser -D selkies-check
mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix
LOG=/tmp/selkies-session.log
su selkies-check -c "exec selkies session --runtime-dir /tmp/selkies-check --session 'sleep 600' \
    -- --port=${PORT} --enable-basic-auth=false" > "${LOG}" 2>&1 &
session="$!"
i=0
until get body / identity 2> /dev/null | grep -q "selkies-core.js"; do
    i=$((i + 1))
    if [ "${i}" -ge 60 ] || ! kill -0 "${session}" 2> /dev/null; then
        echo "::error::the session never served its entry page; its log:"
        tail -n 30 "${LOG}"
        exit 1
    fi
    sleep 1
done
asset="$(get body / identity | grep -o 'assets/[^"]*\.js' | head -n 1)"
head="$(get head "/${asset}" gzip | tr -d '\r')"
# Each process's program as it was started: cmdline, since a container's root
# cannot read the executable link of another account's process
running="$(for proc in /proc/[0-9]*; do
    tr '\0' '\n' < "${proc}/cmdline" 2> /dev/null | head -n 1 || true
done)"
for program in Xvfb dbus-daemon pulseaudio; do
    printf '%s\n' "${running}" | grep -qx "/opt/selkies/libexec/selkies/${program}" || {
        echo "::error::the session runs no bundled ${program}; its log:"
        tail -n 30 "${LOG}"
        exit 1
    }
done
kill "${session}" 2> /dev/null || true
if [ -z "${asset}" ] || ! printf '%s\n' "${head}" | grep -qi '^ *content-encoding: *gzip$' \
        || ! printf '%s\n' "${head}" | grep -qi '^ *etag: *"'; then
    echo "::error::the server did not send its client's ${asset:-scripts} gzip-encoded with a tag:"
    printf '%s\n' "${head}"
    exit 1
fi
echo "the session runs the bundle's X server, buses, and sound server and serves ${asset}, gzip-encoded"
