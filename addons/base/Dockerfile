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
# addons/desktop builds the reference desktop, selkies-desktop, on top of this,
# and any other desktop can be built the same way.
#
#   - X11 display server (XLibre's Xvfb, with glamor and DRI3) and the headless Wayland backend
#     with SELKIES_WAYLAND=true; the Selkies window manager manages either under
#     SELKIES_WM, and a desktop built on the image may nest a session compositor
#     of its own instead (SELKIES_WAYLAND_COMPOSITOR)
#   - s6 service supervision, running as PID 1 or below a foreign init
#   - Joystick interposer + fake-udev (container-only gamepad plumbing) and the
#     V4L2 webcam interposer, wired with LD_PRELOAD
#   - GPU wiring: NVIDIA/VA-API/Vulkan runtime and Zink for hardware OpenGL
#   - Audio: PipeWire with the PulseAudio, ALSA, JACK, and V4L2 interfaces,
#     recorded through the PulseAudio one by pcmflux
#   - Embedded coTURN for WebRTC firewall traversal
#   - The Selkies server and the web client it serves
#
# Nothing is compiled here: the server, the desktop shell, the X server, the
# interposers, and NVIDIA's X11 EGL platform are the glibc bundle's of this tree
# for the image's architecture (infra/bundle), which every other channel
# repackages too, so the image runs the binaries the bundle's checks ran.
#
# Supported bases: Ubuntu 26.04 and Debian Trixie, and later releases of both
# (select with --build-arg DISTRIB_IMAGE=ubuntu|debian DISTRIB_RELEASE=26.04|trixie).

ARG DISTRIB_IMAGE="ubuntu"
ARG DISTRIB_RELEASE="26.04"
ARG TARGETARCH
# The toolchain images the bundle is built in (infra/bundle/toolchain.sh), the
# newest main published for each architecture
ARG TOOLCHAIN_REPOSITORY="ghcr.io/selkies-project/selkies/bundle-toolchain"
# Where the bundle the image installs comes from: the stage below that builds
# it, or a build context naming a directory that holds it already
ARG BUNDLE="bundle-from-source"

# The bundle the image installs, a directory holding the glibc bundle's tarball
# for the image's architecture. A build that has one passes it as a build
# context and names it in BUNDLE (`--build-context bundle=<dir> --build-arg
# BUNDLE=bundle`), and builds none: images.yaml passes the one its run built, a
# local build the out directory of infra/bundle/build.sh. Naming it through an
# argument rather than replacing the stage keeps the stages that would build it
# out of the build, the toolchain image's lookup among them. A build without
# one builds it here, from this tree in the toolchain image, which compiles the
# server, the shell, and the interposers and takes the rest from the image, or
# builds that too where the tree changed what the image is built from
# (infra/bundle/build-in-image.sh); `--build-context
# toolchain-<arch>=docker-image://<image>` names another toolchain image. The
# web client is built through scripts/ci/build-web.sh as every channel builds
# it, its controller database's GitHub lookups using the build secret
# github_token where one is given (`--secret id=github_token,env=GITHUB_TOKEN`).
FROM node:26-alpine AS web
WORKDIR /build
COPY Cargo.toml ./
COPY scripts/ci/build-web.sh ./scripts/ci/
COPY web/selkies-web-core ./web/selkies-web-core
COPY web/selkies-dashboard ./web/selkies-dashboard
COPY web/universal-touch-gamepad ./web/universal-touch-gamepad
COPY docs/assets/logo ./docs/assets/logo
RUN --mount=type=secret,id=github_token sh scripts/ci/build-web.sh

FROM ${TOOLCHAIN_REPOSITORY}:glibc-x86_64 AS toolchain-amd64
FROM ${TOOLCHAIN_REPOSITORY}:glibc-aarch64 AS toolchain-arm64

FROM toolchain-${TARGETARCH} AS bundle-build
ARG SELKIES_VERSION=""
WORKDIR /repo
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY infra ./infra
COPY addons/base/patches ./addons/base/patches
COPY addons/input-interposer ./addons/input-interposer
COPY addons/fake-udev ./addons/fake-udev
COPY addons/v4l2-interposer ./addons/v4l2-interposer
COPY docs/assets/logo ./docs/assets/logo
COPY --from=web /build/dist/web ./dist/web
RUN --mount=type=cache,target=/build/target --mount=type=cache,target=/opt/rust/cargo/registry \
    sh infra/bundle/build-in-image.sh /out

FROM scratch AS bundle-from-source
COPY --from=bundle-build /out/ /

FROM ${BUNDLE} AS bundle-source

