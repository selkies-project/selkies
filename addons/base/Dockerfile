# check=skip=SecretsUsedInArgOrEnv,UndefinedVar

# Build checks are skipped for two intentional patterns, and a parser directive
# only counts on the first line: PASSWD is a published placeholder login
# password meant to be overridden with -e rather than a build secret, and
# LD_LIBRARY_PATH is extended from whatever the base image set, which is nothing
# on the bases supported here.

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Base container for Selkies: a complete session with no desktop environment.
#
# Everything a Selkies session needs and nothing that decides what the session
# looks like: no window manager, no panel, no file manager, no applications.
# addons/desktop builds the reference LXQt desktop on top of this, and any
# other desktop can be built the same way.
#
#   - X11 display server (a DRI3-capable Xvfb) and the headless Wayland backend
#     with SELKIES_WAYLAND=true, whose nested labwc supplies window management,
#     decorations and XWayland
#   - s6 service supervision, running as PID 1 or below a foreign init
#   - Joystick interposer + fake-udev (container-only gamepad plumbing) and the
#     V4L2 webcam interposer, all built in-tree and wired with LD_PRELOAD
#   - GPU wiring: NVIDIA/VA-API/Vulkan runtime, Zink for hardware OpenGL
#   - Audio: PipeWire with the PulseAudio interface pcmflux records through
#   - Embedded coTURN for WebRTC firewall traversal
#   - Selkies installed with pip: from wheels dropped into `wheels/`, or from
#     PyPI (latest release) when `wheels/` is empty
#
# Supported bases: Ubuntu 26.04 and Debian Trixie, and later releases of both
# (select with --build-arg DISTRIB_IMAGE=ubuntu|debian DISTRIB_RELEASE=26.04|trixie).

ARG DISTRIB_IMAGE="ubuntu"
ARG DISTRIB_RELEASE="26.04"
ARG PYPI_PACKAGE="selkies"
# The X server the session runs on, built here rather than installed: the
# distribution's Xvfb has no DRI3, so GL clients render in software even with a
# GPU present. The release is pinned instead of taken from the base, because
# the tree addons/base/patches/xvfb-dri3.patch edits moves within the 21.1
# series; the pair moves together when it is rebased.
ARG XORG_SERVER_VERSION="21.1.24"
ARG WLROOTS_VERSION="0.19.3"
ARG LABWC_VERSION="0.9.7"
ARG XORG_SERVER_SHA256="1a4eb36ca65cc3b1b936566d677a9786e13c11cd5806e951ac55f3f5ce3984af"

FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE} AS xvfb
ARG DEBIAN_FRONTEND="noninteractive"
ARG XORG_SERVER_VERSION
ARG XORG_SERVER_SHA256
# The retry configuration the final stage sets further down, repeated here for
# the same reason the add-ons stage repeats it.
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        > /etc/apt/apt.conf.d/99-selkies-retries
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        ca-certificates \
        curl \
        gcc \
        meson \
        ninja-build \
        patch \
        pkg-config \
        xz-utils \
        # The server itself
        libaudit-dev \
        libbsd-dev \
        libpixman-1-dev \
        libselinux1-dev \
        libssl-dev \
        libx11-dev \
        libxau-dev \
        libxcvt-dev \
        libxdmcp-dev \
        libxext-dev \
        libxfont-dev \
        libxkbfile-dev \
        libxshmfence-dev \
        mesa-common-dev \
        x11proto-dev \
        xtrans-dev \
        # glamor's EGL/GBM backend, which is what DRI3 hands buffers out of
        libdrm-dev \
        libepoxy-dev \
        libgbm-dev && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
