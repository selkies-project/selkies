# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Reference minimal example container for Selkies.
#
#   - LXQt desktop (Openbox) on X11 (Xvfb) by default; headless Wayland backend
#     with SELKIES_WAYLAND=true
#   - s6 service supervision (works both as PID 1 and below a foreign init,
#     unlike s6-overlay; no Python-based supervisor, no tini)
#   - Joystick interposer + fake-udev (container-only gamepad plumbing) built
#     in-tree and wired with LD_PRELOAD
#   - Selkies installed with pip: from wheels dropped into `wheels/`, or from
#     PyPI (latest release) when `wheels/` is empty
#
# Supported bases: Ubuntu 24.04 / 26.04, Debian Bookworm / Trixie and later
# (select with --build-arg DISTRIB_IMAGE=ubuntu|debian DISTRIB_RELEASE=24.04|26.04|bookworm|trixie)

ARG DISTRIB_IMAGE=ubuntu
ARG DISTRIB_RELEASE=24.04
ARG PYPI_PACKAGE=selkies

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
LABEL org.opencontainers.image.description="Reference minimal Selkies container: LXQt (X11) by default, headless Wayland with SELKIES_WAYLAND=true, s6 service supervision, embedded coTURN"
LABEL org.opencontainers.image.source="https://github.com/selkies-project/selkies"
LABEL org.opencontainers.image.licenses="MPL-2.0"

ARG DISTRIB_IMAGE
ARG DISTRIB_RELEASE
ARG PYPI_PACKAGE

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
        # Process supervision (no Python, works below any PID 1 as well)
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
        vainfo \
        # Fonts
        fonts-dejavu-core \
        fonts-liberation \
        # Python runtime and the embedded coTURN server (WebRTC mode)
        python3 \
        python3-pip \
        coturn \
        ssl-cert && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# Install Firefox from Mozilla's official APT repository, which serves real .deb
# builds on both Debian and Ubuntu and takes priority over any
# distribution-provided Firefox packages
RUN mkdir -pm755 /etc/apt/keyrings /etc/apt/sources.list.d /etc/apt/preferences.d && \
    curl -o /etc/apt/keyrings/packages.mozilla.org.asc -fsSL "https://packages.mozilla.org/apt/repo-signing-key.gpg" && \
    printf 'Types: deb\nURIs: https://packages.mozilla.org/apt\nSuites: mozilla\nComponents: main\nSigned-By: /etc/apt/keyrings/packages.mozilla.org.asc\n' > /etc/apt/sources.list.d/mozilla.sources && \
    printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla && \
    apt-get clean && apt-get update && apt-get install --no-install-recommends -y firefox && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# Install Selkies with pip: from the wheels dropped into `wheels/`, or the
# latest PyPI release when `wheels/` is empty. When fresh pixelflux/pcmflux
# wheels (built from the master HEAD of linuxserver/pixelflux and
# linuxserver/pcmflux) ride along, pip selects the ones matching this image's
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
    PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --force-reinstall ${PKGS}; \
    rm -rf /tmp/wheels /tmp/picked

# Install the joystick interposer and fake-udev libraries (container-only)
COPY --from=add-ons /opt/add-ons/lib/ /usr/lib/

# Copy the startup scripts and s6 service definitions (same /etc/service/<svc>/run layout as runit, supervised by s6-supervise)
COPY --chown=1000:1000 addons/example/container-entrypoint.sh /etc/container-entrypoint.sh
COPY --chown=1000:1000 addons/example/selkies-entrypoint.sh /etc/selkies-entrypoint.sh
COPY --chown=1000:1000 addons/example/services/ /etc/service/
RUN chmod -f 755 /etc/container-entrypoint.sh /etc/selkies-entrypoint.sh && \
    find /etc/service -name run -exec chmod -f 755 {} +

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
# Start the LXQt session (set to false to run only the display server)
ENV START_LXQT=true
# Web interface port
ENV SELKIES_PORT=8081

ENV SHELL=/bin/bash
ENV USER=ubuntu
ENV HOME=/home/ubuntu
WORKDIR /home/ubuntu

EXPOSE 8081

ENTRYPOINT ["/etc/container-entrypoint.sh"]
