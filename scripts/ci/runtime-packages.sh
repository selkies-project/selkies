#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Record the Debian packages whose libraries a build under PREFIX loads, or
# install the packages such a record names. A build restored from a cache
# comes back without the -dev packages that pulled its libraries in, and which
# libraries it links depends on what the build found on that release (a newer
# libliftoff, say), so the build writes down its own runtime closure and a
# cache hit installs it.
#
# Usage: runtime-packages.sh record|install <prefix>

set -eu

MODE="${1:?usage: runtime-packages.sh record|install <prefix>}"
PREFIX="${2:?usage: runtime-packages.sh record|install <prefix>}"
LIST="${PREFIX}/runtime-packages"

case "${MODE}" in
  record)
    # The libraries resolved the way the build runs them: through its rpath and
    # the system's, not a toolchain's LD_LIBRARY_PATH.
    find "${PREFIX}" -type f \( -perm -u+x -o -name '*.so*' \) -print0 \
      | xargs -0 -r env -u LD_LIBRARY_PATH ldd 2>/dev/null \
      | awk '$2 == "=>" && $3 ~ /^\// { print $3 }' | sort -u \
      | while read -r lib; do
          case "${lib}" in "${PREFIX}"/*) continue ;; esac
          # dpkg lists a file under the path its package shipped it at, which
          # can be the /lib or the /usr/lib side of whatever the loader resolved
          real="$(readlink -f "${lib}")"
          for path in "${real}" "${lib}" "/usr${real}" "${real#/usr}"; do
            dpkg -S "${path}" 2>/dev/null && break
          done || true
        done \
      | grep -v '^diversion ' | sed 's/: .*//; s/:[^,]*//g' | tr ',' '\n' | tr -d ' ' \
      | sort -u > "${LIST}"
    [ -s "${LIST}" ] || { echo "no packaged libraries found under ${PREFIX}" >&2; exit 1; }
    tr '\n' ' ' < "${LIST}"
    echo
    ;;
  install)
    [ -s "${LIST}" ] || { echo "${PREFIX} has no runtime package record" >&2; exit 1; }
    xargs -r -a "${LIST}" sudo apt-get install --no-install-recommends -y
    ;;
  *)
    echo "usage: runtime-packages.sh record|install <prefix>" >&2
    exit 2
    ;;
esac
