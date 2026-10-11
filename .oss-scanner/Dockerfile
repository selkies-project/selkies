# Builds Selkies for Anthropic's OSS Scanner (github.com/anthropics/oss-scanner) the way CI's unit tier does: the
# source tree with its runtime dependencies (pixelflux and pcmflux, the capture stack, are stubbed by the suites),
# the LD_PRELOAD interposers and test tools with debug info, and the web client's npm dependencies. The scan runs
# offline, so everything a test needs is fetched here.
FROM node:26-trixie
ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/opt/venv/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates build-essential gcc-multilib git libxkbcommon0 pkg-config python3 python3-dev python3-venv \
      shellcheck xkb-data \
    && rm -rf /var/lib/apt/lists/*

# scanner contract: the checkout lives inside the image, at /src
COPY . /src
WORKDIR /src

RUN python3 -m venv /opt/venv && python3 scripts/ci/unit-deps.py > /tmp/unit-requirements.txt \
    && pip install --no-cache-dir pytest -r /tmp/unit-requirements.txt && pip install --no-cache-dir --no-deps -e .
RUN for dir in addons/input-interposer addons/v4l2-interposer addons/fake-udev tests/tools; do \
      make -C "$dir" CFLAGS="-O1 -g -fno-omit-frame-pointer" || exit 1; \
    done
RUN for dir in addons/selkies-web-core addons/selkies-dashboard addons/selkies-dashboard-wish; do \
      (cd "$dir" && npm ci --no-audit --no-fund) || exit 1; \
    done

# As an unprivileged user, since the suites check that permission errors are handled.
RUN useradd -m tester && runuser -u tester -- env E2E_WORKDIR=/tmp/selkies-unit \
      python -m pytest tests -m unit -q -rs -p no:cacheprovider || echo "WARNING: the unit tier reported failures"
