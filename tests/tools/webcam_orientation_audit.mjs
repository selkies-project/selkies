/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the webcam uplink puts on each encoded frame, and what it does when it
// cannot send one. The upright transform is relayed rather than drawn into the
// pixels, so it has to be read from the frame where the engine exposes it and
// derived from the window where it does not -- and derived on that engine alone,
// because deriving on one that already pre-rotates the camera turns every
// upright frame sideways. The rate the frames are admitted at and the chain the
// server's decoder follows are checked here too: both are silent failures in a
// browser (a halved uplink, a permanently smeared picture) and neither shows up
// in a screenshot.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const MODULE = new URL('../../addons/selkies-web-core/lib/webcam-capture.js', import.meta.url).href;

let failed = 0;
let variant = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [webcam-orient] ${label}  ${detail}`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** A worker that answers the handshake the module expects and records what it was sent. */
class FakeWorker {
    constructor() {
        this.sent = [];
        this.listeners = [];
    }

    postMessage(message) {
        this.sent.push(message);
        if (message.type === 'source') {
            setTimeout(() => this.deliver({ type: 'ready' }), 0);
        }
    }

    deliver(data) {
        if (this.onmessage) this.onmessage({ data });
        this.listeners.forEach((fn) => fn({ data }));
    }

    addEventListener(type, fn) {
        if (type === 'message') this.listeners.push(fn);
    }

    removeEventListener(type, fn) {
        this.listeners = this.listeners.filter((f) => f !== fn);
    }

    terminate() {}
}

/**
 * Load the module against one engine shape: `orientation` is what the window
 * reports (undefined for a desktop one), `metadata` whether VideoFrames carry
 * their own transform. The query string is what gets a fresh module instance,
 * since the capability is read once at load.
 */
async function load({ orientation, metadata }) {
    if (metadata) {
        globalThis.VideoFrame = class VideoFrame {};
        Object.defineProperty(globalThis.VideoFrame.prototype, 'rotation', {
            get() { return 0; },
            configurable: true,
        });
    } else {
        delete globalThis.VideoFrame;
    }
    globalThis.window = { orientation, addEventListener() {}, removeEventListener() {} };
    globalThis.screen = {};
    globalThis.VideoEncoder = class VideoEncoder {};
    globalThis.Worker = FakeWorker;
    globalThis.Blob = class Blob {};
    globalThis.URL.createObjectURL = () => 'blob:audit';
    globalThis.URL.revokeObjectURL = () => {};
    return import(`${MODULE}?variant=${variant++}`);
}

function makeCapture(mod, opts = {}) {
    const sent = [];
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
        globalThis.window.orientation = angle;
        const got = capture._frameOrientation({});
        check(`window at ${angle} needs ${want} clockwise`,
              got.rotation === want && got.flip === false, JSON.stringify(got));
    }
    globalThis.window.orientation = 90;

    // The derived turn is a label the encoder never sees, so it owes no rebuild:
    // only a size or a turn the frame itself carries may rebuild one.
    capture._encodedSize = { w: 640, h: 480, rotation: 0, flip: false };
    capture._encoder = { state: 'configured', encodeQueueSize: 0, encode() { this.encoded = true; }, close() {} };
    capture._handleFrame({ displayWidth: 640, displayHeight: 480, close() {} });
    check('a derived turn keeps the encoder', capture._encoder && capture._encoder.encoded === true,
          `orientation ${JSON.stringify(capture._orientation)}`);
    check('the derived turn is what the chunk carries', capture._orientation.rotation === 180,
          JSON.stringify(capture._orientation));
}

// A worker-only track processor on a mobile viewport is the engine that needs it.
{
    const mod = await load({ orientation: 0, metadata: false });
    const { capture } = makeCapture(mod);
    const track = { clone: () => ({ stop() {} }) };
    const source = await capture._openSource(track, capture._generation);
    check('a worker source on a mobile viewport derives', !!source && capture._deriveOrientation === true,
          `source ${!!source}, derive ${capture._deriveOrientation}`);
}

// A desktop window has no orientation to derive from, whatever its source is.
{
    const mod = await load({ orientation: undefined, metadata: false });
    const { capture } = makeCapture(mod);
    const source = await capture._openSource({ clone: () => ({ stop() {} }) }, capture._generation);
    check('a desktop window never derives', !!source && capture._deriveOrientation === false,
          `derive ${capture._deriveOrientation}`);
}

// An engine that puts the transform on the frame is read, never second-guessed.
{
    const mod = await load({ orientation: 90, metadata: true });
    const { capture } = makeCapture(mod);
    const source = await capture._openSource({ clone: () => ({ stop() {} }) }, capture._generation);
    check('frame metadata is not derived over', !!source && capture._deriveOrientation === false,
          `derive ${capture._deriveOrientation}`);
    const got = capture._frameOrientation({ rotation: 270, flip: true });
    check('frame metadata is relayed as it stands', got.rotation === 270 && got.flip === true,
          JSON.stringify(got));
}

// --- what the chunk carries -------------------------------------------------
{
    const mod = await load({ orientation: 90, metadata: false });
    const { capture, sent } = makeCapture(mod);
    capture._orientation = { rotation: 180, flip: true };
    capture._onChunk({ id: mod.WEBCAM_CODEC_H264 },
                     { byteLength: 4, type: 'key', copyTo() {} }, capture._generation);
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
    const admitted = (gaps, fps) => {
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
    const repeat = (gap, n) => Array.from({ length: n }, () => gap);
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
