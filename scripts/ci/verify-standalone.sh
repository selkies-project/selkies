#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Run a bare `selkies` the way a user does after `pip install selkies` and check
# it. The standalone desktop (crates/selkies/src/commands/standalone.rs) has to
# say where it is, serve HTTPS on localhost:8080 behind the placeholder login the
# containers document, refuse a request without it, and keep its desktop shell
# running: on a host with nothing installed but the wheel, a shell that cannot
# find a font or a library exits and is restarted, which the log shows. Where
# localhost resolves to ::1 the entry page is moved to 127.0.0.1 before the
# login, which curl follows with the login kept.
#
# Usage: verify-standalone.sh <selkies-executable>   (curl on PATH)
set -eu
SELKIES="${1:?usage: verify-standalone.sh <selkies-executable>}"
URL="https://localhost:8080/"
LOGIN="ubuntu:mypasswd"
SESSION_HOME="$(mktemp -d "${TMPDIR:-/tmp}/selkies-standalone-XXXXXX")"
LOG="${SESSION_HOME}/selkies.log"

# The status of a GET of the entry page; a login where one is given
status() {
    curl -sk -L --location-trusted --max-time 10 -o /dev/null -w '%{http_code}' "$@" "${URL}" \
        || echo 000
}

fail() {
    echo "::error::$1; the log:"
    tail -n 30 "${LOG}"
    exit 1
}

# Nothing of the caller's session steers it; a SIGTERM stops it with its
# session, which it tears down itself
(
    for name in $(env | sed -nE 's/^((SELKIES_|XDG_|DISPLAY|WAYLAND)[A-Za-z0-9_]*)=.*/\1/p'); do
        unset "${name}" 2> /dev/null || true
    done
    cd "${SESSION_HOME}"
    HOME="${SESSION_HOME}" exec "${SELKIES}"
) > "${LOG}" 2>&1 &
SESSION=$!
stop() {
    kill -TERM "${SESSION}" 2> /dev/null || return 0
    for _ in $(seq 1 30); do
        kill -0 "${SESSION}" 2> /dev/null || break
        sleep 1
    done
    kill -KILL "${SESSION}" 2> /dev/null || true
    wait "${SESSION}" 2> /dev/null || true
}
trap stop EXIT

code=000
for _ in $(seq 1 90); do
    kill -0 "${SESSION}" 2> /dev/null || break
    code="$(status)"
    [ "${code}" = 401 ] && break
    sleep 1
done
[ "${code}" = 401 ] || fail "${URL} never asked for a login (last status ${code})"
grep -qF "your desktop is at ${URL%/}, signing in as ${LOGIN%%:*} with the placeholder password ${LOGIN#*:}" "${LOG}" \
    || fail "the log does not say where the desktop is and how to sign in"
[ "$(status -u "${LOGIN}")" = 200 ] || fail "the placeholder login does not sign in"
sleep 10
grep -qF "selkies-desktop started" "${LOG}" || fail "the desktop shell never started"
if grep -qE "selkies-desktop exited|panicked" "${LOG}"; then
    fail "the desktop shell exited"
fi
echo "a bare selkies serves its desktop at ${URL} behind the placeholder login"
