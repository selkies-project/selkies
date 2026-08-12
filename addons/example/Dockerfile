# check=skip=SecretsUsedInArgOrEnv,UndefinedVar

# Build checks are skipped for two intentional patterns, and a parser directive
# only counts on the first line: PASSWD is a published placeholder login
# password meant to be overridden with -e rather than a build secret, and
# LD_LIBRARY_PATH is extended from whatever the base image set, which is nothing
# on the bases supported here.

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Reference minimal example container for Selkies.
#
#   - LXQt desktop on X11 (Xvfb, Openbox) by default; the same desktop on the
#     headless Wayland backend with SELKIES_WAYLAND=true, where a nested sway
#     supplies window management and XWayland
#   - s6 service supervision, running as PID 1 or below a foreign init
#   - Joystick interposer + fake-udev (container-only gamepad plumbing) built
#     in-tree and wired with LD_PRELOAD
#   - GPU wiring: NVIDIA/VA-API/Vulkan runtime, Zink for hardware OpenGL, and a
#     DRI3-capable Xvfb
#   - Selkies installed with pip: from wheels dropped into `wheels/`, or from
#     PyPI (latest release) when `wheels/` is empty
#
# Supported bases: Ubuntu 24.04 / 26.04, Debian Bookworm / Trixie and later
# (select with --build-arg DISTRIB_IMAGE=ubuntu|debian DISTRIB_RELEASE=24.04|26.04|bookworm|trixie)

ARG DISTRIB_IMAGE=ubuntu
ARG DISTRIB_RELEASE=24.04
ARG PYPI_PACKAGE=selkies
# Xvfb with DRI3 (https://github.com/linuxserver/docker-xvfb): the distribution
# server has none, so GL clients render in software even with a GPU present.
# Built on the oldest base this image supports, whose glibc the newer ones
# satisfy; override to match a different base or to opt out.
ARG XVFB_IMAGE=lscr.io/linuxserver/xvfb:debianbookworm

FROM ${XVFB_IMAGE} AS xvfb

# Build the joystick interposer and fake-udev inside this repository (they are
# dedicated to containerized environments and are not desktop components)
FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE} AS add-ons
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        build-essential \
        ca-certificates && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
COPY addons/js-interposer /build/js-interposer
COPY addons/fake-udev /build/fake-udev
RUN set -e; \
    LIBDIR="$(gcc -print-multiarch)"; \
    mkdir -p "/opt/add-ons/lib/${LIBDIR}"; \
    gcc -shared -fPIC -o /opt/add-ons/lib/${LIBDIR}/selkies_joystick_interposer.so /build/js-interposer/joystick_interposer.c -ldl -pthread; \
    make -C /build/fake-udev; \
    cp /build/fake-udev/libudev.so.1.0.0-fake /build/fake-udev/libudev.so.1 /build/fake-udev/libudev.so "/opt/add-ons/lib/${LIBDIR}/"; \
    # Also build the 32-bit variants so 32-bit (Wine-era) applications see the
    # virtual gamepads too (`/usr/$LIB` expands per process bitness); amd64 only
    if [ "$(uname -m)" = "x86_64" ]; then \
        apt-get update && apt-get install --no-install-recommends -y gcc-multilib; \
        LIB32DIR="$(gcc -m32 -print-multiarch)"; \
        mkdir -p "/opt/add-ons/lib/${LIB32DIR}"; \
        gcc -m32 -shared -fPIC -o /opt/add-ons/lib/${LIB32DIR}/selkies_joystick_interposer.so /build/js-interposer/joystick_interposer.c -ldl -pthread; \
        make -C /build/fake-udev all32; \
        cp /build/fake-udev/libudev_x86.so.1.0.0-fake "/opt/add-ons/lib/${LIB32DIR}/libudev.so.1.0.0-fake"; \
        ln -sf libudev.so.1.0.0-fake "/opt/add-ons/lib/${LIB32DIR}/libudev.so.1"; \
        ln -sf libudev.so.1.0.0-fake "/opt/add-ons/lib/${LIB32DIR}/libudev.so"; \
    fi

FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies Example Container"
LABEL org.opencontainers.image.description="Reference minimal Selkies container: LXQt on X11 by default, the same desktop on headless Wayland with SELKIES_WAYLAND=true, s6 service supervision, embedded coTURN"
LABEL org.opencontainers.image.source="https://github.com/selkies-project/selkies"
LABEL org.opencontainers.image.licenses="MPL-2.0"

