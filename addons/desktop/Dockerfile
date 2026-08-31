# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Reference minimal desktop container for Selkies: the desktop environment on
# top of the Selkies base container.
#
# The base (addons/base) is the whole session apart from what it looks like —
# X11 and the Wayland backend, audio, GPU wiring, s6, coTURN and Selkies
# itself. This layer adds only the desktop: LXQt with the Openbox window
# manager on X11 and natively on the nested labwc compositor under
# SELKIES_WAYLAND=true, the browsers, and the proot-apps runner behind the
# dashboards' apps panel.
#
# BASE_IMAGE is a whole reference — tag or digest — so this layer follows
# whichever base container it is pointed at: the published Ubuntu 26.04 one by
# default, a Debian Trixie one, or the exact digest a CI run just built. Both
# published bases carry LXQt 2.1 or newer, which anchors its panel and desktop
# through wlr-layer-shell: the Wayland session then runs natively, with none of
# the XWayland detour older releases need.

ARG BASE_IMAGE="ghcr.io/selkies-project/selkies/base:main-ubuntu26.04"

FROM ${BASE_IMAGE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies Desktop Container"
LABEL org.opencontainers.image.description="Reference minimal Selkies container: LXQt on X11 by default, the same desktop on headless Wayland (labwc) with SELKIES_WAYLAND=true, s6 service supervision, embedded coTURN"
LABEL org.opencontainers.image.source="https://github.com/selkies-project/selkies"
LABEL org.opencontainers.image.licenses="MPL-2.0"

ARG DEBIAN_FRONTEND="noninteractive"

# The base ships its setuid and setgid files owned by root, and dpkg replaces a
# file by hardlinking the old one aside first -- which the kernel denies uid
# 1000 on a setuid file it does not own. Released for the layers below, an
# archive update to util-linux, shadow, sudo, fuse3 or dbus is just another
# package; without this it fails the layer that takes it.
USER 0
SHELL ["/bin/sh", "-c"]
# Copied rather than taken from the base, so this image builds on one published
# before the base carried it
COPY --chown=1000:1000 addons/base/selkies-privileged-files /usr/local/bin/selkies-privileged-files
RUN selkies-privileged-files release

# Rootless like the base: layers run as the session user through fakeroot, so
# the package database and apt cache stay owned by that user and in-session
# package management behaves like `/usr/bin/sudo` (the fakeroot alias) does at
# runtime. Real-root operations in the live session use /usr/bin/sudo-root.
USER 1000
SHELL ["/usr/bin/fakeroot", "--", "/bin/sh", "-c"]

RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        # Minimal LXQt desktop (Openbox window manager); breeze is the LXQt 2
        # icon set and actually ships the glyph groups the slim Adwaita build lacks
        lxqt-session \
        lxqt-panel \
        lxqt-runner \
        breeze-icon-theme \
        lxqt-about \
        openbox \
        obconf-qt \
        qterminal \
        pcmanfm-qt \
        # qterminal is the session's terminal; xterm backs the callers that spawn
        # a terminal by hardcoded name (libfm-qt, GLib's Terminal=true handling)
        xterm \
        # Recommends of the packages above, named here because the image installs
        # none of them by default and each is load-bearing rather than optional:
        # lxqt-qtplugin is the platform theme QT_QPA_PLATFORMTHEME=lxqt loads, so
        # without it no Qt application picks up the session's icons or palette;
        # lxqt-config supplies the settings backend the session reads; and an icon
        # theme is named by LXQt rather than shipped, so every icon is blank
        # without one. The notification and PolicyKit agents are what applications
        # expect a session to provide. pavucontrol-qt is the panel volume plugin's
        # mixer, and lximage-qt the image handler and screenshot tool. Deliberately
        # still dropped: screen lockers and power management (a container session
        # suspends nothing), the gvfs mount backends, and the KDE portal.
        adwaita-icon-theme \
        # A file manager keeps whether a desktop entry may be launched in GIO
        # metadata: gvfs carries the client module that implements that
        # attribute namespace, gvfs-daemons the store behind it. Without both,
        # every entry on the desktop is drawn as untrusted, however it got
        # there.
        gvfs \
        gvfs-daemons \
        # notify-send, for the notice the replaced leave binary shows
        libnotify-bin \
        lxqt-qtplugin \
        lxqt-config \
        lxqt-notificationd \
        lxqt-policykit \
        pavucontrol-qt \
        lximage-qt \
        # Translations for each shipped component, which are Recommends too
        lxqt-about-l10n \
        lxqt-config-l10n \
        lxqt-notificationd-l10n \
        lxqt-panel-l10n \
        lxqt-policykit-l10n \
        lxqt-runner-l10n \
        lxqt-session-l10n \
        obconf-qt-l10n \
        pcmanfm-qt-l10n \
        qterminal-l10n \
        # What the LXQt session needs to draw on Wayland: the Qt platform plugin
        # with its xdg-shell integration, and LayerShellQt, which is what anchors
        # the panel and the desktop to their edges. Without the latter both are
        # surfaces with nothing to anchor to and paint nothing at all
        qt6-wayland \
        layer-shell-qt \
        # The session's own portal backend, which answers the file-chooser and
        # screen-share requests toolkits make of a desktop
        xdg-desktop-portal-lxqt \
        # Application menu definitions for the panel and the file manager, which
        # lxqt-panel recommends rather than depends on: without them the session's
        # application menus are empty
        lxqt-menu-data \
        # Qt 6 splits SVG rendering into qt6-svg-plugins with nothing depending on
        # it. Without the SVG icon engine every scalable icon in the session — menu
        # entries, the panel's menu button, tray icons — is silently blank
        qt6-svg-plugins \
        # Qt reads only PNG, JPEG and GIF on its own; the rest of what a clipboard
        # image or a downloaded file arrives as (WebP, TIFF, AVIF, HEIF) needs
        # these, and without them lximage-qt and the file manager show nothing
        qt6-image-formats-plugins \
        kimageformat6-plugins \
        # proot-apps refreshes the icon cache of the per-user theme it installs
        # into by calling gtk-update-icon-cache, which nothing else here brings
        # in; without it that refresh is a silent no-op and the cache a session
        # reads is whatever was there before the application arrived
        gtk-update-icon-cache && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# LXQt's system defaults, which the distribution packages do not ship: without
# them a session has no theme, so lxqt-panel finds no stylesheet, keeps the
# invalid colours it writes as its own default, and paints nothing at all — a
# panel that runs and maps a window but is invisible against the desktop. The
# icon theme goes with it, since LXQt otherwise falls back to Oxygen, which is
# not installed here. This is the system scope, so a user's own
# ~/.config/lxqt/lxqt.conf still overrides it. The names are taken from what is
# installed rather than asserted, so a renamed or dropped theme still leaves a
# session that paints; only an image with none of them at all fails here.
#
# pcmanfm-qt gets its desktop background the same way, for the same reason: its
# own default is #000000 with no wallpaper, which reads as a broken session
# rather than a plain one. A flat colour rather than one of the shipped
# wallpapers, because every full refresh re-encodes whatever the desktop shows
# and a photograph costs far more of the stream than a single tone.
# Both names are taken from what is installed rather than asserted, so a
# distribution that renames or drops one still produces a session that paints.
#
# The terminal goes with them: libfm-qt compiles in `xterm` and libqt5xdg/libqt6xdg
# ship /etc/xdg/qtxdg.conf naming xterm.desktop, so "Open Terminal" and every
# Terminal=true launcher reach for the wrong one. Only the LXQt scope
# (lxqt-qtxdg.conf) is packaged correctly.
RUN set -eu; \
    first_installed() { \
        base="$1"; shift; \
        for name in "$@"; do [ -d "${base}/${name}" ] && { printf '%s' "${name}"; return; }; done; \
    }; \
    first_on_path() { \
        for name in "$@"; do command -v "${name}" >/dev/null 2>&1 && { printf '%s' "${name}"; return; }; done; \
    }; \
    ICON_THEME="$(first_installed /usr/share/icons Adwaita breeze oxygen hicolor)"; \
    THEME="$(first_installed /usr/share/lxqt/themes ambiance system light Leech)"; \
    TERMINAL="$(first_on_path qterminal x-terminal-emulator xterm)"; \
    echo "LXQt defaults: icon_theme=${ICON_THEME} theme=${THEME} terminal=${TERMINAL}"; \
    mkdir -pm755 /etc/xdg/lxqt /etc/xdg/pcmanfm-qt/lxqt; \
    printf '[General]\nicon_theme=%s\ntheme=%s\n' "${ICON_THEME}" "${THEME}" > /etc/xdg/lxqt/lxqt.conf; \
    printf '[Desktop]\nWallpaperMode=color\nBgColor=#2e3436\nFgColor=#ffffff\n[System]\nTerminal=%s\n' \
        "${TERMINAL}" > /etc/xdg/pcmanfm-qt/lxqt/settings.conf; \
    printf 'TerminalEmulator=%s.desktop\n' "${TERMINAL}" > /etc/xdg/qtxdg.conf

# The browsers, in one apt operation: Firefox from Mozilla's own APT repository,
# which serves real .deb builds on both bases and is pinned over the
# distribution package, and Google Chrome as a downloaded .deb for the
# H.264/AV1 media stack the distribution Chromium builds do not carry. Chrome's
# launcher is switched to the basic password store because the container runs no
# keyring daemon to unlock, and it otherwise blocks on a prompt nobody can answer.
RUN mkdir -pm755 /etc/apt/keyrings /etc/apt/sources.list.d /etc/apt/preferences.d && \
    curl -o /etc/apt/keyrings/packages.mozilla.org.asc -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 "https://packages.mozilla.org/apt/repo-signing-key.gpg" && \
    printf 'Types: deb\nURIs: https://packages.mozilla.org/apt\nSuites: mozilla\nComponents: main\nSigned-By: /etc/apt/keyrings/packages.mozilla.org.asc\n' > /etc/apt/sources.list.d/mozilla.sources && \
    printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla && \
    curl -o /tmp/google-chrome-stable.deb -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 "https://dl.google.com/linux/direct/google-chrome-stable_current_$(dpkg --print-architecture).deb" && \
    apt-get clean && apt-get update && \
    apt-get install --no-install-recommends -y firefox /tmp/google-chrome-stable.deb && \
    sed -i '/^Exec=/ s/$/ --password-store=basic/' /usr/share/applications/google-chrome.desktop && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# proot-apps (https://github.com/linuxserver/proot-apps) backs the dashboards'
# apps panel: portable per-user applications in a proot, installed and removed
# through the selkies-proot wrapper without touching the image. The release
# tarball is staged here and linked into each user's ~/.local/bin, because
# proot-apps runs its tools from there by absolute path. proot traces every
# process it starts, so a second copy carries CAP_SYS_PTRACE for the hosts that
# restrict ptrace; it is kept apart because a file capability the container's
# bounding set does not hold stops that binary from execing at all, and the
# wrapper picks whichever of the two runs.
RUN mkdir -pm755 /opt/proot-apps && \
    PAPPS_RELEASE="$(curl -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 -o /dev/null -w '%{url_effective}' "https://github.com/linuxserver/proot-apps/releases/latest" | sed 's|.*/||')" && \
    curl -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 "https://github.com/linuxserver/proot-apps/releases/download/${PAPPS_RELEASE}/proot-apps-$(dpkg --print-architecture | sed 's/amd64/x86_64/;s/arm64/aarch64/').tar.gz" \
        | tar -xzf - -C /opt/proot-apps/ && \
    printf '%s\n' "${PAPPS_RELEASE}" > /opt/proot-apps/pversion && \
    if ! command -v setcap > /dev/null; then \
        apt-get update && apt-get install --no-install-recommends -y libcap2-bin && \
        apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*; \
    fi && \
    mkdir -pm755 /opt/proot-apps-cap && \
    cp /opt/proot-apps/proot /opt/proot-apps-cap/proot && \
    setcap cap_sys_ptrace+ep /opt/proot-apps-cap/proot
COPY --chown=1000:1000 addons/desktop/selkies-proot /usr/local/bin/selkies-proot
RUN chmod -f 755 /usr/local/bin/selkies-proot

# Titlebar buttons for applications that draw their own client-side decoration.
# The session runs no GNOME settings layer, so the compiled-in default applies
# and it hides minimize and maximize: GNOME's button-layout default is
# appmenu:close, which Firefox and other client-decorated GTK applications
# adopt verbatim for their titlebars. The override changes just the default, so
# a user's explicit settings still win.
# glib-compile-schemas fails the whole build below when the overridden schema is
# not on the base system, so its package is installed when nothing pulled it in.
RUN set -eu; \
    if [ ! -f /usr/share/glib-2.0/schemas/org.gnome.desktop.wm.preferences.gschema.xml ]; then \
        apt-get update && apt-get install --no-install-recommends -y gsettings-desktop-schemas && \
        apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*; \
    fi; \
    printf "[org.gnome.desktop.wm.preferences]\nbutton-layout=':minimize,maximize,close'\n" \
        > /usr/share/glib-2.0/schemas/selkies-decoration.gschema.override && \
    glib-compile-schemas /usr/share/glib-2.0/schemas/ && \
    if [ -f /etc/gtk-3.0/settings.ini ] && grep -q '^\[Settings\]' /etc/gtk-3.0/settings.ini; then \
        sed -i '/^\[Settings\]/a gtk-decoration-layout=:minimize,maximize,close' /etc/gtk-3.0/settings.ini; \
    else \
        mkdir -pm755 /etc/gtk-3.0 && \
        printf '[Settings]\ngtk-decoration-layout=:minimize,maximize,close\n' > /etc/gtk-3.0/settings.ini; \
    fi

# Leaving the session is not something this desktop can offer: logging out or
# locking ends or blocks the session the stream is showing and leaves a
# connected client on a dead display, and shutdown, reboot, suspend and
# hibernate address an init this container does not run.
#
# Every one of those entries runs the one lxqt-leave binary, and the panel's own
# menu offers a button that runs it directly rather than through an entry, so
# both halves are covered: the entries are hidden so nothing offers the actions,
# and the binary is replaced so nothing performs them.
#
# Which entries those are is read from the entries themselves rather than
# listed, so one renamed or added upstream is covered too. Hiding them uses the
# XDG rule that the first entry of a given file name across XDG_DATA_DIRS wins,
# and /usr/local/share precedes /usr/share; replacing the binary uses dpkg's own
# diversion, so an upgrade of lxqt-session reinstalls to the diverted path and
# leaves this in place. No lxqt-panel release exposes a setting for the button.
RUN mkdir -pm755 /usr/local/share/applications && \
    grep -lE '^Exec=[^=]*lxqt-leave' /usr/share/applications/*.desktop | while read -r entry; do \
        hidden="/usr/local/share/applications/$(basename "${entry}")"; \
        sed -e '/^NoDisplay=/d' -e '/^\[Desktop Entry\]/a NoDisplay=true' \
            "${entry}" > "${hidden}"; \
    done && \
    if [ -x /usr/bin/lxqt-leave ]; then \
        dpkg-divert --divert /usr/bin/lxqt-leave.real --rename /usr/bin/lxqt-leave && \
        printf '#!/bin/sh\n# See the Dockerfile: this desktop has no session to leave.\nnotify-send -i dialog-information "Not available here" "This desktop runs in a container: there is no session to log out of, lock, or shut down." 2>/dev/null || true\nexit 0\n' \
            > /usr/bin/lxqt-leave && \
        chmod 755 /usr/bin/lxqt-leave; \
    fi

# The session's own s6 service, supervised beside the base container's
COPY --chown=1000:1000 addons/desktop/services/ /etc/service/
RUN find /etc/service \( -name run -o -name finish \) -exec chmod -f 755 {} +

RUN printf "==============================================\n=         Selkies Desktop Container          =\n==============================================\n" > /etc/motd

USER 0
# Real root for the same reason the base needs it: under the fakeroot SHELL an
# ownership change lands only in fakeroot's database, leaving a setuid bit on a
# file owned by uid 1000, which the kernel refuses to honour. Without a
# root-owned chrome-sandbox Chrome aborts at launch wherever unprivileged user
# namespaces are unavailable (the default Docker seccomp profile), and xterm
# brings a setgid utempter. `restore` covers everything released above, which
# is the base's own set rather than these.
SHELL ["/bin/sh", "-c"]
RUN selkies-privileged-files restore && \
    for helper in /opt/google/chrome/chrome-sandbox /usr/lib/polkit-1/polkit-agent-helper-1 /usr/libexec/polkit-agent-helper-1 /usr/bin/pkexec; do \
        if [ -e "$helper" ]; then chown -f root:root "$helper" && chmod -f 4755 "$helper" || echo "Failed to restore setuid for $helper"; fi; \
    done && \
    for helper in /usr/lib/*/utempter/utempter /usr/libexec/utempter/utempter; do \
        if [ -e "$helper" ]; then chown -f root:utmp "$helper" && chmod -f 2755 "$helper" || echo "Failed to restore setgid for $helper"; fi; \
    done

USER 1000

# The dashboards' apps panel drives proot-apps through the command channel;
# without this the server tells clients the panel is unusable and they drop it.
# Commands run as the session user, whom the streamed desktop already hands a
# terminal, so the desktop image enables the channel; disable with
# -e SELKIES_COMMAND_ENABLED=false.
ENV SELKIES_COMMAND_ENABLED="true"
# Start the LXQt session (set to false to run only the display server)
ENV START_LXQT="true"
# This session is the image's to manage, and Openbox is the window manager it
# ships: a desktop that tiles a maximized window across the whole framebuffer
# rather than the per-display regions is handed to it when a second display
# opens. Selkies names none by default, since a session on somebody's own
# desktop is not one to restart window management in.
ENV SELKIES_MULTI_MONITOR_WM="openbox --replace"