COPY addons/base/patches/xvfb-dri3.patch /patches/
# Only the framebuffer server is built. Its paths, extensions and defaults are
# the distribution package's, so the binary drops in where that one installed
# it; anything left at the meson default would move a keymap or a font path out
# from under the rest of the image.
RUN set -e; \
    cd /tmp; \
    curl -fsSL --retry 5 --connect-timeout 30 -o xorg-server.tar.xz \
        "https://www.x.org/releases/individual/xserver/xorg-server-${XORG_SERVER_VERSION}.tar.xz"; \
    echo "${XORG_SERVER_SHA256}  xorg-server.tar.xz" | sha256sum -c -; \
    tar -xf xorg-server.tar.xz; \
    cd "xorg-server-${XORG_SERVER_VERSION}"; \
    patch -p0 < /patches/xvfb-dri3.patch; \
    meson setup build \
        --prefix=/usr --sysconfdir=/etc --localstatedir=/var --buildtype=release \
        -Dxvfb=true -Dglamor=true -Ddri3=true -Dglx=true \
        -Dxcsecurity=true -Dxselinux=true \
        -Dxorg=false -Dxnest=false -Dxephyr=false -Dxwin=false -Dxquartz=false \
        -Dudev=false -Dudev_kms=false -Dsystemd_logind=false -Dsecure-rpc=false \
        -Dxkb_dir=/usr/share/X11/xkb -Dxkb_output_dir=/var/lib/xkb -Dxkb_bin_dir=/usr/bin \
        -Ddefault_font_path=/usr/share/fonts/X11/misc,/usr/share/fonts/X11/cyrillic,/usr/share/fonts/X11/100dpi/:unscaled,/usr/share/fonts/X11/75dpi/:unscaled,/usr/share/fonts/X11/Type1,/usr/share/fonts/X11/100dpi,/usr/share/fonts/X11/75dpi,built-ins \
        -Ddocs=false -Ddevel-docs=false; \
    ninja -C build hw/vfb/Xvfb; \
    install -Dm755 build/hw/vfb/Xvfb /build-out/usr/bin/Xvfb; \
    /build-out/usr/bin/Xvfb -help 2>&1 | grep -q vfbdevice

# Build the joystick and webcam interposers and fake-udev inside this
# repository (they are dedicated to containerized environments and are not
# desktop components)
FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE} AS add-ons
ARG DEBIAN_FRONTEND="noninteractive"
# The retry configuration the final stage sets further down, repeated here: a
# separate stage is a separate filesystem, so the drop-in the later apt-get
# invocations of this stage inherit has to be written in this one.
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        > /etc/apt/apt.conf.d/99-selkies-retries
# libpipewire headers compile the webcam interposer's PipeWire frame source;
# the library itself is dlopen'ed at runtime, so no -dev package is needed in
# the final image.
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        build-essential \
        ca-certificates \
        libpipewire-0.3-dev && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
COPY addons/js-interposer /build/js-interposer
COPY addons/fake-udev /build/fake-udev
COPY addons/v4l2-interposer /build/v4l2-interposer
RUN set -e; \
    LIBDIR="$(gcc -print-multiarch)"; \
    mkdir -p "/opt/add-ons/lib/${LIBDIR}"; \
    gcc -shared -fPIC -o /opt/add-ons/lib/${LIBDIR}/selkies_joystick_interposer.so /build/js-interposer/joystick_interposer.c -ldl -pthread; \
    make -C /build/v4l2-interposer; \
    cp /build/v4l2-interposer/selkies_v4l2_interposer.so "/opt/add-ons/lib/${LIBDIR}/"; \
    make -C /build/fake-udev; \
    cp /build/fake-udev/libudev.so.1.0.0-fake /build/fake-udev/libudev.so.1 /build/fake-udev/libudev.so "/opt/add-ons/lib/${LIBDIR}/"; \
    # Also build the 32-bit variants so 32-bit (Wine-era) applications see the
    # virtual gamepads and webcam too (`/usr/$LIB` expands per process
    # bitness); amd64 only
    if [ "$(uname -m)" = "x86_64" ]; then \
        apt-get update && apt-get install --no-install-recommends -y gcc-multilib; \
        LIB32DIR="$(gcc -m32 -print-multiarch)"; \
        mkdir -p "/opt/add-ons/lib/${LIB32DIR}"; \
        gcc -m32 -shared -fPIC -o /opt/add-ons/lib/${LIB32DIR}/selkies_joystick_interposer.so /build/js-interposer/joystick_interposer.c -ldl -pthread; \
        make -C /build/v4l2-interposer all32; \
        cp /build/v4l2-interposer/selkies_v4l2_interposer_x86.so "/opt/add-ons/lib/${LIB32DIR}/selkies_v4l2_interposer.so"; \
        make -C /build/fake-udev all32; \
        cp /build/fake-udev/libudev_x86.so.1.0.0-fake "/opt/add-ons/lib/${LIB32DIR}/libudev.so.1.0.0-fake"; \
        ln -sf libudev.so.1.0.0-fake "/opt/add-ons/lib/${LIB32DIR}/libudev.so.1"; \
        ln -sf libudev.so.1.0.0-fake "/opt/add-ons/lib/${LIB32DIR}/libudev.so"; \
    fi

