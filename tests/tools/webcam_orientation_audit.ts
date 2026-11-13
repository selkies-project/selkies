/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the webcam uplink puts on each encoded frame, and what it does when it
 * cannot send one. The upright transform is relayed rather than drawn into the
 * pixels, so it has to be read from the frame where the engine exposes it and
 * derived from the window where it does not -- and derived on that engine alone,
 * because deriving on one that already pre-rotates the camera turns every
 * upright frame sideways. The rate the frames are admitted at and the chain the
 * server's decoder follows are checked here too: both are silent failures in a
 * browser (a halved uplink, a permanently smeared picture) and neither shows up
 * in a screenshot.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import type { StandIn } from './stand_in.ts';

/** The module under audit, as each load of it is typed. */
type WebcamModule = typeof import('../../web/selkies-web-core/lib/webcam-capture.ts');
/** A capture of the module. */
type Capture = InstanceType<WebcamModule['WebcamCapture']>;

const MODULE = new URL('../../web/selkies-web-core/lib/webcam-capture.ts', import.meta.url).href;

let failed = 0;
let variant = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [webcam-orient] ${label}  ${String(detail)}`);
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** A message as the stand-in worker hands it to the page. */
type WorkerListener = (event: StandIn<MessageEvent<{ type: string }>>) => void;

/** A worker that answers the handshake the module expects and records what it was sent. */
class FakeWorker implements StandIn<Worker> {
    sent: Array<{ type: string }> = [];
    listeners: WorkerListener[] = [];
    onmessage: WorkerListener | null = null;

    postMessage(message: { type: string }): void {
        this.sent.push(message);
        if (message.type === 'source') {
            setTimeout(() => this.deliver({ type: 'ready' }), 0);
        }
    }

    deliver(data: { type: string }): void {
        if (this.onmessage) this.onmessage({ data });
        this.listeners.forEach((fn) => fn({ data }));
    }

    addEventListener(type: string, fn: WorkerListener): void {
        if (type === 'message') this.listeners.push(fn);
    }

    removeEventListener(type: string, fn: WorkerListener): void {
        this.listeners = this.listeners.filter((f) => f !== fn);
    }

    terminate(): void {}
}

/** The stand-in window of the last load, whose orientation a check turns. */
let page: { orientation: number | undefined; addEventListener(): void; removeEventListener(): void } = {
    orientation: undefined, addEventListener() {}, removeEventListener() {},
};

/**
 * Load the module against one engine shape: `orientation` is what the window
 * reports (undefined for a desktop one), `metadata` whether VideoFrames carry
 * their own transform. The query string is what gets a fresh module instance,
 * since the capability is read once at load.
 */
async function load({ orientation, metadata }: { orientation: number | undefined; metadata: boolean }): Promise<WebcamModule> {
    if (metadata) {
        const Frame = class VideoFrame {};
        Object.defineProperty(Frame.prototype, 'rotation', {
            get() { return 0; },
            configurable: true,
        });
        Object.assign(globalThis, { VideoFrame: Frame });
    } else {
        Reflect.deleteProperty(globalThis, 'VideoFrame');
    }
    page = { orientation, addEventListener() {}, removeEventListener() {} };
    Object.assign(globalThis, {
        window: page satisfies StandIn<Window>,
        screen: {} satisfies StandIn<Screen>,
        VideoEncoder: class VideoEncoder {},
        Worker: FakeWorker,
        Blob: class Blob {},
    });
    globalThis.URL.createObjectURL = () => 'blob:audit';
    globalThis.URL.revokeObjectURL = () => {};
    return import(`${MODULE}?variant=${variant++}`) as Promise<WebcamModule>;
}

/** One frame the capture sent, as the wire names it. */
interface SentFrame {
    codec: number;
    keyframe: boolean;
    rotation?: number;
    flip?: boolean;
}

/** A capture that is running, with what it sends. */
function makeCapture(mod: WebcamModule, opts: { canSend?: () => boolean; fps?: number } = {}): { capture: Capture; sent: SentFrame[] } {
    const sent: SentFrame[] = [];
    const capture = new mod.WebcamCapture({
        sendFrame: (codec, keyframe, bytes, rotation, flip) => sent.push({ codec, keyframe, rotation, flip }),
        canSend: opts.canSend || (() => true),
        fps: opts.fps || 30,
    });
    capture._active = true;
    return { capture, sent };
}

// --- the transform on the wire ---------------------------------------------

// Mobile WebKit: the sensor's own orientation, nothing on the frame to read it from.
{
    const mod = await load({ orientation: 90, metadata: false });
    const { capture } = makeCapture(mod);
    capture._deriveOrientation = true;
    for (const [angle, want] of [[-90, 0], [0, 90], [90, 180], [180, 270]]) {
        page.orientation = angle;
        const got = capture._frameOrientation({} as VideoFrame);
        check(`window at ${angle} needs ${want} clockwise`,
              got.rotation === want && got.flip === false, JSON.stringify(got));
    }
    page.orientation = 90;

    // The derived turn is a label the encoder never sees, so it owes no rebuild:
    // only a size or a turn the frame itself carries may rebuild one.
    capture._encodedSize = { w: 640, h: 480, rotation: 0, flip: false };
    const encoder = { state: 'configured' as const, encodeQueueSize: 0, encoded: false, encode() { this.encoded = true; }, close() {} };
    const configured = encoder as StandIn<VideoEncoder> as VideoEncoder;
    capture._encoder = configured;
    capture._handleFrame({ displayWidth: 640, displayHeight: 480, close() {} } as StandIn<VideoFrame> as VideoFrame);
    check('a derived turn keeps the encoder', capture._encoder === configured && encoder.encoded === true,
          `orientation ${JSON.stringify(capture._orientation)}`);
    check('the derived turn is what the chunk carries', capture._orientation.rotation === 180,
          JSON.stringify(capture._orientation));
}

// A worker-only track processor on a mobile viewport is the engine that needs it.
{
    const mod = await load({ orientation: 0, metadata: false });
    const { capture } = makeCapture(mod);
    const track = { clone: () => ({ stop() {} }) } as StandIn<MediaStreamTrack> as MediaStreamTrack;
    const source = await capture._openSource(track, capture._generation);
    check('a worker source on a mobile viewport derives', !!source && capture._deriveOrientation === true,
          `source ${!!source}, derive ${capture._deriveOrientation}`);
}

// A desktop window has no orientation to derive from, whatever its source is.
{
    const mod = await load({ orientation: undefined, metadata: false });
    const { capture } = makeCapture(mod);
    const source = await capture._openSource(
        { clone: () => ({ stop() {} }) } as StandIn<MediaStreamTrack> as MediaStreamTrack, capture._generation);
    check('a desktop window never derives', !!source && capture._deriveOrientation === false,
          `derive ${capture._deriveOrientation}`);
}

// An engine that puts the transform on the frame is read, never second-guessed.
{
    const mod = await load({ orientation: 90, metadata: true });
    const { capture } = makeCapture(mod);
    const source = await capture._openSource(
        { clone: () => ({ stop() {} }) } as StandIn<MediaStreamTrack> as MediaStreamTrack, capture._generation);
    check('frame metadata is not derived over', !!source && capture._deriveOrientation === false,
          `derive ${capture._deriveOrientation}`);
    const got = capture._frameOrientation({ rotation: 270, flip: true } as StandIn<VideoFrame> as VideoFrame);
    check('frame metadata is relayed as it stands', got.rotation === 270 && got.flip === true,
          JSON.stringify(got));
}

// --- what the chunk carries -------------------------------------------------
{
    const mod = await load({ orientation: 90, metadata: false });
    const { capture, sent } = makeCapture(mod);
    capture._orientation = { rotation: 180, flip: true };
    capture._onChunk({ id: mod.WEBCAM_CODEC_H264 },
                     { byteLength: 4, type: 'key', copyTo() {} } as StandIn<EncodedVideoChunk> as EncodedVideoChunk,
                     capture._generation);
    check('the chunk is stamped with the frame it came from',
          sent.length === 1 && sent[0].rotation === 180 && sent[0].flip === true, JSON.stringify(sent));
}

// --- the chain the server's decoder follows ---------------------------------
{
    const mod = await load({ orientation: undefined, metadata: false });
    let room = false;
    const { capture, sent } = makeCapture(mod, { canSend: () => room });
    let keyframesAsked = 0;
    capture.requestKeyframe = () => { keyframesAsked++; };

    capture._deliverEncoded(mod.WEBCAM_CODEC_H264, false, new Uint8Array(1), 0, false);
    check('a frame the socket cannot take is dropped', sent.length === 0);
    check('no keyframe is asked for while the socket is full', keyframesAsked === 0);

    room = true;
    capture._deliverEncoded(mod.WEBCAM_CODEC_H264, false, new Uint8Array(1), 0, false);
    check('a delta on a broken chain is held back', sent.length === 0);
    check('the keyframe is asked for once there is room', keyframesAsked === 1);

    capture._deliverEncoded(mod.WEBCAM_CODEC_H264, true, new Uint8Array(1), 0, false);
    check('the keyframe restores the chain', sent.length === 1 && sent[0].keyframe === true,
          JSON.stringify(sent));
    capture._deliverEncoded(mod.WEBCAM_CODEC_H264, false, new Uint8Array(1), 0, false);
    check('deltas flow again after it', sent.length === 2);
}

// --- the rate frames are admitted at ----------------------------------------
{
    const mod = await load({ orientation: undefined, metadata: false });
    const { capture } = makeCapture(mod, { fps: 30 });
    const admitted = (gaps: number[], fps?: number): number => {
        const c = fps ? makeCapture(mod, { fps }).capture : capture;
        c._lastFrameMs = 0;
        c._frameCredit = 0;
        let now = 0, count = 0;
        for (const gap of gaps) {
            now += gap;
            if (c._admit(now)) count++;
        }
        return count;
    };
    const repeat = (gap: number, n: number): number[] => Array.from({ length: n }, () => gap);
    // 25 fps, so the interval and the gap are both exact in binary and the case
    // is the delivery being on time rather than the arithmetic rounding.
    check('a camera at the asked rate passes whole', admitted(repeat(40, 90), 25) === 90,
          `${admitted(repeat(40, 90), 25)}/90`);
    const jittered = Array.from({ length: 90 }, (_, i) => (i % 2 ? 31 : 36));
    check('jitter around that rate passes whole', admitted(jittered) >= 89, `${admitted(jittered)}/90`);
    // A <video> element's callbacks arrive on the compositor's grid, so a 30 fps
    // camera can land as pairs 16.7 ms apart: the worst shape for a per-gap rule.
    const quantized = Array.from({ length: 90 }, (_, i) => (i % 2 ? 16.7 : 50));
    check('compositor-quantized delivery passes whole', admitted(quantized) >= 88,
          `${admitted(quantized)}/90`);
    check('twice the asked rate is halved', Math.abs(admitted(repeat(1000 / 60, 90)) - 45) <= 1,
          `${admitted(repeat(1000 / 60, 90))}/90`);
    check('half the asked rate is passed as it comes', admitted(repeat(1000 / 15, 45)) === 45);
}

await sleep(0);
process.exit(failed ? 1 : 0);
