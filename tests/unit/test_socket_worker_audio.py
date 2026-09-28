#!/usr/bin/env python3
"""The socket worker's audio route, driven as plain logic.

The worker source is extracted from the websockets core and run under node
with a stub WebSocket and ports, so it is pinned that audio and the server's
quiet mark behind it both go down the decoder's port, in the order they
arrived, and neither reaches the page: a mark relayed through the page could
overtake the audio before it and leave the playback worklet waiting for more.
With a stub fetch as well, the audio stream is pinned: the worker announces a
nonce and fetches the stream it names beside the socket's own path, confirms
it on its hello (a record split across reads included), holds its records
until the socket's switch mark has followed the audio the socket carried
before them, says when it ends, and leaves audio on the socket when no hello
comes in time or the page did not ask for a stream.
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
    """The worker source, with its interpolated timer intervals made literal."""
    src = open(CORE).read()
    m = re.search(r"const SOCKET_WORKER_SRC = `(.*?)\n`;", src, re.S)
    assert m, "SOCKET_WORKER_SRC not found in the core"
    return re.sub(r"\$\{\w+\}", "50", m.group(1))


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


STREAM_DRIVER = """
const toPage = [], toDecoder = [];
global.self = { postMessage: (m) => toPage.push(m), onmessage: null };
class FakeWebSocket {
  constructor(url) { FakeWebSocket.last = this; this.url = url; this.readyState = 1; this.sent = []; }
  send(d) { this.sent.push(d); }
  close() {}
}
global.WebSocket = FakeWebSocket;
let feed = null, fetched = null, fetchedHeaders = null;
global.fetch = (url, opts) => {
  fetched = url;
  fetchedHeaders = opts.headers;
  const body = new ReadableStream({ start(c) {
    feed = { push: (b) => c.enqueue(new Uint8Array(b)), end: () => c.close() };
    opts.signal.addEventListener('abort', () => c.error(new Error('aborted')));
  } });
  return Promise.resolve({ ok: true, body });
};
const tick = (ms) => new Promise((r) => setTimeout(r, ms || 5));
const bytes = (m) => Array.from(new Uint8Array(m.buffer));
__WORKER__
(async () => {
  const out = {};
  self.onmessage({ data: { type: 'audioPort', port: { postMessage: (m) => toDecoder.push(m) } } });
  self.onmessage({ data: { type: 'open', url: 'ws://host/sub/api/websockets?token=t', primary: true,
    downlinks: MODE !== 'off', headers: { Authorization: 'Bearer t' } } });
  const socket = FakeWebSocket.last;
  socket.onopen();
  await tick();
  out.announce = socket.sent[0] || '';
  out.fetched = fetched;
  out.headers = fetchedHeaders;
  if (MODE === 'stream') {
    feed.push([0, 0, 0, 0, 0, 0, 0, 3, 0x01]);
    feed.push([0x00, 0xaa]);
    await tick();
    out.confirmed = socket.sent.includes('DOWNLINK,audio,ok');
    out.heldBeforeSwitch = toDecoder.length;
    socket.onmessage({ data: new Uint8Array([0x01, 0x00, 0x11]).buffer });
    socket.onmessage({ data: 'DOWNLINK,audio,switch' });
    feed.push([0, 0, 0, 3, 0x01, 0x00, 0xbb]);
    await tick();
    out.order = toDecoder.map(bytes);
    out.pageMessages = toPage.filter((m) => m && m.type === 'message').length;
    feed.end();
    await tick();
    out.last = socket.sent[socket.sent.length - 1];
  } else {
    await tick(120);
    socket.onmessage({ data: new Uint8Array([0x01, 0x00, 0x22]).buffer });
    out.sent = socket.sent;
    out.order = toDecoder.map(bytes);
  }
  console.log(JSON.stringify(out));
  process.exit(0);
})();
"""


def drive(driver: str, mode: str = "") -> dict:
    """Run a driver on the extracted worker under node; its JSON line, or {"error": ...}."""
    code = driver.replace("__WORKER__", extract_worker()).replace("MODE", repr(mode))
    proof = subprocess.run(["node", "-e", code], capture_output=True, text=True, timeout=60)
    if proof.returncode != 0:
        return {"error": proof.stderr.strip()[:300]}
    return json.loads(proof.stdout.strip().splitlines()[-1])


def run_stream() -> None:
    r = drive(STREAM_DRIVER, "stream")
    if "error" in r:
        check("stream driver ran", False, r["error"])
        return
    nonce = r["announce"].rsplit(",", 1)[1] if r["announce"].startswith("DOWNLINK,audio,") else ""
    check("the worker announces a 128-bit nonce on the socket",
          len(nonce) == 32 and all(c in "0123456789abcdef" for c in nonce), r["announce"])
    check("and fetches the stream it names beside the socket's own path, credentials kept",
          r["fetched"] == f"http://host/sub/api/downlink/audio?token=t&stream={nonce}", r["fetched"])
    check("with the credentials the page's own API calls carry", r["headers"] == {"Authorization": "Bearer t"},
          r["headers"])
    check("the hello, even with a record split across reads behind it, confirms the stream", r["confirmed"])
    check("stream records wait for the switch mark", r["heldBeforeSwitch"] == 0, r["heldBeforeSwitch"])
    check("the socket's audio before the mark, then the held record, then the live one",
          r["order"] == [[1, 0, 0x11], [1, 0, 0xaa], [1, 0, 0xbb]], r["order"])
    check("neither the records nor the mark reach the page", r["pageMessages"] == 0, r["pageMessages"])
    check("a stream that ends is reported, so audio returns to the socket", r["last"] == "DOWNLINK,audio,off", r["last"])

    r = drive(STREAM_DRIVER, "silent")
    if "error" in r:
        check("silent driver ran", False, r["error"])
        return
    check("no hello in time: the stream is dropped without a confirmation",
          len(r["sent"]) == 1 and r["sent"][0].startswith("DOWNLINK,audio,"), r["sent"])
    check("and audio stays on the socket", r["order"] == [[1, 0, 0x22]], r["order"])

    r = drive(STREAM_DRIVER, "off")
    if "error" in r:
        check("off driver ran", False, r["error"])
        return
    check("a page that asks for no stream opens none", r["sent"] == [] and r["fetched"] is None, r)


def run() -> int:
    proof = subprocess.run(["node", "-e", DRIVER.replace("__WORKER__", extract_worker())],
                           capture_output=True, text=True, timeout=60)
    if proof.returncode != 0:
        check("driver ran", False, proof.stderr.strip()[:300])
        return 1
    r = json.loads(proof.stdout.strip().splitlines()[-1])
    check("audio and the quiet mark go down the decoder's port in arrival order", r["got"] == r["sent"], r["got"])
    check("neither is handed to the page", r["pageBuffers"] == 0, r["pageBuffers"])
    run_stream()
    print(f"[socket-worker-audio] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