ARG DISTRIB_IMAGE
ARG DISTRIB_RELEASE
ARG PYPI_PACKAGE

# Every network fetch in this image is retried, because a build that reaches for
# a package index, a signing key or a wheel is one transient failure away from
# losing the whole run. apt is configured once here so each of its later
# invocations inherits it; curl and pip carry their own flags at each call.
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        > /etc/apt/apt.conf.d/99-selkies-retries

ARG DEBIAN_FRONTEND=noninteractive
# Configure rootless user environment for constrained conditions without escalated root privileges inside containers
ARG TZ=UTC
ENV PASSWD=mypasswd
RUN apt-get clean && apt-get update && apt-get dist-upgrade -y && apt-get install --no-install-recommends -y \
        apt-utils \
        dbus-user-session \
        fakeroot \
        fuse \
        kmod \
        locales \
        ssl-cert \
        sudo \
        udev \
        tzdata && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/* && \
    locale-gen en_US.UTF-8 && \
    ln -snf "/usr/share/zoneinfo/${TZ}" /etc/localtime && echo "${TZ}" > /etc/timezone && \
    # Only use sudo-root for root-owned directory (/dev, /proc, /sys) or user/group permission operations, not for apt-get installation or file/directory operations
    mv -f /usr/bin/sudo /usr/bin/sudo-root && \
    ln -snf /usr/bin/fakeroot /usr/bin/sudo && \
    groupadd -g 1000 ubuntu || echo 'Failed to add ubuntu group' && \
    useradd -ms /bin/bash ubuntu -u 1000 -g 1000 || echo 'Failed to add ubuntu user' && \
    usermod -a -G adm,audio,cdrom,dialout,dip,fax,floppy,games,input,lp,plugdev,render,ssl-cert,sudo,tape,tty,video,voice ubuntu && \
    echo "ubuntu ALL=(ALL:ALL) NOPASSWD: ALL" >> /etc/sudoers && \
    echo "ubuntu:${PASSWD}" | chpasswd && \
    chown -R -f -h --no-preserve-root ubuntu:ubuntu / || echo 'Failed to set filesystem ownership in some paths to ubuntu user' && \
    # Preserve setuid/setgid removed by chown
    chmod -f 4755 /usr/lib/dbus-1.0/dbus-daemon-launch-helper /usr/bin/chfn /usr/bin/chsh /usr/bin/mount /usr/bin/gpasswd /usr/bin/passwd /usr/bin/newgrp /usr/bin/umount /usr/bin/su /usr/bin/sudo-root /usr/bin/fusermount || echo 'Failed to set chmod setuid for some paths' && \
    chmod -f 2755 /var/local /var/mail /usr/sbin/unix_chkpwd /usr/sbin/pam_extrausers_chkpwd /usr/bin/expiry /usr/bin/chage || echo 'Failed to set chmod setgid for some paths'

# Set locales
ENV LANG="en_US.UTF-8"
ENV LANGUAGE="en_US:en"
ENV LC_ALL="en_US.UTF-8"

USER 0

# Install and configure below this line
###
# Debian packaging frontends: apt-get only, never dpkg
###
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        # Process supervision
        s6 \
        # Base utilities
        ca-certificates \
        curl \
        jq \
        dnsutils \
        # X11 display server and utilities
        xserver-xorg-core \
        xvfb \
        x11-xserver-utils \
        x11-xkb-utils \
        x11-utils \
        xauth \
        wmctrl \
        dbus-x11 \
        # X11 client libraries required by pixelflux (XShm capture) and input injection
        libx11-xcb1 \
        libxcb1 \
        libxcb-shm0 \
        libxcb-xfixes0 \
        libxcb-randr0 \
        libxcb-dri3-0 \
        libxkbcommon0 \
        libxdamage1 \
        libxfixes3 \
        libxtst6 \
        libxext6 \
        # Wayland backend: pixelflux opens libwayland-server for the capture
        # compositor, and XWayland serves the X11-only applications running
        # under the nested session compositor installed below
        libwayland-server0 \
        libwayland-client0 \
        libwayland-egl1 \
        xwayland \
        # Audio (PipeWire with the PulseAudio interface for pcmflux)
        libpulse0 \
        pipewire \
        pipewire-audio-client-libraries \
        pipewire-pulse \
        wireplumber \
        # Minimal LXQt desktop (Openbox window manager)
        lxqt-session \
        lxqt-panel \
        lxqt-runner \
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
        # suspends nothing), gvfs, and the KDE portal.
        adwaita-icon-theme \
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
        # GPU support: Mesa (llvmpipe fallback, VA-API) and Vulkan/GL plumbing
        libgl1 \
        libgl1-mesa-dri \
        libegl1 \
        libgles2 \
        mesa-utils \
        mesa-va-drivers \
        mesa-vulkan-drivers \
        libva2 \
        libva-drm2 \
        libdrm2 \
        libgbm1 \
        libvulkan1 \
        libglu1-mesa \
        # VDPAU and the tools that inspect whichever GPU is passed in. The
        # drivers themselves rather than vdpau-driver-all: that metapackage
        # pulls the proprietary NVIDIA driver and is absent on some releases.
        vainfo \
        vdpauinfo \
        mesa-vdpau-drivers \
        libvdpau-va-gl1 \
        vulkan-tools \
        radeontop \
        # Fonts: DejaVu/Liberation for Latin, Noto for worldwide script and emoji coverage
        fonts-dejavu-core \
        fonts-liberation \
        fonts-noto-core \
        fonts-noto-cjk \
        fonts-noto-color-emoji \
        # Python runtime and the embedded coTURN server (WebRTC mode)
        python3 \
        python3-pip \
        coturn \
        ssl-cert && \
    # The Intel media stack exists for amd64 alone
    if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
        apt-get install --no-install-recommends -y \
            intel-media-va-driver intel-gpu-tools; \
    fi && \
    # Session compositor for the Wayland backend. sway, because `create_output`
    # lets the session gain and lose a screen while it runs, which is what a second
    # display appearing on demand needs. sway-backgrounds supplies the wallpaper the
    # packaged config names, whose absence is a parse error that greets every session
    # with sway's error dialog. Not wayfire: it demands a DRM file descriptor the
    # nested Wayland backend has none of without a GPU.
    apt-get install --no-install-recommends -y sway sway-backgrounds && \
    # wayland-info reads how many outputs the capture compositor drives, which is what
    # tells the session compositor whether the spare screen belongs in the layout.
    apt-get install --no-install-recommends -y wayland-utils && \
    # The session's own portal backend, which answers the file-chooser and
    # screen-share requests toolkits make of a desktop. Debian Bookworm predates
    # it, and a session without a portal still runs.
    { apt-get install --no-install-recommends -y xdg-desktop-portal-lxqt || true; } && \
    # The LXQt Wayland session, where the distribution packages it (LXQt 2.1+).
    # The Qt platform plugin is installed only alongside it, never on its own:
    # without layer-shell behind it, Qt picking Wayland gives a session whose
    # panel and desktop are surfaces with nothing to anchor to, and which
    # therefore paint nothing at all. The service reads the same pair back to
    # decide which session to run.
    if apt-get install --no-install-recommends -y lxqt-wayland-session; then \
        apt-get install --no-install-recommends -y qt6-wayland || \
        apt-get install --no-install-recommends -y qtwayland5 || true; \
    fi && \
    # Ubuntu carries nvtop in universe, which is enabled; Debian puts it in
    # contrib, which is not, so that component is added on the second attempt
    if ! apt-get install --no-install-recommends -y nvtop; then \
        if [ -f /etc/apt/sources.list.d/debian.sources ]; then \
            sed -i 's/^Components: .*/& contrib/' /etc/apt/sources.list.d/debian.sources; \
        elif [ -f /etc/apt/sources.list ]; then \
            sed -i 's/ main/ main contrib/g' /etc/apt/sources.list; \
        fi && \
        apt-get update && apt-get install --no-install-recommends -y nvtop; \
    fi && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# LXQt's system defaults, which the distribution packages do not ship: without
