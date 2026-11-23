# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# What every start of Google Chrome gets in the desktop image. Chrome's own
# launcher (/opt/google/chrome/google-chrome) sources this just before it
# execs the browser, so a menu entry, xdg-open, and a shell all start it the
# same way, and it passes SELKIES_CHROME_FLAGS ahead of the caller's arguments:
#
# - --no-sandbox where the container denies Chrome's sandbox, which needs
#   CAP_SYS_ADMIN for its setuid helper or unprivileged user namespaces for its
#   zygote: a container's default seccomp profile grants neither, and a stock
#   Chrome aborts at the zygote (the container is the isolation boundary
#   there). The namespaces the zygote creates are asked for at every start, so
#   the sandbox stays wherever they are granted, as under an unconfined seccomp
#   profile.
# - --password-store=basic, because the session runs no keyring daemon to
#   unlock and Chrome otherwise blocks on a prompt nobody can answer.
# - --ozone-platform=wayland where the session has a Wayland display, so Chrome
#   draws on the compositor itself rather than through XWayland.
# - --force-renderer-accessibility while the session asks its applications to
#   describe themselves (SELKIES_ACCESSIBILITY=1, set while Pelorus reads the
#   windows): Chrome fills its accessibility tree only when started with it,
#   and the window manager's menus and the shell add it to their own launches
#   but not to a terminal's, xdg-open's, or the autostart file's.
#
# It also clears the profile's singleton lock when no Chrome of this user is
# running. The lock names the host and the process that held it, and a
# container that stopped uncleanly comes back under another hostname, where
# Chrome takes the leftover lock for a browser running on another computer and
# refuses to open the profile at all.

SELKIES_CHROME_FLAGS=(--password-store=basic)
if ! unshare --user --map-root-user --pid --net --fork true 2> /dev/null; then
    SELKIES_CHROME_FLAGS+=(--no-sandbox)
fi
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
    case "${WAYLAND_DISPLAY}" in
        /*) selkies_wayland_socket="${WAYLAND_DISPLAY}" ;;
        *) selkies_wayland_socket="${XDG_RUNTIME_DIR:-/nonexistent}/${WAYLAND_DISPLAY}" ;;
    esac
    if [ -S "${selkies_wayland_socket}" ]; then
        SELKIES_CHROME_FLAGS+=(--ozone-platform=wayland)
    fi
    unset selkies_wayland_socket
fi
if [ "${SELKIES_ACCESSIBILITY:-}" = 1 ]; then
    SELKIES_CHROME_FLAGS+=(--force-renderer-accessibility)
fi
if ! pgrep -u "$(id -u)" -x chrome > /dev/null 2>&1; then
    rm -f "${CHROME_CONFIG_HOME:-${XDG_CONFIG_HOME:-${HOME}/.config}}/google-chrome/Singleton"*
fi
