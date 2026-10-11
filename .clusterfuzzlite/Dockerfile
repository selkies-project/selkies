# The OSS-Fuzz builder that ClusterFuzzLite (.github/workflows/fuzz.yaml) and OSS-Fuzz build tests/fuzz with.
FROM gcr.io/oss-fuzz-base/base-builder-python
COPY . $SRC/selkies
COPY .clusterfuzzlite/build.sh $SRC/
WORKDIR $SRC/selkies
