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
 * @module
 */
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { createStripeClock } from '../../addons/selkies-web-core/lib/stripe-clock.js';

const TOOLS = dirname(fileURLToPath(import.meta.url));
const WEB = join(TOOLS, '..', '..', 'addons', 'selkies-web-core');
const CORE = join(WEB, 'selkies-ws-core.js');
const WEBCAM = join(WEB, 'lib', 'webcam-capture.js');

let passed = 0, failed = 0;
const check = (label, ok, detail = '') => {
    (ok ? passed++ : failed++);
    console.log(`${ok ? 'PASS' : 'FAIL'}  [track-generator] ${label}  ${detail}`);
};

/** Where `name` is declared in `text`: a `const` or a function, exported or
 *  not, with the offset of its value and of the declaration itself. */
function declarationAt(text, name) {
    const m = new RegExp(`(?:export\\s+)?(?:const|function)\\s+${name}\\s*(=\\s*|\\()`).exec(text);
    if (!m) throw new Error(`${name} not found`);
    const from = m.index + (m[0].startsWith('export ') ? 'export '.length : 0);
    return { from, value: m[1].startsWith('=') ? m.index + m[0].length : from };
}

/** The declaration of `name`, to the brace that closes it, so a helper the
 *  page splices into a worker comes over whole and stays a valid expression. */
function declaration(text, name) {
    const { from } = declarationAt(text, name);
    let depth = 0, seen = false;
    for (let i = from; i < text.length; i++) {
        const c = text[i];
        if (c === '{') { depth++; seen = true; }
        else if (c === '}') { depth--; if (seen && depth === 0) return text.slice(from, i + 1); }
    }
    throw new Error(`${name} is unterminated`);
}

/** The value `name` is declared with, up to the semicolon that ends it. */
function literal(text, name) {
    const { value } = declarationAt(text, name);
    let depth = 0;
    for (let i = value; i < text.length; i++) {
        const c = text[i];
        if ('{[('.includes(c)) depth++;
        else if (')]}'.includes(c)) depth--;
        else if (c === ';' && depth === 0) return text.slice(value, i);
    }
    throw new Error(`${name} is unterminated`);
}

/** Helpers a worker source splices in that its own module imports. */
const IMPORTED = { createStripeClock };

/** Resolves one `${...}` the client would have interpolated. */
function splice(text, token) {
    let m = token.match(/^(\w+)\.toString\(\)$/);
    if (m) {
        return IMPORTED[m[1]]
            ? IMPORTED[m[1]].toString()
            : declaration(text, m[1]).replace(/^const \S+ = /, '');
    }
    // The encoder candidate table is spliced empty: the track path is what is
    // under test here, and the ladder that reads it has coverage of its own.
    if (/^JSON\.stringify\(\w+\)$/.test(token)) return '[]';
    if (/^\w+$/.test(token)) return literal(text, token);
    throw new Error(`no rule for \${${token}}`);
}

/**
 * The named worker source out of `text`, spliced the way the client splices it
 * before handing it to `new Worker`: every value it interpolates is read back
 * out of the same module, and the template's own escapes are resolved by
 * evaluating it as one.
 */
function workerSource(text, name) {
    const open = text.indexOf(`const ${name} = \``);
    if (open < 0) throw new Error(`${name} not found`);
    const start = text.indexOf('`', open) + 1;
    // The first backtick an even number of backslashes deep closes it; the
    // ones the worker code uses itself are escaped.
    let end = -1;
    for (let i = start; i < text.length; i++) {
        if (text[i] !== '`') continue;
        let slashes = 0;
        while (text[i - 1 - slashes] === '\\') slashes++;
        if (slashes % 2 === 0) { end = i; break; }
    }
    if (end < 0) throw new Error(`${name} is unterminated`);
    let body = text.slice(start, end);
    // Resolved against the module minus this template: the worker source
    // declares the same names it interpolates, and a search over the whole
    // file would answer with the placeholder instead of the value.
    const outside = text.slice(0, open) + text.slice(end);
    const values = [];
    body = body.replace(/(^|[^\\])\$\{([^{}]*(?:\{[^{}]*\}[^{}]*)*)\}/g, (whole, lead, token) => {
        values.push(splice(outside, token));
        return `${lead}__SPLICE${values.length - 1}__`;
    });
    // The module holds this as a template literal, so its `\$` and backtick
    // escapes are the outer template's; evaluating one resolves them exactly.
    body = new Function('return `' + body + '`;')();
    return values.reduce((out, value, i) => out.replace(`__SPLICE${i}__`, () => value), body);
}

