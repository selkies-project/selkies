/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The audio decoder worker's hand-off to the playback worklet, driven as plain logic.
 *
 * The worker source the WebSockets core starts runs under a stand-in WebCodecs
 * decoder whose output lags each decode, so it is pinned that a sound's PCM,
 * the server's bare quiet mark behind it (arriving while that decode is still
 * in flight), and the silent frame that ends the sound reach the worklet's
 * port in that order, the last one marked quiet.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import vm from 'node:vm';
import { CORE, readSource, workerSource } from './core_source.ts';

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [audio-decoder-worker] ${label}  ${detail}`);
}

/** What the worker posts to the worklet's port. */
interface ToWorklet {
  audioData?: ArrayBuffer;
  quiet?: boolean;
}

/** A message the worker takes on its own port or on the audio port it is handed. */
interface WorkerEvent {
  data: Record<string, unknown>;
}

/** The worker's global scope, as far as the checks drive it. */
interface WorkerScope {
  postMessage(message: unknown): void;
  onmessage: ((event: WorkerEvent) => unknown) | null;
  close(): void;
}

/** A decoded frame as the stand-in decoder outputs it. */
interface StandInFrame {
  allocationSize(): number;
  copyTo(destination: Float32Array): void;
  close(): void;
}

const toWorklet: ToWorklet[] = [];
const scope: WorkerScope = { postMessage() {}, onmessage: null, close() {} };
Object.assign(globalThis, {
  self: scope,
  EncodedAudioChunk: class {
    data: ArrayBuffer;
    constructor(init: { data: ArrayBuffer }) { this.data = init.data; }
  },
  AudioDecoder: class {
    output: (frame: StandInFrame) => void;
    state = 'unconfigured';
    constructor({ output }: { output: (frame: StandInFrame) => void }) { this.output = output; }
    static isConfigSupported(): Promise<{ supported: boolean }> { return Promise.resolve({ supported: true }); }
    configure(): void { this.state = 'configured'; }
    decode(chunk: { data: ArrayBuffer }): void {
      const bytes = new Uint8Array(chunk.data).length;
      setTimeout(() => this.output({
        allocationSize: () => 4 * 960,
        copyTo: (dst) => { dst.fill(bytes); },
        close() {},
      }), 5);
    }
    close(): void { this.state = 'closed'; }
  },
});
vm.runInThisContext(workerSource(readSource(CORE), 'audioDecoderWorkerCode'), { filename: 'audio-decoder-worker.js' });

/** Hands the worker a message on its own port. */
const message = (data: Record<string, unknown>): unknown => {
  if (!scope.onmessage) throw new Error('the worker listens on no port');
  return scope.onmessage({ data });
};
await message({ type: 'init', data: { initialPipelineStatus: true, channels: 2 } });
message({ type: 'pcmPort', port: { postMessage: (m: ToWorklet) => toWorklet.push(m) } });
const audioIn: { onmessage: ((event: WorkerEvent) => void) | null } = { onmessage: null };
message({ type: 'audioIn', port: audioIn });
const send = (bytes: number[]): void => {
  if (!audioIn.onmessage) throw new Error('the worker listens on no audio port');
  audioIn.onmessage({ data: { buffer: new Uint8Array(bytes).buffer } });
};
send([0x01, 0x00, 0xfc, 1, 2, 3]);
send([0x01, 0x80]);
send([0x01, 0x80, 0xfc, 9]);
await new Promise((resolve) => setTimeout(resolve, 50));
const seq = toWorklet.map((m) => m.audioData
  ? (m.quiet ? 'ending:' : 'audio:') + String(new Float32Array(m.audioData)[0])
  : (m.quiet ? 'mark' : 'other'));
check('the sound, the mark that waited for its decode, then the frame that ends it, marked quiet',
  JSON.stringify(seq) === JSON.stringify(['audio:4', 'mark', 'ending:2']),
  `[${seq.map((s) => `'${s}'`).join(', ')}]`);
console.log(`[audio-decoder-worker] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
