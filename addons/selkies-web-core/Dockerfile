FROM ghcr.io/linuxserver/baseimage-alpine:3.22

# Build frontend core along with dashboard

ARG SELKIES_MODE
ARG SELKIES_UPLOAD_DIR

RUN apk add \
    cmake \
    git \
    nodejs \
    npm

WORKDIR /tmp

COPY ./gst-web-core /tmp/gst-web-core
COPY ./selkies-dashboard /tmp/selkies-dashboard
COPY ./universal-touch-gamepad /tmp/universal-touch-gamepad

RUN \
  cd ./gst-web-core && \
  npm install && \
  npm run build && \
  DASHBOARDS="selkies-dashboard" && \
  mkdir /buildout && \
  cd /tmp/selkies-dashboard && \
  cp ../gst-web-core/dist/selkies-core.js src/ && \
  npm install && \
  export SELKIES_INJECT=1 && \
  export SELKIES_MODE="${SELKIES_MODE:-webrtc}" &&\
  export SELKIES_UPLOAD_DIR="${SELKIES_UPLOAD_DIR:-/home/ubuntu/Desktop}" &&\
  npm run build && \
  mkdir -p dist/src dist/nginx && \
  cp ../gst-web-core/dist/selkies-core.js dist/src/ && \
  cp ../universal-touch-gamepad/universalTouchGamepad.js dist/src/ && \
  cp ../gst-web-core/nginx/* dist/nginx/ && \
  cp -r ../gst-web-core/dist/jsdb dist/ && \
  mkdir -p /usr/share/nginx/html && \
  cp -ar dist/* /usr/share/nginx/html/;