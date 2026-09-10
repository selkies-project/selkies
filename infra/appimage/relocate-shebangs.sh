#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Point the entry points of a bundled conda prefix at the interpreter that
# prefix carries, rather than at the path it was installed to.
#
# conda and pip write that build path into every script they generate, in one
# of three shapes: a plain shebang, and two spellings of the `/bin/sh` + `exec`
# prologue conda gives an entry point whose shebang the kernel could not carry.
# An AppImage mounts at a path chosen at run time, so a script left as
# generated dies with `exec: <build path>/bin/python: not found`, whether it is
# started through AppRun or by hand from an extracted tree. Resolving the
# interpreter from the script's own location holds in both, and in a prefix
# that was merely moved.
#
# A shebang naming an interpreter other than python takes no such prologue, so
# it is reported and fails the caller rather than shipping a script that cannot
# start.
#
# Usage: relocate-shebangs.sh <prefix>

set -eu

PREFIX="$(readlink -f "${1:?usage: relocate-shebangs.sh <prefix>}")"

# The interpreter is named from the prefix root rather than from the script's
# own directory, which condabin/ is not, and through `readlink -f "$0"` rather
# than `dirname "$0"`, the entry points being reached through symlinks too: the
# AppDir's usr/bin holds one per script.
# shellcheck disable=SC2016  # this expression is written into the scripts, not run here
LOCATE='"$(dirname "$(dirname "$(readlink -f "$0")")")/bin/'

status=0
for dir in bin condabin; do
    [ -d "${PREFIX}/${dir}" ] || continue
    for path in "${PREFIX}/${dir}"/*; do
        [ -f "${path}" ] || continue
        [ "$(head -c 2 "${path}" 2>/dev/null)" = '#!' ] || continue

        sed -i \
            -e "1s|^#\!${PREFIX}/bin/\(python[0-9.]*\)\(.*\)|#!/bin/sh\n'''exec' ${LOCATE}\1\"\2 \"\$0\" \"\$@\"\n' '''|" \
            -e "2s|^'''exec' \"\{0,1\}${PREFIX}/bin/\(python[0-9.]*\)\"\{0,1\} |'''exec' ${LOCATE}\1\" |" \
            "${path}"

        launcher="$(head -2 "${path}" | grep -e '^#!' -e "^'''exec' " | tr '\n' ' ' || true)"
        case "${launcher}" in
            *"${PREFIX}"*)
                echo "ERROR: ${path##*/} still starts ${launcher}" >&2
                status=1
                ;;
        esac
    done
done
exit "${status}"
