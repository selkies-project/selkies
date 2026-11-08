# Builds Selkies for Anthropic's OSS Scanner (github.com/anthropics/oss-scanner) the way CI's unit tier does: the
# server's crates with the toolchain and libraries they build with and every crate they depend on (pixelflux and
# pcmflux among them), the LD_PRELOAD interposers and test tools with debug info, and the web
# client's npm dependencies and bundle. The scan runs offline, so everything a test needs is fetched here.
FROM node:26-trixie
ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/venv/bin:/opt/cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates build-essential gcc-multilib git jq libkvazaar-dev libxkbcommon0 pkg-config python3 python3-dev python3-venv \
      pulseaudio pulseaudio-utils shellcheck xkb-data xvfb \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m tester

# scanner contract: the checkout lives inside the image, at /src; the tests build into it as the unprivileged user
COPY --chown=tester:tester . /src
WORKDIR /src

# Owned by that user in the layer that writes them, since cargo locks its home even offline
RUN scripts/ci/server-build-deps.sh && rm -rf /var/lib/apt/lists/* && cargo fetch --locked \
    && chown -R tester:tester /opt/cargo /opt/rustup
RUN python3 -m venv /opt/venv && pip install --no-cache-dir pytest
RUN for dir in addons/input-interposer addons/v4l2-interposer addons/fake-udev tests/tools; do \
      make -C "$dir" CFLAGS="-O1 -g -fno-omit-frame-pointer" || exit 1; \
    done
# The npm dependencies of both packages, and the bundle the server's HTTP tests serve from dist/web
RUN scripts/ci/build-web.sh
RUN runuser -u tester -- cargo test --offline --locked --workspace --no-run \
    && runuser -u tester -- cargo test --offline --locked -p pixelflux --lib --features python --no-run \
    && runuser -u tester -- cargo test --offline --locked -p pixelflux --lib --no-default-features --features openh264 --no-run \
    && runuser -u tester -- cargo test --offline --locked -p pcmflux --lib --features python --no-run

# As an unprivileged user, since the suites check that permission errors are handled. The sound server the audio
# tests drive is the one CI's suites stream from.
RUN runuser -u tester -- sh -c 'scripts/ci/test-stack.sh > /dev/null \
      && PULSE_SERVER="unix:$(pactl info | sed -n "s/^Server String: //p")" \
         cargo test --offline --locked --workspace' || echo "WARNING: the server's unit tests reported failures"; \
    runuser -u tester -- cargo test --offline --locked -p pixelflux --lib --features python \
      || echo "WARNING: pixelflux Python tests reported failures"; \
    runuser -u tester -- cargo test --offline --locked -p pixelflux --lib --no-default-features --features openh264 \
      || echo "WARNING: pixelflux OpenH264 tests reported failures"; \
    runuser -u tester -- cargo test --offline --locked -p pcmflux --lib --features python \
      || echo "WARNING: pcmflux Python tests reported failures"; \
    runuser -u tester -- env E2E_WORKDIR=/tmp/selkies-unit \
      python -m pytest tests -m unit -q -rs -p no:cacheprovider || echo "WARNING: the unit tier reported failures"
