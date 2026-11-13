# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The TURN REST credential service (crates/selkies-turn-rest), built from the
# repository root: docker build -f addons/turn-rest/Dockerfile .
FROM rust:1-alpine AS build

# AWS-LC, the service's HMAC, is compiled from the source its crate vendors.
# apk has no retry option of its own, so the one package fetch this image makes
# is bounded-retried in the shell rather than left to fail the build outright.
RUN i=1; until apk add --no-cache build-base cmake; do \
        [ "$i" -ge 5 ] && exit 1; \
        i=$((i+1)); sleep 5; \
    done

WORKDIR /build

# The workspace's manifest and lockfile and every member they name; only the
# service and the crates it depends on are compiled.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p selkies-turn-rest

FROM alpine:3

COPY --from=build /build/target/release/selkies-turn-rest /usr/local/bin/selkies-turn-rest

# The published port is the container's boundary, so the service listens on
# every interface of both families; a kernel without IPv6 serves IPv4 alone.
ENV TURN_REST_ADDR="0.0.0.0,::"
# Number of worker threads
ENV WEB_CONCURRENCY="3"
# Replace environment with customized TURN server
ENV TURN_SHARED_SECRET="openrelayprojectsecret"
ENV TURN_HOST="staticauth.openrelay.metered.ca"
ENV TURN_PORT="443"
ENV TURN_PROTOCOL="udp"
ENV TURN_TLS="false"
ENV STUN_HOST="stun.l.google.com"
ENV STUN_PORT="19302"

EXPOSE 8008

# nobody by number, so a cluster that requires a non-root user can verify it
# from the image; the service reads only its environment and binds above 1024.
USER 65534:65534

CMD ["selkies-turn-rest"]
