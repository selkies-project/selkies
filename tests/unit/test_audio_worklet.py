#!/usr/bin/env python3
"""The playback worklet's adaptive jitter depth, driven as plain logic.

AudioFrameProcessor is extracted from the websockets core and run under node
with a stub AudioWorkletProcessor, so priming, an underrun re-priming one
packet deeper once a late packet ends it, the server's quiet mark playing a
sound shorter than the priming depth at once and making the silence after it
no underrun (also when the mark trails the queue running dry), the
standing-depth trim, the clean-stretch decay, the drop-oldest ceiling, and the
depths sized from the frame duration are pinned without a browser.
"""
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
    print(f"{'PASS' if ok else 'FAIL'}  [audio-worklet] {label}  {detail}", flush=True)


def extract_worklet() -> str:
    src = open(CORE).read()
    m = re.search(
        r"class AudioFrameProcessor extends AudioWorkletProcessor \{.*?\n"
        r"\s*registerProcessor\('audio-frame-processor', AudioFrameProcessor\);",
        src, re.S)
    assert m, "AudioFrameProcessor not found in the core"
    return m.group(0)


DRIVER = """
global.AudioWorkletProcessor = class {
  constructor() { this.port = { onmessage: null, postMessage() {} }; }
};
let cls = null;
global.registerProcessor = (name, c) => { cls = c; };
global.sampleRate = 48000;
__WORKLET__

const p = new cls({ processorOptions: { channels: 1 } });
const PKT = 480;
const feed = (v) => p.enqueue(new Float32Array(PKT).fill(v).buffer);
const run = () => {
  const buf = new Float32Array(128);
  p.process([], [[buf]], {});
  return buf;
};
const silent = (b) => b.every((x) => x === 0);
const out = {};

out.freshSilent = silent(run());
feed(0.5);
out.onePacketHolds = silent(run());
feed(0.5);
out.minTargetPlays = !silent(run());

// Drain to a mid-stream underrun: the late packet that ends it deepens the
// target by one, and output re-primes at the new depth.
for (let i = 0; i < 30; i++) run();
out.underrunSilent = silent(run());
out.judgedOnArrival = (p.target === 2 && p.underrunPending);
feed(0.25);
out.deepened = (p.target === 3 && p.underrunSamples > 0);
feed(0.25);
out.reprimeHolds = silent(run());
feed(0.25);
out.reprimePlays = !silent(run());

// The server's quiet mark: a one-packet sound, short of the priming depth,
// plays as soon as the mark says nothing more is coming, and running dry
// after it neither deepens the target nor counts as concealment; nor does
// running dry just before a mark that trails the last packet.
p.port.onmessage({ data: { quiet: true } });
for (let i = 0; i < 40; i++) run();
const quietTarget = p.target, quietUnder = p.underrunSamples;
feed(0.3);
out.shortSoundHeld = silent(run());
p.port.onmessage({ data: { quiet: true } });
out.quietPlaysShortSound = !silent(run());
for (let i = 0; i < 40; i++) run();
out.quietKeepsDepth = (p.priming && p.target === quietTarget && p.underrunSamples === quietUnder);
for (let i = 0; i < quietTarget; i++) feed(0.3);
out.nextSoundPrimesAgain = p.senderQuiet === false && !silent(run());
for (let i = 0; i < 40; i++) run();
out.dryBeforeMarkPending = p.underrunPending === true;
p.port.onmessage({ data: { quiet: true } });
out.markAfterDryKeepsDepth = (!p.underrunPending && p.target === quietTarget && p.underrunSamples === quietUnder);

// Standing depth above target trims away and the deepened target decays
// over proven slack, under arrival exactly rate-matched to consumption
// (4 packets per 15 calls = 128 samples per call) -- with no audible
// probe: reclaiming latency must not itself conceal.
for (let v = 1; v <= 8; v++) feed(v / 10);
const before = p.audioBufferQueue.length;
const underBefore = p.underrunSamples;
for (let i = 0; i < 4600; i++) {
  run();
  if (i % 15 === 3 || i % 15 === 7 || i % 15 === 11 || i % 15 === 14) feed(0.9);
}
out.trimmed = (p.audioBufferQueue.length <= p.target + 1);
out.trimStats = `before=${before} after=${p.audioBufferQueue.length} ` +
  `target=${p.target} underruns=${p.underrunSamples - underBefore}`;
out.decayedOverProvenSlack = (p.target <= 3);
out.reclaimWasSilentFree = (p.underrunSamples === underBefore);

// Drop-oldest ceiling is intact.
for (let v = 0; v < 12; v++) feed(v);
out.ringCapped = (p.audioBufferQueue.length === p.MAX_BUFFER_PACKETS);
out.droppedCounted = (p.droppedOldest > 0);

// The direct port line feeds the same queue and carries the quiet mark.
const fakePort = { onmessage: null };
p.port.onmessage({ data: { type: 'pcmPort', port: fakePort } });
fakePort.onmessage({ data: { audioData: new Float32Array(PKT).fill(0.7).buffer } });
out.portFeeds = Math.abs(
  p.audioBufferQueue[p.audioBufferQueue.length - 1][0] - 0.7) < 1e-6;
fakePort.onmessage({ data: { quiet: true } });
out.portMarksQuiet = p.senderQuiet === true;

// The depths follow the frame duration: the default 10 ms frame keeps two to
// six packets under a ceiling of eight, 2.5 ms frames reach the same 50 ms of
// slack in twenty-one, and 60 ms frames stop at two.
const sized = (frames) => {
  const q = new cls({ processorOptions: { channels: 1 } });
  q.enqueue(new Float32Array(frames).buffer);
  return [q.TARGET_MIN, q.TARGET_MAX, q.STEP, q.MAX_BUFFER_PACKETS].join(',');
};
out.sized10ms = sized(480) === '2,6,1,8';
out.sized2_5ms = sized(120) === '2,21,2,29';
out.sized60ms = sized(2880) === '2,2,1,3';

console.log(JSON.stringify(out));
"""


def run() -> int:
    worklet = extract_worklet()
    proof = subprocess.run(
        ["node", "-e", DRIVER.replace("__WORKLET__", worklet)], capture_output=True, text=True,
        timeout=60)
    if proof.returncode != 0:
        check("driver ran", False, proof.stderr.strip()[:300])
        return 1
    import json
    results = json.loads(proof.stdout.strip().splitlines()[-1])
    detail = results.pop("trimStats", "")
    for name, ok in results.items():
        check(name, bool(ok), detail if name == "trimmed" else "")
    print(f"[audio-worklet] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