# them a session has no theme, so lxqt-panel finds no stylesheet, keeps the
# invalid colours it writes as its own default, and paints nothing at all — a
# panel that runs and maps a window but is invisible against the desktop. The
# icon theme goes with it, since LXQt otherwise falls back to Oxygen, which is
# not installed here. This is the system scope, so a user's own
# ~/.config/lxqt/lxqt.conf still overrides it. The names are checked against
# what the packages above actually installed, so a package change fails the
# build here rather than reaching a user as a desktop that will not paint.
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

# NVIDIA runtime wiring. The container toolkit injects the host driver, but only
# the capabilities requested below (pixelflux's NVENC path needs `video`, the GL
# and Vulkan stacks need `graphics`), and it mounts the libraries under
# /usr/local/nvidia, which the loader has to be told about. The ICD and vendor
# files select the injected driver over Mesa; they are inert without one.
RUN printf '/usr/local/nvidia/lib\n/usr/local/nvidia/lib64\n' > /etc/ld.so.conf.d/nvidia.conf && \
    mkdir -pm755 /etc/OpenCL/vendors /etc/vulkan/icd.d /usr/share/glvnd/egl_vendor.d && \
    echo 'libnvidia-opencl.so.1' > /etc/OpenCL/vendors/nvidia.icd && \
    VULKAN_API_VERSION="$(dpkg -s libvulkan1 | grep -oP 'Version: [0-9|\.]+' | grep -oP '[0-9]+(\.[0-9]+)(\.[0-9]+)')" && \
    printf '{\n  "file_format_version": "1.0.0",\n  "ICD": {\n    "library_path": "libGLX_nvidia.so.0",\n    "api_version": "%s"\n  }\n}\n' "${VULKAN_API_VERSION}" > /etc/vulkan/icd.d/nvidia_icd.json && \
    printf '{\n  "file_format_version": "1.0.0",\n  "ICD": {\n    "library_path": "libEGL_nvidia.so.0"\n  }\n}\n' > /usr/share/glvnd/egl_vendor.d/10_nvidia.json
