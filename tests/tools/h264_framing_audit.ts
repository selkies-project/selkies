/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * How `h264FramingReady` settles the H.264 bitstream framing (`annexb` vs
 * `avcc`), both when the blob worker answers and when a page policy such as
 * `worker-src 'none'` refuses the worker synchronously or asynchronously.
 *
 * WebKit refuses Annex B key frames without an `avcC` description and needs
 * `avcc` framing (`selkies-ws-core.ts`), so a blocked blob worker must fall
 * back to running the 16x16 `VideoDecoder` probe on the page rather than
 * assuming `annexb`.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { type StandIn, freshModule } from './stand_in.ts';

type UtilModule = typeof import('../../web/selkies-web-core/lib/util.ts');

const MODULE = new URL('../../web/selkies-web-core/lib/util.ts', import.meta.url).href;

let passed = 0;
let failed = 0;
let variant = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (ok) passed++;
    else failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [h264-framing] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** How the stand-in worker and `VideoDecoder` behave in one scenario. */
interface Scenario {
    /** How the blob worker behaves: answers with `annexb`/`avcc`, throws from `new Worker()`, or fires `onerror` asynchronously. */
    worker: 'annexb' | 'avcc' | 'throw' | 'onerror';
    /** Whether the page's `VideoDecoder` decodes the 16x16 Annex B sample (`true`), refuses it (`false`), or is absent (`none`). */
    decoder: boolean | 'none';
}

/**
 * Loads a fresh instance of `lib/util.ts` against the given worker and
 * `VideoDecoder` stand-ins and returns its settled `h264FramingReady` answer,
 * `h264Framing()` snapshot, and how many times the page's `VideoDecoder` decoded
 * a chunk.
 * @param scenario The worker and decoder behavior under test.
 * @returns The settled framing answer, synchronous getter value, decoder call count, and revoked blob URLs.
 */
async function runScenario(scenario: Scenario): Promise<{ ready: string; current: string; pageDecodes: number; revoked: number }> {
    let pageDecodes = 0;
    let revoked = 0;

    if (scenario.decoder === 'none') {
        Reflect.deleteProperty(globalThis, 'VideoDecoder');
        Reflect.deleteProperty(globalThis, 'EncodedVideoChunk');
    } else {
        const acceptsAnnexB = scenario.decoder;
        class FakeEncodedVideoChunk {
            type: string;
            timestamp: number;
            byteLength: number;
            constructor(init: { type: string; timestamp: number; data: BufferSource }) {
                this.type = init.type;
                this.timestamp = init.timestamp;
                this.byteLength = init.data.byteLength;
            }
        }
        class FakeVideoDecoder {
            state: CodecState = 'unconfigured';
            private readonly _output: (frame: { close: () => void }) => void;
            private readonly _error: (err: DOMException) => void;
            constructor(init: { output: (frame: { close: () => void }) => void; error: (err: DOMException) => void }) {
                this._output = init.output;
                this._error = init.error;
            }
            static isConfigSupported(config: VideoDecoderConfig): Promise<VideoDecoderSupport> {
                return Promise.resolve({ supported: true, config });
            }
            configure(): void {
                this.state = 'configured';
            }
            decode(): void {
                pageDecodes++;
                if (acceptsAnnexB) {
                    this._output({ close() {} });
                } else {
                    this._error(new DOMException('Annex B not supported without description', 'EncodingError'));
                }
            }
            flush(): Promise<void> {
                return acceptsAnnexB ? Promise.resolve() : Promise.reject(new DOMException('flush failed', 'EncodingError'));
            }
            close(): void {
                this.state = 'closed';
            }
        }
        Object.assign(globalThis, {
            VideoDecoder: FakeVideoDecoder,
            EncodedVideoChunk: FakeEncodedVideoChunk,
        });
    }

    class ScenarioWorker implements StandIn<Worker> {
        onmessage: ((ev: StandIn<MessageEvent>) => void) | null = null;
        onerror: ((ev: StandIn<ErrorEvent>) => void) | null = null;
        constructor() {
            if (scenario.worker === 'throw') {
                throw new DOMException("Refused to create a worker from 'blob:audit' because it violates Content Security Policy", 'SecurityError');
            }
        }
        postMessage(): void {
            if (scenario.worker === 'onerror') {
                setTimeout(() => {
                    if (this.onerror) this.onerror({ message: 'worker blocked by CSP' });
                }, 0);
                return;
            }
            const reply = scenario.worker;
            setTimeout(() => {
                if (this.onmessage) this.onmessage({ data: reply });
            }, 0);
        }
        terminate(): void {}
    }

    Object.assign(globalThis, {
        Worker: ScenarioWorker,
        Blob: class Blob {},
    });
    globalThis.URL.createObjectURL = () => 'blob:h264-framing-audit';
    globalThis.URL.revokeObjectURL = () => { revoked++; };

    const mod = freshModule<UtilModule>(
        await import(`${MODULE}?variant=${variant++}`),
        ['h264FramingReady', 'h264Framing'],
    );
    const ready = await mod.h264FramingReady;
    const current = mod.h264Framing();
    return { ready, current, pageDecodes, revoked };
}

{
    const res = await runScenario({ worker: 'annexb', decoder: true });
    check('worker answering annexb settles to annexb without decoding on the page',
        res.ready === 'annexb' && res.current === 'annexb' && res.pageDecodes === 0, res);
}

{
    const res = await runScenario({ worker: 'avcc', decoder: false });
    check('worker answering avcc settles to avcc without decoding on the page',
        res.ready === 'avcc' && res.current === 'avcc' && res.pageDecodes === 0, res);
}

{
    const res = await runScenario({ worker: 'onerror', decoder: true });
    check('when the blob worker fails via onerror on an Annex B engine (Chromium/Firefox), the page probe settles to annexb',
        res.ready === 'annexb' && res.current === 'annexb' && res.pageDecodes === 1, res);
}

{
    const res = await runScenario({ worker: 'onerror', decoder: false });
    check('when the blob worker fails via onerror on an avcC-only engine (WebKit), the page probe settles to avcc',
        res.ready === 'avcc' && res.current === 'avcc' && res.pageDecodes === 1, res);
}

{
    const res = await runScenario({ worker: 'throw', decoder: false });
    check('when new Worker() throws synchronously on an avcC-only engine (WebKit), the page probe settles to avcc',
        res.ready === 'avcc' && res.current === 'avcc' && res.pageDecodes === 1, res);
}

{
    const res = await runScenario({ worker: 'throw', decoder: true });
    check('when new Worker() throws synchronously on an Annex B engine, the page probe settles to annexb',
        res.ready === 'annexb' && res.current === 'annexb' && res.pageDecodes === 1, res);
}

{
    const res = await runScenario({ worker: 'onerror', decoder: 'none' });
    check('without VideoDecoder at all, h264FramingReady settles immediately to annexb',
        res.ready === 'annexb' && res.current === 'annexb' && res.pageDecodes === 0, res);
}

console.log(`[h264-framing] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
