/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The WebSockets decoder worker's split of a surround packet into its streams.
 *
 * The worker decodes surround one elementary stream at a time, so it has to
 * cut a multistream Opus packet at the stream boundaries itself: every stream
 * but the last is self-delimited (RFC 6716 Appendix B), carrying one more
 * length field in front of its last frame's data. `frameLength` and
 * `splitStreams` are taken from the worker source the WebSockets core starts
 * and run against packets built here from known elementary packets, covering
 * every frame-count code (one frame, two equal, two unequal, and an arbitrary
 * count with constant or variable lengths), padding, and one- and two-byte
 * length fields; each split must give back the elementary packets byte for
 * byte, and a truncated packet must give back nothing.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import vm from 'node:vm';
import { CORE, declaration, readSource, workerSource } from './core_source.ts';

const worker = workerSource(readSource(CORE), 'audioDecoderWorkerCode');
const compiled: unknown = vm.runInThisContext(
  `${declaration(worker, 'frameLength')}\n${declaration(worker, 'splitStreams')}\nsplitStreams`,
  { filename: 'audio-decoder-worker.js' });
if (typeof compiled !== 'function') throw new Error('splitStreams is not a function in the worker source');
const split = compiled;

/** The worker's split, its answer checked to be packets or nothing. */
function splitStreams(buf: ArrayBuffer, streams: number): ArrayBuffer[] | null {
  const out: unknown = Reflect.apply(split, undefined, [buf, streams]);
  if (out === null) return null;
  if (!Array.isArray(out)) throw new Error('splitStreams returned no list');
  const packets = out.filter((p): p is ArrayBuffer => p instanceof ArrayBuffer);
  if (packets.length !== out.length) throw new Error('splitStreams returned something other than packets');
  return packets;
}

/** An Opus frame length field for `n` bytes (RFC 6716 3.2.1). */
const len = (n: number): number[] => n < 252 ? [n] : [252 + ((n - 252) & 3), (n - 252 - ((n - 252) & 3)) >> 2];
const bytes = (n: number, seed: number): number[] => Array.from({ length: n }, (_, i) => (i * 31 + seed * 7) & 0xff);

/** A plain packet and its self-delimited form, from its code and frame sizes. */
function packet(code: number, frames: number[], pad: number, seed: number): { plain: number[]; delimited: number[] } {
  const data = frames.flatMap((n, i) => bytes(n, seed + i));
  const last = frames[frames.length - 1];
  const toc = (seed << 3) & 0xf8 | code;
  const head = [toc];
  let mid: number[] = [];
  if (code === 2) mid = len(frames[0]);
  if (code === 3) {
    const vbr = frames.some((n) => n !== frames[0]);
    head.push((vbr ? 0x80 : 0) | (pad ? 0x40 : 0) | frames.length);
    if (pad) head.push(pad);
    if (vbr) mid = frames.slice(0, -1).flatMap(len);
  }
  const padding = Array<number>(pad).fill(0);
  const plain = [...head, ...mid, ...data, ...padding];
  const delimited = [...head, ...mid, ...len(code === 1 ? frames[0] : last), ...data, ...padding];
  return { plain, delimited };
}

const shapes: [number, number[], number][] = [
  [0, [120], 0], [0, [300], 0], [1, [80, 80], 0], [1, [260, 260], 0],
  [2, [40, 90], 0], [2, [300, 10], 0], [3, [60, 60, 60], 0], [3, [30, 270, 5, 90], 0],
  [3, [50, 50], 7], [3, [20, 280, 40], 3],
];
const out: Record<string, boolean> = {};
let seed = 1;
for (const streams of [4, 5]) {
  for (let s = 0; s < shapes.length; s++) {
    const parts: { plain: number[]; delimited: number[] }[] = [];
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

let failed = 0;
for (const [name, ok] of Object.entries(out)) {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [audio-multistream] ${name}`);
}
console.log(`[audio-multistream] ${Object.keys(out).length - failed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