ENV PATH="/usr/local/nvidia/bin${PATH:+:${PATH}}"
ENV LD_LIBRARY_PATH="${LD_LIBRARY_PATH:+${LD_LIBRARY_PATH}:}/usr/local/nvidia/lib:/usr/local/nvidia/lib64"
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=all
ENV __GL_SYNC_TO_VBLANK=0

# Install Firefox from Mozilla's official APT repository, which serves real .deb
# builds on both Debian and Ubuntu and takes priority over any
# distribution-provided Firefox packages
RUN mkdir -pm755 /etc/apt/keyrings /etc/apt/sources.list.d /etc/apt/preferences.d && \
    curl -o /etc/apt/keyrings/packages.mozilla.org.asc -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 "https://packages.mozilla.org/apt/repo-signing-key.gpg" && \
    printf 'Types: deb\nURIs: https://packages.mozilla.org/apt\nSuites: mozilla\nComponents: main\nSigned-By: /etc/apt/keyrings/packages.mozilla.org.asc\n' > /etc/apt/sources.list.d/mozilla.sources && \
    printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla && \
    apt-get clean && apt-get update && apt-get install --no-install-recommends -y firefox && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# Google Chrome, for the H.264/AV1 media stack the Chromium builds in the
# distribution registries do not carry. The launcher is switched to the basic
# password store because the container runs no keyring daemon to unlock, and
# Chrome otherwise blocks on a prompt nobody can answer.
RUN cd /tmp && \
    curl -o google-chrome-stable.deb -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 "https://dl.google.com/linux/direct/google-chrome-stable_current_$(dpkg --print-architecture).deb" && \
    apt-get update && apt-get install --no-install-recommends -y ./google-chrome-stable.deb && \
    rm -f google-chrome-stable.deb && \
    sed -i '/^Exec=/ s/$/ --password-store=basic/' /usr/share/applications/google-chrome.desktop && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# Install Selkies with pip: from the wheels dropped into `wheels/`, or the
# latest PyPI release when `wheels/` is empty. When fresh pixelflux/pcmflux
# wheels (built from the main HEAD of selkies-project/pixelflux and
# selkies-project/pcmflux) ride along, pip selects the ones matching this image's
# Python and platform; everything else still resolves from PyPI
COPY addons/example/wheels/ /tmp/wheels/
RUN set -e; \
    SELKIES_WHL="$(ls /tmp/wheels/selkies-*.whl 2>/dev/null || true)"; \
    if [ -z "${SELKIES_WHL}" ]; then \
      PKGS="${PYPI_PACKAGE}"; \
    else \
      PKGS="${SELKIES_WHL}"; \
      if ls /tmp/wheels/pixelflux-*.whl > /dev/null 2>&1; then \
        pip3 download --no-cache-dir --no-deps --no-index --find-links /tmp/wheels --dest /tmp/picked pixelflux pcmflux > /dev/null; \
        PKGS="${PKGS} $(ls /tmp/picked/*.whl)"; \
      fi; \
    fi; \
    PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --retries 5 --timeout 60 --force-reinstall ${PKGS}; \
    rm -rf /tmp/wheels /tmp/picked

