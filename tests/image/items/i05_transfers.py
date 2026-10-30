"""5. Test file upload and download, preferably for large files over 100MB. Make sure it's fast enough and uses up the available bandwidth well.

A 150 MiB file goes up through the Files section's Upload Files button and its
file chooser, and must land in the session byte for byte; another comes down
through the Download Files listing's own link. Each is timed and set against
the link's capacity, measured first with a plain TCP stream each way between
the same two hosts (when the target's address can be reached directly, as a
pod's can from inside its cluster).
"""
import hashlib
import os
import socket
import subprocess
import time
from typing import Any, Optional

from image_lib import H, reveal
import test_file_transfer as FT

ITEM = 5
TITLE = "file upload and download over 100 MB"
SIZE = 150 * 1024 * 1024
PORT = 8791
# A transfer has to reach this share of the measured capacity, or 100 Mbit/s
# where the link is faster than a user's would be (a pod network runs at
# several Gbit/s, past what one TLS stream through the browser carries).
FLOOR = 0.4
ENOUGH = 12.5
TMP = "/tmp/selkies-imagetest"
BW = '''
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", int(sys.argv[1]))); s.listen(2); s.settimeout(120)
for _ in range(2):
    c, _ = s.accept(); mode = c.recv(1); n = int(sys.argv[2])
    if mode == b"d":
        block = bytes(1 << 20)
        while n > 0: c.sendall(block[:min(n, len(block))]); n -= len(block)
    else:
        got = 0
        while got < n:
            b = c.recv(1 << 20)
            if not b: break
            got += len(b)
        c.sendall(str(got).encode())
    c.close()
'''


def capacity(cell: Any, n: int = 100 * 1024 * 1024) -> Optional[dict]:
    """MB/s down and up over one TCP stream to the session's host, or None where it cannot be reached."""
    host = cell.target.url.split("//", 1)[1].rsplit(":", 1)[0].strip("[]")
    if host in ("127.0.0.1", "localhost"):
        return None
    cell.target.put(f"{TMP}/bw.py", BW.encode())
    cell.target.sh(f"(setsid timeout 150 python3 {TMP}/bw.py {PORT} {n} > /dev/null 2>&1 &)")
    time.sleep(1.5)
    out = {}
    try:
        with socket.create_connection((host, PORT), timeout=10) as c:
            c.sendall(b"d")
            t0, got = time.time(), 0
            while got < n:
                b = c.recv(1 << 20)
                if not b:
                    break
                got += len(b)
            out["down"] = round(got / (time.time() - t0) / 1e6, 1)
        with socket.create_connection((host, PORT), timeout=10) as c:
            c.sendall(b"u")
            block, sent, t0 = os.urandom(1 << 20), 0, time.time()
            while sent < n:
                c.sendall(block)
                sent += len(block)
            c.shutdown(socket.SHUT_WR)
            c.recv(64)
            out["up"] = round(n / (time.time() - t0) / 1e6, 1)
    except OSError as e:
        return {"error": str(e)}
    return out


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    page.evaluate(FT.UPLOAD_JS)
    link = capacity(cell)
    name = f"it-{cell.engine}-{cell.transport}-{int(time.time())}.bin"
    local = os.path.join(H.WORKDIR, name)
    data = os.urandom(SIZE)
    with open(local, "wb") as f:
        f.write(data)
    digest = hashlib.sha256(data).hexdigest()
    del data
    R.check("the Files section opens", reveal(page, 'button:has-text("Upload Files")'))
    t0 = time.time()
    try:
        with page.expect_file_chooser(timeout=8000) as chooser:
            page.locator('button:has-text("Upload Files")').first.click()
        chooser.value.set_files(local)
        chosen = True
    except Exception as e:
        chosen = False
        print(f"      (file chooser: {e!r})")
    ups = FT.wait_upload_end(page, name, timeout=600)
    up_secs = time.time() - t0
    where = cell.target.out(f"find ~ /config -maxdepth 4 -name {name} 2>/dev/null | head -1")
    remote = cell.target.out(f"sha256sum {where} | cut -c1-64") if where else ""
    up_rate = round(SIZE / up_secs / 1e6, 1)
    R.check(f"a {SIZE >> 20} MiB upload through Upload Files lands byte for byte", chosen and remote == digest,
            f"{where or 'not found'} in {up_secs:.1f} s ({up_rate} MB/s); {ups[-1:] if ups else 'no progress'}")
    os.remove(local)

    # Download: a file of the session's through the file manager the Download Files button opens.
    down = f"down-{name}"
    folder = os.path.dirname(where) if where else "~/Desktop"
    cell.target.sh(f"head -c {SIZE} /dev/urandom > {folder}/{down}")
    want = cell.target.out(f"sha256sum {folder}/{down} | cut -c1-64")
    got_sha, down_secs, why = "", None, ""
    try:
        reveal(page, 'button:has-text("Download Files")')
        page.locator('button:has-text("Download Files")').first.click(timeout=5000)
        dialog = page.locator('[role="dialog"]:has-text("Files")').first
        dialog.wait_for(timeout=45000)
        R.check("Download Files opens the session's file manager", dialog.count() > 0)
        t0 = time.time()
        found, got_sha = FT.wish_row_download(page, dialog, down, timeout=600)
        down_secs = time.time() - t0
        if not found:
            why = "the manager never listed the file"
    except Exception as e:
        why = f"{type(e).__name__}: {e}"[:160]
    down_rate = round(SIZE / down_secs / 1e6, 1) if down_secs else None
    R.check(f"a {SIZE >> 20} MiB download through the manager's row arrives byte for byte",
            got_sha == want and bool(want), f"in {down_secs and round(down_secs, 1)} s ({down_rate} MB/s) {why}")
    # The same file fetched by curl: what the server's route gives without a browser.
    t0 = time.time()
    fetched = subprocess.run(["curl", "-sk", "-o", "/dev/null", "-w", "%{size_download}",
                                f"{cell.target.url}/api/files/{down}"], capture_output=True, text=True, timeout=600)
    curl_rate = round(int(fetched.stdout or 0) / (time.time() - t0) / 1e6, 1)
    cell.target.sh(f"rm -f {folder}/{down} {where}")
    if link and "down" in link:
        for way, rate, cap in (("upload", up_rate, link["up"]), ("download", down_rate or 0, link["down"])):
            R.check(f"the {way} is fast enough for the link ({rate} of {cap} MB/s)",
                    rate >= min(FLOOR * cap, ENOUGH),
                    f"{rate / cap:.0%} of a plain TCP stream; curl on the route {curl_rate} MB/s down")
    else:
        R.skip("throughput against the link", f"the session's host cannot be reached directly ({link})")
