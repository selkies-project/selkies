#!/usr/bin/env python3
"""The images' published placeholder password starts the server with a warning.

The Selkies images default `PASSWD` to a placeholder their documentation prints,
which becomes the login of a server listening on every container interface. The
server still starts on it, as an image's deliberate default, but says in the log
that the login is one anybody knows; a password of the operator's own passes
quietly, and no login at all has nothing to warn of.
"""
import logging
import os
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.settings import SETTING_DEFINITIONS, AppSettings  # noqa: E402
from selkies.stream_server import CentralizedStreamServer  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    ok = bool(ok)
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [placeholder-password] {label}  {detail}", flush=True)


class Capture(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.records = []

    def emit(self, record: logging.LogRecord) -> None:
        self.records.append(record)


def warnings_for(*flags: str, env=None) -> list:
    saved = {k: os.environ.get(k) for k in ("PASSWD", "PASSWORD", "SELKIES_BASIC_AUTH_PASSWORD")}
    for k in saved:
        os.environ.pop(k, None)
    os.environ.update(env or {})
    sys.argv = ["selkies", "--enable-https=false", *flags]
    try:
        settings = AppSettings(SETTING_DEFINITIONS)
    finally:
        for k, v in saved.items():
            os.environ.pop(k, None)
            if v is not None:
                os.environ[k] = v
    sink = Capture()
    log = logging.getLogger("server")
    log.addHandler(sink)
    try:
        CentralizedStreamServer(settings)._require_configured_credentials()
    finally:
        log.removeHandler(sink)
    return [r.getMessage() for r in sink.records if r.levelno == logging.WARNING]


def main() -> int:
    warned = warnings_for(env={"PASSWD": "mypasswd"})
    check("the images' PASSWD placeholder is warned of", any("placeholder" in w for w in warned), warned)
    warned = warnings_for("--basic-auth-password=mypasswd")
    check("so is the same placeholder given on the command line", any("placeholder" in w for w in warned), warned)
    check("a password of the operator's own passes quietly", not warnings_for(env={"PASSWD": "correct-horse"}))
    check("no login has nothing to warn of", not warnings_for("--enable-basic-auth=false", env={"PASSWD": "mypasswd"}))
    print(f"[placeholder-password] {passed}/{passed + failed} passed", flush=True)
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
