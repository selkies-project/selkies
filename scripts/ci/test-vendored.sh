#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# The vendored rtc crates' own tests, with the crypto provider the server
# builds them with. Each vendored crate is a workspace of its own with its own
# lockfile, so the workspace's `cargo test` runs none of them. Run from the
# repository root; extra arguments (`--offline`, `--no-run`) go to every
# `cargo test`.
#
# CI's server unit tests and the OSS Scanner's image both call this, so the
# two cannot drift apart.
set -eu

test -f Cargo.toml

# Under the workspace's target, which git and codespell already leave alone,
# rather than in a target of each vendored crate's own
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/vendored}"

vendor=crates/selkies-rtc/vendor
cargo test --locked --manifest-path "${vendor}/rtc-sctp/Cargo.toml" "$@"
for crate in rtc-dtls rtc-srtp; do
    cargo test --locked --manifest-path "${vendor}/${crate}/Cargo.toml" \
        --no-default-features --features crypto-aws-lc-rs "$@"
done
