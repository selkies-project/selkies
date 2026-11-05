# The OSS-Fuzz builder that ClusterFuzzLite (.github/workflows/fuzz.yml) builds pixelflux/fuzz with.
FROM gcr.io/oss-fuzz-base/base-builder-rust
COPY . $SRC/pixelflux
COPY .clusterfuzzlite/build.sh $SRC/
WORKDIR $SRC/pixelflux
