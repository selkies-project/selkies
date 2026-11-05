# The OSS-Fuzz builder that ClusterFuzzLite (.github/workflows/fuzz.yml) builds pcmflux/fuzz with.
FROM gcr.io/oss-fuzz-base/base-builder-rust
COPY . $SRC/pcmflux
COPY .clusterfuzzlite/build.sh $SRC/
WORKDIR $SRC/pcmflux
