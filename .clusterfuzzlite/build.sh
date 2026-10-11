#!/bin/bash -eu
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds tests/fuzz in an OSS-Fuzz builder, for ClusterFuzzLite and OSS-Fuzz
# alike. The runtime dependencies are installed without the capture stack
# (pixelflux, pcmflux), which none of the fuzzed modules imports.
python3 scripts/ci/unit-deps.py > /tmp/requirements.txt
pip3 install -r /tmp/requirements.txt
pip3 install --no-deps .

for fuzzer in tests/fuzz/fuzz_*.py; do
  compile_python_fuzzer "$fuzzer"
done
zip -j "$OUT/fuzz_sdp_seed_corpus.zip" tests/fuzz/corpus/sdp/*
