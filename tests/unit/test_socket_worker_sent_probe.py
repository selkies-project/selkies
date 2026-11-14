#!/usr/bin/env python3
"""The socket worker's `_sent,<n>` probe, driven as plain logic.

Firefox takes each pong's payload off `bufferedAmount` and then refuses to take
off a message larger than what is left, so the message stays counted after it
went out. The worker source is extracted from the websockets core and run under
node with a stub WebSocket, a virtual clock and manual timers, so it is pinned
that a count staying up is probed, that the server's echo discounts what the
count still holds of the bytes sent before the probe and nothing sent after it,
that a stale echo changes nothing, and that a browser whose count is honest
reports exactly what it did before.
"""
import json
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CORE = os.path.join(REPO, "web/selkies-web-core/selkies-ws-core.ts")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [socket-worker-sent-probe] {label}  {detail}", flush=True)


def extract_worker() -> str:
    """The worker source, with its interpolated constants taken from the core."""
    src = open(CORE).read()
    m = re.search(r"const SOCKET_WORKER_SRC = `(.*?)\n`;", src, re.S)
    assert m, "SOCKET_WORKER_SRC not found in the core"
    consts = dict(re.findall(r"^const (\w+) = (\d+);", src, re.M))
    return re.sub(r"\$\{(\w+)\}", lambda c: consts[c.group(1)], m.group(1))


DRIVER = """
let clock = 1000;
global.performance = { now: () => clock, timeOrigin: 0 };
const timers = new Map();
let nextTimer = 1;
global.setInterval = (fn, ms) => { timers.set(nextTimer, { fn, ms, due: clock + ms }); return nextTimer++; };
global.clearInterval = (id) => { timers.delete(id); };
function advance(ms) {
  const end = clock + ms;
  for (;;) {
    let next = null;
    for (const [id, t] of timers) if (t.due <= end && (!next || t.due < next[1].due)) next = [id, t];
    if (!next) break;
    clock = next[1].due;
    next[1].due += next[1].ms;
    next[1].fn();
  }
  clock = end;
}
const toPage = [];
global.self = { postMessage: (m) => toPage.push(m), onmessage: null };
class FakeWebSocket {
  constructor() { FakeWebSocket.last = this; this.readyState = 1; this.bufferedAmount = 0; this.sent = []; }
  send(d) {
    this.sent.push(d);
    this.bufferedAmount += typeof d === 'string' ? Buffer.byteLength(d) : d.byteLength;
  }
  close() {}
}
global.WebSocket = FakeWebSocket;
__WORKER__
const lastReport = () => toPage.filter((m) => m.type === 'buffered').at(-1).amount;
const probes = (s) => s.sent.filter((d) => typeof d === 'string' && d.startsWith('_sent,'));
const r = {};

// Firefox: a 200 kB message goes out, a pong takes 8 bytes off, the message's
// own acknowledgement is refused, and 199992 bytes stay counted.
self.onmessage({ data: { type: 'open', url: 'ws://stub', primary: true } });
let s = FakeWebSocket.last;
self.onmessage({ data: { type: 'send', data: new ArrayBuffer(200000) } });
s.bufferedAmount = 199992;
advance(150);
r.noProbeEarly = probes(s).length;
advance(200);
r.probes = probes(s);
const probeBytes = Buffer.byteLength(r.probes[0] || '');
// The probe went out and was acknowledged (its own size came off), then a
// 50 kB message is queued behind it before the echo arrives.
s.bufferedAmount -= probeBytes;
self.onmessage({ data: { type: 'send', data: new ArrayBuffer(50000) } });
const pageBefore = toPage.filter((m) => m.type === 'message').length;
s.onmessage({ data: '_sent,999' });
r.staleEcho = lastReport();
s.onmessage({ data: r.probes[0] });
r.afterEcho = lastReport();
r.echoForwarded = toPage.filter((m) => m.type === 'message').length - pageBefore;
s.bufferedAmount -= 50000;
advance(40);
r.drained = lastReport();
r.timersLeft = timers.size;
// Later pongs take more off what stays counted: never a negative report.
s.bufferedAmount -= 16;
self.onmessage({ data: { type: 'send', data: new ArrayBuffer(10) } });
r.afterPongs = lastReport();

// An honest count: a slow message really is in flight while the probe goes
// out, and is gone (with the probe) by the echo.
self.onmessage({ data: { type: 'open', url: 'ws://stub', primary: true } });
s = FakeWebSocket.last;
self.onmessage({ data: { type: 'send', data: new ArrayBuffer(200000) } });
advance(400);
r.honestProbes = probes(s);
r.honestStalled = lastReport();
s.bufferedAmount = 0;
s.onmessage({ data: r.honestProbes[0] });
r.honestAfterEcho = lastReport();
self.onmessage({ data: { type: 'send', data: new ArrayBuffer(30000) } });
r.honestNext = lastReport();
advance(3000);
r.honestReprobes = probes(s).length;
console.log(JSON.stringify(r));
process.exit(0);
"""


def run() -> int:
    proof = subprocess.run(["node", "-e", DRIVER.replace("__WORKER__", extract_worker())],
                           capture_output=True, text=True, timeout=60)
    if proof.returncode != 0:
        check("driver ran", False, proof.stderr.strip()[:600])
        return 1
    r = json.loads(proof.stdout.strip().splitlines()[-1])
    check("a count staying up is not probed before 250 ms", r["noProbeEarly"] == 0, r["noProbeEarly"])
    check("then one probe goes out", r["probes"] == ["_sent,1"], r["probes"])
    check("an echo of another probe changes nothing", r["staleEcho"] == 199992 + 50000, r["staleEcho"])
    check("the echo discounts what stayed counted, not what was sent after the probe",
          r["afterEcho"] == 50000, r["afterEcho"])
    check("the echo is not handed to the page", r["echoForwarded"] == 0, r["echoForwarded"])
    check("the socket reads empty once the later message goes", r["drained"] == 0, r["drained"])
    check("and the drain timer stops", r["timersLeft"] == 0, r["timersLeft"])
    check("pongs taking more off never turn the report negative", r["afterPongs"] == 10, r["afterPongs"])
    check("an honest count in a stall is probed too", r["honestProbes"] == ["_sent,2"], r["honestProbes"])
    check("and reported in full meanwhile", r["honestStalled"] == 200000 + len("_sent,2"), r["honestStalled"])
    check("its echo discounts nothing", r["honestAfterEcho"] == 0, r["honestAfterEcho"])
    check("the next message is counted in full", r["honestNext"] == 30000, r["honestNext"])
    check("an unanswered probe is repeated no sooner than every 2 s",
          r["honestReprobes"] <= 3, r["honestReprobes"])
    print(f"[socket-worker-sent-probe] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
