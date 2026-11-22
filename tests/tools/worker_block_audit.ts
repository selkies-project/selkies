/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The gate the client's worker fallbacks take (`lib/worker-block.ts`) and the
 * clipboard bridge behind it (`lib/clipboard-worker-bridge.ts`).
 *
 * The gate opens on an enforced `worker-src` violation of a `blob:` or `data:`
 * URL alone: not on a report-only policy, another directive, or another
 * scheme. The bridge keeps every path it had while the gate is shut: its
 * buffers are handed to the worker, and a worker whose own script fails is
 * left as it is, its request unanswered as before. With the gate open, a
 * bridge made afterwards never starts a worker and answers on the page, and a
 * clipboard worker the policy refuses first rejects what it was asked and
 * answers the next request on the page, a multipart download included,
 * whichever of the violation and the worker's `error` event comes first.
 *
 * The bridge is loaded in process with the worker module Vite inlines
 * replaced by a stand-in that runs the real codec (`lib/clipboard-codec.ts`).
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { registerHooks } from 'node:module';

import type { WorkerReply } from '../../web/selkies-web-core/lib/clipboard-replies.ts';
import type { ClipboardWorkerBridge } from '../../web/selkies-web-core/lib/clipboard-worker-bridge.ts';
import { freshModule, standIn, type StandIn } from './stand_in.ts';

type BlockModule = typeof import('../../web/selkies-web-core/lib/worker-block.ts');
type BridgeModule = typeof import('../../web/selkies-web-core/lib/clipboard-worker-bridge.ts');
type CodecModule = typeof import('../../web/selkies-web-core/lib/clipboard-codec.ts');

const LIB = new URL('../../web/selkies-web-core/lib/', import.meta.url);
const FACTORY = 'clipboardWorkerStandIn';

registerHooks({
    resolve(specifier, context, nextResolve) {
        // A bridge imported under a query takes a gate of its own.
        const query = context.parentURL?.split('?')[1];
        if (specifier === './worker-block.ts' && query) return nextResolve(`${specifier}?${query}`, context);
        if (specifier.endsWith('?worker&inline')) {
            const source = `export default class { constructor() { return globalThis.${FACTORY}(); } }`;
            return { url: `data:text/javascript,${encodeURIComponent(source)}`, shortCircuit: true };
        }
        return nextResolve(specifier, context);
    },
});

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [worker-block] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** The violation fields the gate reads. */
type Violation = Pick<SecurityPolicyViolationEvent, 'disposition' | 'effectiveDirective' | 'blockedURI'>;

/** A document that takes the gate's listener and hands it violations. */
class StandInDocument {
    listeners: ((e: Violation) => void)[] = [];
    addEventListener(type: string, fn: (e: Violation) => void): void {
        if (type === 'securitypolicyviolation') this.listeners.push(fn);
    }
    violate(e: Violation): void {
        for (const fn of this.listeners) fn(e);
    }
}

let variant = 0;

/**
 * A fresh gate bound to `doc`.
 * @param doc The document it listens on.
 * @returns The module.
 */
async function freshGate(doc: StandInDocument): Promise<BlockModule> {
    Reflect.set(globalThis, 'document', doc);
    return freshModule<BlockModule>(await import(new URL(`worker-block.ts?v=${variant++}`, LIB).href), ['workersBlocked']);
}

const REFUSED: Violation = { disposition: 'enforce', effectiveDirective: 'worker-src', blockedURI: 'blob' };

