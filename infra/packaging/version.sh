#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Native spellings of the wheel's PEP 440 version, for the packaging scripts
# that need one.
#
# dpkg and rpm sort `~` below every other separator, so a pre-release built as
# 2.0.0~rc0 is superseded by the final 2.0.0 instead of outranking it. Alpine
# cannot parse that spelling and orders its own `_alpha`/`_beta`/`_rc` suffixes
# the same way. pacman needs no translation: it already sorts a trailing
# alphabetic suffix below the bare release, and a tilde would only push
# 2.0.0~rc0 above 2.0.0. Post-releases outrank the plain version everywhere.

# 2.0.0rc0 -> 2.0.0~rc0
tilde_version() {
    printf '%s' "$1" | sed -E 's/^([0-9]+(\.[0-9]+)*)\.?(a|b|rc|dev)([0-9]+)$/\1~\3\4/'
}

# 2.0.0rc0 -> 2.0.0_rc0, and 0.0.0.dev0 -> 0.0.0_alpha0 for want of a dev suffix
alpine_version() {
    printf '%s' "$1" | sed -E \
        -e 's/^([0-9]+(\.[0-9]+)*)\.?a([0-9]+)$/\1_alpha\3/' \
        -e 's/^([0-9]+(\.[0-9]+)*)\.?b([0-9]+)$/\1_beta\3/' \
        -e 's/^([0-9]+(\.[0-9]+)*)\.?rc([0-9]+)$/\1_rc\3/' \
        -e 's/^([0-9]+(\.[0-9]+)*)\.?dev([0-9]+)$/\1_alpha\3/' \
        -e 's/^([0-9]+(\.[0-9]+)*)\.?post([0-9]+)$/\1_p\3/'
}
