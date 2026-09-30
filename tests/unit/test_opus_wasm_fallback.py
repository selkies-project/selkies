#!/usr/bin/env python3
"""The audio workers on an engine without WebCodecs audio, driven as plain logic.

Both worker sources are extracted from the websockets core and run under node with
no AudioDecoder, AudioEncoder, AudioData or EncodedAudioChunk, their `opusUrl`
pointing at a stand-in for libopus-wasm that records what it is asked. It is
pinned that the decode worker opens the codec for the stream's layout, plays each
packet's PCM in order with the server's quiet mark behind a decode still in
flight, puts a surround packet's channels where the mapping table says, and tries
once rather than per packet when the codec cannot be loaded; and that the
microphone worker asks for what the WebCodecs path asks for, gathers the
worklet's render quanta into 20 ms frames, and sends each as a 0x02 frame.
"""
import json
import os
import re
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CORE = os.path.join(REPO, "addons/selkies-web-core/selkies-ws-core.js")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [opus-wasm-fallback] {label}  {detail}", flush=True)


def extract(name: str) -> str:
    """A worker source from the core, with the constants it splices in made literal."""
    src = open(CORE).read()
    m = re.search(rf"const {name} = `(.*?)\n`;", src, re.S)
    assert m, f"{name} not found in the core"
    code = m.group(1)
    for const in ("AUDIO_QUIET", "MIC_BITRATE"):
        v = re.search(rf"const {const} = (0x[0-9a-fA-F]+|\d+);", src)
        assert v, f"{const} not found in the core"
        code = code.replace("${" + const + "}", str(int(v.group(1), 0)))
    assert "${" not in code, "an unhandled splice is left in " + name
    return code


# A stand-in for libopus-wasm: a decoder whose PCM is its packet's first byte
# plus a tenth per channel, and an encoder that keeps its frames.
STUB = """
export const Application = { Voip: 2048, Audio: 2049, RestrictedLowDelay: 2051 };
export const made = [];
export async function createDecoder(o) {
  const d = { o, decodeFloat(packet) {
    const out = new Float32Array(960 * o.channels);
    for (let f = 0; f < 960; f++) for (let c = 0; c < o.channels; c++) out[f * o.channels + c] = packet[0] + c / 10;
    return out;
  }, free() { d.freed = true; } };
  made.push(d);
  return d;
}
export async function createEncoder(o) {
  const e = { o, frameSize: o.frameSize, frames: [], encode(frame) {
    e.frames.push(Array.from(frame));
    return new Uint8Array([0xfc, e.frames.length]);
  }, free() { e.freed = true; } };
  made.push(e);
  return e;
}
"""

DECODE_DRIVER = """
const posted = [], toWorklet = [];
global.self = { postMessage: (m) => posted.push(m), onmessage: null, close() {} };
__WORKER__
const tick = () => new Promise((resolve) => setTimeout(resolve, 20));
(async () => {
  const stub = await import(process.env.STUB_URL);
  const out = {};
  await self.onmessage({ data: { type: 'init', data: { initialPipelineStatus: true, channels: 2, opusUrl: process.env.STUB_URL } } });
  out.stereoInit = posted.slice();
  out.stereoDecoders = stub.made.map((d) => d.o);
  self.onmessage({ data: { type: 'pcmPort', port: { postMessage: (m) => toWorklet.push(m) } } });
  const audioIn = { onmessage: null };
  self.onmessage({ data: { type: 'audioIn', port: audioIn } });
  const send = (bytes) => audioIn.onmessage({ data: { buffer: new Uint8Array(bytes).buffer } });
  send([0x01, 0x00, 0x10, 1, 2]);
  send([0x01, 0x80]);
  send([0x01, 0x80, 0x20, 9]);
  await tick();
  out.stereo = toWorklet.map((m) => m.audioData
    ? [m.quiet ? 'ending' : 'audio', Array.from(new Float32Array(m.audioData).slice(0, 2)), new Float32Array(m.audioData).length]
    : [m.quiet ? 'mark' : 'other']);

  // 5.1: streams 4, coupled 2, mapping 0 4 1 2 3 5, each elementary packet a
  // one-frame packet whose TOC byte names its stream, all but the last
  // self-delimited.
  toWorklet.length = 0;
  stub.made.length = 0;
  const head = new Uint8Array(27);
  head.set([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 1, 6]);
  head[18] = 1; head[19] = 4; head[20] = 2;
  head.set([0, 4, 1, 2, 3, 5], 21);
  await self.onmessage({ data: { type: 'init', data: { initialPipelineStatus: true, channels: 6, description: head.buffer } } });
  out.surroundDecoders = stub.made.map((d) => d.o);
  send([0x01, 0x00, 0x10, 1, 7, 0x20, 1, 7, 0x30, 1, 7, 0x40, 7]);
  await tick();
  const pcm = toWorklet.length ? new Float32Array(toWorklet[0].audioData) : new Float32Array(0);
  out.surround = { length: pcm.length, first: Array.from(pcm.slice(0, 6)), last: Array.from(pcm.slice(-6)) };
  process.stdout.write(JSON.stringify(out) + '\\n');
})().catch((e) => { console.error(e); process.exit(1); });
"""