FROM ${DISTRIB_IMAGE}:${DISTRIB_RELEASE}

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"
LABEL org.opencontainers.image.title="Selkies Base Container"
LABEL org.opencontainers.image.description="Selkies session without a desktop environment: X11 (Xvfb) and the headless Wayland backend, PipeWire audio, GPU wiring, s6 service supervision, embedded coTURN"
LABEL org.opencontainers.image.source="https://github.com/selkies-project/selkies"
LABEL org.opencontainers.image.licenses="MPL-2.0"

ARG DISTRIB_IMAGE
ARG DISTRIB_RELEASE

# Every network fetch in this image is retried, because a build that reaches for
# a package index, a signing key, or a wheel is one transient failure away from
# losing the whole run. apt is configured once here so each of its later
# invocations inherits it; curl and pip carry their own flags at each call.
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        > /etc/apt/apt.conf.d/99-selkies-retries && \
    # nvtop sits in Debian's contrib component and the Intel media drivers below
    # in its non-free one, both of which its base image leaves off; Ubuntu ships
    # them in universe and multiverse, which are already enabled there.
    if [ -f /etc/apt/sources.list.d/debian.sources ]; then \
        sed -i 's/^Components: .*/& contrib non-free/' /etc/apt/sources.list.d/debian.sources; \
    fi

# Ownership of the image's setuid and setgid files, used at both ends of the
# rootless build below and by every image layered on this one
COPY addons/base/selkies-privileged-files /usr/local/bin/selkies-privileged-files
# ucf and ucfr for the rootless layers and sudo in the session (`ucf-fakeroot`)
COPY addons/base/ucf-fakeroot /usr/local/bin/ucf-fakeroot
# Downloads checked against the hash their source publishes for whatever
# version is taken (`fetch-verified`), for this image's layers and the images
# built on it
COPY addons/base/fetch-verified /usr/local/bin/fetch-verified

