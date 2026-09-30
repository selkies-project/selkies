#!/usr/bin/env python3
"""The microphone worklet at a context rate other than 24 kHz, driven as plain logic.

An engine that cannot resample a stream into a context of another rate leaves the
capture context at its own rate, and the worklet brings it to the 24 kHz the
encoders take. The worklet source is extracted from the websockets core and run
under node at 48 and 44.1 kHz: a tone under 12 kHz has to come out at 24 kHz at
its own pitch and level, one above it has to be filtered out rather than fold
back into the band, and at 24 kHz the samples pass through untouched.
"""
import json
import math
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
    print(f"{'PASS' if ok else 'FAIL'}  [mic-worklet-rate] {label}  {detail}", flush=True)


def extract() -> str:
    src = open(CORE).read()
    m = re.search(r"const micWorkletProcessorCode = `(.*?)\n`;", src, re.S)
    assert m, "micWorkletProcessorCode not found in the core"
    assert "${" not in m.group(1), "the worklet source splices in a value this test does not supply"
    return m.group(1)


# One second of each tone through the worklet in 128-frame render quanta; what
# it posts is read back as 24 kHz and measured with a Goertzel filter at the
# tone's frequency and at the frequency it would alias to.
DRIVER = """
const [rate, hz, amp] = process.argv.slice(1).map(Number);
global.sampleRate = rate;
let Processor = null;
global.registerProcessor = (name, cls) => { Processor = cls; };
global.AudioWorkletProcessor = class { constructor() { this.port = { postMessage: (b) => posted.push(b), onmessage: null }; } };
const posted = [];
__WORKLET__
const p = new Processor();
let n = 0;
for (let q = 0; q < Math.ceil(rate / 128); q++) {
  const block = new Float32Array(128);
  for (let i = 0; i < 128; i++, n++) block[i] = amp * Math.sin(2 * Math.PI * hz * n / rate);
  p.process([[block]], [], {});
}
const out = [];
for (const b of posted) out.push(...new Int16Array(b));
const tone = (f, xs) => {
  const c = 2 * Math.cos(2 * Math.PI * f / 24000);
  let s1 = 0, s2 = 0;
  for (const x of xs) { const s0 = x + c * s1 - s2; s2 = s1; s1 = s0; }
  return Math.sqrt(Math.max(s1 * s1 + s2 * s2 - c * s1 * s2, 0)) * 2 / xs.length;
};
const tail = out.slice(2400);
const alias = hz > 12000 ? 24000 - (hz % 24000) : hz;
const rms = Math.sqrt(tail.reduce((a, x) => a + x * x, 0) / tail.length);
process.stdout.write(JSON.stringify({ fed: n, samples: out.length, amplitude: tone(hz <= 12000 ? hz : alias, tail),
  alias: tone(alias, tail), rms, first: out.slice(0, 4) }) + '\\n');
"""


def run_case(worklet: str, rate: int, hz: int, amp: float = 0.5) -> dict:
    proof = subprocess.run(["node", "-e", DRIVER.replace("__WORKLET__", worklet), str(rate), str(hz), str(amp)],
                           capture_output=True, text=True, timeout=60)
    if proof.returncode != 0:
        check(f"driver ran at {rate} Hz", False, proof.stderr.strip()[:300])
        return {}
    return json.loads(proof.stdout.strip().splitlines()[-1])


def main() -> int:
    worklet = extract()
    full = 0.5 * 32767
    for rate in (48000, 44100):
        got = run_case(worklet, rate, 1000)
        if got:
            want = got["fed"] * 24000 / rate
            check(f"{rate} Hz in, a second's samples come out as a second at 24 kHz", abs(got["samples"] - want) <= 2,
                  f"{got['samples']} of {want:.0f}")
            check(f"{rate} Hz in, a 1 kHz tone keeps its pitch and level at 24 kHz",
                  abs(got["amplitude"] - full) / full < 0.05, round(got["amplitude"]))
        got = run_case(worklet, rate, 15000)
        if got:
            check(f"{rate} Hz in, a 15 kHz tone is filtered, not folded to {24000 - 15000} Hz",
                  got["alias"] < 0.02 * full, round(got["alias"]))
    got = run_case(worklet, 24000, 1000)
    if got:
        want = [int(0.5 * 32767 * math.sin(2 * math.pi * 1000 * i / 24000)) for i in range(4)]
        check("at 24 kHz the samples pass straight through", got["samples"] == 24064 and
              all(abs(a - b) <= 1 for a, b in zip(got["first"], want)), (got["samples"], got["first"], want))
    print(f"[mic-worklet-rate] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
