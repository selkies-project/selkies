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
# Xvfb with DRI3 (https://github.com/linuxserver/docker-xvfb): the distribution
# server has none, so GL clients render in software even with a GPU present.
# Built against an older glibc than either base carries, which they both
# satisfy; override to match a different base or to opt out.
ARG XVFB_IMAGE="lscr.io/linuxserver/xvfb:debianbookworm"

FROM ${XVFB_IMAGE} AS xvfb

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
    chown -R -f -h --no-preserve-root ubuntu:ubuntu / || echo 'Failed to set filesystem ownership in some paths to ubuntu user' && \
    # Preserve setuid/setgid removed by chown; a setuid bit on a file not owned
    # by root is rejected, so the ownership has to be restored as well.
    # sudo.ws is the classic sudo binary under the name sudo-rs packaging
    # gives it; sudo-root is the real sudo, renamed above so that plain `sudo`
    # reaches fakeroot. /usr/bin/su ships only under its real name from
    # util-linux. wall and write are setgid tty rather than setuid, and the
    # two spool directories are setgid to the group that owns their contents.
    chown root:root /usr/lib/dbus-1.0/dbus-daemon-launch-helper /usr/bin/chfn /usr/bin/chsh /usr/bin/mount /usr/bin/gpasswd /usr/bin/passwd /usr/bin/newgrp /usr/bin/umount /usr/bin/su /usr/bin/sudo.ws /usr/bin/sudo-root /usr/bin/fusermount 2>/dev/null; \
    chmod -f 4755 /usr/lib/dbus-1.0/dbus-daemon-launch-helper /usr/bin/chfn /usr/bin/chsh /usr/bin/mount /usr/bin/gpasswd /usr/bin/passwd /usr/bin/newgrp /usr/bin/umount /usr/bin/su /usr/bin/sudo.ws /usr/bin/sudo-root /usr/bin/fusermount || echo 'Failed to set chmod setuid for some paths' && \
    chown root:shadow /usr/sbin/unix_chkpwd /usr/sbin/pam_extrausers_chkpwd /usr/bin/expiry /usr/bin/chage 2>/dev/null; \
    chown root:tty /usr/bin/wall /usr/bin/write 2>/dev/null; \
    chown root:staff /var/local 2>/dev/null; \
    chown root:mail /var/mail 2>/dev/null; \
    chmod -f 2755 /var/local /var/mail /usr/bin/wall /usr/bin/write /usr/sbin/unix_chkpwd /usr/sbin/pam_extrausers_chkpwd /usr/bin/expiry /usr/bin/chage || echo 'Failed to set chmod setgid for some paths'

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
        # requests a Wayland taskbar sends over wlr-foreign-toplevel. Its screen
        # count is fixed at startup, which is what the wayland service and
        # Selkies' spare-screen hold are built around. Not wayfire: it demands a
        # DRM file descriptor the nested Wayland backend has none of without a GPU
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
COPY --from=xvfb --chown=1000:1000 / /

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
# The uid-1000 fakeroot layers above install packages whose setuid and setgid
# helpers (polkit's agent helper and pkexec, utempter) then really belong to
# uid 1000, and the kernel honours neither bit on a file it does not own, so
# their ownership is restored here. Every image layered on this one repeats
# this for the helpers its own packages bring.
RUN if [ -d "/usr/libexec/sudo" ]; then SUDO_LIB="/usr/libexec/sudo"; else SUDO_LIB="/usr/lib/sudo"; fi && \
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

# The snakeoil pair ssl-cert generated above was generated during the image
# build: every pull of this image holds that private key, so serving on it
# would let any of them impersonate any other. Removed, so the per-install
# pair is what a deployment falls back to.
RUN rm -f /etc/ssl/private/ssl-cert-snakeoil.key /etc/ssl/certs/ssl-cert-snakeoil.pem
# AppImages mount themselves through FUSE, which a container is usually not
# given; extracting instead is what makes them run at all here.
ENV APPIMAGE_EXTRACT_AND_RUN="1"

ENV SHELL="/bin/bash"
ENV USER="ubuntu"
ENV HOME="/home/ubuntu"
WORKDIR /home/ubuntu

EXPOSE 8080

ENTRYPOINT ["/etc/container-entrypoint.sh"]
