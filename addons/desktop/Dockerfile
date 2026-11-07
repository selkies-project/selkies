# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Reference minimal desktop container for Selkies: the desktop environment on
# top of the Selkies base container.
#
# The base (addons/base) is the whole session apart from what it looks like —
# X11 and the Wayland backend, audio, GPU wiring, s6, coTURN, and Selkies
# itself. This layer adds only the desktop: selkies-desktop, Selkies' own
# panel, start menu, and desktop, over the Openbox window manager on X11 and
# the nested labwc compositor under SELKIES_WAYLAND=true, the browsers, and the
# proot-apps runner behind the dashboards' apps panel.
#
# BASE_IMAGE is a whole reference — tag or digest — so this layer follows
# whichever base container it is pointed at: the published Ubuntu 26.04 one by
# default, a Debian Trixie one, or the exact digest a CI run just built.
# SELKIES_DESKTOP_VERSION is the selkies-desktop release the image installs.

ARG BASE_IMAGE="ghcr.io/selkies-project/selkies/base:main-ubuntu26.04"

FROM ${BASE_IMAGE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies Desktop Container"
LABEL org.opencontainers.image.description="Reference minimal Selkies container: selkies-desktop over Openbox on X11 by default, the same desktop over labwc on headless Wayland with SELKIES_WAYLAND=true, s6 service supervision, embedded coTURN"
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

RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        # The window manager under selkies-desktop on X11; on Wayland the base's
        # labwc is, and it reads Openbox's stock themes as well
        openbox \
        # The icon sets selkies-desktop, labwc's titlebar, and GTK resolve names
        # in: breeze ships the full-color glyph groups the slim Adwaita build
        # lacks, and both bring hicolor, every icon theme's fallback
        breeze-icon-theme \
        adwaita-icon-theme \
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
        # The notification server applications call over the session bus,
        # started on the first notification by D-Bus activation on either
        # backend
        dunst \
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

# The desktop itself: selkies-desktop (https://github.com/selkies-project/selkies-desktop)
# draws the panel, start menu, taskbar, wallpaper, and file manager, and
# generates the window manager's theme from its own settings. It is one static
# binary per architecture that links no system library, taken at the release
# SELKIES_DESKTOP_VERSION names and checked against the SHA-256 that release
# publishes beside it.
ARG SELKIES_DESKTOP_VERSION="1.0.0"
RUN set -eu; \
    arch="$(dpkg --print-architecture | sed 's/amd64/x86_64/;s/arm64/aarch64/')"; \
    release="https://github.com/selkies-project/selkies-desktop/releases/download/${SELKIES_DESKTOP_VERSION}"; \
    mkdir -p /tmp/selkies-desktop; \
    cd /tmp/selkies-desktop; \
    curl -fsSL --retry 5 -o "selkies-desktop-${arch}" "${release}/selkies-desktop-${arch}"; \
    curl -fsSL --retry 5 -o "selkies-desktop-${arch}.sha256" "${release}/selkies-desktop-${arch}.sha256"; \
    sha256sum -c "selkies-desktop-${arch}.sha256"; \
    install -m 0755 "selkies-desktop-${arch}" /usr/bin/selkies-desktop; \
    cd /; \
    rm -rf /tmp/selkies-desktop

# What the desktop finds in place of settings of its own. Its start button and
# default wallpaper are the Selkies logo it reads from a fixed path: the bare
# mark, not the PWA icon's white plate, which only a launcher's mask needs;
# without it the start button is a blank strip. Its icon theme is GTK's
# setting, which Ubuntu's file names as Yaru, not installed here, leaving every
# themed glyph to hicolor's few and the volume control blank; breeze, which is,
# replaces it for GTK as well. labwc names Openbox's stock Clearlooks theme, as
# the Openbox configuration below does, the name selkies-desktop writes its
# generated theme under ~/.themes as, so a frame looks the same on both
# backends with or without that theme. Super alone opens the start menu on its
# release, so a Super chord still reaches its own binding, with `<default />`
# keeping labwc's stock keybindings beside it; the terminal on Control+Alt+T is
# Openbox's as well. The bare desktop answers clicks as Openbox's does: its menu
# on a right click only, where labwc's stock bindings open it on any button so
# a stray left click pops it up, and the window list on a middle click; an
# empty binding is how labwc removes a stock one. Each edit is checked, so an
# upstream file that no longer matches fails the build rather than a session.
COPY --chown=1000:1000 docs/assets/logo/mark-512x512.png /usr/share/selkies/www/icon.png
RUN set -eu; \
    if grep -q '^gtk-icon-theme-name' /etc/gtk-3.0/settings.ini; then \
        sed -i 's/^gtk-icon-theme-name.*/gtk-icon-theme-name = breeze/' /etc/gtk-3.0/settings.ini; \
    else \
        sed -i '/^\[Settings\]/a gtk-icon-theme-name = breeze' /etc/gtk-3.0/settings.ini; \
    fi; \
    grep -q '^gtk-icon-theme-name = breeze$' /etc/gtk-3.0/settings.ini; \
    [ -d /usr/share/themes/Clearlooks/openbox-3 ]; \
    sed -i 's|<theme>|<theme>\n    <name>Clearlooks</name>|' /etc/xdg/labwc/rc.xml; \
    sed -i 's|</labwc_config>|  <keyboard>\n    <default />\n    <keybind key="Super_L" onRelease="yes"><action name="Execute" command="selkies-desktop --toggle-menu" /></keybind>\n    <keybind key="C-A-t"><action name="Execute" command="x-terminal-emulator" /></keybind>\n    <keybind key="W-e"><action name="Execute" command="selkies-desktop --files" /></keybind>\n    <keybind key="W-d"><action name="Execute" command="selkies-desktop --show-desktop" /></keybind>\n  </keyboard>\n  <mouse>\n    <default />\n    <context name="Root">\n      <mousebind button="Left" action="Press" />\n      <mousebind button="Middle" action="Press"><action name="ShowMenu" menu="client-list-combined-menu" /></mousebind>\n    </context>\n  </mouse>\n</labwc_config>|' /etc/xdg/labwc/rc.xml; \
    grep -q '<name>Clearlooks</name>' /etc/xdg/labwc/rc.xml; \
    grep -q 'command="selkies-desktop --toggle-menu"' /etc/xdg/labwc/rc.xml; \
    grep -q 'command="selkies-desktop --show-desktop"' /etc/xdg/labwc/rc.xml; \
    grep -q '<mousebind button="Left" action="Press" />' /etc/xdg/labwc/rc.xml

# The desktop's defaults, each documented in its own file: Openbox's settings
# and right-click menu and labwc's menu, which selkies-wm-menu fills with the
# installed applications; the terminals' look, shared between foot and
# xfce4-terminal; the applications files and links open with; and the one
# Terminal entry in the start menu, which selkies-terminal backs as the
# x-terminal-emulator on either backend, in place of each terminal's own.
# Copied over what the packages above installed at the same paths. An
# interactive shell also gets proot-apps on its PATH with its completion, as
# the desktop's own session does (services/desktop/run).
COPY --chown=1000:1000 addons/desktop/rootfs/ /
COPY --chown=1000:1000 addons/desktop/selkies-terminal addons/desktop/selkies-wm-menu /usr/local/bin/
RUN chmod -f 755 /usr/local/bin/selkies-terminal /usr/local/bin/selkies-wm-menu && \
    update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator /usr/local/bin/selkies-terminal 50 && \
    [ "$(readlink -f /etc/alternatives/x-terminal-emulator)" = /usr/local/bin/selkies-terminal ] && \
    grep -q '<name>Clearlooks</name>' /etc/xdg/openbox/rc.xml && \
    printf '%s\n' \
        '' \
        '# proot-apps from a terminal: its tools on the PATH and its completion' \
        'case ":${PATH}:" in *":${HOME}/.local/bin:"*) ;; *) PATH="${HOME}/.local/bin:${PATH}" ;; esac' \
        'command -v proot-apps > /dev/null 2>&1 && . <(proot-apps completion bash)' \
        >> /etc/bash.bashrc

# The desktop's s6 services, supervised beside the base container's: the shell
# on either backend, and the window manager X11 needs
COPY --chown=1000:1000 addons/desktop/services/ /etc/service/
# The same desktop for what starts one from a session file or as the
# x-session-manager, `selkies session` among them. The alternative outranks
# Openbox's own, which would start the window manager with no desktop on it.
COPY --chown=1000:1000 addons/desktop/selkies-desktop-session /usr/local/bin/selkies-desktop-session
COPY --chown=1000:1000 addons/desktop/selkies-desktop.desktop /usr/share/xsessions/selkies-desktop.desktop
COPY --chown=1000:1000 addons/desktop/selkies-desktop.desktop /usr/share/wayland-sessions/selkies-desktop.desktop
RUN chmod -f 755 /usr/local/bin/selkies-desktop-session && \
    update-alternatives --install /usr/bin/x-session-manager x-session-manager /usr/local/bin/selkies-desktop-session 100 && \
    find /etc/service \( -name run -o -name finish \) -exec chmod -f 755 {} +

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
# Start the desktop: selkies-desktop, and Openbox under it on X11 (set to false
# to run only the display server)
ENV START_DESKTOP="true"