UNAVAILABLE_DRIVER = """
const posted = [];
global.self = { postMessage: (m) => posted.push(m), onmessage: null, close() {} };
__WORKER__
(async () => {
  await self.onmessage({ data: { type: 'init', data: { initialPipelineStatus: true, channels: 2, opusUrl: process.env.MISSING_URL } } });
  for (let i = 0; i < 50; i++) await self.onmessage({ data: { type: 'decode', data: { opusBuffer: new Uint8Array([0x10, 1]).buffer } } });
  const failedBefore = posted.filter((m) => m.type === 'decoderInitFailed').length;
  await self.onmessage({ data: { type: 'reinitialize' } });
  process.stdout.write(JSON.stringify({ failedBefore, failedAfter: posted.filter((m) => m.type === 'decoderInitFailed').length,
    types: [...new Set(posted.map((m) => m.type))] }) + '\\n');
})().catch((e) => { console.error(e); process.exit(1); });
"""

ENCODE_DRIVER = """
const posted = [], wire = [];
global.self = { postMessage: (m) => posted.push(m), onmessage: null, close() {} };
__WORKER__
(async () => {
  const stub = await import(process.env.STUB_URL);
  await self.onmessage({ data: { type: 'init', opusUrl: process.env.STUB_URL } });
  self.onmessage({ data: { type: 'wirePort', port: { postMessage: (buf) => wire.push(Array.from(new Uint8Array(buf))) } } });
  // Fifteen render quanta of 128 samples: four 20 ms frames at 24 kHz.
  let n = 0;
  for (let q = 0; q < 15; q++) {
    const s = new Int16Array(128);
    for (let i = 0; i < 128; i++) s[i] = n++;
    self.onmessage({ data: { type: 'pcm', buffer: s.buffer } });
  }
  const enc = stub.made[0];
  const frames = enc.frames;
  const contiguous = frames.flat().every((v, i) => v === i);
  self.onmessage({ data: { type: 'stop' } });
  self.onmessage({ data: { type: 'pcm', buffer: new Int16Array(480).buffer } });
  process.stdout.write(JSON.stringify({ posted, options: enc.o, frameSizes: frames.map((f) => f.length), contiguous,
    wire, freed: !!enc.freed }) + '\\n');
})().catch((e) => { console.error(e); process.exit(1); });
"""


def node(driver: str, worker: str, env: dict) -> dict:
    proof = subprocess.run(["node", "-e", driver.replace("__WORKER__", worker)], capture_output=True, text=True,
                           timeout=60, env={**os.environ, **env})
    if proof.returncode != 0:
        check("driver ran", False, proof.stderr.strip()[:400])
        return {}
    return json.loads(proof.stdout.strip().splitlines()[-1])


def near(got, want) -> bool:
    return len(got) == len(want) and all(abs(g - w) < 1e-4 for g, w in zip(got, want))


def run() -> int:
    tmp = tempfile.mkdtemp(prefix="selkies-opus-wasm-")
    stub = os.path.join(tmp, "stub.mjs")
    with open(stub, "w") as f:
        f.write(STUB)
    env = {"STUB_URL": "file://" + stub, "MISSING_URL": "file://" + os.path.join(tmp, "missing.mjs")}
    decoder = extract("audioDecoderWorkerCode")

    got = node(DECODE_DRIVER, decoder, env)
    if got:
        check("the codec is loaded and said to be WASM",
              [m for m in got["stereoInit"] if m.get("type") == "decoderInitialized"] == [{"type": "decoderInitialized", "wasm": True}],
              got["stereoInit"])
        check("one stereo decoder at 48 kHz", got["stereoDecoders"] == [{"sampleRate": 48000, "channels": 2}],
              got["stereoDecoders"])
        want = [["audio", [16, 16.1], 1920], ["mark"], ["ending", [32, 32.1], 1920]]
        seq = got["stereo"]
        check("the sound, the mark that waited for its decode, then the frame that ends it, marked quiet",
              len(seq) == 3 and [s[0] for s in seq] == [w[0] for w in want]
              and all(len(s) == 1 or (near(s[1], w[1]) and s[2] == w[2]) for s, w in zip(seq, want)), seq)
        check("5.1 opens a decoder per elementary stream, the coupled ones stereo",
              got["surroundDecoders"] == [{"sampleRate": 48000, "channels": c} for c in (2, 2, 1, 1)],
              got["surroundDecoders"])
        s = got["surround"]
        want6 = [16, 48, 16.1, 32, 32.1, 64]
        check("each surround channel comes from the stream and channel its mapping names",
              s["length"] == 960 * 6 and near(s["first"], want6) and near(s["last"], want6), s)

    got = node(UNAVAILABLE_DRIVER, decoder, env)
    if got:
        check("a codec that cannot be loaded fails once, not once per packet", got["failedBefore"] == 1, got)
        check("and is tried again when the page asks", got["failedAfter"] == 2, got)
        check("with nothing claiming to be initialized", "decoderInitialized" not in got["types"], got["types"])

    got = node(ENCODE_DRIVER, extract("micEncodeWorkerCode"), env)
    if got:
        opts = got["options"]
        check("the encoder is asked for what WebCodecs is: 24 kHz mono, restricted low delay, the mic bitrate, 20 ms",
              opts.get("sampleRate") == 24000 and opts.get("channels") == 1 and opts.get("application") == 2051
              and opts.get("frameSize") == 480 and isinstance(opts.get("bitrate"), int) and opts["bitrate"] > 0, opts)
        check("the worker says it is ready on WASM", {"type": "ready", "wasm": True} in got["posted"], got["posted"])
        check("fifteen render quanta become four whole 20 ms frames, in order",
              got["frameSizes"] == [480] * 4 and got["contiguous"], got["frameSizes"])
        check("each goes out as a 0x02 frame around its packet",
              got["wire"] == [[0x02, 0xFC, k] for k in (1, 2, 3, 4)], got["wire"])
        check("stopping frees the encoder and sends nothing after", got["freed"] and len(got["wire"]) == 4, got["freed"])
    print(f"[opus-wasm-fallback] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
