#!/usr/bin/env python3
"""The websockets decoder worker's split of a surround packet into its streams.

The worker decodes surround one elementary stream at a time, so it has to cut
a multistream Opus packet at the stream boundaries itself: every stream but
the last is self-delimited (RFC 6716 Appendix B), carrying one more length
field in front of its last frame's data. `frameLength` and `splitStreams` are
extracted from the worker source in the websockets core and run under node
against packets built here from known elementary packets, covering every
frame-count code (one frame, two equal, two unequal, and an arbitrary count
with constant or variable lengths), padding, and one- and two-byte length
fields; each split must give back the elementary packets byte for byte, and a
truncated packet must give back nothing.
"""
import json
import os
import re
import shutil
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CORE = os.path.join(REPO, "addons/selkies-web-core/selkies-ws-core.js")

DRIVER = r"""
__SPLIT__
const len = (n) => n < 252 ? [n] : [252 + ((n - 252) & 3), (n - 252 - ((n - 252) & 3)) >> 2];
const bytes = (n, seed) => Array.from({ length: n }, (_, i) => (i * 31 + seed * 7) & 0xff);
// A plain packet and its self-delimited form, from its code and frame sizes.
function packet(code, frames, pad, seed) {
  const data = frames.flatMap((n, i) => bytes(n, seed + i));
  const last = frames[frames.length - 1];
  const toc = (seed << 3) & 0xf8 | code;
  let head = [toc], mid = [];
  if (code === 2) mid = len(frames[0]);
  if (code === 3) {
    const vbr = frames.some((n) => n !== frames[0]);
    head.push((vbr ? 0x80 : 0) | (pad ? 0x40 : 0) | frames.length);
    if (pad) head.push(pad);
    if (vbr) mid = frames.slice(0, -1).flatMap(len);
  }
  const padding = Array(pad).fill(0);
  const plain = [...head, ...mid, ...data, ...padding];
  const delimited = [...head, ...mid, ...len(code === 1 ? frames[0] : last), ...data, ...padding];
  return { plain, delimited };
}
const shapes = [
  [0, [120], 0], [0, [300], 0], [1, [80, 80], 0], [1, [260, 260], 0],
  [2, [40, 90], 0], [2, [300, 10], 0], [3, [60, 60, 60], 0], [3, [30, 270, 5, 90], 0],
  [3, [50, 50], 7], [3, [20, 280, 40], 3],
];
const out = {};
let seed = 1;
for (const streams of [4, 5]) {
  for (let s = 0; s < shapes.length; s++) {
    const parts = [];
    for (let k = 0; k < streams; k++) {
      const [code, frames, pad] = shapes[(s + k) % shapes.length];
      parts.push(packet(code, frames, pad, seed++));
    }
    const whole = [...parts.slice(0, -1).flatMap((p) => p.delimited), ...parts[parts.length - 1].plain];
    const got = splitStreams(new Uint8Array(whole).buffer, streams);
    out[`${streams} streams, shape ${s}`] = !!got && got.length === streams
      && got.every((g, k) => JSON.stringify(Array.from(new Uint8Array(g))) === JSON.stringify(parts[k].plain));
    const cut = splitStreams(new Uint8Array(whole.slice(0, parts[0].delimited.length - 1)).buffer, streams);
    out[`${streams} streams, shape ${s}, truncated`] = cut === null;
  }
}
console.log(JSON.stringify(out));
"""


def main() -> int:
    node = shutil.which("node")
    if not node:
        print("SKIP node not found, so the multistream split cannot run", flush=True)
        return 77
    src = open(CORE).read()
    m = re.search(r"  // An Opus frame length field.*?\n  function splitStreams\(buf, streams\) \{.*?\n  \}\n",
                  src, re.S)
    if not m:
        print("FAIL  [audio-multistream] splitStreams found in the worker source", flush=True)
        return 1
    r = subprocess.run([node, "-e", DRIVER.replace("__SPLIT__", m.group(0))],
                       capture_output=True, text=True, timeout=60)
    if r.returncode != 0:
        print(f"FAIL  [audio-multistream] driver ran  {r.stderr.strip()[:300]}", flush=True)
        return 1
    results = json.loads(r.stdout.strip().splitlines()[-1])
    failed = 0
    for name, ok in results.items():
        failed += not ok
        print(f"{'PASS' if ok else 'FAIL'}  [audio-multistream] {name}", flush=True)
    print(f"[audio-multistream] {len(results) - failed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
