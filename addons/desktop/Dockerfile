# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Reference minimal desktop container for Selkies: the desktop environment on
# top of the Selkies base container.
#
# The base (addons/base) is the whole session apart from what it looks like —
# X11 and the Wayland backend, audio, GPU wiring, s6, coTURN, and Selkies
# itself. This layer adds only the desktop: selkies-desktop, Selkies' own
# panel, start menu, and desktop, which the server runs under its own window
# manager on either backend (SELKIES_DESKTOP, set here), the browsers, and the
# proot-apps runner behind the dashboards' apps panel. No other window manager
# runs in this image: the window manager is the server's, on X11 and on Wayland.
#
# BASE_IMAGE is a whole reference — tag or digest — so this layer follows
# whichever base container it is pointed at: the published Ubuntu 26.04 one by
# default, a Debian Trixie one, or the exact digest a CI run just built.

ARG BASE_IMAGE="ghcr.io/selkies-project/selkies/base:main-ubuntu26.04"

FROM ${BASE_IMAGE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies Desktop Container"
LABEL org.opencontainers.image.description="Reference minimal Selkies container: selkies-desktop under the Selkies window manager on X11 by default and on headless Wayland with SELKIES_WAYLAND=true, s6 service supervision, embedded coTURN"
LABEL org.opencontainers.image.source="https://github.com/selkies-project/selkies"
LABEL org.opencontainers.image.licenses="MPL-2.0"

ARG DEBIAN_FRONTEND="noninteractive"

# The base ships its setuid and setgid files owned by root, and dpkg replaces a
# file by hardlinking the old one aside first -- which the kernel denies uid
# 1000 on a setuid file it does not own. Released for the layers below, an
# archive update to util-linux, shadow, sudo, fuse3, or dbus is just another
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

# The wallpaper packages the second set below installs are held to a few with
# light and dark pictures, which dpkg keeps out on every later upgrade too.
RUN printf '%s\n' \
        'path-exclude=/usr/share/wallpapers/*' \
        'path-include=/usr/share/wallpapers/Next/*' \
        'path-include=/usr/share/wallpapers/Nuvole/*' \
        'path-include=/usr/share/wallpapers/Mountain/*' \
        'path-include=/usr/share/wallpapers/Flow/*' \
        'path-include=/usr/share/wallpapers/Kay/*' \
        > /etc/dpkg/dpkg.cfg.d/selkies-wallpapers
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        # The icon sets selkies-desktop, the window manager's menus, and GTK
        # resolve names
        # in: breeze ships the full-color glyph groups the slim Adwaita build
        # lacks, and both bring hicolor, every icon theme's fallback
        breeze-icon-theme \
        adwaita-icon-theme \
        # The GTK theme pair selkies-desktop switches applications between
        # with its light and dark scheme, and the platform themes Qt 5 and
        # Qt 6 draw through to follow GTK's theme and dark preference
        # (QT_QPA_PLATFORMTHEME below)
        breeze-gtk-theme \
        qt5-gtk-platformtheme \
        qt6-gtk-platformtheme \
        # The terminals: foot on Wayland, xfce4-terminal on X11, which
        # selkies-terminal picks between as the x-terminal-emulator below
        foot \
        xfce4-terminal \
        # A text editor, image viewer, calculator, and archive manager for the
        # files the desktop's file manager opens; they share xfce4-terminal's
        # libraries, so together they add little to it
        mousepad \
        ristretto \
        galculator \
        xarchiver \
        # The image's wallpaper: a package with light and dark pictures the
        # desktop switches between with its theme, linked below as the
        # wallpaper a session starts with, and four more of the same kind
        # to choose from out of the set the filter above trims
        breeze-wallpaper \
        plasma-workspace-wallpapers \
        # What a terminal session is expected to have beyond the base's
        # editors: completion, remote shells and copies, and the network and
        # process tools
        bash-completion \
        openssh-client \
        rsync \
        tree \
        psmisc \
        iproute2 \
        iputils-ping \
        # pactl, which selkies-desktop's volume control drives the sink through
        pulseaudio-utils \
        # A file manager keeps whether a desktop entry may be launched in GIO
        # metadata: gvfs carries the client module that implements that
        # attribute namespace, gvfs-daemons the store behind it. Without both,
        # every entry on the desktop is drawn as untrusted, however it got
        # there.
        gvfs \
        gvfs-daemons \
        # gio, which selkies-proot sets those marks with, and
        # glib-compile-schemas for the GSettings override below
        libglib2.0-bin \
        # proot-apps refreshes the icon cache of the per-user theme it installs
        # into by calling gtk-update-icon-cache, which nothing else here brings
        # in; without it that refresh is a silent no-op and the cache a session
        # reads is whatever was there before the application arrived
        gtk-update-icon-cache && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# The browsers, in one apt operation: Firefox from Mozilla's own APT repository,
# which serves real .deb builds and is pinned over the distribution package, and
# Google Chrome as a downloaded .deb for the H.264/AV1 media stack the
# distribution Chromium builds do not carry. Chrome's own launcher script gains
# two switches, so every way of starting it -- menu entry, xdg-open, a shell --
# carries them. --no-sandbox where the container denies Chrome's sandbox, which
# needs CAP_SYS_ADMIN for its setuid helper or unprivileged user namespaces for
# its zygote: a container's default seccomp profile grants neither, and a stock
# Chrome aborts at the zygote (the container is the isolation boundary there).
# The launcher asks for the namespaces the zygote creates at every start and
# keeps the sandbox wherever they are granted, as under an unconfined seccomp
# profile. And the basic password store, because the container runs no keyring
# daemon to unlock and Chrome otherwise blocks on a prompt nobody can answer.
# Both come from their vendors' apt repositories, whose packages apt checks
# against the signing key each names; the keys are checked against the
# fingerprints Mozilla and Google publish (`fetch-verified`). Chrome's package
# rewrites google-chrome.sources to trust a key it carries itself, and writes
# one on a first install; the entry here is named apart from it and the
# package's defaults file keeps it from writing its own.
RUN mkdir -pm755 /etc/apt/keyrings /etc/apt/sources.list.d /etc/apt/preferences.d && \
    fetch-verified key "https://packages.mozilla.org/apt/repo-signing-key.gpg" \
        35BAA0B33E9EB396F59CA838C0BA5CE6DC6315A3 /etc/apt/keyrings/packages.mozilla.org.asc && \
    printf 'Types: deb\nURIs: https://packages.mozilla.org/apt\nSuites: mozilla\nComponents: main\nSigned-By: /etc/apt/keyrings/packages.mozilla.org.asc\n' > /etc/apt/sources.list.d/mozilla.sources && \
    printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla && \
    fetch-verified key "https://dl.google.com/linux/linux_signing_key.pub" \
        EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796 /etc/apt/keyrings/google-chrome.asc && \
    printf 'Types: deb\nURIs: https://dl.google.com/linux/chrome/deb/\nSuites: stable\nComponents: main\nSigned-By: /etc/apt/keyrings/google-chrome.asc\n' > /etc/apt/sources.list.d/google-linux.sources && \
    printf 'repo_add_once="false"\nrepo_reenable_on_distupgrade="false"\n' > /etc/default/google-chrome && \
    apt-get clean && apt-get update && \
    apt-get install --no-install-recommends -y firefox google-chrome-stable && \
    sed -i 's#^exec -a "$0" "$HERE/chrome" "$@"$#exec -a "$0" "$HERE/chrome" $(unshare --user --map-root-user --pid --net --fork true 2> /dev/null || echo --no-sandbox) --password-store=basic "$@"#' /opt/google/chrome/google-chrome && \
    grep -q -- '|| echo --no-sandbox) --password-store=basic "$@"' /opt/google/chrome/google-chrome && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# proot-apps (https://github.com/linuxserver/proot-apps) backs the dashboards'
# apps panel: portable per-user applications in a proot, installed and removed
# through the selkies-proot wrapper without touching the image. The release
# tarball is staged here and linked into each user's ~/.local/bin, because
# proot-apps runs its tools from there by absolute path. proot traces every
# process it starts, so a second copy carries CAP_SYS_PTRACE for the hosts that
# restrict ptrace (set below as real root); it is kept apart because a file
# capability the container's bounding set does not hold stops that binary from
# execing at all, and the wrapper picks whichever of the two runs.
# The release is checked against its digest, looked up with the build's token
# where one is given (mounted for uid 1000, which these layers run as).
RUN --mount=type=secret,id=github_token,uid=1000 mkdir -pm755 /opt/proot-apps && \
    PAPPS_RELEASE="$(fetch-verified release linuxserver/proot-apps latest \
        "proot-apps-$(dpkg --print-architecture | sed 's/amd64/x86_64/;s/arm64/aarch64/').tar.gz" /tmp/proot-apps.tar.gz)" && \
    tar -xzf /tmp/proot-apps.tar.gz -C /opt/proot-apps/ && rm -f /tmp/proot-apps.tar.gz && \
    printf '%s\n' "${PAPPS_RELEASE}" > /opt/proot-apps/pversion && \
    if ! command -v setcap > /dev/null; then \
        apt-get update && apt-get install --no-install-recommends -y libcap2-bin && \
        apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*; \
    fi && \
    mkdir -pm755 /opt/proot-apps-cap && \
    cp /opt/proot-apps/proot /opt/proot-apps-cap/proot
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

# The desktop itself, selkies-desktop, is the base image's /usr/local/bin/selkies-desktop:
# built from the workspace's crates/selkies-desktop beside the server, so the shell and
# the window manager that frames its windows under SELKIES_DESKTOP come from one tree.

# What the desktop finds in place of settings of its own. Its start button and
# its application-icon wallpaper are the Selkies logo it reads from a fixed path: the bare
# mark, not the PWA icon's white plate, which only a launcher's mask needs;
# without it the start button is a blank strip. Its icon theme is GTK's
# setting, which Ubuntu's file names as Yaru, not installed here, leaving every
# themed glyph to hicolor's few and the volume control blank; breeze, which is,
# replaces it for GTK as well. The edit is checked, so an upstream file that no
# longer matches fails the build rather than a session.
COPY --chown=1000:1000 docs/assets/logo/mark-512x512.png /usr/share/selkies/www/icon.png
# The logo as a vector, the start button's icon, which the desktop draws in one
# color to suit the theme.
COPY --chown=1000:1000 docs/assets/logo/selkies.svg /usr/share/selkies/www/icon.svg
# The wallpaper a session starts with, where the desktop looks for the image's own.
RUN test -d /usr/share/wallpapers/Next && ln -sfn /usr/share/wallpapers/Next /usr/share/selkies/wallpaper
RUN set -eu; \
    if grep -q '^gtk-icon-theme-name' /etc/gtk-3.0/settings.ini; then \
        sed -i 's/^gtk-icon-theme-name.*/gtk-icon-theme-name = breeze/' /etc/gtk-3.0/settings.ini; \
    else \
        sed -i '/^\[Settings\]/a gtk-icon-theme-name = breeze' /etc/gtk-3.0/settings.ini; \
    fi; \
    grep -q '^gtk-icon-theme-name = breeze$' /etc/gtk-3.0/settings.ini

# The desktop's defaults, each documented in its own file: the window
# manager's settings and right-click menu in the openbox format it reads, under
# /etc/xdg/openbox on both backends; the autostart that prepares the home; the
# terminals' look,
# shared between foot and xfce4-terminal; the applications files and links
# open with; and the one Terminal entry in the start menu, which
# selkies-terminal backs as the x-terminal-emulator on either backend, in place
# of each terminal's own. Copied over what the packages above installed at the
# same paths. An interactive shell also gets proot-apps on its PATH with its
# completion, as the desktop's own session does.
COPY --chown=1000:1000 addons/desktop/rootfs/ /
COPY --chown=1000:1000 addons/desktop/selkies-terminal /usr/local/bin/
RUN chmod -f 755 /usr/local/bin/selkies-terminal /etc/xdg/openbox/autostart && \
    update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator /usr/local/bin/selkies-terminal 50 && \
    [ "$(readlink -f /etc/alternatives/x-terminal-emulator)" = /usr/local/bin/selkies-terminal ] && \
    printf '%s\n' \
        '' \
        '# proot-apps from a terminal: its tools on the PATH and its completion' \
        'case ":${PATH}:" in *":${HOME}/.local/bin:"*) ;; *) PATH="${HOME}/.local/bin:${PATH}" ;; esac' \
        'command -v proot-apps > /dev/null 2>&1 && . <(proot-apps completion bash)' \
        >> /etc/bash.bashrc

# The desktop as a session, for what starts one from a session file or as the
# x-session-manager, `selkies session` among them: the server runs the window
# manager and the shell, so the session lasts as long as the shell does.
COPY --chown=1000:1000 addons/desktop/selkies-desktop-session /usr/local/bin/selkies-desktop-session
COPY --chown=1000:1000 addons/desktop/selkies-desktop.desktop /usr/share/xsessions/selkies-desktop.desktop
COPY --chown=1000:1000 addons/desktop/selkies-desktop.desktop /usr/share/wayland-sessions/selkies-desktop.desktop
RUN chmod -f 755 /usr/local/bin/selkies-desktop-session && \
    update-alternatives --install /usr/bin/x-session-manager x-session-manager /usr/local/bin/selkies-desktop-session 100

RUN printf "==============================================\n=         Selkies Desktop Container          =\n==============================================\n" > /etc/motd

USER 0
# Real root for the same reason the base needs it: under the fakeroot SHELL an
# ownership change lands only in fakeroot's database, leaving a setuid bit on a
# file owned by uid 1000, which the kernel refuses to honor. The terminals' utempter
# needs its setgid to record sessions. `restore` covers everything released
# above, which is the base's own set rather than these. A file capability set
# under fakeroot is lost the same way, so the apps panel's ptrace-capable proot
# gets its CAP_SYS_PTRACE here.
SHELL ["/bin/sh", "-c"]
RUN chown -R root:root /opt/proot-apps-cap && setcap cap_sys_ptrace+ep /opt/proot-apps-cap/proot && \
    selkies-privileged-files restore && \
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
# Qt applications take GTK's theme, which selkies-desktop sets from its colors,
# so they follow the shell's scheme with the GTK ones
ENV QT_QPA_PLATFORMTHEME="gtk3"
# The desktop: the server manages the windows itself on either backend and
# starts and keeps the selkies-desktop shell (set to false to run only the
# display server)
ENV SELKIES_DESKTOP="true"