{
    const cases: [string, Violation, boolean][] = [
        ["an enforced worker-src refusal of a blob: worker opens the gate", REFUSED, true],
        ["an enforced worker-src refusal of a data: worker opens the gate", { ...REFUSED, blockedURI: 'data' }, true],
        ['a report-only policy, which refuses nothing, leaves it shut', { ...REFUSED, disposition: 'report' }, false],
        ['a refusal under another directive leaves it shut', { ...REFUSED, effectiveDirective: 'img-src' }, false],
        ['a refused worker from another origin leaves it shut', { ...REFUSED, blockedURI: 'https://example.com/w.js' }, false],
    ];
    for (const [label, violation, opens] of cases) {
        const doc = new StandInDocument();
        const gate = await freshGate(doc);
        const before = gate.workersBlocked();
        doc.violate(violation);
        check(label, !before && gate.workersBlocked() === opens, { before, after: gate.workersBlocked() });
    }
    Reflect.deleteProperty(globalThis, 'document');
    const gate = freshModule<BlockModule>(await import(new URL(`worker-block.ts?v=${variant++}`, LIB).href), ['workersBlocked']);
    check('without a document the gate loads shut', !gate.workersBlocked());
}

/**
 * A bridge class with a gate of its own on `on`.
 * @param on The document its gate listens on.
 * @param tag Tells its module apart.
 * @returns The class.
 */
async function bridgeOn(on: StandInDocument, tag: string): Promise<BridgeModule['ClipboardWorkerBridge']> {
    Reflect.set(globalThis, 'document', on);
    return freshModule<BridgeModule>(await import(new URL(`clipboard-worker-bridge.ts?${tag}`, LIB).href), ['ClipboardWorkerBridge']).ClipboardWorkerBridge;
}

const doc = new StandInDocument();
const lateDoc = new StandInDocument();
const Bridge = await bridgeOn(doc, 'ordered');
const LateBridge = await bridgeOn(lateDoc, 'reversed');
/** The document the stand-in workers report their refusal to. */
let refusing = doc;
const { createClipboardCodec } = freshModule<CodecModule>(await import(new URL('clipboard-codec.ts', LIB).href), ['createClipboardCodec']);

/** How a stand-in clipboard worker behaves. */
type Behavior = 'answers' | 'faults' | 'refused' | 'refusedLate';

/** One message the bridge posted to its worker. */
interface Posted {
    action: string;
    transferred: number;
}

/** The workers the bridge started, newest last. */
const started: StandInWorker[] = [];
let behavior: Behavior = 'answers';

/**
 * A clipboard worker: runs the real codec, fails as a worker whose own script
 * throws does (its `error` event, no violation), or is refused by the page's
 * policy (a violation, then its `error` event, or the two the other way).
 */
class StandInWorker implements StandIn<Worker> {
    onmessage: ((e: StandIn<MessageEvent<WorkerReply>>) => void) | null = null;
    onerror: ((e: StandIn<ErrorEvent>) => void) | null = null;
    posted: Posted[] = [];
    terminated = false;
    private readonly behavior: Behavior;
    private readonly codec = createClipboardCodec((reply) => {
        setTimeout(() => { if (this.onmessage) this.onmessage({ data: reply }); }, 0);
    });
    constructor(behavior: Behavior) {
        this.behavior = behavior;
        if (behavior === 'faults') setTimeout(() => { if (this.onerror) this.onerror({ message: 'Uncaught Error: stand-in' }); }, 0);
        if (behavior === 'refused') {
            setTimeout(() => {
                refusing.violate(REFUSED);
                if (this.onerror) this.onerror({});
            }, 0);
        }
        if (behavior === 'refusedLate') {
            setTimeout(() => {
                if (this.onerror) this.onerror({});
                refusing.violate(REFUSED);
            }, 0);
        }
    }
    postMessage(message: unknown, transfer?: Transferable[] | StructuredSerializeOptions): void {
        if (!isRequest(message)) throw new Error('the bridge posted no request');
        const handed = Array.isArray(transfer) ? transfer : transfer?.transfer ?? [];
        this.posted.push({ action: message.action, transferred: handed.length });
        if (this.behavior === 'answers') this.codec(message);
    }
    terminate(): void {
        this.terminated = true;
    }
}

/** Whether `m` is a codec request. */
function isRequest(m: unknown): m is Parameters<ReturnType<typeof createClipboardCodec>>[0] {
    return typeof m === 'object' && m !== null && 'id' in m && 'action' in m && typeof m.action === 'string';
}

