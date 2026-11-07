#!/bin/bash -eu
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Build each independent fuzz workspace in the OSS-Fuzz builder. The native
# targets compile their parser modules directly and need no media libraries.
for crate in crates/selkies-rtc crates/pixelflux crates/pcmflux; do
  (
    cd "${crate}"
    cargo fuzz build -O
    for target in $(cargo fuzz list); do
      cp "${CARGO_TARGET_DIR:-fuzz/target}/x86_64-unknown-linux-gnu/release/${target}" "$OUT/"
      if [ -d "fuzz/seeds/${target}" ]; then
        zip -q -j "$OUT/${target}_seed_corpus.zip" "fuzz/seeds/${target}"/*
      fi
    done
  )
done
