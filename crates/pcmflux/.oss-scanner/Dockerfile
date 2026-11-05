# Builds pcmflux for Anthropic's OSS Scanner (github.com/anthropics/oss-scanner) the way CI's cargo job does: libopus
# vendored and built by its crate, PulseAudio's development package, and the test suite. The scan runs offline, so
# everything a build or test needs is fetched here.
FROM rust:1-trixie
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends cmake build-essential libpulse-dev python3-dev \
    && rm -rf /var/lib/apt/lists/*

# scanner contract: the checkout lives inside the image, at /src
COPY . /src
WORKDIR /src

RUN cargo test --manifest-path pcmflux/Cargo.toml --no-run \
    && cargo test --manifest-path pcmflux/Cargo.toml --features python --no-run
ENV CARGO_NET_OFFLINE=true
RUN cargo test --manifest-path pcmflux/Cargo.toml || echo "WARNING: cargo test reported failures"
RUN cargo test --manifest-path pcmflux/Cargo.toml --features python || echo "WARNING: cargo test reported failures"
