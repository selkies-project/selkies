/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The audio workers on an engine without WebCodecs audio, driven as plain logic.
 *
 * Both worker sources the WebSockets core starts run with no AudioDecoder,
 * AudioEncoder, AudioData or EncodedAudioChunk, their `opusUrl` pointing at a
 * stand-in for libopus-wasm that records what it is asked. It is pinned that
 * the decode worker opens the codec for the stream's layout, plays each
 * packet's PCM in order with the server's quiet mark behind a decode still in
 * flight, puts a surround packet's channels where the mapping table says, and
 * tries once rather than per packet when the codec cannot be loaded; and that
 * the microphone worker asks for what the WebCodecs path asks for, gathers the
 * worklet's render quanta into 20 ms frames, and sends each as a 0x02 frame.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { CORE, post, readSource, startWorker, workerSource, type WorkerScope } from './core_source.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [opus-wasm-fallback] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** A stand-in for libopus-wasm: a decoder whose PCM is its packet's first byte
 *  plus a tenth per channel, and an encoder that keeps its frames. */
const STUB = `
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
`;
const dir = mkdtempSync(join(tmpdir(), 'selkies-opus-wasm-'));
writeFileSync(join(dir, 'stub.mjs'), STUB);
/** The stand-in's URL for one scenario: its own module, so what it records starts empty. */
const stubUrl = (scenario: string): string => `${pathToFileURL(join(dir, 'stub.mjs')).href}?${scenario}`;
const MISSING_URL = pathToFileURL(join(dir, 'missing.mjs')).href;

/** The stand-in module's record of the codecs it made. */
interface Made {
  o: Record<string, unknown>;
  frames?: number[][];
  freed?: boolean;
}

/** Whether an entry of the stand-in's record is a codec it made. */
const isMade = (m: unknown): m is Made => typeof fields(m).o === 'object';

/** The stand-in's live record for one scenario: what it made so far, and a reset. */
async function stubOf(url: string): Promise<{ made: () => Made[]; clear: () => void }> {
  const record = fields(await import(url)).made;
  if (!Array.isArray(record)) throw new Error('the stand-in exports no record');
  const live: unknown[] = record;
  return { made: () => live.filter(isMade), clear: () => { live.length = 0; } };
}

/** Starts a worker source over a fresh scope and keeps what it posts. */
function start(source: string, filename: string): { scope: WorkerScope; posted: Record<string, unknown>[] } {
  const posted: Record<string, unknown>[] = [];
  const scope: WorkerScope = { postMessage: (m) => posted.push(fields(m)), onmessage: null, close() {} };
  startWorker(source, filename, scope);
  return { scope, posted };
}

/** Hands a worker a message on its own port. */
const message = post;

const near = (got: number[], want: number[]): boolean =>
  got.length === want.length && got.every((g, i) => Math.abs(g - want[i]) < 1e-4);
const tick = (): Promise<void> => new Promise((resolve) => { setTimeout(resolve, 20); });
const text = readSource(CORE);
const decoderSource = workerSource(text, 'audioDecoderWorkerCode');

// The decode worker over the stand-in: stereo, then 5.1.
{
  const url = stubUrl('decode');
  const stub = await stubOf(url);
  const { scope, posted } = start(decoderSource, 'audio-decoder-worker.js');
  const toWorklet: Record<string, unknown>[] = [];
  await message(scope, { type: 'init', data: { initialPipelineStatus: true, channels: 2, opusUrl: url } });
  const stereoInit = posted.slice();
  const stereoDecoders = stub.made().map((d) => d.o);
  message(scope, { type: 'pcmPort', port: { postMessage: (m: unknown) => toWorklet.push(fields(m)) } });
  const audioIn: { onmessage: ((event: { data: { buffer: ArrayBuffer } }) => void) | null } = { onmessage: null };
  message(scope, { type: 'audioIn', port: audioIn });
  const send = (bytes: number[]): void => {
    if (!audioIn.onmessage) throw new Error('the worker listens on no audio port');
    audioIn.onmessage({ data: { buffer: new Uint8Array(bytes).buffer } });
  };
  send([0x01, 0x00, 0x10, 1, 2]);
  send([0x01, 0x80]);
  send([0x01, 0x80, 0x20, 9]);
  await tick();
  const seq = toWorklet.map((m) => m.audioData instanceof ArrayBuffer
    ? [m.quiet ? 'ending' : 'audio', Array.from(new Float32Array(m.audioData).slice(0, 2)), new Float32Array(m.audioData).length]
    : [m.quiet ? 'mark' : 'other']);

  // 5.1: streams 4, coupled 2, mapping 0 4 1 2 3 5, each elementary packet a
  // one-frame packet whose TOC byte names its stream, all but the last
  // self-delimited.
  toWorklet.length = 0;
  stub.clear();
  const head = new Uint8Array(27);
  head.set([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 1, 6]);
  head[18] = 1; head[19] = 4; head[20] = 2;
  head.set([0, 4, 1, 2, 3, 5], 21);
  await message(scope, { type: 'init', data: { initialPipelineStatus: true, channels: 6, description: head.buffer } });
  const surroundDecoders = stub.made().map((d) => d.o);
  send([0x01, 0x00, 0x10, 1, 7, 0x20, 1, 7, 0x30, 1, 7, 0x40, 7]);
  await tick();
  const audio = toWorklet.length ? toWorklet[0].audioData : undefined;
  const pcm = audio instanceof ArrayBuffer ? new Float32Array(audio) : new Float32Array(0);
  const surround = { length: pcm.length, first: Array.from(pcm.slice(0, 6)), last: Array.from(pcm.slice(-6)) };

  const initialized = stereoInit.filter((m) => m.type === 'decoderInitialized');
  check('the codec is loaded and said to be WASM',
        initialized.length === 1 && initialized[0].wasm === true && Object.keys(initialized[0]).length === 2, stereoInit);
  check('one stereo decoder at 48 kHz',
        JSON.stringify(stereoDecoders) === JSON.stringify([{ sampleRate: 48000, channels: 2 }]), stereoDecoders);
  const want: [string, number[]?, number?][] = [['audio', [16, 16.1], 1920], ['mark'], ['ending', [32, 32.1], 1920]];
  check('the sound, the mark that waited for its decode, then the frame that ends it, marked quiet',
        seq.length === 3 && seq.every((s, i) => s[0] === want[i][0])
        && seq.every((s, i) => s.length === 1
          || (Array.isArray(s[1]) && near(s[1], want[i][1] ?? []) && s[2] === want[i][2])), seq);
  check('5.1 opens a decoder per elementary stream, the coupled ones stereo',
        JSON.stringify(surroundDecoders) === JSON.stringify([2, 2, 1, 1].map((channels) => ({ sampleRate: 48000, channels }))),
        surroundDecoders);
  const want6 = [16, 48, 16.1, 32, 32.1, 64];
  check('each surround channel comes from the stream and channel its mapping names',
        surround.length === 960 * 6 && near(surround.first, want6) && near(surround.last, want6), surround);
}

