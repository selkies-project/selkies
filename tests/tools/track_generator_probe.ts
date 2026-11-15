/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Runs the video worker's sink handshake against stubs that hold the
 * mediacapture-transform interfaces to their IDL, so the generator path is
 * exercised where no browser the suites can launch exposes it.
 *
 * `VideoTrackGenerator` and `MediaStreamTrackProcessor` are `Exposed=
 * DedicatedWorker`, so a page that asks for either sees `undefined`, and
 * WebKit gates both on `MediaStreamTrackProcessingEnabled`, whose default is
 * on under `PLATFORM(COCOA)` and off in every other build of that engine. The
 * worker source is read out of the client and evaluated here with the globals
 * a dedicated worker has, which
 * is enough to answer what it does with a generator: construct it with no
 * arguments, take the writer from `writable`, and hand `track` to the page in
 * the transfer list, without which the page receives a detached track.
 *
 * The canvas sink an engine without one gets is run the same way, on a clock
 * each draw advances, with frames that count their closes, which no browser
 * reports: a thread that keeps up draws every frame at once, one still busy
 * with its last draw draws only the newest of what arrived, once the work
 * queued ahead of it has run, and every frame is closed exactly once, drawn or
 * not.
 * @module
 */
import { join } from 'node:path';
import vm from 'node:vm';
import { CORE, WEB, readSource, workerSource } from './core_source.ts';

const WEBCAM = join(WEB, 'lib', 'webcam-capture.ts');

/** What a worker posts: the fields of its messages the checks read. */
interface WorkerMessage {
    type?: string;
    mode?: string;
    track?: object;
    jpegDecode?: boolean;
    shown?: { presented: number; superseded: number };
}

/** One `postMessage` of the video worker. */
interface Post {
    msg: WorkerMessage;
    transfer?: unknown[];
}

/** A worker's message handler, as the worker leaves it on `self`. */
type Handler = ((event: { data: object }) => void) | null;

/** What a run of the video worker left behind. */
interface WorkerRun {
    constructed: boolean;
    readReadable: boolean;
    posts: Post[];
    onmessage?: Handler;
}

/** What a run of the webcam's encode worker left behind. */
interface EncodeRun {
    posts: Array<WorkerMessage | undefined>;
    processorTrack: object | null;
    track?: object;
}

/** A decoded frame that counts its closes. */
interface CountedFrame {
    id: number;
    displayWidth: number;
    displayHeight: number;
    timestamp: number;
    closes: number;
    close(): void;
}