/** An IDL-faithful `VideoTrackGenerator`: no constructor arguments, a
 *  `WritableStream` and a `MediaStreamTrack`, and nothing else. */
function generatorStub(state) {
    return class VideoTrackGenerator {
        constructor(...args) {
            if (args.length) throw new TypeError('VideoTrackGenerator takes no arguments');
            state.constructed = true;
            this.writable = new WritableStream({ write: () => {} });
            this.track = { kind: 'video', __mediaStreamTrack: true };
            this.muted = false;
        }
        get readable() { state.readReadable = true; return undefined; }
    };
}

/** Evaluates the worker source with a dedicated worker's globals; returns what
 *  it posted and whether it built a generator. */
function runWorker({ withGenerator }) {
    const state = { constructed: false, readReadable: false, posts: [] };
    const scope = {
        VideoDecoder: class { constructor() {} },
        VideoEncoder: class {},
        VideoFrame: class { constructor() {} close() {} },
        OffscreenCanvas: class { constructor() {} getContext() { return null; } },
        ImageDecoder: class {},
        WritableStream, ReadableStream, MessageChannel,
        createImageBitmap: () => Promise.resolve({}),
        performance, setInterval: () => 0, clearInterval: () => {}, setTimeout, clearTimeout,
        console,
    };
    if (withGenerator) scope.VideoTrackGenerator = generatorStub(state);
    const self = {
        postMessage: (msg, transfer) => state.posts.push({ msg, transfer }),
        onmessage: null,
    };
    Object.assign(self, scope);
    scope.self = self;
    vm.createContext(scope);
    new vm.Script(workerSource(readFileSync(CORE, 'utf8'), 'VIDEO_WORKER_SRC'),
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
check('the striped and jpeg capabilities ride the same reply',
      !!(mode && mode.msg.stripedDecode === true && 'jpegDecode' in mode.msg),
      mode && [mode.msg.stripedDecode, mode.msg.jpegDecode]);
check('the worker is left listening', typeof withGen.onmessage === 'function', '');

const without = runWorker({ withGenerator: false });
const canvasMode = without.posts.find((p) => p.msg && p.msg.type === 'mode');
check('an engine without one is told to send a canvas',
      canvasMode && canvasMode.msg.mode === 'canvas', canvasMode && canvasMode.msg.mode);
check('and nothing is transferred with it',
      canvasMode && !canvasMode.transfer, canvasMode && canvasMode.transfer);

/** An IDL-faithful `MediaStreamTrackProcessor`: a dictionary carrying a video
 *  track, and a `ReadableStream` of frames. */
function processorStub(state) {
    return class MediaStreamTrackProcessor {
        constructor(init) {
            if (!init || typeof init !== 'object' || !init.track) {
                throw new TypeError('MediaStreamTrackProcessor takes { track }');
            }
            state.processorTrack = init.track;
            this.readable = new ReadableStream({ pull() { /* never resolves */ } });
        }
    };
}

/** Runs the webcam's encode worker and hands it a transferred camera track. */
function runEncodeWorker({ withProcessor }) {
    const state = { posts: [], processorTrack: null };
    const scope = {
        VideoEncoder: class { constructor() {} static isConfigSupported() { return Promise.resolve({ supported: false }); } },
        VideoFrame: class { constructor() {} close() {} },
        OffscreenCanvas: class { constructor() {} getContext() { return null; } },
        ImageEncoder: class {}, ReadableStream, WritableStream,
        createImageBitmap: () => Promise.resolve({}),
        performance, setTimeout, clearTimeout, setInterval: () => 0, clearInterval: () => {},
        console,
    };
    if (withProcessor) scope.MediaStreamTrackProcessor = processorStub(state);
    const self = { postMessage: (msg) => state.posts.push(msg), onmessage: null };
    Object.assign(self, scope);
    scope.self = self;
    vm.createContext(scope);
    new vm.Script(workerSource(readFileSync(WEBCAM, 'utf8'), 'ENCODE_WORKER_SRC'),
                  { filename: 'encode-worker.js' }).runInContext(scope);
    const track = { kind: 'video', __mediaStreamTrack: true };
    self.onmessage({ data: { type: 'track', track } });
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
