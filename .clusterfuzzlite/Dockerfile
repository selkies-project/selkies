# The OSS-Fuzz builder for the selkies-rtc, pixelflux, and pcmflux fuzz workspaces.
FROM gcr.io/oss-fuzz-base/base-builder-rust
COPY . $SRC/selkies
COPY .clusterfuzzlite/build.sh $SRC/
WORKDIR $SRC/selkies
