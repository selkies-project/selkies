#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Run a bare `selkies` the way a user does after `pip install selkies` and check
# it. The standalone desktop (crates/selkies/src/commands/standalone.rs) carries
# no login of its own: with no password set it has to refuse to start, saying
# how to name one, before any of its session comes up. Given a password file, it
# has to say where it is, serve HTTPS on localhost:8080 behind that login,
# refuse a request without it, and keep its desktop shell running: on a host
# with nothing installed but the wheel, a shell that cannot find a font or a
# library exits and is restarted, which the log shows. Where localhost resolves
# to ::1 the entry page is moved to 127.0.0.1 before the login, which curl
# follows with the login kept.
#
# Usage: verify-standalone.sh <selkies-executable>   (curl on PATH)
set -eu
SELKIES="${1:?usage: verify-standalone.sh <selkies-executable>}"
URL="https://localhost:8080/"
SESSION_HOME="$(mktemp -d "${TMPDIR:-/tmp}/selkies-standalone-XXXXXX")"
LOG="${SESSION_HOME}/selkies.log"
USER_NAME="standalone"
PASSWORD_FILE="${SESSION_HOME}/password"

# The status of a GET of the entry page; a login where one is given. curl
# writes 000 itself when nothing answers.
status() {
    curl -sk -L --location-trusted --max-time 10 -o /dev/null -w '%{http_code}' "$@" "${URL}" \
        || true
}

fail() {
    echo "::error::$1; the log:"
    tail -n 30 "${LOG}"
    exit 1
}

# Become a bare selkies in an environment where nothing of the caller's session
# steers it, and no login is set, with the variables given as arguments added.
# It replaces the shell it runs in, so it runs in a subshell of its own: in the
# foreground in parentheses, and in the background as `&` makes one, whose
# process is then selkies itself for `$!`.
bare() {
    for name in $(env | sed -nE 's/^((SELKIES_|XDG_|DISPLAY|WAYLAND|PASSWORD|PASSWD|CUSTOM_USER)[A-Za-z0-9_]*)=.*/\1/p'); do
        unset "${name}" 2> /dev/null || true
    done
    cd "${SESSION_HOME}"
    exec env HOME="${SESSION_HOME}" "$@" "${SELKIES}"
}

# With no password set, it refuses before its session starts: the status that
# names a configuration error, the way to name one, and nothing listening
exited=0
(bare) > "${LOG}" 2>&1 || exited=$?
[ "${exited}" = 78 ] || fail "with no password set it exited ${exited}, not 78"
grep -qF "no password was set" "${LOG}" || fail "with no password set it does not say how to name one"
grep -qF "SELKIES_BASIC_AUTH_PASSWORD_FILE" "${LOG}" || fail "the refusal does not name SELKIES_BASIC_AUTH_PASSWORD_FILE"
[ "$(status)" = 000 ] || fail "with no password set something answers on ${URL}"

# A password of the user's own, in a file only they can read; a SIGTERM stops
# the desktop with its session, which it tears down itself
(umask 077 && head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n' > "${PASSWORD_FILE}")
LOGIN="${USER_NAME}:$(cat "${PASSWORD_FILE}")"
bare SELKIES_BASIC_AUTH_USER="${USER_NAME}" SELKIES_BASIC_AUTH_PASSWORD_FILE="${PASSWORD_FILE}" > "${LOG}" 2>&1 &
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
grep -qF "your desktop is at ${URL%/}, signing in as ${USER_NAME} with the password in ${PASSWORD_FILE}" "${LOG}" \
    || fail "the log does not say where the desktop is and how to sign in"
grep -qF "${LOGIN#*:}" "${LOG}" && fail "the log names the password"
[ "$(status -u "${LOGIN}")" = 200 ] || fail "the login from the password file does not sign in"
sleep 10
grep -qF "selkies-desktop started" "${LOG}" || fail "the desktop shell never started"
if grep -qE "selkies-desktop exited|panicked" "${LOG}"; then
    fail "the desktop shell exited"
fi
echo "a bare selkies refuses a login with no password, and serves its desktop at ${URL} behind the one it is given"
