/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The playback worklet's adaptive jitter depth, driven as plain logic.
 *
 * AudioFrameProcessor is the source the WebSockets core hands `addModule`,
 * run under a stand-in AudioWorkletProcessor, so priming, an underrun
 * re-priming one packet deeper once a late packet ends it, the server's quiet
 * mark playing a sound shorter than the priming depth at once and making the
 * silence after it no underrun (also when the mark trails the queue running
 * dry), the standing-depth trim, the clean-stretch decay, the ceiling that
 * cuts a backlog to the target at once, and the depths sized from the frame
 * duration are pinned without a browser, and so is the smoothing of every
 * seam: a continuous tone stays continuous through a trim, a cut at the
 * ceiling, running dry, and the re-prime after it. The frame the server sends behind the mark, carrying the end of
 * the sound and marked quiet too, is appended to that sound and never taken
 * for new sound or a late one.
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
  console.log(`${ok ? 'PASS' : 'FAIL'}  [audio-worklet] ${label}  ${detail}`);
}

/** What the worklet's port carries: decoded audio, the quiet mark, or a direct port. */
interface PortData {
  audioData?: ArrayBuffer;
  quiet?: boolean;
  type?: string;
  port?: StandInPort;
}

/** The stand-in of a MessagePort, which the worklet answers on. */
interface StandInPort {
  onmessage: ((event: { data: PortData }) => void) | null;
}

/** The fields of AudioFrameProcessor the checks read and drive. */
interface Processor {
  port: StandInPort;
  target: number;
  underrunPending: boolean;
  underrunSamples: number;
  priming: boolean;
  senderQuiet: boolean;
  audioBufferQueue: Float32Array[];
  droppedOldest: number;
  MAX_BUFFER_PACKETS: number;
  TARGET_MIN: number;
  TARGET_MAX: number;
  STEP: number;
  enqueue(buffer: ArrayBuffer): void;
  process(inputs: Float32Array[][], outputs: Float32Array[][], parameters: object): boolean;
}

type ProcessorClass = new (options: { processorOptions: { channels: number } }) => Processor;

let registered: ProcessorClass | null = null;
Object.assign(globalThis, {
  AudioWorkletProcessor: class {
    port: StandInPort & { postMessage(): void } = { onmessage: null, postMessage() {} };
  },
  registerProcessor: (_name: string, cls: ProcessorClass) => { registered = cls; },
  sampleRate: 48000,
});
vm.runInThisContext(workerSource(readSource(CORE), 'audioWorkletProcessorCode'), { filename: 'audio-worklet.js' });
if (!registered) throw new Error('the worklet registered no processor');
const Cls: ProcessorClass = registered;

/** Hands the worklet a port message, as the main thread's port would. */
function post(p: Processor, data: PortData): void {
  if (!p.port.onmessage) throw new Error('the worklet listens on no port');
  p.port.onmessage({ data });
}

const p = new Cls({ processorOptions: { channels: 1 } });
const PKT = 480;
const feed = (v: number): void => p.enqueue(new Float32Array(PKT).fill(v).buffer);
const run = (): Float32Array => {
  const buf = new Float32Array(128);
  p.process([], [[buf]], {});
  return buf;
};
const silent = (b: Float32Array): boolean => b.every((x) => x === 0);
const out: Record<string, boolean> = {};

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
post(p, { quiet: true });
for (let i = 0; i < 40; i++) run();
const quietTarget = p.target, quietUnder = p.underrunSamples;
feed(0.3);
out.shortSoundHeld = silent(run());
post(p, { quiet: true });
out.quietPlaysShortSound = !silent(run());
for (let i = 0; i < 40; i++) run();
out.quietKeepsDepth = (p.priming && p.target === quietTarget && p.underrunSamples === quietUnder);
for (let i = 0; i < quietTarget; i++) feed(0.3);
out.nextSoundPrimesAgain = p.senderQuiet === false && !silent(run());
for (let i = 0; i < 40; i++) run();
out.dryBeforeMarkPending = p.underrunPending;
post(p, { quiet: true });
out.markAfterDryKeepsDepth = (!p.underrunPending && p.target === quietTarget && p.underrunSamples === quietUnder);

// The frame that ends a sound, sent right behind the mark and marked too, is
// appended to that sound: the silence after it is still the sender's, and a
// queue that ran dry before it arrived is judged silence, not a late delivery.
for (let i = 0; i < 40; i++) run();
const endTarget = p.target, endUnder = p.underrunSamples;
const ending = (): void => post(p, { audioData: new Float32Array(PKT).fill(0.2).buffer, quiet: true });
feed(0.3);
post(p, { quiet: true });
ending();
out.endingFramePlays = !silent(run()) && p.audioBufferQueue.length === 1;
out.endingFrameKeepsQuiet = p.senderQuiet;
for (let i = 0; i < 40; i++) run();
out.endingFrameKeepsDepth = (p.priming && !p.underrunPending && p.target === endTarget && p.underrunSamples === endUnder);
for (let i = 0; i < endTarget; i++) feed(0.3);
for (let i = 0; i < 40; i++) run();
out.dryBeforeEndingPending = p.underrunPending;
ending();
out.endingFrameEndsPendingAsSilence = (!p.underrunPending && p.senderQuiet && p.target === endTarget && p.underrunSamples === endUnder);

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
const trimStats = `before=${before} after=${p.audioBufferQueue.length} ` +
  `target=${p.target} underruns=${p.underrunSamples - underBefore}`;
