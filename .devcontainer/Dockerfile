ARG IMAGE_TAG=ubuntu-24.04
FROM mcr.microsoft.com/devcontainers/base:${IMAGE_TAG}

ARG DEBIAN_FRONTEND=noninteractive

# Install base Selkies dependencies (X11 capture/input, PulseAudio, Mesa/VA-API, Wayland plumbing)
RUN sudo apt-get update && sudo apt-get install --no-install-recommends -y \
        ca-certificates \
        curl \
        jq \
        python3-pip \
        python3-dev \
        python3-setuptools \
        python3-wheel \
        python3-venv \
        libgcrypt20 \
        libglib2.0-0 \
        glib-networking \
        libpulse0 \
        pulseaudio \
        pulseaudio-utils \
        libdrm2 \
        libegl1 \
        libgl1 \
        libgles2 \
        libglvnd0 \
        libglx0 \
        libva2 \
        libva-drm2 \
        libgbm1 \
        mesa-utils \
        wayland-protocols \
        libwayland-egl1 \
        libxkbcommon0 \
        wmctrl \
        x11-utils \
        x11-xkb-utils \
        x11-xserver-utils \
        xserver-xorg-core \
        xvfb \
        libx11-xcb1 \
        libxcb1 \
        libxcb-shm0 \
        libxcb-xfixes0 \
        libxcb-randr0 \
        libxcb-dri3-0 \
        libxdamage1 \
        libxfixes3 \
        libxtst6 \
        libxext6 \
        coturn \
        imagemagick \
        jstest-gtk \
        netcat-openbsd && \
    sudo apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/* && \
    sudo python3 -m pip install --no-cache-dir --upgrade build

# Install the LXQt desktop environment
COPY ./features/desktop-selkies/src/install-desktop-environment.sh /tmp/
RUN /tmp/install-desktop-environment.sh

# Install the Chrome browser (package available for amd64 only)
RUN if [ "$(uname -m)" = "x86_64" ]; then \
        cd /tmp && curl -fsSL -o ./google-chrome-stable.deb "https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb" && \
        sudo apt-get update && sudo apt-get install --no-install-recommends -y xdg-utils ./google-chrome-stable.deb && \
        sudo apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/* && \
        sudo rm -f google-chrome-stable.deb && \
        xdg-settings set default-web-browser google-chrome.desktop; \
    fi

# Web client build toolchain (Node.js LTS)
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | sudo bash - && \
    sudo apt-get update && sudo apt-get install --no-install-recommends -y nodejs && \
    sudo apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
