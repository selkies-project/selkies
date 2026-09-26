#!/usr/bin/env python3
"""The images' published placeholder password starts the container with a warning.

The Selkies images default `PASSWD` to a placeholder their documentation prints,
which becomes the login of a server listening on every container interface. The
image's entrypoint says so in the container's log, resolving the login the way
settings.py does; a password of the operator's own passes quietly, no login has
nothing to warn of, and the warning never carries the password. The server knows
nothing of any image's defaults: it only refuses a login nobody set.
"""
import logging
import os
import re
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ENTRYPOINT = os.path.join(REPO, "addons", "base", "container-entrypoint.sh")
DOCKERFILE = os.path.join(REPO, "addons", "base", "Dockerfile")
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.settings import SETTING_DEFINITIONS, AppSettings  # noqa: E402
from selkies.stream_server import CentralizedStreamServer  # noqa: E402

AUTH_VARS = ("PASSWD", "PASSWORD", "SELKIES_BASIC_AUTH_PASSWORD", "SELKIES_ENABLE_BASIC_AUTH")
PLACEHOLDER = re.search(r'^ENV PASSWD="([^"]*)"', open(DOCKERFILE).read(), re.M).group(1)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    ok = bool(ok)
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [placeholder-password] {label}  {detail}", flush=True)


def entrypoint_warning(env: dict) -> str:
    """What the entrypoint's placeholder check prints, run with its own parsing helpers."""
    body = open(ENTRYPOINT).read()
    parts = []
    for name in ("setting_value() {", "is_true() {"):
        begin = body.index(name)
        parts.append(body[begin:body.index("\n}\n", begin) + 3])
    begin = body.index("\nif { [ -z \"$(setting_value \"${SELKIES_ENABLE_BASIC_AUTH") + 1
    parts.append(body[begin:body.index("\nfi\n", begin) + 4])
    base = {k: v for k, v in os.environ.items() if k not in AUTH_VARS}
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "check.sh")
        with open(path, "w") as f:
            f.write("\n".join(parts))
        return subprocess.run(["bash", path], capture_output=True, text=True, timeout=60,
                              env={**base, **env}).stderr.strip()


def server_messages(env: dict) -> list:
    """Every record the server's credential check logs for this environment."""
    saved = {k: os.environ.get(k) for k in AUTH_VARS}
    for k in saved:
        os.environ.pop(k, None)
    os.environ.update(env)
    sys.argv = ["selkies", "--enable-https=false"]
    try:
        settings = AppSettings(SETTING_DEFINITIONS)
    finally:
        for k, v in saved.items():
            os.environ.pop(k, None)
            if v is not None:
                os.environ[k] = v

    class Capture(logging.Handler):
        records: list = []

        def emit(self, record: logging.LogRecord) -> None:
            self.records.append(record)

    sink = Capture()
    log = logging.getLogger("server")
    log.addHandler(sink)
    try:
        CentralizedStreamServer(settings)._require_configured_credentials()
    finally:
        log.removeHandler(sink)
    return [r.getMessage() for r in sink.records]


def main() -> int:
    check("the image publishes a placeholder", PLACEHOLDER, PLACEHOLDER)
    warned = entrypoint_warning({"PASSWD": PLACEHOLDER})
    check("the image's PASSWD placeholder is warned of", "placeholder" in warned, warned)
    check("the warning does not carry the password", warned and PLACEHOLDER not in warned, warned)
    check("so is PASSWORD, which settings.py reads ahead of PASSWD",
          entrypoint_warning({"PASSWORD": PLACEHOLDER, "PASSWD": "correct-horse"}))
    check("so is a blank SELKIES_ENABLE_BASIC_AUTH, which keeps the default",
          entrypoint_warning({"PASSWD": PLACEHOLDER, "SELKIES_ENABLE_BASIC_AUTH": " "}))
    check("SELKIES_BASIC_AUTH_PASSWORD of the operator's own passes quietly",
          not entrypoint_warning({"PASSWD": PLACEHOLDER, "SELKIES_BASIC_AUTH_PASSWORD": "correct-horse"}))
    check("a PASSWD of the operator's own passes quietly", not entrypoint_warning({"PASSWD": "correct-horse"}))
    check("no login has nothing to warn of",
          not entrypoint_warning({"PASSWD": PLACEHOLDER, "SELKIES_ENABLE_BASIC_AUTH": "false"}))
    logged = server_messages({"PASSWD": PLACEHOLDER})
    check("the server starts on it without logging anything", not logged, logged)
    print(f"[placeholder-password] {passed}/{passed + failed} passed", flush=True)
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
