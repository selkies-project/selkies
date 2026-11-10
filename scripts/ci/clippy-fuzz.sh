#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# clippy over every fuzz target, with the fuzzing cfg cargo fuzz builds them
# with. Each fuzz directory is a workspace of its own that compiles modules of
# its crate's src on their own, so the workspace's clippy sees none of them.
# Run from the repository root.
#
# The CI lint gate and the pre-commit hook both call this, so the two cannot
# drift apart.
set -eu

test -f Cargo.toml

for manifest in crates/*/fuzz/Cargo.toml; do
    cargo clippy --manifest-path "${manifest}" --bins \
        --config 'build.rustflags = ["--cfg", "fuzzing"]' -- -D warnings
done
