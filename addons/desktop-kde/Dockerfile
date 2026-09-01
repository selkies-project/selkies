# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# KDE Plasma desktop container for Selkies: the desktop environment on top of
# the Selkies base container.
#
# The base (addons/base) is the whole session apart from what it looks like —
# the Wayland backend, audio, GPU wiring, s6, coTURN and Selkies itself. This
# layer adds only the desktop: Plasma with kwin as the nested session
# compositor (SELKIES_WAYLAND_COMPOSITOR=selkies-kwin), the browsers, and the
# proot-apps runner behind the dashboards' apps panel.
#
# Unlike the LXQt desktop this image is Wayland-only and Ubuntu 26.04
# (resolute) only: resolute ships no Plasma X11 session, and the kwin it ships
# is rebuilt here with the patches under patches/ubuntu26.04/ so a nested kwin
# registers the virtual outputs Selkies grows a second display from
# (zkde_screencast); the patch header carries the mechanism. The base image
# this builds on must therefore be the Ubuntu 26.04 one.

ARG BASE_IMAGE="ghcr.io/selkies-project/selkies/base:main-ubuntu26.04"
ARG DISTRIB_IMAGE="ubuntu"
ARG DISTRIB_RELEASE="26.04"
# Which patch set rebuilds kwin, named for the base it applies to.
ARG KDE_PATCH_SET="ubuntu26.04"

# The distribution kwin, rebuilt with the Selkies patches. Built on the plain
# distribution image rather than the Selkies base: the archive and toolchain
# are the same, and a throwaway root stage needs none of the base's rootless
# arrangements. The stage rebuilds whatever kwin version the archive currently
# serves, which is also the version the desktop layer below installs.
FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE} AS kwinbuild
ARG DEBIAN_FRONTEND="noninteractive"
ARG KDE_PATCH_SET
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        > /etc/apt/apt.conf.d/99-selkies-retries
COPY addons/desktop-kde/patches/${KDE_PATCH_SET}/ /tmp/kwin-patches/
RUN sed -i 's/^Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/ubuntu.sources && \
    apt-get clean && apt-get update && \
    apt-get install --no-install-recommends -y \
        ca-certificates \
        dpkg-dev && \
    apt-get build-dep -y kwin && \
    mkdir -p /build && \
    cd /build && \
    apt-get source kwin && \
    cd kwin-*/ && \
    for kwin_patch in /tmp/kwin-patches/*.patch; do patch -p1 < "${kwin_patch}"; done && \
    DEB_BUILD_OPTIONS="nocheck parallel=$(nproc)" \
        dpkg-buildpackage -b -uc -us && \
    mkdir -p /build/debs && \
    mv /build/*.deb /build/debs/

FROM ${BASE_IMAGE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies KDE Container"
LABEL org.opencontainers.image.description="Selkies KDE Plasma container: Plasma on headless Wayland with kwin as the nested session compositor, s6 service supervision, embedded coTURN. Ubuntu 26.04 base only."
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

COPY --from=kwinbuild --chown=1000:1000 /build/debs /tmp/kwin-debs

# The Plasma desktop, then the patched kwin over the packaged one in the same
# operation. kwin-wayland is named because --no-install-recommends leaves the
# session compositor to chance otherwise; no kwin-x11 and no X11 session
# packages, since this image is Wayland-only. The dpkg -i takes only the
# rebuilt debs whose package the install above put on the system (kwin-common,
# kwin-data, kwin-wayland — not the debug or X11 splits), in one invocation so
# dpkg orders them itself.
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        dolphin \
        gwenview \
        kde-config-gtk-style \
        kdialog \
        kfind \
        khotkeys \
        kio-extras \
        knewstuff-dialog \
        konsole \
        ksystemstats \
        kubuntu-settings-desktop \
        kubuntu-wallpapers \
        kwin-addons \
        kwin-wayland \
        kwrite \
        plasma-desktop \
        plasma-discover \
        plasma-workspace \
        qml-module-qt-labs-platform \
        systemsettings && \
    debs="" && \
    for deb in /tmp/kwin-debs/*.deb; do \
        if dpkg -s "$(dpkg-deb -f "${deb}" Package)" > /dev/null 2>&1; then \
            debs="${debs} ${deb}"; \
        fi; \
    done && \
    dpkg -i ${debs} && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# The browsers, in one apt operation: Firefox from Mozilla's own APT repository,
# which serves real .deb builds and is pinned over the distribution package,
# and Google Chrome as a downloaded .deb for the H.264/AV1 media stack the
# distribution Chromium builds do not carry. Chrome's launcher is switched to
# the basic password store because the container runs no keyring daemon to
# unlock, and it otherwise blocks on a prompt nobody can answer.
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

# The nested session compositor the base's wayland service execs
# (SELKIES_WAYLAND_COMPOSITOR below), and the session's own s6 services —
# plasmashell and the session bus it shares with kwin — supervised beside the
# base container's.
COPY --chown=1000:1000 addons/desktop-kde/selkies-kwin /usr/local/bin/selkies-kwin
COPY --chown=1000:1000 addons/desktop-kde/services/ /etc/service/
RUN chmod -f 755 /usr/local/bin/selkies-kwin && \
    find /etc/service \( -name run -o -name finish \) -exec chmod -f 755 {} +

RUN printf "==============================================\n=           Selkies KDE Container            =\n==============================================\n" > /etc/motd

USER 0
# Real root for the same reason the base needs it: under the fakeroot SHELL an
# ownership change lands only in fakeroot's database, leaving a setuid bit on a
# file owned by uid 1000, which the kernel refuses to honour. Without a
# root-owned chrome-sandbox Chrome aborts at launch wherever unprivileged user
# namespaces are unavailable (the default Docker seccomp profile). `restore`
# covers everything released above, which is the base's own set rather than
# these.
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
# This image is Wayland-only: resolute ships no Plasma X11 session, so the
# backend is forced on and the entrypoint's fall-back-to-X11-for-the-GPU path
# is disabled — a GPU the compositor cannot reach composites in software
# instead of switching to a backend with no desktop behind it.
ENV PIXELFLUX_WAYLAND="true"
ENV SELKIES_WAYLAND_X11_FALLBACK="false"
# kwin is the nested session compositor the base's wayland service starts
ENV SELKIES_WAYLAND_COMPOSITOR="selkies-kwin"
# Start the Plasma shell (set to false to run only kwin)
ENV START_KDE="true"
# Plasma's menu definitions carry its prefix; the base defaults to lxqt-
ENV XDG_MENU_PREFIX="plasma-"
# Second displays on this image are kwin virtual outputs, and the two halves of
# that travel together: the interface stays visible to Selkies (kwin 6.x hides
# it from clients without an X-KDE-Wayland-Interfaces desktop entry, a check a
# python process can never satisfy and upstream has since dropped), and the
# capability is declared because this image ships the patched kwin that
# registers the outputs (see patches/).
ENV KWIN_WAYLAND_NO_PERMISSION_CHECKS="1"
ENV SELKIES_KWIN_MULTI="true"
