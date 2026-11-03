#!/bin/bash -eu
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds crates/selkies-rtc/fuzz in an OSS-Fuzz builder, for ClusterFuzzLite and OSS-Fuzz
# alike. The targets drive selkies-rtc, which links no system library and needs neither
# pixelflux nor pcmflux.
cd crates/selkies-rtc
cargo fuzz build -O
for target in $(cargo fuzz list); do
  cp "${CARGO_TARGET_DIR:-fuzz/target}/x86_64-unknown-linux-gnu/release/$target" "$OUT/"
  if [ -d "fuzz/seeds/$target" ]; then
    zip -q -j "$OUT/${target}_seed_corpus.zip" "fuzz/seeds/$target"/*
  fi
done