out.decayedOverProvenSlack = (p.target <= 3);
out.reclaimWasSilentFree = (p.underrunSamples === underBefore);

// A packet arriving at the ceiling cuts the queue to the target in one seam.
while (p.audioBufferQueue.length < p.MAX_BUFFER_PACKETS) feed(0.9);
const cutBefore = p.droppedOldest;
feed(0.9);
out.ceilingCutsToTarget = (p.audioBufferQueue.length === p.target);
out.droppedCounted = (p.droppedOldest - cutBefore === p.MAX_BUFFER_PACKETS - p.target + 1);

// A backlog reaching the page three times faster than it plays (three seconds
// of it, four packets every five quanta) is skipped through in cuts, each to
// the target, not shed a packet per arrival: far fewer seams than packets shed.
let backlogStats: string;
{
  const q = new Cls({ processorOptions: { channels: 1 } });
  const quantum = (): void => { q.process([], [[new Float32Array(128)]], {}); };
  let seams = 0;
  for (let i = 0; i < 375; i++) {
    quantum();
    if (i % 5 === 4) continue;
    const before = q.droppedOldest;
    q.enqueue(new Float32Array(PKT).fill(0.5).buffer);
    if (q.droppedOldest > before) seams++;
  }
  out.backlogCutNotShed = q.droppedOldest >= 150 && seams * 4 <= q.droppedOldest;
  backlogStats = `shed=${q.droppedOldest} seams=${seams}`;
}

// The direct port line feeds the same queue and carries the quiet mark.
const fakePort: StandInPort = { onmessage: null };
post(p, { type: 'pcmPort', port: fakePort });
const direct = (data: PortData): void => {
  if (!fakePort.onmessage) throw new Error('the worklet listens on no direct port');
  fakePort.onmessage({ data });
};
direct({ audioData: new Float32Array(PKT).fill(0.7).buffer });
out.portFeeds = Math.abs(p.audioBufferQueue[p.audioBufferQueue.length - 1][0] - 0.7) < 1e-6;
direct({ quiet: true });
out.portMarksQuiet = p.senderQuiet;

// The depths follow the frame duration: the default 10 ms frame keeps two to
// six packets under a ceiling of eight, 2.5 ms frames reach the same 50 ms of
// slack in twenty-one, and 60 ms frames stop at two.
const sized = (frames: number): string => {
  const q = new Cls({ processorOptions: { channels: 1 } });
  q.enqueue(new Float32Array(frames).buffer);
  return [q.TARGET_MIN, q.TARGET_MAX, q.STEP, q.MAX_BUFFER_PACKETS].join(',');
};
out.sized10ms = sized(480) === '2,6,1,8';
out.sized2_5ms = sized(120) === '2,21,2,29';
out.sized60ms = sized(2880) === '2,2,1,3';

// Seams on tonal content: a 1037 Hz tone (10 ms is not a whole number of its
// periods, so every dropped packet shifts its phase) must step from sample to
// sample no more than about its own slope, 0.068 at half scale, through a
// trim, a cut at the ceiling, running dry, and the fade back in.
let seamStats: string;
{
  const q = new Cls({ processorOptions: { channels: 1 } });
  let n = 0;
  const tone = (): ArrayBuffer => {
    const a = new Float32Array(PKT);
    for (let i = 0; i < PKT; i++, n++) a[i] = 0.5 * Math.sin(2 * Math.PI * 1037 * n / 48000);
    return a.buffer;
  };
  let prev = 0, maxStep = 0;
  const step = (): void => {
    const buf = new Float32Array(128);
    q.process([], [[buf]], {});
    for (const x of buf) { maxStep = Math.max(maxStep, Math.abs(x - prev)); prev = x; }
  };
  for (let i = 0; i < 12; i++) q.enqueue(tone());
  out.seamsCutAtCeiling = q.droppedOldest > 0;
  const droppedBefore = q.droppedOldest;
  for (let i = 0; i < 1200; i++) {
    step();
    if (i % 15 === 3 || i % 15 === 7 || i % 15 === 11 || i % 15 === 14) q.enqueue(tone());
  }
  out.seamsTrimmed = q.droppedOldest - droppedBefore >= 1;
  for (let i = 0; i < 60; i++) step();
  out.seamsRanDry = q.priming;
  for (let i = 0; i < 3; i++) q.enqueue(tone());
  for (let i = 0; i < 20; i++) step();
  out.seamsStepSmall = maxStep < 0.08;
  seamStats = `dropped=${q.droppedOldest} maxStep=${maxStep.toFixed(4)}`;
}

for (const [name, ok] of Object.entries(out)) {
  check(name, ok, name === 'trimmed' ? trimStats : name === 'seamsStepSmall' ? seamStats : name === 'backlogCutNotShed' ? backlogStats : '');
}
console.log(`[audio-worklet] ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
