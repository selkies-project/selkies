# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Run a bare `selkies` the way a user does after `pip install selkies` and check it.

The standalone desktop (crates/selkies/src/commands/standalone.rs) has to say
where it is, serve HTTPS on localhost:8080 behind the placeholder login the
containers document, refuse a request without it, and keep its desktop shell
running: on a host with nothing installed but the wheel, a shell that cannot
find a font or a library exits and is restarted, which the log shows. Where
localhost resolves to ::1 the entry page is moved to 127.0.0.1 before the
login, which urllib follows.

Usage: verify-standalone.py <selkies-executable>
"""

import base64
import os
import signal
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

URL = "https://localhost:8080/"
USER = "ubuntu"
PASSWORD = "mypasswd"


def fetch(password: str = "") -> int:
    """The status of a GET of the entry page, with a login when `password` is given."""
    request = urllib.request.Request(URL)
    if password:
        token = base64.b64encode(f"{USER}:{password}".encode()).decode()
        request.add_header("Authorization", f"Basic {token}")
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    try:
        with urllib.request.urlopen(request, context=context, timeout=10) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def fail(message: str, log: Path) -> None:
    """Print `message` and the tail of the session's log, and exit."""
    print(f"::error::{message}; the log:")
    print("\n".join(log.read_text(errors="replace").splitlines()[-30:]))
    sys.exit(1)


def main() -> None:
    """Start the standalone desktop, check it, and stop it."""
    if len(sys.argv) != 2:
        sys.exit("usage: verify-standalone.py <selkies-executable>")
    home = Path(tempfile.mkdtemp(prefix="selkies-standalone-"))
    log = home / "selkies.log"
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SELKIES_", "XDG_", "DISPLAY", "WAYLAND"))}
    env["HOME"] = str(home)
    with open(log, "w") as out:
        session = subprocess.Popen(
            [sys.argv[1]], env=env, cwd=home, stdout=out, stderr=subprocess.STDOUT, start_new_session=True
        )
    try:
        deadline = time.monotonic() + 90
        status = 0
        while time.monotonic() < deadline and session.poll() is None:
            try:
                status = fetch()
            except OSError:
                status = 0
            if status == 401:
                break
            time.sleep(1)
        if status != 401:
            fail(f"{URL} never asked for a login (last status {status})", log)
        text = log.read_text(errors="replace")
        if f"your desktop is at {URL.rstrip('/')}, signing in as {USER} with the placeholder password {PASSWORD}" not in text:
            fail("the log does not say where the desktop is and how to sign in", log)
        if fetch(PASSWORD) != 200:
            fail("the placeholder login does not sign in", log)
        time.sleep(10)
        text = log.read_text(errors="replace")
        if "selkies-desktop started" not in text:
            fail("the desktop shell never started", log)
        if "selkies-desktop exited" in text or "panicked" in text:
            fail("the desktop shell exited", log)
        print(f"a bare selkies serves its desktop at {URL} behind the placeholder login")
    finally:
        try:
            os.killpg(session.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        session.wait(timeout=30)


if __name__ == "__main__":
    main()
