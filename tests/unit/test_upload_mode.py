#!/usr/bin/env python3
"""An upload carries the mode of a file it replaces only from this user's own file.

Re-uploading over one's own file keeps its permission bits, never its set-ID or
sticky bits. A file another account owns, planted under the name in a
directory both can write, passes nothing on: the upload keeps the staging
file's creation mode. Another account cannot be created without root, so the
foreign owner is the server's effective uid reading differently from the
file's owner.
"""
import os
import stat
import sys
import tempfile
from unittest.mock import patch

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.stream_server import _carry_destination_mode  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [upload-mode] {label}  {detail}", flush=True)


def carried(dest_mode: int, foreign: bool = False) -> int:
    """The staging file's mode after carrying from a destination of `dest_mode`."""
    root = tempfile.mkdtemp(prefix="selkies-upload-mode-")
    dest, staging = os.path.join(root, "dest"), os.path.join(root, "staging")
    for path in (dest, staging):
        open(path, "wb").close()
    os.chmod(dest, dest_mode)
    os.chmod(staging, 0o644)
    if foreign:
        with patch("os.geteuid", return_value=os.lstat(dest).st_uid + 1):
            _carry_destination_mode(staging, dest)
    else:
        _carry_destination_mode(staging, dest)
    return stat.S_IMODE(os.lstat(staging).st_mode)


def main() -> int:
    mode = carried(0o700)
    check("this user's own file passes its permission bits on", mode == 0o700, oct(mode))
    mode = carried(0o4755)
    check("this user's own set-UID file passes its permission bits on, not set-UID", mode == 0o755, oct(mode))
    mode = carried(0o2775)
    check("this user's own set-GID file passes its permission bits on, not set-GID", mode == 0o775, oct(mode))
    mode = carried(0o777, foreign=True)
    check("another account's world-writable file passes nothing on", mode == 0o644, oct(mode))
    mode = carried(0o4755, foreign=True)
    check("another account's set-UID file passes nothing on", mode == 0o644, oct(mode))
    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
