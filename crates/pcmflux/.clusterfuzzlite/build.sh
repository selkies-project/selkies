#!/bin/bash -eu
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds pcmflux/fuzz in an OSS-Fuzz builder. Each target compiles the modules it fuzzes on
# their own, so the build needs none of the crate's system libraries.
cd pcmflux
cargo fuzz build -O
for target in $(cargo fuzz list); do
  cp "fuzz/target/x86_64-unknown-linux-gnu/release/$target" "$OUT/"
  if [ -d "fuzz/seeds/$target" ]; then
    zip -q -j "$OUT/${target}_seed_corpus.zip" "fuzz/seeds/$target"/*
  fi
done
