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

apt-get clean && sudo rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*
