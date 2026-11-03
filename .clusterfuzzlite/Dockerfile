# The OSS-Fuzz builder that ClusterFuzzLite (.github/workflows/fuzz.yaml) and OSS-Fuzz build crates/selkies-rtc/fuzz with.
FROM gcr.io/oss-fuzz-base/base-builder-rust
COPY . $SRC/selkies
COPY .clusterfuzzlite/build.sh $SRC/
WORKDIR $SRC/selkies
