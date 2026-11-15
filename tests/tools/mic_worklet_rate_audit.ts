/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The microphone worklet at a context rate other than 24 kHz, driven as plain logic.
 *
 * An engine that cannot resample a stream into a context of another rate
 * leaves the capture context at its own rate, and the worklet brings it to the
 * 24 kHz the encoders take. The worklet source the WebSockets core hands
 * `addModule` runs at 48 and 44.1 kHz, each rate in a context of its own: a
 * tone under 12 kHz has to come out at 24 kHz at its own pitch and level, one
 * above it has to be filtered out rather than fold back into the band, and at
 * 24 kHz the samples pass through untouched.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import vm from 'node:vm';
import { CORE, readSource, workerSource } from './core_source.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [mic-worklet-rate] ${label}  ${String(detail)}`);
}

/** The worklet as far as a render quantum drives it. */
interface Processor {
  process(inputs: Float32Array[][], outputs: Float32Array[][], parameters: object): boolean;
}

/** What one second of a tone through the worklet measures as at 24 kHz. */
interface Case {
  fed: number;
  samples: number;
  amplitude: number;
  alias: number;
  first: number[];
}

const WORKLET = workerSource(readSource(CORE), 'micWorkletProcessorCode');

/** A Goertzel filter's amplitude at `f` over `xs`, read as 24 kHz. */
function tone(f: number, xs: number[]): number {
  const c = 2 * Math.cos(2 * Math.PI * f / 24000);
  let s1 = 0, s2 = 0;
  for (const x of xs) { const s0 = x + c * s1 - s2; s2 = s1; s1 = s0; }
  return Math.sqrt(Math.max(s1 * s1 + s2 * s2 - c * s1 * s2, 0)) * 2 / xs.length;
}

/**
 * One second of a tone through the worklet in 128-frame render quanta, in a
 * context at `rate`; what it posts is read back as 24 kHz and measured at the
 * tone's frequency and at the frequency it would alias to.
 */
function runCase(rate: number, hz: number, amp = 0.5): Case {
  const posted: ArrayBuffer[] = [];
  let registered: (new () => Processor) | null = null;
  const scope = vm.createContext({
    sampleRate: rate,
    registerProcessor: (_name: string, cls: new () => Processor) => { registered = cls; },
    AudioWorkletProcessor: class {
      port = { postMessage: (b: ArrayBuffer) => posted.push(b), onmessage: null };
    },
  });
  new vm.Script(WORKLET, { filename: 'mic-worklet.js' }).runInContext(scope);
  if (!registered) throw new Error('the worklet registered no processor');
  const Cls: new () => Processor = registered;
  const p = new Cls();
  let n = 0;
  for (let q = 0; q < Math.ceil(rate / 128); q++) {
    const block = new Float32Array(128);
    for (let i = 0; i < 128; i++, n++) block[i] = amp * Math.sin(2 * Math.PI * hz * n / rate);
    p.process([[block]], [], {});
  }
  const out: number[] = [];
  for (const b of posted) out.push(...new Int16Array(b));
  const tail = out.slice(2400);
  const alias = hz > 12000 ? 24000 - (hz % 24000) : hz;
  return { fed: n, samples: out.length, amplitude: tone(hz <= 12000 ? hz : alias, tail), alias: tone(alias, tail),
           first: out.slice(0, 4) };
}

const full = 0.5 * 32767;
for (const rate of [48000, 44100]) {
  let got = runCase(rate, 1000);
  const want = got.fed * 24000 / rate;
  check(`${rate} Hz in, a second's samples come out as a second at 24 kHz`, Math.abs(got.samples - want) <= 2,
        `${got.samples} of ${want.toFixed(0)}`);
  check(`${rate} Hz in, a 1 kHz tone keeps its pitch and level at 24 kHz`,
        Math.abs(got.amplitude - full) / full < 0.05, Math.round(got.amplitude));
  got = runCase(rate, 15000);
  check(`${rate} Hz in, a 15 kHz tone is filtered, not folded to ${24000 - 15000} Hz`,
        got.alias < 0.02 * full, Math.round(got.alias));
}
const got = runCase(24000, 1000);
const want = [0, 1, 2, 3].map((i) => Math.trunc(0.5 * 32767 * Math.sin(2 * Math.PI * 1000 * i / 24000)));
check('at 24 kHz the samples pass straight through',
      got.samples === 24064 && want.every((w, i) => Math.abs(got.first[i] - w) <= 1),
      `(${got.samples}, [${got.first.join(', ')}], [${want.join(', ')}])`);
console.log(`[mic-worklet-rate] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