# Session compositor for the Wayland backend, built from source so it carries
# the Selkies patches: a control socket at `$XDG_RUNTIME_DIR/labwc.sock`, served
# unconditionally, over which Selkies adds a screen when a second display
# attaches and removes it when that display disconnects (window queries ride the
# same socket), and a cursor path that carries a held drag across the screen
# boundary instead of clamping it at the edge. wlroots is built first at the
# version this labwc pins; both install under /usr/lib unqualified so the copy
# into the runtime stage is the same on every architecture.
FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE} AS labwc
ARG DEBIAN_FRONTEND="noninteractive"
ARG WLROOTS_VERSION
ARG LABWC_VERSION
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        > /etc/apt/apt.conf.d/99-selkies-retries
RUN apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
        build-essential \
        ca-certificates \
        gettext \
        git \
        glslang-tools \
        hwdata \
        meson \
        ninja-build \
        pkg-config \
        libcairo2-dev \
        libdisplay-info-dev \
        libdrm-dev \
        libgbm-dev \
        libglib2.0-dev \
        libglvnd-dev \
        libinput-dev \
        liblcms2-dev \
        libliftoff-dev \
        libpango1.0-dev \
        libpixman-1-dev \
        libpng-dev \
        librsvg2-dev \
        libseat-dev \
        libsfdo-dev \
        libudev-dev \
        libvulkan-dev \
        libwayland-dev \
        libxcb1-dev \
        libxcb-composite0-dev \
        libxcb-dri3-dev \
        libxcb-errors-dev \
        libxcb-ewmh-dev \
        libxcb-icccm4-dev \
        libxcb-present-dev \
        libxcb-randr0-dev \
        libxcb-render0-dev \
        libxcb-render-util0-dev \
        libxcb-res0-dev \
        libxcb-shape0-dev \
        libxcb-shm0-dev \
        libxcb-sync-dev \
        libxcb-xfixes0-dev \
        libxcb-xinput-dev \
        libxkbcommon-dev \
        wayland-protocols \
        xwayland && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
COPY addons/base/patches/labwc-ipc.patch addons/base/patches/labwc-seam.patch addons/base/patches/labwc-screens.patch /build/patches/
COPY addons/base/build-labwc.sh /build/
RUN /build/build-labwc.sh

FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies Base Container"
LABEL org.opencontainers.image.description="Selkies session without a desktop environment: X11 (Xvfb) and the headless Wayland backend (labwc), PipeWire audio, GPU wiring, s6 service supervision, embedded coTURN"
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
        > /etc/apt/apt.conf.d/99-selkies-retries && \
    # nvtop sits in Debian's contrib component, which its base image leaves off;
    # Ubuntu ships it in universe, which is already enabled there.
    if [ -f /etc/apt/sources.list.d/debian.sources ]; then \
        sed -i 's/^Components: .*/& contrib/' /etc/apt/sources.list.d/debian.sources; \
    fi

# Ownership of the image's setuid and setgid files, used at both ends of the
# rootless build below and by every image layered on this one
COPY addons/base/selkies-privileged-files /usr/local/bin/selkies-privileged-files

