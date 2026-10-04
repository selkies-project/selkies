# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Office desktop: published Ubuntu 26.04 Selkies desktop, plus the NETCORE
# wheel (sidebar and title) and the NETCORE wallpaper.
ARG BASE_IMAGE=ghcr.io/selkies-project/selkies/desktop@sha256:17366ded0187e635bc40bdafaf6b9955854a70a4ff091a17f3034a12d7a74090
FROM ${BASE_IMAGE}

USER 0
COPY selkies-0.0.0.dev0-py3-none-any.whl /tmp/selkies-0.0.0.dev0-py3-none-any.whl
RUN python3 -m pip install --break-system-packages --no-deps --no-cache-dir --force-reinstall /tmp/selkies-0.0.0.dev0-py3-none-any.whl \
    && rm -f /tmp/selkies-0.0.0.dev0-py3-none-any.whl
COPY wallpaper.svg /usr/share/lxqt/themes/selkies/wallpaper.svg
ENV SELKIES_UI_TITLE="NETCORE⚡DIGITAL"
USER 1000
