#!/usr/bin/env python3
"""The audio decoder worker's hand-off to the playback worklet, driven as plain logic.

The worker source is extracted from the websockets core and run under node with a
stub WebCodecs decoder whose output lags each decode, so it is pinned that a
sound's PCM, the server's bare quiet mark behind it (arriving while that decode is
still in flight), and the silent frame that ends the sound reach the worklet's
port in that order, the last one marked quiet.
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
    print(f"{'PASS' if ok else 'FAIL'}  [audio-decoder-worker] {label}  {detail}", flush=True)


def extract_worker() -> str:
    """The worker source, with the quiet bit made literal."""
    src = open(CORE).read()
    m = re.search(r"const audioDecoderWorkerCode = `(.*?)\n`;", src, re.S)
    assert m, "audioDecoderWorkerCode not found in the core"
    quiet = re.search(r"const AUDIO_QUIET = (0x[0-9a-fA-F]+);", src)
    assert quiet, "AUDIO_QUIET not found in the core"
    return m.group(1).replace("${AUDIO_QUIET}", str(int(quiet.group(1), 16)))


DRIVER = """
const toWorklet = [];
global.self = { postMessage() {}, onmessage: null, close() {} };
global.EncodedAudioChunk = class { constructor(o) { this.data = o.data; } };
global.AudioDecoder = class {
  constructor({ output }) { this.output = output; this.state = 'unconfigured'; }
  static async isConfigSupported() { return { supported: true }; }
  configure() { this.state = 'configured'; }
  decode(chunk) {
    const bytes = new Uint8Array(chunk.data).length;
    setTimeout(() => this.output({
      allocationSize: () => 4 * 960,
      copyTo: (dst) => { dst.fill(bytes); },
      close() {},
    }), 5);
  }
  close() { this.state = 'closed'; }
};
__WORKER__
(async () => {
  await self.onmessage({ data: { type: 'init', data: { initialPipelineStatus: true, channels: 2 } } });
  self.onmessage({ data: { type: 'pcmPort', port: { postMessage: (m) => toWorklet.push(m) } } });
  const audioIn = { onmessage: null };
  self.onmessage({ data: { type: 'audioIn', port: audioIn } });
  const send = (bytes) => audioIn.onmessage({ data: { buffer: new Uint8Array(bytes).buffer } });
  send([0x01, 0x00, 0xfc, 1, 2, 3]);
  send([0x01, 0x80]);
  send([0x01, 0x80, 0xfc, 9]);
  await new Promise((resolve) => setTimeout(resolve, 50));
  console.log(JSON.stringify(toWorklet.map((m) => m.audioData
    ? (m.quiet ? 'ending:' : 'audio:') + new Float32Array(m.audioData)[0]
    : (m.quiet ? 'mark' : 'other'))));
  process.exit(0);
})();
"""


def run() -> int:
    proof = subprocess.run(["node", "-e", DRIVER.replace("__WORKER__", extract_worker())],
                           capture_output=True, text=True, timeout=60)
    if proof.returncode != 0:
        check("driver ran", False, proof.stderr.strip()[:300])
        return 1
    seq = json.loads(proof.stdout.strip().splitlines()[-1])
    check("the sound, the mark that waited for its decode, then the frame that ends it, marked quiet",
          seq == ["audio:4", "mark", "ending:2"], seq)
    print(f"[audio-decoder-worker] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