ARG DEBIAN_FRONTEND="noninteractive"
# Configure rootless user environment for constrained conditions without escalated root privileges inside containers
ARG TZ="UTC"
ENV PASSWD="mypasswd"
# The one knob for the generated locale; the ENV block below reuses it.
ARG LOCALE="en_US.UTF-8"
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
    # Debian's locale-gen takes no locale arguments and generates only what
    # /etc/locale.gen has uncommented, which Ubuntu's honours as well
    sed -i "s/^# *\(${LOCALE} UTF-8\)/\1/" /etc/locale.gen && locale-gen && \
    ln -snf "/usr/share/zoneinfo/${TZ}" /etc/localtime && echo "${TZ}" > /etc/timezone && \
    # Only use sudo-root for root-owned directory (/dev, /proc, /sys) or user/group permission operations, not for apt-get installation or file/directory operations
    mv -f /usr/bin/sudo /usr/bin/sudo-root && \
    ln -snf /usr/bin/fakeroot /usr/bin/sudo && \
    groupadd -g 1000 ubuntu || echo 'Failed to add ubuntu group' && \
    useradd -ms /bin/bash ubuntu -u 1000 -g 1000 || echo 'Failed to add ubuntu user' && \
    usermod -a -G adm,audio,cdrom,dialout,dip,fax,floppy,games,input,lp,plugdev,render,ssl-cert,sudo,tape,tty,video,voice ubuntu && \
    echo "ubuntu ALL=(ALL:ALL) NOPASSWD: ALL" >> /etc/sudoers && \
    echo "ubuntu:${PASSWD}" | chpasswd && \
    # The recursive chown clears every setuid and setgid bit, and a bit on a
    # file uid 1000 owns is refused anyway, so what is privileged is recorded
    # first and given back by the layer at the end of this file. It stays
    # released for the whole rootless build in between: dpkg replaces a file by
    # hardlinking the old one aside, which the kernel denies uid 1000 on a
    # setuid file it does not own, and an archive update to util-linux, shadow,
    # sudo, fuse3 or dbus would fail the layer taking it.
    selkies-privileged-files release && \
    { chown -R -f -h --no-preserve-root ubuntu:ubuntu / || echo 'Failed to set filesystem ownership in some paths to ubuntu user'; }

# Set locales
ENV LANG="${LOCALE}"
ENV LANGUAGE="en_US:en"
ENV LC_ALL="${LOCALE}"