Reflect.set(globalThis, FACTORY, (): Worker => {
    const w = new StandInWorker(behavior);
    started.push(w);
    return standIn<Worker>(w);
});

/** Lets the stand-in's timers run. */
const ticks = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 20));

/** How `p` stands after the stand-in's timers ran: its value, its error, or `pending`. */
async function outcome<T>(p: Promise<T>): Promise<{ state: string; value?: T; error?: string }> {
    const result = await Promise.race([
        p.then((value) => ({ state: 'resolved', value }), (e: unknown) => ({ state: 'rejected', error: e instanceof Error ? e.message : String(e) })),
        ticks().then(() => ({ state: 'pending' })),
    ]);
    return result;
}

const bytes = (): ArrayBuffer => new TextEncoder().encode('hi there').buffer;
const B64 = btoa('hi there');

let workerDigest: number | undefined;
{
    behavior = 'answers';
    const bridge: ClipboardWorkerBridge = new Bridge();
    const got = await outcome(bridge.encodeBinary(bytes(), undefined));
    const w = started.at(-1);
    workerDigest = got.value?.hash;
    check('with workers allowed, a buffer is handed to the worker and its answer comes back',
        started.length === 1 && w?.posted[0]?.transferred === 1 && got.value?.result === B64 && bridge.onPage === null,
        { started: started.length, posted: w?.posted, got });
}

{
    behavior = 'faults';
    const count = started.length;
    const bridge: ClipboardWorkerBridge = new Bridge();
    const asked = outcome(bridge.encodeText('hi there'));
    await ticks();
    const got = await asked;
    const w = started.at(-1);
    check("with workers allowed, a worker whose own script fails is kept and nothing moves to the page",
        started.length === count + 1 && bridge.worker !== null && w?.terminated === false && bridge.onPage === null && got.state === 'pending',
        { got, terminated: w?.terminated, onPage: bridge.onPage !== null });
}

{
    behavior = 'refused';
    const bridge: ClipboardWorkerBridge = new Bridge();
    const first = await outcome(bridge.encodeText('hi there'));
    const w = started.at(-1);
    check('a clipboard worker the policy refuses rejects what it was asked',
        first.state === 'rejected' && w?.terminated === true && bridge.worker === null && bridge.onPage !== null, { first, terminated: w?.terminated });
    const next = await outcome(bridge.encodeBinary(bytes(), undefined));
    check("and answers the next request on the page as the worker would",
        next.value?.result === B64 && next.value.hash === workerDigest, { next, workerDigest });
}

{
    const count = started.length;
    const bridge: ClipboardWorkerBridge = new Bridge();
    const encoded = await outcome(bridge.encodeBinary(bytes(), undefined));
    const stream = bridge.decodeStream('text/plain');
    for (const part of [B64.slice(0, 3), B64.slice(3, 7), B64.slice(7)]) stream.push(part);
    const decoded = await outcome(stream.finish());
    const whole = await outcome(bridge.decode(B64, 'application/octet-stream'));
    check('once the policy refused a worker, a new bridge starts none and answers on the page',
        started.length === count && bridge.worker === null && encoded.value?.result === B64, { started: started.length - count, encoded });
    check('a multipart download split anywhere decodes whole on the page', decoded.value?.result === 'hi there', decoded);
    check('a single decode on the page answers with the bytes and their digest',
        whole.value?.result instanceof ArrayBuffer && whole.value.byteLength === 8 && whole.value.hash === workerDigest,
        { byteLength: whole.value?.byteLength, hash: whole.value?.hash });
}

{
    behavior = 'refusedLate';
    refusing = lateDoc;
    const bridge: ClipboardWorkerBridge = new LateBridge();
    const first = await outcome(bridge.encodeText('hi there'));
    const next = await outcome(bridge.encodeText('hi there'));
    check("a refusal reported after the worker's error event is answered the same",
        first.state === 'rejected' && bridge.worker === null && next.value?.result === B64, { first, next });
}

console.log(`[worker-block] ${failed ? `${failed} failed` : 'all passed'}`);
process.exit(failed ? 1 : 0);
