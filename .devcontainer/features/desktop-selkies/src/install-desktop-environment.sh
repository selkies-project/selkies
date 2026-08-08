#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

# Install the minimal LXQt desktop (Openbox window manager)
apt-get clean && sudo apt-get update && sudo DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y \
    lxqt-session \
    lxqt-panel \
    lxqt-runner \
    lxqt-about \
    openbox \
    obconf-qt \
    qterminal \
    pcmanfm-qt \
    fonts-dejavu-core \
    fonts-liberation

# LXQt's system defaults, which the distribution packages do not ship: without
# them a session has no theme, so lxqt-panel finds no stylesheet, keeps the
# invalid colours it writes as its own default, and paints nothing at all — a
# panel that runs and maps a window but is invisible against the desktop. The
# icon theme goes with it, since LXQt otherwise falls back to Oxygen, which is
# not installed here. This is the system scope, so a user's own
# ~/.config/lxqt/lxqt.conf still overrides it. The names are taken from what the
# packages above pulled in, so a package change surfaces here rather than
# reaching a user as a desktop that will not paint.
icon_theme=$(for n in Adwaita breeze oxygen hicolor; do
    [ -d "/usr/share/icons/${n}" ] && { echo "${n}"; break; }
done)
theme=$(for n in ambiance system light; do
    [ -d "/usr/share/lxqt/themes/${n}" ] && { echo "${n}"; break; }
done)
sudo mkdir -pm755 /etc/xdg/lxqt /etc/xdg/pcmanfm-qt/lxqt
printf '[General]\nicon_theme=%s\ntheme=%s\n' "${icon_theme}" "${theme}" \
    | sudo tee /etc/xdg/lxqt/lxqt.conf > /dev/null
# pcmanfm-qt gets its desktop background the same way, for the same reason: its
# own default is #000000 with no wallpaper, which reads as a broken session
# rather than a plain one. A flat colour rather than one of the shipped
# wallpapers, because every full refresh re-encodes whatever the desktop shows
# and a photograph costs far more of the stream than a single tone.
printf '[Desktop]\nWallpaperMode=color\nBgColor=#2e3436\nFgColor=#ffffff\n' \
    | sudo tee /etc/xdg/pcmanfm-qt/lxqt/settings.conf > /dev/null

apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