# Rootless build from here on: layers run as the session user through
# fakeroot, so the package database and apt cache stay owned by that user and
# in-session package management behaves like `/usr/bin/sudo` (the fakeroot
# alias) does at runtime. Real-root operations in the live session use
# /usr/bin/sudo-root.
USER 1000
# Use BUILDAH_FORMAT=docker in buildah
SHELL ["/usr/bin/fakeroot", "--", "/bin/sh", "-c"]

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
        # A session hands the user a terminal: `file` is what desktop tooling
        # types a path with, `less` is the pager man and git reach for, and nano
        # is the only editor otherwise present
        file \
        less \
        nano \
        # Archive handling, which a file manager shells out to and a session
        # with no unpacker cannot do at all
        zip \
        unzip \
        xz-utils \
        bzip2 \
        zstd \
        # How an application opens a file or a URL in the session, and the
        # standard home directories a file manager and desktop expect
        xdg-utils \
        xdg-user-dirs \
        desktop-file-utils \
        # X11 display server and utilities
        xserver-xorg-core \
        xvfb \
        x11-xserver-utils \
        x11-xkb-utils \
        x11-utils \
        xauth \
        wmctrl \
        dbus-x11 \
        # Forked by Selkies where the in-process paths cannot serve: xdotool for
        # key injection a layout cannot express, xclip for the X11 clipboard
        # fallback, and cvt (xcvt) for the modeline a resize needs when the
        # requested mode is not already on the output
        xdotool \
        xclip \
        xcvt \
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
        # x264 for pixelflux's striped software H.264 encoder (optional GPL
        # feature of prebuilt wheels; the package pulls the matching runtime)
        x264 \
        # ffmpeg sidecars pixelflux links (hardware VA-API and PPM sinks); the
        # metapackage resolves the ABI-versioned libavutil/libavfilter/libavcodec
        # per release, whose suffixes move every LTS. opus-tools covers pcmflux's
        # audio encoder
        ffmpeg \
        opus-tools \
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
        pipewire-audio \
        pipewire-pulse \
        wireplumber \
        # Applications that open an ALSA device directly rather than through
        # PulseAudio, which find no sound card at all without this
        pipewire-alsa \
        # Xft resources alone reach a toolkit only at process start, and few
        # desktop environments run an XSETTINGS manager of their own: without
        # one on the selection a DPI change moves nothing until the session
        # restarts. Wayland has no equivalent gap, where the compositor hands
        # applications the scale.
        xsettingsd \
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
        # The ICD written below names an OpenCL driver; without a loader to read
        # it the file is inert and no application sees a device
        ocl-icd-libopencl1 \
        clinfo \
        # VDPAU and the tools that inspect whichever GPU is passed in. The
        # drivers themselves rather than vdpau-driver-all: that metapackage
        # pulls the proprietary NVIDIA driver and is absent on some releases.
        vainfo \
        vdpauinfo \
        mesa-vdpau-drivers \
        libvdpau-va-gl1 \
        vulkan-tools \
        # Per-vendor GPU monitors; nvtop sits in Debian's contrib, enabled above
        radeontop \
        nvtop \
        # Fonts: DejaVu/Liberation for Latin, Noto for worldwide script and emoji coverage
        fonts-dejavu-core \
        fonts-liberation \
        fonts-noto-core \
        fonts-noto-cjk \
        fonts-noto-color-emoji \
        fonts-noto-extra \
        fonts-noto-cjk-extra \
        fonts-symbola \
        # The SVG loader GdkPixbuf needs, so GTK applications render scalable
        # icons rather than blanks
        librsvg2-common \
        # Python runtime and the embedded coTURN server (WebRTC mode)
        python3 \
        python3-pip \
        coturn \
        ssl-cert \
        # Session compositor for the Wayland backend: labwc, which draws
        # server-side titlebars with the iconify/maximize/close buttons a desktop
        # session is expected to have and performs the maximize and minimize
        # requests a Wayland taskbar sends over wlr-foreign-toplevel. The
        # distribution package supplies the runtime libraries; the binary itself
        # is replaced below by the source build carrying the Selkies patches,
        # whose control socket grows and shrinks the screen count as displays
        # come and go. Not wayfire: it demands a DRM file descriptor the nested
        # Wayland backend has none of without a GPU
        labwc && \
    # The Intel media stack exists for amd64 alone
    if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
        apt-get install --no-install-recommends -y \
            intel-media-va-driver intel-gpu-tools; \
    fi && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

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
ENV NVIDIA_VISIBLE_DEVICES="all"
ENV NVIDIA_DRIVER_CAPABILITIES="all"
ENV __GL_SYNC_TO_VBLANK="0"

# Package state is already owned by the session user: the recursive chown above
# handed it over, and the apt runs after it went through fakeroot as uid 1000.
# Keep the log dir present for in-session apt runs.
RUN install -d /var/log/apt

# Install Selkies with pip: from the wheels dropped into `wheels/`, or the
# latest PyPI release when `wheels/` is empty. When fresh pixelflux/pcmflux
# wheels (built from the main HEAD of selkies-project/pixelflux and
# selkies-project/pcmflux) ride along, pip selects the ones matching this image's
# Python and platform; everything else still resolves from PyPI.
# `wheels/` is bind-mounted rather than copied: it holds every interpreter and
# platform variant while this image installs only the matching ones, so a copy
# would commit the whole set to a layer no later removal can reclaim.
RUN --mount=type=bind,source=addons/base/wheels,target=/tmp/wheels \
    set -e; \
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
    rm -rf /tmp/picked