# The DRI3-capable server replaces the distribution's /usr/bin/Xvfb
COPY --from=xvfb / /

# Install the joystick interposer and fake-udev libraries (container-only)
COPY --from=add-ons /opt/add-ons/lib/ /usr/lib/

# Makes the sway session look like the Openbox one: floating windows, no bar of
# its own, and XWayland for the X11-only applications
COPY --chown=1000:1000 addons/example/sway.conf /etc/sway/config.d/99-selkies.conf

# Copy the startup scripts and s6 service definitions (same /etc/service/<svc>/run layout as runit, supervised by s6-supervise)
COPY --chown=1000:1000 addons/example/container-entrypoint.sh /etc/container-entrypoint.sh
COPY --chown=1000:1000 addons/example/selkies-entrypoint.sh /etc/selkies-entrypoint.sh
COPY --chown=1000:1000 addons/example/services/ /etc/service/
RUN chmod -f 755 /etc/container-entrypoint.sh /etc/selkies-entrypoint.sh && \
    find /etc/service \( -name run -o -name finish \) -exec chmod -f 755 {} +

# Configure bashrc to show /etc/motd and enable bash completion
RUN echo "[ ! -z \"\$TERM\" -a -r /etc/motd ] && echo \"\$(grep '^PRETTY_NAME=' /etc/os-release | cut -d= -f2 | tr -d '\"')\" && cat /etc/motd" >> /etc/bash.bashrc && \
    printf "==============================================\n=         Selkies Example Container          =\n==============================================\n" > /etc/motd

USER 0
# Enable sudo through sudo-root with uid 0
RUN if [ -d "/usr/libexec/sudo" ]; then SUDO_LIB="/usr/libexec/sudo"; else SUDO_LIB="/usr/lib/sudo"; fi && \
    chown -R -f -h --no-preserve-root root:root /usr/bin/sudo-root /etc/sudo.conf /etc/sudoers /etc/sudoers.d /etc/sudo_logsrvd.conf "${SUDO_LIB}" || echo 'Failed to provide root permissions in some paths relevant to sudo' && \
    chmod -f 4755 /usr/bin/sudo-root || echo 'Failed to set chmod setuid for root'

USER 1000

# Default runtime environment (overridable with -e)
ENV PIPEWIRE_LATENCY="128/48000"
ENV XDG_RUNTIME_DIR=/tmp/runtime-ubuntu
ENV PIPEWIRE_RUNTIME_DIR="${XDG_RUNTIME_DIR}"
ENV PULSE_RUNTIME_PATH="${XDG_RUNTIME_DIR}/pulse"
ENV PULSE_SERVER="unix:${XDG_RUNTIME_DIR}/pulse/native"
# dbus-daemon to the below address is required during startup
ENV DBUS_SYSTEM_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/dbus-system-bus"
# X11 (Xvfb) by default; set to true for the headless Wayland backend
ENV SELKIES_WAYLAND=false
# Session compositor nested inside the Wayland capture compositor: empty picks
# the installed one, "none" leaves applications on the capture compositor with
# no window management and no XWayland
ENV SELKIES_WAYLAND_COMPOSITOR=""
# Screens the nested compositor opens at startup. Empty is on demand: the session
# gains a screen when a second display's client connects and loses it when that
# client leaves. Setting this opens that many outright instead.
ENV SELKIES_WAYLAND_OUTPUTS=""
# Run X11 instead when a GPU is present but the Wayland compositor cannot reach
# it, so the session keeps hardware acceleration. Without a GPU both backends
# render in software and Wayland stays; set false to keep Wayland either way,
# which renders the whole session in software
ENV SELKIES_WAYLAND_X11_FALLBACK=true
# Start the LXQt session (set to false to run only the display server)
ENV START_LXQT=true
# Web interface port
ENV SELKIES_PORT=8080

ENV SHELL=/bin/bash
ENV USER=ubuntu
ENV HOME=/home/ubuntu
WORKDIR /home/ubuntu

EXPOSE 8080

ENTRYPOINT ["/etc/container-entrypoint.sh"]