ARG DEBIAN_FRONTEND="noninteractive"
# Configure rootless user environment for constrained conditions without escalated root privileges inside containers
ARG TZ="UTC"
ENV PASSWD="mypasswd"
# The one knob for the generated locale; the ENV block below reuses it.
ARG LOCALE="en_US.UTF-8"
# The distribution archive's state, which CI reads before the build: an archive
# update rebuilds the layers from here where a build cache would hand back the
# packages of the last build. Empty in a build that does not read it.
ARG DISTRIB_ARCHIVE=""
RUN apt-get clean && apt-get update && apt-get dist-upgrade -y && apt-get install --no-install-recommends -y \
        apt-utils \
        dbus-user-session \
        fakeroot \
        fuse \
        kmod \
        locales \
        ssl-cert \
        sudo \
        ucf \
        udev \
        tzdata && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/* && \
    # Debian's locale-gen takes no locale arguments and generates only what
    # /etc/locale.gen has uncommented, which Ubuntu's honors as well
    sed -i "s/^# *\(${LOCALE} UTF-8\)/\1/" /etc/locale.gen && locale-gen && \
    ln -snf "/usr/share/zoneinfo/${TZ}" /etc/localtime && echo "${TZ}" > /etc/timezone && \
    # Only use sudo-root for root-owned directory (/dev, /proc, /sys) or user/group permission operations, not for apt-get installation or file/directory operations
    mv -f /usr/bin/sudo /usr/bin/sudo-root && \
    ln -snf /usr/bin/fakeroot /usr/bin/sudo && \
    for tool in ucf ucfr; do \
        dpkg-divert --quiet --local --rename --add "/usr/bin/${tool}" && \
        ln -snf /usr/local/bin/ucf-fakeroot "/usr/bin/${tool}" || exit 1; \
    done && \
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
    # sudo, fuse3, or dbus would fail the layer taking it.
    selkies-privileged-files release && \
    # The chown copies each file of the layers below into this one once per
    # name, so a file the distribution hardlinks lands here as that many
    # copies (its Rust coreutils is one 11.8 MB program under 116 names, 1.3 GB
    # copied). The links are recorded first and made again after it.
    find / -xdev -type f -links +1 -printf '%i %p\n' | sort -n > /tmp/selkies-hardlinks && \
    { chown -R -f -h --no-preserve-root ubuntu:ubuntu / || echo 'Failed to set filesystem ownership in some paths to ubuntu user'; } && \
    awk '{ path = substr($0, index($0, " ") + 1) } $1 == inode { print first "\t" path; next } { inode = $1; first = path }' /tmp/selkies-hardlinks | \
        while IFS="$(printf '\t')" read -r first path; do ln -f "${first}" "${path}" || echo "Failed to relink ${path}"; done && \
    rm -f /tmp/selkies-hardlinks

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
        # types a path with, `less` is the pager man and git reach for, and the
        # editors, monitor, and version control a shell session is expected to
        # have. Not desktop components: any session built on this one wants them
        file \
        less \
        nano \
        vim \
        htop \
        git \
        # Networking and diagnostics, and the entropy daemon a container needs
        # to seed its pool where the host gives it little
        net-tools \
        netcat-openbsd \
        haveged \
        # Archive handling, which a file manager shells out to and a session
        # with no unpacker cannot do at all
        zip \
        unzip \
        xz-utils \
        bzip2 \
        zstd \
        unrar \
        unar \
        # How an application opens a file or a URL in the session, and the
        # standard home directories a file manager and desktop expect
        xdg-utils \
        xdg-user-dirs \
        desktop-file-utils \
        # NVIDIA's EGL platform libraries for GBM and Wayland, open source and
        # independent of the driver's version: the injected driver's EGL reaches
        # them whether or not the runtime that injects the driver carries them
        # along. The X11 one is built above and copied in below, with what it
        # links against here.
        libnvidia-egl-gbm1 \
        libnvidia-egl-wayland1 \
        libx11-xcb1 \
        libxcb-dri3-0 \
        libxcb-present0 \
        # Printing: the CUPS scheduler Selkies runs as the session user for its
        # Selkies queue, and the filters that make a PDF of whatever an
        # application prints
        cups-daemon \
        cups-client \
        cups-filters \
        # X11 display server and utilities
        xserver-xorg-core \
        xvfb \
        x11-xserver-utils \
        x11-xkb-utils \
        x11-utils \
        xauth \
        wmctrl \
        dbus-x11 \
        # The accessibility bus the session's applications describe their windows on,
        # which Pelorus reads (SELKIES_PELORUS)
        at-spi2-core \
        # Forked by Selkies where the in-process paths cannot serve: xdotool for
        # key injection a layout cannot express, and xclip for the X11 clipboard
        # fallback
        xdotool \
        xclip \
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
        pipewire-audio \
        pipewire-pulse \
        wireplumber \
        # Applications that open an ALSA device directly rather than through
        # PulseAudio, which find no sound card at all without this
        pipewire-alsa \
        # The rest of what PipeWire serves an application: JACK for the audio
        # tools that speak only that, V4L2 so a camera reaches applications
        # through the same graph the webcam interposer feeds, libcamera for the
        # sensors that have no V4L2 node, and the ALSA command-line tools for
        # looking at the graph from a shell
        pipewire-jack \
        pipewire-audio-client-libraries \
        libspa-0.2-jack \
        pipewire-v4l2 \
        pipewire-libcamera \
        alsa-utils \
        jackd2 \
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
        # VA-API on an NVIDIA GPU, which has no Mesa driver: this maps it onto
        # NVDEC through the injected driver, and is inert without one
        nvidia-vaapi-driver \
        vulkan-tools \
        # Per-vendor GPU monitors; nvtop sits in Debian's contrib, enabled above
        radeontop \
        nvtop \
        # Spell checking, which text components look for at runtime through
        # Enchant rather than linking against
        aspell \
        aspell-en \
        hunspell \
        enchant-2 \
        # The distribution's ffmpeg, with the codecs its default build leaves
        # out, for the desktop's players and browsers, which fall back to it
        # for anything they cannot decode themselves
        ffmpeg \
        libavcodec-extra \
        # Fonts: DejaVu/Liberation for Latin, Noto for worldwide script and emoji coverage
        fonts-dejavu-core \
        fonts-liberation \
        fonts-noto-core \
        fonts-noto-cjk \
        fonts-noto-color-emoji \
        fonts-noto-extra \
        fonts-noto-cjk-extra \
        fonts-symbola \
        fonts-noto-ui-core \
        fonts-noto-ui-extra \
        fonts-dejavu-extra \
        fonts-freefont-ttf \
        fonts-hack \
        fonts-ubuntu \
        fonts-opensymbol \
        # The bitmap and scalable fonts an X server's default font path names,
        # which an X11 application that asks for a core font needs
        xfonts-scalable \
        xbitmaps \
        # The SVG loader GdkPixbuf needs, so GTK applications render scalable
        # icons rather than blanks
        librsvg2-common \
        # Python for the image's build helper (fetch-verified) and the embedded
        # coTURN server (WebRTC mode)
        python3 \
        coturn \
        ssl-cert && \
    # The Intel media stack exists for amd64 alone. The non-free builds of the
    # Intel drivers are the ones taken: they decode more formats than the free
    # builds, which they conflict with. A 32-bit application brings the 32-bit
    # graphics runtime it needs in the container it runs in: proot-apps gives
    # Steam its own.
    if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
        apt-get install --no-install-recommends -y \
            intel-media-va-driver-non-free i965-va-driver-shaders intel-gpu-tools \
            rar; \
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
# Off: vsync here costs an application about two frames of input latency, the
# frames its driver queues behind the fake vblank, and does not reliably keep
# a GPU-bound one from starving the capture, which a frame limiter does
# (docs/components/desktop-image.md). GLX takes it as the default an
# application's own swap interval overrides; NVIDIA's EGL applies it over the
# application's choice.
ENV __GL_SYNC_TO_VBLANK="0"

# Package state is already owned by the session user: the recursive chown above
# handed it over, and the apt runs after it went through fakeroot as uid 1000.
# Keep the log dir present for in-session apt runs.
RUN install -d /var/log/apt

# The bundle's server and shell at /usr/local/bin, the libraries their run path
# names beside them at /usr/local/lib/selkies, and its X server, the patched
# XLibre Xvfb with glamor and DRI3 (infra/bundle/xservers.sh), with its keymap
# compiler and data under /usr/local/libexec/selkies and /usr/local/share/selkies,
# where it finds them relative to itself; it takes the distribution's place at
# /usr/bin/Xvfb, whose own has neither glamor nor DRI3, so GL clients would
# render in software with a GPU present. The interposers go where the
# linuxserver.io images and proot-apps' app roots put theirs (/usr/lib and
# /opt/lib, the 32-bit builds beside them with a _32 suffix), so the preload an
# application inherits names the same files inside an app root as here;
# fake-udev stays out of the system library directory, reached only through the
# preload. NVIDIA's X11 EGL platform goes where the injected driver's EGL looks
# for it. The bundle's sound server, buses, Xwayland, and print scheduler stay
# out: the image runs the distribution's PipeWire, buses, and Xwayland, and its
# CUPS, whose filters convert what the bundle's scheduler refuses.
RUN --mount=type=bind,from=bundle-source,target=/tmp/bundle \
    set -e; \
    tarball="$(find /tmp/bundle -maxdepth 1 -name "selkies-*-$(uname -m)-linux-gnu.tar.gz")"; \
    if [ "$(echo "${tarball}" | grep -c .)" != 1 ]; then \
        echo "the bundle holds no single glibc bundle for $(uname -m) (infra/bundle/build.sh)" >&2; \
        exit 1; \
    fi; \
    unpacked=/tmp/selkies-bundle; \
    mkdir -p "${unpacked}"; \
    tar -xzf "${tarball}" -C "${unpacked}" --strip-components=1; \
    install -Dm755 "${unpacked}/bin/selkies" /usr/local/bin/selkies; \
    install -Dm755 "${unpacked}/bin/selkies-desktop" /usr/local/bin/selkies-desktop; \
    mkdir -p /usr/local/lib/selkies /usr/local/libexec/selkies /usr/local/share/selkies; \
    find "${unpacked}/lib/selkies" -maxdepth 1 -type f -exec cp -a {} /usr/local/lib/selkies/ \;; \
    cp -a "${unpacked}/libexec/selkies/Xvfb" "${unpacked}/libexec/selkies/xkbcomp" /usr/local/libexec/selkies/; \
    cp -a "${unpacked}/share/selkies/xkb" "${unpacked}/share/selkies/fonts" \
        "${unpacked}"/share/selkies/icon.* /usr/local/share/selkies/; \
    ln -sf /usr/local/libexec/selkies/Xvfb /usr/bin/Xvfb; \
    preload="${unpacked}/lib/selkies/preload"; \
    mkdir -p /opt/lib; \
    for suffix in "" _32; do \
        [ -f "${preload}/selkies_input_interposer${suffix}.so" ] || continue; \
        install -m755 "${preload}/selkies_input_interposer${suffix}.so" "${preload}/selkies_v4l2_interposer${suffix}.so" /usr/lib/; \
        ln -sf "selkies_input_interposer${suffix}.so" "/usr/lib/selkies_joystick_interposer${suffix}.so"; \
        install -m755 "${preload}/libudev.so.1.0.0-fake${suffix}" /opt/lib/; \
    done; \
    multiarch="/usr/lib/$(uname -m)-linux-gnu"; \
    install -m755 "${unpacked}"/lib/selkies/egl-x11/libnvidia-egl-*.so.1 "${multiarch}/"; \
    mkdir -p /usr/share/egl/egl_external_platform.d; \
    install -m644 "${unpacked}"/share/selkies/egl/egl_external_platform.d/*.json /usr/share/egl/egl_external_platform.d/; \
    rm -rf "${unpacked}"

# Fails the image build rather than publishing one whose payload cannot start:
# the server carries its web client, every library it and the X server link
# resolves, the ones pixelflux and the X server open by name from the host at
# run time, which no ELF dependency names and only the apt lines above put
# here, are present, and the X server is the patched one.
RUN set -e; \
    selkies --version; \
    selkies --help > /dev/null; \
    selkies --version | grep -F "web client: embedded ("; \
    selkies-desktop --help > /dev/null; \
    for elf in /usr/local/bin/selkies /usr/local/bin/selkies-desktop /usr/local/libexec/selkies/Xvfb; do \
        ! ldd "${elf}" | grep 'not found'; \
    done; \
    for name in libgbm.so.1 libva.so.2 libva-drm.so.2 libdrm.so.2; do \
        ldconfig -p | awk '{print $1}' | grep -qxF "${name}" || { echo "missing ${name}"; exit 1; }; \
    done; \
    usage="$(Xvfb -help 2>&1 || true)"; \
    echo "${usage}" | grep -q -- '-glamor'; \
    echo "${usage}" | grep -q -- 'CRTCs per screen (default: 4)'; \
    test -f /usr/lib/selkies_input_interposer.so && test -f /opt/lib/libudev.so.1.0.0-fake

# A PulseAudio client that leaves its buffer to pipewire-pulse gets 40 ms, not 2 s
COPY --chown=1000:1000 addons/base/pipewire-pulse.conf.d/ /etc/pipewire/pipewire-pulse.conf.d/

# Copy the startup scripts and s6 service definitions (same /etc/service/<svc>/run layout as runit, supervised by s6-supervise)
COPY --chown=1000:1000 addons/base/container-entrypoint.sh /etc/container-entrypoint.sh
COPY --chown=1000:1000 addons/base/selkies-entrypoint.sh /etc/selkies-entrypoint.sh
COPY --chown=1000:1000 addons/base/services/ /etc/service/
RUN chmod -f 755 /etc/container-entrypoint.sh /etc/selkies-entrypoint.sh && \
    find /etc/service \( -name run -o -name finish \) -exec chmod -f 755 {} +

# Configure bashrc to show /etc/motd and enable bash completion, and to give an
# interactive shell the running session's environment. A shell started with
# `docker exec` is not a child of the entrypoint and inherits none of what it
# resolved, so DISPLAY, WAYLAND_DISPLAY, and the audio socket are all unset and
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
# which the kernel refuses to honor.
SHELL ["/bin/sh", "-c"]
# Enable sudo through sudo-root with uid 0.
# `restore` returns the owners and bits recorded when the build released them.
# The uid-1000 fakeroot layers above install helpers of their own (polkit's
# agent helper and pkexec, utempter) that no record covers and that really
# belong to uid 1000, and the kernel honors neither bit on a file it does not
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
# Session compositor nested inside the Wayland capture compositor, for a desktop
# built on this image that installs one and runs without the Selkies window
# manager: its command name, run as installed. Empty or "none" leaves
# applications on the capture compositor, with no window management and no
# XWayland unless SELKIES_WM supplies them
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
# VS Code's launcher reads a Microsoft kernel as "inside WSL" and exits with a
# prompt to install the Windows build instead; a container on Docker Desktop
# or WSL is that kernel, and the prompt is meaningless there.
ENV DONT_PROMPT_WSL_INSTALL="1"

ENV SHELL="/bin/bash"
ENV USER="ubuntu"
ENV HOME="/home/ubuntu"
WORKDIR /home/ubuntu

EXPOSE 8080

# s6 respawns a service that cannot start, so "running" says nothing: only the
# port answering tells a session from a container that never served. /api/health
# needs no credentials, and only the http-to-TLS direction fails outright. The
# route moves under the deployment prefix with every other, and the prefix is
# spelled with or without slashes, as the server takes it.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD p="${SELKIES_SUBFOLDER-${SUBFOLDER-}}"; p="${p#/}"; p="${p%/}"; p="${p:+/${p}}"; \
        curl -fsS -k -o /dev/null "https://localhost:${SELKIES_PORT:-8080}${p}/api/health" || \
        curl -fsS -o /dev/null "http://localhost:${SELKIES_PORT:-8080}${p}/api/health"

ENTRYPOINT ["/etc/container-entrypoint.sh"]