# The DRI3-capable server replaces the distribution's /usr/bin/Xvfb; chowned
# to the session user like the rest of the image so fakeroot-sudo apt can
# still write under /usr after the copy
COPY --from=xvfb --chown=1000:1000 /build-out/usr/bin/Xvfb /usr/bin/Xvfb

# The patched labwc replaces the distribution binary, and the wlroots it was
# built against comes along; the distribution labwc package above keeps every
# other runtime library in place
COPY --from=labwc --chown=1000:1000 /usr/bin/labwc /usr/bin/labwc
COPY --from=labwc --chown=1000:1000 /usr/lib/libwlroots-0.19.so* /usr/lib/

# Install the joystick and webcam interposers and fake-udev (container-only)
COPY --from=add-ons --chown=1000:1000 /opt/add-ons/lib/ /usr/lib/

# Makes the session look like the Openbox one: floating windows with their usual
# titlebar buttons, no bar of the compositor's own, and XWayland for the X11-only
# applications
COPY --chown=1000:1000 addons/base/labwc-rc.xml /etc/xdg/labwc/rc.xml

# Copy the startup scripts and s6 service definitions (same /etc/service/<svc>/run layout as runit, supervised by s6-supervise)
COPY --chown=1000:1000 addons/base/container-entrypoint.sh /etc/container-entrypoint.sh
COPY --chown=1000:1000 addons/base/selkies-entrypoint.sh /etc/selkies-entrypoint.sh
COPY --chown=1000:1000 addons/base/services/ /etc/service/
RUN chmod -f 755 /etc/container-entrypoint.sh /etc/selkies-entrypoint.sh && \
    find /etc/service \( -name run -o -name finish \) -exec chmod -f 755 {} +

# Configure bashrc to show /etc/motd and enable bash completion, and to give an
# interactive shell the running session's environment. A shell started with
# `docker exec` is not a child of the entrypoint and inherits none of what it
# resolved, so DISPLAY, WAYLAND_DISPLAY and the audio socket are all unset and
# every graphical command fails. Only filled in when the caller passed no
# display of their own.
RUN echo "[ -n \"\$PS1\" ] && [ -z \"\$DISPLAY\" ] && [ -z \"\$WAYLAND_DISPLAY\" ] && [ -r \"\${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}/container-env\" ] && . \"\${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}/container-env\"" >> /etc/bash.bashrc && \
    echo "[ ! -z \"\$TERM\" -a -r /etc/motd ] && echo \"\$(grep '^PRETTY_NAME=' /etc/os-release | cut -d= -f2 | tr -d '\"')\" && cat /etc/motd" >> /etc/bash.bashrc && \
    printf "==============================================\n=           Selkies Base Container           =\n==============================================\n" > /etc/motd