let passed = 0, failed = 0;
const check = (label: string, ok: boolean | undefined, detail: unknown = ''): void => {
    if (ok) passed++; else failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [track-generator] ${label}  ${String(detail)}`);
};

/** An IDL-faithful `VideoTrackGenerator`: no constructor arguments, a
 *  `WritableStream` and a `MediaStreamTrack`, and nothing else. */
function generatorStub(state: WorkerRun): new (...args: unknown[]) => object {
    return class VideoTrackGenerator {
        writable: WritableStream;
        track: object;
        muted: boolean;
        constructor(...args: unknown[]) {
            if (args.length) throw new TypeError('VideoTrackGenerator takes no arguments');
            state.constructed = true;
            this.writable = new WritableStream({ write: () => {} });
            this.track = { kind: 'video', __mediaStreamTrack: true };
            this.muted = false;
        }
        get readable() { state.readReadable = true; return undefined; }
    };
}

/** Evaluates the worker source with a dedicated worker's globals, over which
 *  `globals` goes; returns what it posted and whether it built a generator. */
function runWorker({ withGenerator, globals = {} }: { withGenerator: boolean; globals?: Record<string, unknown> }): WorkerRun {
    const state: WorkerRun = { constructed: false, readReadable: false, posts: [] };
    const scope: Record<string, unknown> = {
        VideoDecoder: class { constructor() {} },
        VideoEncoder: class {},
        VideoFrame: class { constructor() {} close() {} },
        OffscreenCanvas: class { constructor() {} getContext() { return null; } },
        ImageDecoder: class {},
        WritableStream, ReadableStream, MessageChannel,
        createImageBitmap: () => Promise.resolve({}),
        performance, setInterval: () => 0, clearInterval: () => {}, setTimeout, clearTimeout,
        console, navigator: { userAgent: '', platform: '' },
    };
    if (withGenerator) scope.VideoTrackGenerator = generatorStub(state);
    Object.assign(scope, globals);
    const self: { postMessage: (msg: WorkerMessage, transfer?: unknown[]) => void; onmessage: Handler } = {
        postMessage: (msg, transfer) => state.posts.push({ msg, transfer }),
        onmessage: null,
    };
    Object.assign(self, scope);
    scope.self = self;
    vm.createContext(scope);
    new vm.Script(workerSource(readSource(CORE), 'VIDEO_WORKER_SRC'),
                  { filename: 'video-worker.js' }).runInContext(scope);
    state.onmessage = self.onmessage;
    return state;
}

const withGen = runWorker({ withGenerator: true });
const mode = withGen.posts.find((p) => p.msg && p.msg.type === 'mode');
check('a generator is built with no arguments', withGen.constructed, '');
check('the sink it reports is the generator', mode && mode.msg.mode === 'vtg',
      mode && mode.msg.mode);
check('the track goes to the page', !!(mode && mode.msg.track), mode && !!mode.msg.track);
check('and rides the transfer list, so the page gets a live one',
      !!(mode && mode.transfer && mode.transfer.includes(mode.msg.track)),
      mode && mode.transfer);
check('the writer comes from writable, never readable', !withGen.readReadable, '');
check('the jpeg capability rides the same reply',
      !!(mode && mode.msg.jpegDecode === true), mode && mode.msg.jpegDecode);
check('the worker is left listening', typeof withGen.onmessage === 'function', '');

const without = runWorker({ withGenerator: false });
const canvasMode = without.posts.find((p) => p.msg && p.msg.type === 'mode');
check('an engine without one is told to send a canvas',
      canvasMode && canvasMode.msg.mode === 'canvas', canvasMode && canvasMode.msg.mode);
check('and nothing is transferred with it',
      canvasMode && !canvasMode.transfer, canvasMode && canvasMode.transfer);

/** A decoded frame that counts its closes. */
const frame = (id: number): CountedFrame => ({ id, displayWidth: 1280, displayHeight: 800, timestamp: id, closes: 0,
                         close() { this.closes++; } });
/** The worker's clock, which a draw advances by what it costs. */
let clock = 1000;
const DRAW_MS = 10, FRAME_MS = 16.7;
/** A decoder the worker made: what it was made with, and its state. */
interface FakeDecoder {
    init: { output: (frame: CountedFrame) => void };
    state: string;
    decodeQueueSize: number;
}
const decoders: FakeDecoder[] = [], draws: number[] = [];
const sink = runWorker({ withGenerator: false, globals: {
    performance: { now: () => clock, timeOrigin: 0 },
    VideoDecoder: class implements FakeDecoder {
        init: FakeDecoder['init'];
        state = 'unconfigured';
        decodeQueueSize = 0;
        constructor(init: FakeDecoder['init']) { this.init = init; decoders.push(this); }
        configure() { this.state = 'configured'; }
        decode() {}
        close() { this.state = 'closed'; }
        static isConfigSupported() { return Promise.resolve({ supported: true }); }
    },
} });
const tell = (data: object): void => sink.onmessage!({ data });
tell({ canvas: { width: 300, height: 150, getContext: () => ({
    // A browser refuses to draw a closed frame.
    drawImage: (f: CountedFrame) => {
        if (f.closes) throw new Error(`frame ${f.id} drawn after its close`);
        draws.push(f.id);
        clock += DRAW_MS;
    },
}) } });
tell({ type: 'statsOpen', open: true });
tell({ type: 'decoderConfig', codec: 'avc1.42e01f', codedWidth: 1280, codedHeight: 800 });
const decoded = (f: CountedFrame): void => decoders[decoders.length - 1].init.output(f);
const nextTask = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 5));

const kept = [frame(1), frame(2)];
decoded(kept[0]);
clock += FRAME_MS;
decoded(kept[1]);
check('a canvas that keeps up draws each decoded frame at once', JSON.stringify(draws) === '[1,2]', draws);
const burst = [frame(3), frame(4), frame(5)];
burst.forEach(decoded);
check('one decoded while its last draw is still recent waits for the work queued ahead of it',
      JSON.stringify(draws) === '[1,2]', draws);
await nextTask();
check('then only the newest of a burst is drawn', JSON.stringify(draws) === '[1,2,5]', draws);
clock += FRAME_MS;
const paced = frame(6);
decoded(paced);
check('and a thread caught up again draws at once', JSON.stringify(draws) === '[1,2,5,6]', draws);
const stale = frame(7);
decoded(stale);
tell({ type: 'wireMode', striped: true });
await nextTask();
check('a frame its decoder left behind is dropped, never drawn over what replaced it',
      !draws.includes(7), draws);
tell({ type: 'decodeStats' });
const stats = sink.posts.filter((p) => p.msg && p.msg.type === 'decodeStats').pop();
check('the frames it never drew count as not shown',
      stats && stats.msg.shown!.presented === 4 && stats.msg.shown!.superseded === 3,
      stats && JSON.stringify(stats.msg.shown));
const all = [...kept, ...burst, paced, stale];
check('every frame is closed exactly once, drawn or not', all.every((f) => f.closes === 1), all.map((f) => f.closes));
check('and the page hears once that the canvas holds a picture',
      sink.posts.filter((p) => p.msg && p.msg.type === 'presented').length === 1,
      sink.posts.map((p) => p.msg && p.msg.type).join());

/** An IDL-faithful `MediaStreamTrackProcessor`: a dictionary carrying a video
 *  track, and a `ReadableStream` of frames. */
function processorStub(state: EncodeRun): new (init?: { track?: object }) => object {
    return class MediaStreamTrackProcessor {
        readable: ReadableStream;
        constructor(init?: { track?: object }) {
            if (!init || typeof init !== 'object' || !init.track) {
                throw new TypeError('MediaStreamTrackProcessor takes { track }');
            }
            state.processorTrack = init.track;
            this.readable = new ReadableStream({ pull() { /* never resolves */ } });
        }
    };
}

/** Runs the webcam's encode worker and hands it a transferred camera track. */
function runEncodeWorker({ withProcessor }: { withProcessor: boolean }): EncodeRun {
    const state: EncodeRun = { posts: [], processorTrack: null };
    const scope: Record<string, unknown> = {
        VideoEncoder: class { constructor() {} static isConfigSupported() { return Promise.resolve({ supported: false }); } },
        VideoFrame: class { constructor() {} close() {} },
        OffscreenCanvas: class { constructor() {} getContext() { return null; } },
        ImageEncoder: class {}, ReadableStream, WritableStream,
        createImageBitmap: () => Promise.resolve({}),
        performance, setTimeout, clearTimeout, setInterval: () => 0, clearInterval: () => {},
        console,
    };
    if (withProcessor) scope.MediaStreamTrackProcessor = processorStub(state);
    const self: { postMessage: (msg?: WorkerMessage) => void; onmessage: Handler } = {
        postMessage: (msg) => state.posts.push(msg), onmessage: null };
    Object.assign(self, scope);
    scope.self = self;
    vm.createContext(scope);
    new vm.Script(workerSource(readSource(WEBCAM), 'ENCODE_WORKER_SRC'),
                  { filename: 'encode-worker.js' }).runInContext(scope);
    const track = { kind: 'video', __mediaStreamTrack: true };
    self.onmessage!({ data: { type: 'track', track } });
    state.track = track;
    return state;
}

const camera = runEncodeWorker({ withProcessor: true });
check('the camera track is read through a processor', camera.processorTrack === camera.track,
      camera.processorTrack === camera.track);
check('which is told so in a dictionary, as its init takes',
      camera.posts.some((m) => m && m.type === 'track_reading'),
      camera.posts.map((m) => m && m.type));

const noProcessor = runEncodeWorker({ withProcessor: false });
check('a worker without one says so instead of throwing',
      noProcessor.posts.some((m) => m && m.type === 'track_unsupported'),
      noProcessor.posts.map((m) => m && m.type));

console.log(`[track-generator] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
