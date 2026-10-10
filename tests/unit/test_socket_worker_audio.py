#!/usr/bin/env python3
"""The socket worker's audio route, driven as plain logic.

The worker source is extracted from the websockets core and run under node
with a stub WebSocket and ports, so it is pinned that audio and the server's
quiet mark behind it both go down the decoder's port, in the order they
arrived, and neither reaches the page: a mark relayed through the page could
overtake the audio before it and leave the playback worklet waiting for more.
"""
import json
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CORE = os.path.join(REPO, "addons/selkies-web-core/selkies-ws-core.js")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [socket-worker-audio] {label}  {detail}", flush=True)


def extract_worker() -> str:
    """Embed the real protocol helper and make test timer intervals literal."""
    src = open(CORE).read()
    m = re.search(r"const SOCKET_WORKER_SRC = `(.*?)\n`;", src, re.S)
    assert m, "SOCKET_WORKER_SRC not found in the core"
    helper_path = os.path.join(REPO, "addons/selkies-web-core/lib/lossless-static.js")
    with open(helper_path) as helper_file:
        helper = re.sub(r"^export ", "", helper_file.read(), flags=re.M)
    body = m.group(1).replace("${losslessStaticSource.replace(/^export /gm, '')}", helper)
    return re.sub(r"\$\{\w+\}", "50", body)


DRIVER = """
const toPage = [], toDecoder = [];
global.self = { postMessage: (m) => toPage.push(m), onmessage: null };
class FakeWebSocket { constructor() { FakeWebSocket.last = this; this.readyState = 1; } send() {} close() {} }
global.WebSocket = FakeWebSocket;
__WORKER__
self.onmessage({ data: { type: 'audioPort', port: { postMessage: (m) => toDecoder.push(m) } } });
self.onmessage({ data: { type: 'open', url: 'ws://stub', primary: true } });
const socket = FakeWebSocket.last;
const sent = [[0x01, 0x00, 0xfc, 0x01, 0x02], [0x01, 0x00, 0xfc, 0x03], [0x01, 0x80]];
for (const bytes of sent) socket.onmessage({ data: new Uint8Array(bytes).buffer });
const got = toDecoder.map((m) => Array.from(new Uint8Array(m.buffer)));
console.log(JSON.stringify({
  sent, got,
  pageBuffers: toPage.filter((m) => m && m.type === 'message').length,
}));
process.exit(0);
"""


def run() -> int:
    proof = subprocess.run(["node", "-e", DRIVER.replace("__WORKER__", extract_worker())],
                           capture_output=True, text=True, timeout=60)
    if proof.returncode != 0:
        check("driver ran", False, proof.stderr.strip()[:300])
        return 1
    r = json.loads(proof.stdout.strip().splitlines()[-1])
    check("audio and the quiet mark go down the decoder's port in arrival order", r["got"] == r["sent"], r["got"])
    check("neither is handed to the page", r["pageBuffers"] == 0, r["pageBuffers"])
    print(f"[socket-worker-audio] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