USER 0
# The restores below need the real chown syscall: under the fakeroot SHELL even
# a uid-0 process gets the LD_PRELOAD wrappers, and an ownership change lands
# only in fakeroot's database while the chmod goes through to the filesystem —
# so the committed layer keeps the setuid bit on a file owned by uid 1000,
# which the kernel refuses to honour.
SHELL ["/bin/sh", "-c"]
# Enable sudo through sudo-root with uid 0.
# `restore` returns the owners and bits recorded when the build released them.
# The uid-1000 fakeroot layers above install helpers of their own (polkit's
# agent helper and pkexec, utempter) that no record covers and that really
# belong to uid 1000, and the kernel honours neither bit on a file it does not
# own, so those are named here. Every image layered on this one releases what
# it inherits the same way and names the helpers its own packages bring.
RUN selkies-privileged-files restore && \
    if [ -d "/usr/libexec/sudo" ]; then SUDO_LIB="/usr/libexec/sudo"; else SUDO_LIB="/usr/lib/sudo"; fi && \
    chown -R -f -h --no-preserve-root root:root /usr/bin/sudo-root /etc/sudo.conf /etc/sudoers /etc/sudoers.d /etc/sudo_logsrvd.conf "${SUDO_LIB}" || echo 'Failed to provide root permissions in some paths relevant to sudo' && \
    chmod -f 4755 /usr/bin/sudo-root || echo 'Failed to set chmod setuid for root' && \
    for helper in /usr/lib/polkit-1/polkit-agent-helper-1 /usr/libexec/polkit-agent-helper-1 /usr/bin/pkexec; do \
        if [ -e "$helper" ]; then chown -f root:root "$helper" && chmod -f 4755 "$helper" || echo "Failed to restore setuid for $helper"; fi; \
    done && \
    for helper in /usr/lib/*/utempter/utempter /usr/libexec/utempter/utempter; do \
        if [ -e "$helper" ]; then chown -f root:utmp "$helper" && chmod -f 2755 "$helper" || echo "Failed to restore setgid for $helper"; fi; \
    done

USER 1000

# Default runtime environment (overridable with -e)
ENV PIPEWIRE_LATENCY="256/48000"
ENV XDG_RUNTIME_DIR="/tmp/runtime-ubuntu"
ENV PIPEWIRE_RUNTIME_DIR="${XDG_RUNTIME_DIR}"
ENV PULSE_RUNTIME_PATH="${XDG_RUNTIME_DIR}/pulse"
ENV PULSE_SERVER="unix:${XDG_RUNTIME_DIR}/pulse/native"
# dbus-daemon to the below address is required during startup
ENV DBUS_SYSTEM_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/dbus-system-bus"
# X11 (Xvfb) by default; set SELKIES_WAYLAND=true — or the legacy
# PIXELFLUX_WAYLAND spelling — for the headless Wayland backend. Deliberately
# not baked as an ENV: in the resolution order every reader follows, a set
# SELKIES_WAYLAND would shadow the legacy spelling for good.
# Session compositor nested inside the Wayland capture compositor: empty picks
# labwc, "none" leaves applications on the capture compositor with no window
# management and no XWayland, and any other name runs that compositor as it is
# installed
ENV SELKIES_WAYLAND_COMPOSITOR=""
# Run X11 instead when a GPU is present but the Wayland compositor cannot reach
# it, so the session keeps hardware acceleration. Without a GPU both backends
# render in software and Wayland stays; set false to keep Wayland either way,
# which renders the whole session in software
ENV SELKIES_WAYLAND_X11_FALLBACK="true"
# Web interface port
ENV SELKIES_PORT="8080"
# A session carries a login and a clipboard, so the container serves TLS by
# default. With nothing at the configured certificate paths the server mints a
# pair per install in its state directory; --https-cert / --https-key point at
# a real one, and SELKIES_ENABLE_HTTPS=false serves plain HTTP for a
# deployment that terminates TLS in front of the container.
ENV SELKIES_ENABLE_HTTPS="true"

# ssl-cert generates its snakeoil pair during the image build, so every pull
# would hold the same private key and any of them could impersonate any other.
# Dropping it leaves the per-install pair as what a deployment falls back to.
RUN rm -f /etc/ssl/private/ssl-cert-snakeoil.key /etc/ssl/certs/ssl-cert-snakeoil.pem
# AppImages mount themselves through FUSE, which a container is usually not
# given; extracting instead is what makes them run at all here.
ENV APPIMAGE_EXTRACT_AND_RUN="1"

ENV SHELL="/bin/bash"
ENV USER="ubuntu"
ENV HOME="/home/ubuntu"
WORKDIR /home/ubuntu

EXPOSE 8080

# s6 respawns a service that cannot start, so "running" says nothing: only the
# port answering tells a session from a container that never served. /api/health
# needs no credentials, and only the http-to-TLS direction fails outright.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS -k -o /dev/null "https://localhost:${SELKIES_PORT:-8080}/api/health" || \
        curl -fsS -o /dev/null "http://localhost:${SELKIES_PORT:-8080}/api/health"

ENTRYPOINT ["/etc/container-entrypoint.sh"]
