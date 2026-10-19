ARG IMAGE_TAG="ubuntu-24.04"
FROM mcr.microsoft.com/devcontainers/base:${IMAGE_TAG}

ARG DEBIAN_FRONTEND="noninteractive"

# Every network fetch in this image is retried, because a rebuild that reaches
# for the apt index, a signing key, or a .deb is one transient failure away from
# losing the whole run. apt is configured once here so each later invocation
# inherits it -- including the ones inside install-desktop-environment.sh and
# the vendors' repositories added below; curl and pip carry their own flags at
# each call.
RUN printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' \
        | sudo tee /etc/apt/apt.conf.d/99-selkies-retries > /dev/null

# Install base Selkies dependencies (X11 capture/input, PulseAudio, Mesa/VA-API, Wayland plumbing)
RUN sudo apt-get update && sudo apt-get install --no-install-recommends -y \
        ca-certificates \
        curl \
        gnupg \
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
        libpixman-1-0 \
        libxcb-render0 \
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
        build-essential \
        python3-tk \
        wl-clipboard \
        libsdl2-2.0-0 \
        openbox \
        kwin-x11 \
        kwin-wayland \
        netcat-openbsd && \
    sudo apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/* && \
    sudo python3 -m pip install --no-cache-dir --retries 5 --timeout 60 --upgrade build

# Install the LXQt desktop environment
COPY ./features/desktop-selkies/src/install-desktop-environment.sh /tmp/
RUN /tmp/install-desktop-environment.sh

# The Chrome browser and the web client's build toolchain (Node.js LTS), from
# Google's and NodeSource's apt repositories, whose packages apt checks against
# the signing key each names. Each key has to be the one primary key whose
# fingerprint its vendor publishes. Chrome's package rewrites
# google-chrome.sources to trust a key it carries itself, and writes one on a
# first install, so the entry here is named apart from it and the package's
# defaults file keeps it from writing its own. NodeSource's Node.js is
# preferred over the distribution's, as its setup script would.
RUN key() { \
        curl -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180 "$1" | sudo tee "$3" > /dev/null && \
        [ "$(gpg --show-keys --with-colons "$3" | awk -F: '$1 == "pub" { p = 1; next } p && $1 == "fpr" { print $10; p = 0 }')" = "$2" ] || \
        { echo "$1 is not the key $2" >&2; sudo rm -f "$3"; return 1; }; \
    } && \
    sudo install -dm755 /etc/apt/keyrings /etc/apt/sources.list.d /etc/apt/preferences.d && \
    key https://dl.google.com/linux/linux_signing_key.pub EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796 /etc/apt/keyrings/google-chrome.asc && \
    key https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key 6F71F525282841EEDAF851B42F59B5F99B1BE0B4 /etc/apt/keyrings/nodesource.asc && \
    printf 'Types: deb\nURIs: https://dl.google.com/linux/chrome/deb/\nSuites: stable\nComponents: main\nSigned-By: /etc/apt/keyrings/google-chrome.asc\n' \
        | sudo tee /etc/apt/sources.list.d/google-linux.sources > /dev/null && \
    printf 'repo_add_once="false"\nrepo_reenable_on_distupgrade="false"\n' | sudo tee /etc/default/google-chrome > /dev/null && \
    printf 'Types: deb\nURIs: https://deb.nodesource.com/node_22.x\nSuites: nodistro\nComponents: main\nSigned-By: /etc/apt/keyrings/nodesource.asc\n' \
        | sudo tee /etc/apt/sources.list.d/nodesource.sources > /dev/null && \
    printf 'Package: nodejs\nPin: origin deb.nodesource.com\nPin-Priority: 600\n' | sudo tee /etc/apt/preferences.d/nodejs > /dev/null && \
    sudo apt-get update && sudo apt-get install --no-install-recommends -y xdg-utils google-chrome-stable nodejs && \
    sudo apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/* && \
    xdg-settings set default-web-browser google-chrome.desktop