// The decode worker where the codec cannot be loaded.
{
  const { scope, posted } = start(decoderSource, 'audio-decoder-worker.js');
  await message(scope, { type: 'init', data: { initialPipelineStatus: true, channels: 2, opusUrl: MISSING_URL } });
  for (let i = 0; i < 50; i++) await message(scope, { type: 'decode', data: { opusBuffer: new Uint8Array([0x10, 1]).buffer } });
  const failedBefore = posted.filter((m) => m.type === 'decoderInitFailed').length;
  await message(scope, { type: 'reinitialize' });
  const got = { failedBefore, failedAfter: posted.filter((m) => m.type === 'decoderInitFailed').length,
                types: [...new Set(posted.map((m) => m.type))] };
  check('a codec that cannot be loaded fails once, not once per packet', got.failedBefore === 1, got);
  check('and is tried again when the page asks', got.failedAfter === 2, got);
  check('with nothing claiming to be initialized', !got.types.includes('decoderInitialized'), got.types);
}

// The microphone worker over the stand-in.
{
  const url = stubUrl('encode');
  const stub = await stubOf(url);
  const { scope, posted } = start(workerSource(text, 'micEncodeWorkerCode'), 'mic-encode-worker.js');
  const wire: number[][] = [];
  await message(scope, { type: 'init', opusUrl: url });
  message(scope, { type: 'wirePort', port: { postMessage: (buf: ArrayBuffer) => wire.push(Array.from(new Uint8Array(buf))) } });
  // Fifteen render quanta of 128 samples: four 20 ms frames at 24 kHz.
  let n = 0;
  for (let q = 0; q < 15; q++) {
    const s = new Int16Array(128);
    for (let i = 0; i < 128; i++) s[i] = n++;
    message(scope, { type: 'pcm', buffer: s.buffer });
  }
  const enc = stub.made()[0];
  const frames = enc.frames ?? [];
  const contiguous = frames.flat().every((v, i) => v === i);
  message(scope, { type: 'stop' });
  message(scope, { type: 'pcm', buffer: new Int16Array(480).buffer });
  const opts = enc.o;
  check('the encoder is asked for what WebCodecs is: 24 kHz mono, restricted low delay, the mic bitrate, 20 ms',
        opts.sampleRate === 24000 && opts.channels === 1 && opts.application === 2051 && opts.frameSize === 480
        && typeof opts.bitrate === 'number' && Number.isInteger(opts.bitrate) && opts.bitrate > 0, opts);
  check('the worker says it is ready on WASM',
        posted.some((m) => m.type === 'ready' && m.wasm === true && Object.keys(m).length === 2), posted);
  check('fifteen render quanta become four whole 20 ms frames, in order',
        JSON.stringify(frames.map((f) => f.length)) === JSON.stringify([480, 480, 480, 480]) && contiguous,
        frames.map((f) => f.length));
  check('each goes out as a 0x02 frame around its packet',
        JSON.stringify(wire) === JSON.stringify([1, 2, 3, 4].map((k) => [0x02, 0xfc, k])), wire);
  check('stopping frees the encoder and sends nothing after', !!enc.freed && wire.length === 4, !!enc.freed);
}

console.log(`[opus-wasm-fallback] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
