# Builds pixelflux for Anthropic's OSS Scanner (github.com/anthropics/oss-scanner) the way CI's cargo job does: the
# distribution's codec and display development packages, the crate with its dependencies fetched, and the test
# suite (tests needing a GPU, a render node or an encoder device mark themselves ignored). The scan runs offline,
# so everything a build or test needs is fetched here.
FROM rust:1-trixie
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake nasm libclang-dev python3-dev libx265-dev libvpx-dev libsvtav1enc-dev libdav1d-dev \
      libde265-dev libgbm-dev libdrm-dev libwayland-dev libinput-dev libudev-dev libxkbcommon-dev libpixman-1-dev \
    && rm -rf /var/lib/apt/lists/*

# scanner contract: the checkout lives inside the image, at /src
COPY . /src
WORKDIR /src
# pixelflux reads the API patches/x264 adds, so libx264 is built with them.
RUN sh scripts/build-x264.sh /usr/local

RUN cargo test --manifest-path pixelflux/Cargo.toml --no-run \
    && cargo test --manifest-path pixelflux/Cargo.toml --features python --no-run
ENV CARGO_NET_OFFLINE=true
RUN cargo test --manifest-path pixelflux/Cargo.toml || echo "WARNING: cargo test reported failures"
RUN cargo test --manifest-path pixelflux/Cargo.toml --features python || echo "WARNING: cargo test (python) reported failures"
