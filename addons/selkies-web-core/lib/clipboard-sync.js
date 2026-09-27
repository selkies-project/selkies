/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Client/server clipboard synchronization, shared by both transports.
 *
 * The pieces are factories the cores compose: `createClipboardSync` owns the
 * server-clipboard cache and the change-only signature (unchanged content
 * never re-crosses the transport in either direction), `createIncomingClipboard`
 * is the server-to-client path (reassembly through `createMultipartClipboardState`,
 * the cache, and the local write, issued when a payload is announced),
 * `createTaggedClipboardFetch` marks the connect-time cache-only fetch,
 * `createLocalClipboardSender` is the focus-driven local-to-server path,
 * `createDeferredClipboardWriter` keeps a write the engine refused (no focus,
 * no user activation) for the next gesture, `localClipboardBlocker` names what
 * stops a local write at all, and `createClipboardGestures` wires the copy and
 * paste keystrokes. The transports differ only in the hooks they inject: how
 * a request or payload is sent and the enablement gates, which are closures
 * re-read per event so runtime settings changes apply immediately.
 * @module
 */

/**
 * @typedef {{kind: 'text', text: string}|{kind: 'image', blob: Blob, mime: string}} LocalClipboardContent
 */

/**
 * A payload the clipboard worker has already digested, standing in for its
 * bytes wherever a signature is taken.
 *
 * The worker walks every byte of a clipboard payload anyway, to decode or
 * encode it, and digests it on the way through; passing the result here is
 * what keeps the page from running the same per-byte loop over a payload of
 * any size, several times per transfer.
 * @param {number} byteLength Size of the payload.
 * @param {number} hash Its digest, seeded and computed as `hashBytes` does.
 * @returns {{__clipDigest: true, byteLength: number, hash: number}}
 */
export function digestedPayload(byteLength, hash) {
    return { __clipDigest: true, byteLength, hash };
}

/**
 * Re-encodes a raster blob as PNG.
 *
 * Chromium's async clipboard accepts only `image/png` on write, but a source
 * may offer only JPEG, BMP, or WebP, so the blob is decoded with the browser's
 * own decoders and re-encoded first.
 * @param {Blob} blob The image.
 * @returns {Promise<Blob>} The PNG.
 * @throws When the blob is undecodable (a dimensionless SVG) or the encode fails.
 */
export async function reencodeBlobAsPng(blob) {
    const bmp = await createImageBitmap(blob);
    try {
        const canvas = document.createElement('canvas');
        canvas.width = bmp.width;
        canvas.height = bmp.height;
        canvas.getContext('2d').drawImage(bmp, 0, 0);
        return await new Promise((resolve, reject) =>
            canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('PNG encode failed'))), 'image/png'));
    } finally {
        bmp.close();
    }
}

/**
 * Why the browser cannot be asked to write the local clipboard, or `null`
 * when it can.
 *
 * Both engines expose `navigator.clipboard` in a secure context only, so a
 * deployment served over http:// on anything but localhost has no clipboard
 * API at all: a server image then lands nowhere, and saying so is the
 * difference between a bug report and a certificate.
 * @returns {string|null} The reason, ready to show.
 */
export function localClipboardBlocker() {
    if (typeof navigator !== 'undefined' && navigator.clipboard) return null;
    return (typeof window !== 'undefined' && window.isSecureContext === false)
        ? 'this page is not a secure context, so the browser exposes no clipboard (serve it over https, or from localhost)'
        : 'this browser exposes no clipboard API';
}

/**
 * Wire type for a copy carrying more than one flavour: the markup and the plain
 * text its source wrote for it, as a JSON object of mime to content. It never
 * reaches a clipboard itself; the flavours inside it do.
 */
export const CLIPBOARD_FLAVOURS_MIME = 'application/x-selkies-clipboard-flavours';

/** The wire payload for one copy's flavours. */
export function packClipboardFlavours({ html, text }) {
    const flavours = { 'text/html': html };
    if (text) flavours['text/plain'] = text;
    return new TextEncoder().encode(JSON.stringify(flavours)).buffer;
}

/** The flavours a wire payload carries, as `{html, text}`. */
export function unpackClipboardFlavours(bytes) {
    const flavours = JSON.parse(new TextDecoder().decode(bytes));
    return { html: flavours['text/html'] || '', text: flavours['text/plain'] || '' };
}

/**
 * The text a markup flavour reads as, for a copy whose source wrote no plain
 * text beside it: the item's types are declared before its data arrive, so it
 * carries a plain flavour either way, and an empty one would paste nothing
 * into a plain field.
 * @param {string} html The markup.
 * @returns {string}
 */
function textOfMarkup(html) {
    try {
        return new DOMParser().parseFromString(html, 'text/html').body.textContent || '';
    } catch (_) {
        return '';
    }
}

/**
 * Reads the local clipboard for the focus and gesture send path.
 *
 * Chromium's `read()`/`getType()` throw `DataError` on large text and some
 * images while `readText()` still returns the text, so every such failure
 * falls back to it rather than dropping the sync.
 * @param {boolean} binaryEnabled Whether images may be read.
 * @returns {Promise<LocalClipboardContent|null>} The content, or `null` when empty.
 * @throws Only genuinely unexpected errors, for the caller to log.
 */
export async function readLocalClipboard(binaryEnabled) {
    const textFallback = async () => {
        const t = await navigator.clipboard.readText().catch(() => '');
        return t ? { kind: 'text', text: t } : null;
    };
    if (!binaryEnabled) {
        const text = await navigator.clipboard.readText();
        return text ? { kind: 'text', text } : null;
    }
    let items;
    try {
        items = await navigator.clipboard.read();
    } catch (err) {
        if (err && err.name === 'DataError') return textFallback();
        throw err;
    }
    if (!items || items.length === 0) return null;
    const item = items[0];
    const imageType = item.types.find((t) => t.startsWith('image/'));
    try {
        if (imageType) {
            const blob = await item.getType(imageType);
            return { kind: 'image', blob, mime: imageType };
        }
        if (item.types.includes('text/html')) {
            const html = await (await item.getType('text/html')).text();
            const text = item.types.includes('text/plain')
                ? await (await item.getType('text/plain')).text() : '';
            if (html) return { kind: 'flavours', html, text };
        }
        if (item.types.includes('text/plain')) {
            const blob = await item.getType('text/plain');
            const text = await blob.text();
            return text ? { kind: 'text', text } : null;
        }
    } catch (err) {
        if (err && err.name === 'DataError') return textFallback();
        throw err;
    }
    return null;
}

/**
 * @typedef {object} MultipartClipboardState
 * @property {(mime: string, total: number) => void} begin Arms a transfer.
 * @property {(b64: string) => void} push Hands one base64 chunk to the worker.
 * @property {() => Promise<{result: *, mimeType: string, byteLength: number}>} finish
 *     Resolves with the decoded payload and resets; rejects when no transfer
 *     is in progress.
 * @property {() => void} reset Drops the transfer.
 * @property {boolean} inProgress
 * @property {string|null} mimeType
 * @property {number} totalSize Declared size in bytes.
 * @property {number} receivedSize Decoded bytes accumulated so far.
 */

/**
 * Multipart server-to-client clipboard download state.
 *
 * Chunks go straight to the worker as they arrive and the decoded byte count
 * is tracked from their base64 lengths, so a multi-MB clipboard is never held
 * on the main thread, let alone joined into one string there and copied again
 * into the message carrying it off. A truncated stream must never be
 * delivered as content: callers compare `receivedSize` against `totalSize`
 * before finishing, or the decoded byte length after, and discard on a
 * mismatch.
 * @param {(mime: string) => {push: (b64: string) => void, finish: () => Promise<*>, abort: () => void}} openStream
 *     Opens the worker-side accumulation for one transfer.
 * @returns {MultipartClipboardState}
 */
export function createMultipartClipboardState(openStream) {
    let stream = null;
    let mimeType = null;
    let totalSize = 0;
    let receivedSize = 0;
    let inProgress = false;

    function base64DecodedSize(b64) {
        if (!b64) return 0;
        const pad = b64.endsWith('==') ? 2 : (b64.endsWith('=') ? 1 : 0);
        return (b64.length / 4) * 3 - pad;
    }

    function clear() {
        stream = null;
        mimeType = null;
        totalSize = 0;
        receivedSize = 0;
        inProgress = false;
    }

    return {
        begin(mime, total) {
            this.reset();
            mimeType = mime;
            totalSize = total;
            stream = openStream(mime);
            inProgress = true;
        },
        push(b64) {
            if (!inProgress) return;
            stream.push(b64);
            receivedSize += base64DecodedSize(b64);
        },
        finish() {
            if (!inProgress) return Promise.reject(new Error('no transfer in progress'));
            const pending = stream.finish();
            clear();
            return pending;
        },
        reset() {
            if (stream) stream.abort();
            clear();
        },
        get inProgress() { return inProgress; },
        get mimeType() { return mimeType; },
        get totalSize() { return totalSize; },
        get receivedSize() { return receivedSize; },
    };
}

/**
 * @typedef {object} TaggedClipboardFetch
 * @property {() => void} arm Records that the server tags the reply.
 * @property {(ms: number) => void} armLegacyWindow Starts the timed fallback
 *     after sending `cr`.
 * @property {() => boolean} consume Whether the next payload is the fetch reply.
 */

/**
 * Tracker for the connect-time cache-only clipboard fetch (`cr`).
 *
 * The reply must populate the sync cache and preview but never be written to
 * the local clipboard, which would clobber whatever the user copied just
 * before connecting. A tagging server marks the answering payload
 * deterministically; for a server that never tags, a short timed window
 * stands in, so a dropped reply cannot swallow a later genuine push.
 * @returns {TaggedClipboardFetch}
 */
export function createTaggedClipboardFetch() {
    let deadline = 0;
    let serverTags = false;
    let pending = false;
    return {
        arm() {
            serverTags = true;
            pending = true;
            deadline = 0;
        },
        armLegacyWindow(ms) {
            deadline = Date.now() + ms;
        },
        consume() {
            if (pending) {
                pending = false;
                return true;
            }
            if (serverTags) return false;
            if (!deadline) return false;
            const isInit = Date.now() < deadline;
            deadline = 0;
            return isInit;
        },
    };
}

/**
 * How long a push the user asked for keeps precedence over a focus read once
 * it has settled: long enough to cover the refocus the file picker raises as
 * it closes, far too short to contain a trip to another window and back.
 */
const EXPLICIT_PRECEDENCE_MS = 1000;

/** Longest a focus read waits for a server value still landing locally. */
const SERVER_WRITE_WAIT_MS = 10000;

/**
 * Longest a server payload may go without a chunk before it is dropped: well
 * past the server's own per-chunk bound, after which it leaves a client out
 * of the rest of a payload.
 */
const INCOMING_STALL_MS = 30000;

/**
 * @typedef {object} LocalClipboardSender
 * @property {() => Promise<void>} readAndSend Reads the local clipboard and
 *     pushes any content to the server.
 * @property {(data: string|ArrayBuffer|Blob, mime?: string, onSkip?: Function) => Promise<void>} sendExplicit
 *     Pushes content the user named, outranking a concurrent `readAndSend`.
 * @property {() => Promise<void>} maybeInitial The connect-time one-shot send.
 * @property {() => (Promise<void>|null)} getSendInFlight The send the
 *     paste-ordering hold awaits, or `null`.
 */

/**
 * Focus and gesture driven local-to-server clipboard sync.
 *
 * `readAndSend` is serialized so the paste-ordering hold can hold Ctrl/Cmd+V
 * until the send settles. `maybeInitial` covers a focused Chromium tab, which
 * gets no `focus` event after connect and would otherwise leave the server on
 * its stale clipboard until the first alt-tab; it runs only when clipboard
 * read is already granted, since it must never raise a prompt at load.
 *
 * `sendExplicit` carries the dashboard's own pushes -- the clipboard box and
 * the image upload -- and outranks a read while it runs and briefly after:
 * choosing a file blurs the page and refocuses it, and the read that refocus
 * fires would put the local clipboard straight back over the upload. It reads
 * a Blob itself, so the push is on record from the call rather than from
 * whenever its bytes arrive; a copy made later reaches the session on the next
 * focus or paste.
 * @param {object} hooks
 * @param {boolean} hooks.isChromium Engine flag.
 * @param {() => boolean} hooks.isSharedMode Viewer sessions never send.
 * @param {() => boolean} hooks.canSync Clipboard sync enabled.
 * @param {() => boolean} hooks.canRead Local-to-server direction enabled.
 * @param {() => boolean} hooks.binaryEnabled Whether images are sent.
 * @param {(data: string|ArrayBuffer, mime?: string, onSkip?: Function) => Promise<void>} hooks.sendClipboardData
 *     Transport send.
 * @param {boolean} [hooks.dedupeText] Suppresses re-sending unchanged text;
 *     the WebRTC core's behavior, while the WebSocket core sends per event
 *     and dedupes at the server.
 * @param {(() => (Promise<*>|null))|null} [hooks.getDeferredWriteInFlight]
 *     The deferred writer's pending write, awaited before reading.
 * @returns {LocalClipboardSender}
 */
export function createLocalClipboardSender({
    isChromium,
    isSharedMode,
    canSync,
    canRead,
    binaryEnabled,
    sendClipboardData,
    dedupeText = false,
    getDeferredWriteInFlight = null,
}) {
    let sendInFlight = null;
    let lastText = null;
    let initialAttempted = false;
    let explicitRunning = 0;
    let explicitSettledAt = -Infinity;

    /** Whether a push the user asked for is still the session's latest word. */
    function explicitHasPrecedence() {
        return explicitRunning > 0
            || Date.now() - explicitSettledAt < EXPLICIT_PRECEDENCE_MS;
    }

    /** Runs `work` as the send the paste-ordering hold waits on. */
    async function asSendInFlight(work) {
        let settle;
        const tracker = new Promise((resolve) => { settle = resolve; });
        sendInFlight = tracker;
        try {
            await work;
        } finally {
            settle();
            if (sendInFlight === tracker) sendInFlight = null;
        }
    }

    /**
     * A server push still settling through the deferred writer must land
     * before this read: reading around it returns the pre-push content,
     * which then reads as a change and bounces the stale value back to the
     * server. The wait is bounded, since a push whose bytes stopped arriving
     * never settles.
     */
    async function readAndSend() {
        // navigator.clipboard is undefined on insecure origins.
        if (!window.isSecureContext || !navigator.clipboard) return;
        if (isSharedMode() || !canSync() || !canRead()) return;

        if (getDeferredWriteInFlight) {
            const giveUpAt = Date.now() + SERVER_WRITE_WAIT_MS;
            for (let i = 0; i < 2; i++) {
                const w = getDeferredWriteInFlight();
                const remaining = giveUpAt - Date.now();
                if (!w || remaining <= 0) break;
                await Promise.race([w.catch(() => {}),
                    new Promise((r) => setTimeout(r, remaining))]);
                if (!getDeferredWriteInFlight()) break;
            }
        }

        if (explicitHasPrecedence()) return;

        const work = (async () => {
            try {
                const res = await readLocalClipboard(binaryEnabled());
                if (!res || explicitHasPrecedence()) return;
                if (res.kind === 'image') {
                    const arrayBuffer = await res.blob.arrayBuffer();
                    if (explicitHasPrecedence()) return;
                    await sendClipboardData(arrayBuffer, res.mime);
                    console.log(`Sent binary clipboard: ${res.mime}, size: ${res.blob.size} bytes`);
                } else if (res.kind === 'flavours') {
                    if (!dedupeText || res.html !== lastText) {
                        await sendClipboardData(packClipboardFlavours(res), CLIPBOARD_FLAVOURS_MIME);
                        lastText = res.html;
                        console.log(`Sent clipboard markup with its text, ${res.html.length} characters`);
                    }
                } else if (!dedupeText || res.text !== lastText) {
                    await sendClipboardData(res.text);
                    lastText = res.text;
                    console.log("Sent clipboard text to server");
                }
            } catch (err) {
                if (err.name !== 'NotFoundError' && err.name !== 'DataError' && err.name !== 'NotAllowedError'
                    && !(err.message && err.message.includes('not focused'))) {
                    console.warn(`Could not read clipboard: ${err.name} - ${err.message}`);
                }
            }
        })();
        await asSendInFlight(work);
    }

    /**
     * Pushes content the user named. A Blob is read here, so the push counts
     * from this call rather than from whenever its bytes arrive.
     */
    async function sendExplicit(data, mime, onSkip) {
        explicitRunning++;
        await asSendInFlight((async () => {
            try {
                const payload = (data && typeof data.arrayBuffer === 'function')
                    ? await data.arrayBuffer() : data;
                await sendClipboardData(payload, mime, onSkip);
            } finally {
                explicitRunning--;
                explicitSettledAt = Date.now();
            }
        })());
    }

    async function maybeInitial() {
        if (initialAttempted) return;
        initialAttempted = true;
        if (!isChromium || isSharedMode() || !document.hasFocus()) return;
        if (!navigator.permissions || !navigator.permissions.query) return;
        try {
            const st = await navigator.permissions.query({ name: 'clipboard-read' });
            if (st.state === 'granted') readAndSend();
        } catch (_) { /* permission name unsupported (non-Chromium engines) */ }
    }

    return { readAndSend, sendExplicit, maybeInitial, getSendInFlight: () => sendInFlight };
}

/**
 * @typedef {object} DeferredClipboardWriter
 * @property {(attempt: () => Promise<void>, options?: {onSuccess?: () => (Promise<*>|void), onFailure?: (err: Error) => void, settled?: Promise<*>}) => Promise<boolean>} write
 *     Runs an async clipboard write now, stashing it for the next gesture on
 *     an activation rejection. `settled`, when given, resolves once the
 *     write's payload is complete.
 * @property {() => void} flush Retries the stashed write.
 * @property {() => (Promise<boolean>|null)} getInFlight The most recent
 *     attempt, immediate or flushed, or `null`.
 * @property {() => (Promise<boolean>|null)} getLanding The most recent
 *     attempt once its payload is complete, or `null`.
 * @property {() => boolean} hasPending Whether a write is stashed or in flight.
 */

/**
 * Deferred local-clipboard writer for server pushes.
 *
 * Engines decide whether a clipboard write may happen when `write()` is
 * called: Chromium rejects it from an unfocused document, Firefox and WebKit
 * without a user activation a few seconds old at most, and a server push
 * handler holds neither once the user has moved on. On such a rejection the
 * write is stashed and retried on the next real gesture instead of being
 * lost. Only the newest write is kept, since the clipboard is
 * last-value-wins: issuing a write drops any older stash whatever becomes of
 * it, and a write refused after a newer one was issued is dropped rather than
 * stashed, so no retry can land an older value over a newer one. An attempt
 * rejected with `AbortError` was withdrawn by its caller and is neither
 * stashed nor reported. The paste-ordering hold awaits the landing attempt
 * (`getLanding`) so a server-to-client write lands before a paste reads the
 * local clipboard; otherwise the stash flushes on the paste's own keydown and
 * lands just after the read, and the first paste is one behind. The flush
 * rides keydown and pointerdown, which carry a user activation, and focus and
 * visibilitychange, which land the write the instant Chromium accepts it
 * again, well before the user's next paste.
 * @returns {DeferredClipboardWriter}
 */
export function createDeferredClipboardWriter() {
    let pending = null;
    let writeSeq = 0;
    let inFlight = null;

    function isActivationError(err) {
        return !!err && (err.name === 'NotAllowedError' || err.name === 'SecurityError');
    }

    function track(promise, w) {
        const entry = { promise, settled: !w.settled };
        if (w.settled) w.settled.then(() => { entry.settled = true; }, () => { entry.settled = true; });
        inFlight = entry;
        promise.finally(() => { if (inFlight === entry) inFlight = null; });
    }

    /**
     * Runs one write. An activation rejection (a synthetic event, or a
     * blurred tab) stashes it for the next gesture unless a newer write was
     * issued meanwhile, which the stash would otherwise land over once
     * flushed; any other error but a withdrawal reaches `onFailure`. The
     * attempt settles only after `onSuccess` has, so whoever waits on it
     * also waits for what the landing set off.
     *
     * The attempt is started inside a promise chain so that a caller passing
     * a plain expression -- `navigator.clipboard.write(...)`, which throws
     * outright where the browser exposes no clipboard -- fails the same way
     * an async one does, instead of throwing past `onFailure`.
     */
    function attemptOnce(w) {
        return Promise.resolve().then(() => w.attempt()).then(
            () => Promise.resolve(w.onSuccess && w.onSuccess()).then(() => true, () => true),
            (err) => {
                if (isActivationError(err)) {
                    if (w.seq === writeSeq) pending = w;
                    return false;
                }
                if (w.onFailure && !(err && err.name === 'AbortError')) w.onFailure(err);
                return false;
            });
    }

    function flush() {
        const w = pending;
        if (!w) return;
        pending = null;
        track(attemptOnce(w), w);
    }

    for (const type of ['pointerdown', 'keydown', 'focus']) {
        window.addEventListener(type, flush, true);
    }
    document.addEventListener('visibilitychange', () => { if (!document.hidden) flush(); }, true);

    /**
     * Runs `attempt` now; on an activation or focus rejection queues it for
     * the next gesture. `onSuccess` fires whenever the write eventually lands,
     * `onFailure` only for errors that are neither activation nor withdrawal.
     */
    function write(attempt, { onSuccess, onFailure, settled } = {}) {
        const w = { attempt, onSuccess, onFailure, settled, seq: ++writeSeq };
        pending = null;
        const p = attemptOnce(w);
        track(p, w);
        return p;
    }

    return {
        write,
        flush,
        getInFlight: () => (inFlight ? inFlight.promise : null),
        getLanding: () => (inFlight && inFlight.settled ? inFlight.promise : null),
        hasPending: () => !!pending || !!inFlight,
    };
}

/**
 * @typedef {object} IncomingClipboard
 * @property {(mime: string, total: number, cacheOnly: boolean) => void} begin
 *     Announces a multipart payload of `total` bytes.
 * @property {(b64: string) => void} push Hands one base64 chunk on.
 * @property {() => void} finish Ends the multipart payload.
 * @property {(mime: string, b64: string, cacheOnly: boolean) => void} single
 *     Takes a whole payload carried by one message.
 * @property {() => void} reset Drops a payload in progress.
 */

/**
 * The server-to-client clipboard, shared by both transports: reassembly, the
 * cache and preview, and the local write.
 *
 * The local write of a multipart payload is issued the moment the payload is
 * announced, with its bytes still crossing, as a ClipboardItem whose data are
 * Promises. Engines decide whether a write may happen when `write()` is
 * called -- Chromium wants the document focused, Firefox and WebKit a user
 * activation a few seconds old at most -- and let the data settle long
 * after, while a multi-megabyte image takes seconds to arrive: a write issued
 * only then finds the user already back in the application they copied for,
 * is refused, and lands on their next gesture, one copy behind. Whether the
 * payload repeats what the client already holds is known only once its bytes
 * are digested, so a repeat, a payload that arrived broken, and one a newer
 * payload superseded all withdraw the write by rejecting its data, which
 * leaves the local clipboard as it was. A whole payload in one message is
 * written as soon as it is decoded. Every write goes through `writer`, which
 * stashes one the engine refuses until the next gesture, and an older write
 * still waiting for its data is withdrawn when a newer one is issued, so a
 * slow conversion can never land an older value over a newer one.
 *
 * A payload superseded before it landed is dropped whole, its cache update
 * included: the newer one carries the session's clipboard, and a repeat of the
 * dropped content would otherwise read as already held and never land.
 * `cacheOnly` marks the reply to the connect-time fetch, which fills the cache
 * and preview but never the local clipboard.
 * @param {object} hooks
 * @param {{decode: Function, decodeStream: Function}} hooks.worker The
 *     clipboard worker bridge.
 * @param {ClipboardSync} hooks.clipboardSync The server-clipboard state.
 * @param {DeferredClipboardWriter} hooks.writer The local writer.
 * @param {(blob: Blob) => Promise<Blob>} hooks.toPng Off-thread PNG conversion.
 * @param {() => boolean} hooks.canWriteLocal Whether server content may reach
 *     the local clipboard now.
 * @param {() => boolean} hooks.binaryEnabled Whether images are taken.
 * @param {(text: string) => void} hooks.onPreview Shows server text to the dashboards.
 * @param {(mime: string) => (Promise<*>|void)} hooks.onImageWritten An image landed
 *     locally; what it returns is awaited before the landing counts as done.
 * @param {(err: *) => void} hooks.onImageWriteFailed An image write failed for good.
 * @returns {IncomingClipboard}
 */
export function createIncomingClipboard({
    worker, clipboardSync, writer, toPng, canWriteLocal, binaryEnabled,
    onPreview, onImageWritten, onImageWriteFailed,
}) {
    const multipart = createMultipartClipboardState((mime) => worker.decodeStream(mime));
    let current = null;
    let generation = 0;
    let lastLanding = null;
    let stallTimer = null;

    /**
     * A payload whose chunks stopped arriving is dropped, so its write settles
     * rather than holding every later paste decision on a value that is never
     * coming.
     */
    function armStallTimer() {
        clearTimeout(stallTimer);
        stallTimer = setTimeout(() => {
            if (!current) return;
            console.warn('Session clipboard transfer stalled; dropping it.');
            withdraw(current.landing, 'stalled');
            multipart.reset();
            current = null;
        }, INCOMING_STALL_MS);
    }

    function kindOf(mime) {
        if (mime === 'text/plain') return 'text';
        if (mime === CLIPBOARD_FLAVOURS_MIME) return 'flavours';
        if (typeof mime === 'string' && mime.startsWith('image/')) return 'image';
        return null;
    }

    function wanted(kind, cacheOnly) {
        return !cacheOnly && canWriteLocal() && (kind !== 'image' || binaryEnabled());
    }

    function canWriteItems() {
        return typeof ClipboardItem !== 'undefined'
            && typeof navigator !== 'undefined' && !!navigator.clipboard && !!navigator.clipboard.write;
    }

    function withdrawal(reason) {
        return new DOMException(reason, 'AbortError');
    }

    function settleable() {
        let resolve;
        let reject;
        const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
        // Rejection is how a write is withdrawn, not an error anyone must see.
        promise.catch(() => {});
        return { promise, resolve, reject };
    }

    /** Withdraws a write still waiting for its data; the local clipboard keeps its value. */
    function withdraw(landing, reason) {
        if (!landing || landing.settled || landing.withdrawn) return;
        landing.withdrawn = true;
        for (const part of Object.values(landing.parts)) part.reject(withdrawal(reason));
    }

    /**
     * Issues a local write whose data settle later: one Promise per type the
     * item carries, declared now because a ClipboardItem cannot add a type.
     */
    function issue(kind, mime) {
        withdraw(lastLanding, 'superseded');
        const types = kind === 'image' ? ['image/png']
            : kind === 'text' ? ['text/plain'] : ['text/html', 'text/plain'];
        const landing = { parts: {}, settled: false, withdrawn: false, failed: null };
        for (const type of types) landing.parts[type] = settleable();
        const settled = Promise.all(types.map((t) => landing.parts[t].promise));
        settled.then(() => { landing.settled = true; }, () => {});
        lastLanding = landing;
        writer.write(async () => {
            const item = {};
            for (const type of types) item[type] = landing.parts[type].promise;
            try {
                await navigator.clipboard.write([new ClipboardItem(item)]);
            } catch (err) {
                // Engines report data that never came under names of their own
                // choosing, an activation error's among them.
                if (landing.withdrawn) throw withdrawal('withdrawn');
                if (landing.failed) throw landing.failed;
                throw err;
            }
        }, {
            settled,
            onSuccess: kind === 'image' ? () => onImageWritten(mime) : undefined,
            onFailure: kind === 'image' ? onImageWriteFailed
                : (err) => console.error(`Could not copy the session clipboard to the local one: ${err && err.name} - ${err && err.message}`),
        });
        return landing;
    }

    /**
     * Hands a landing its data. A conversion still running when a newer write
     * withdraws this one resolves nothing: the part was rejected first.
     */
    function fulfill(landing, kind, content) {
        if (kind === 'image') {
            const part = landing.parts['image/png'];
            if (content.mime === 'image/png') {
                part.resolve(content.blob);
            } else {
                toPng(content.blob).catch(() => reencodeBlobAsPng(content.blob)).then(part.resolve, (err) => {
                    landing.failed = err;
                    part.reject(err);
                });
            }
        } else if (kind === 'text') {
            landing.parts['text/plain'].resolve(new Blob([content.text], { type: 'text/plain' }));
        } else {
            landing.parts['text/html'].resolve(new Blob([content.html], { type: 'text/html' }));
            landing.parts['text/plain'].resolve(new Blob([content.text || textOfMarkup(content.html)],
                { type: 'text/plain' }));
        }
    }

    /**
     * Caches a complete payload, shows its preview, and lands it locally
     * through the write already issued for it or a new one.
     */
    function land(t, decoded) {
        if (t.gen !== generation && t.landing && t.landing.withdrawn) return;
        let fresh;
        let content;
        if (t.kind === 'text') {
            const text = decoded.result;
            fresh = clipboardSync.shouldSend(text, 'text/plain');
            clipboardSync.resolveServer(text, null, 'text/plain');
            onPreview(text);
            content = { text };
        } else {
            const digest = digestedPayload(decoded.byteLength, decoded.hash);
            fresh = clipboardSync.shouldSend(digest, t.mime);
            if (t.kind === 'flavours') {
                const flavours = unpackClipboardFlavours(decoded.result);
                clipboardSync.resolveServer(flavours.text || flavours.html, null, t.mime, digest);
                onPreview(flavours.text || flavours.html);
                content = flavours;
            } else {
                const blob = new Blob([decoded.result], { type: t.mime });
                clipboardSync.resolveServer(undefined, blob, t.mime, digest);
                content = { blob, mime: t.mime };
            }
        }
        if (!fresh || !wanted(t.kind, t.cacheOnly)) {
            withdraw(t.landing, 'unchanged');
            return;
        }
        if (t.landing) {
            fulfill(t.landing, t.kind, content);
        } else if (canWriteItems()) {
            fulfill(issue(t.kind, t.mime), t.kind, content);
        } else if (t.kind === 'image') {
            onImageWriteFailed(new Error('the local clipboard takes no images here'));
        } else if (typeof navigator !== 'undefined' && navigator.clipboard) {
            const text = t.kind === 'text' ? content.text : (content.text || content.html);
            writer.write(() => navigator.clipboard.writeText(text), {
                onFailure: (err) => console.error(`Could not copy the session clipboard to the local one: ${err && err.name}`),
            });
        }
    }

    /** Opens a payload, superseding whatever was still arriving. */
    function open(mime, cacheOnly) {
        if (current) {
            withdraw(current.landing, 'superseded');
            multipart.reset();
            current = null;
        }
        const kind = kindOf(mime);
        if (!kind || (kind === 'image' && !binaryEnabled())) return null;
        return { kind, mime, cacheOnly, gen: ++generation, landing: null };
    }

    return {
        begin(mime, total, cacheOnly) {
            const t = open(mime, cacheOnly);
            if (!t) return;
            multipart.begin(mime, total);
            if (wanted(t.kind, cacheOnly) && canWriteItems()) t.landing = issue(t.kind, mime);
            current = t;
            armStallTimer();
        },
        push(b64) {
            if (!current) return;
            multipart.push(b64);
            armStallTimer();
        },
        finish() {
            const t = current;
            current = null;
            clearTimeout(stallTimer);
            if (!t || !multipart.inProgress) return;
            const declared = multipart.totalSize;
            if (multipart.receivedSize !== declared) {
                console.error(`Multipart clipboard size mismatch: received ${multipart.receivedSize} of ${declared} bytes.`);
                multipart.reset();
                withdraw(t.landing, 'incomplete');
                return;
            }
            multipart.finish().then((decoded) => {
                if (decoded.byteLength !== declared) {
                    withdraw(t.landing, 'incomplete');
                    return;
                }
                land(t, decoded);
            }).catch((err) => {
                withdraw(t.landing, 'undecodable');
                console.error('Error assembling final clipboard content:', err);
            });
        },
        single(mime, b64, cacheOnly) {
            const t = open(mime, cacheOnly);
            if (!t) return;
            worker.decode(b64, t.kind === 'text' ? 'text/plain' : mime).then(
                (decoded) => land(t, decoded),
                (err) => console.error('Error processing clipboard data from the session:', err));
        },
        reset() {
            if (current) withdraw(current.landing, 'dropped');
            clearTimeout(stallTimer);
            multipart.reset();
            current = null;
        },
    };
}

/** Longest server clipboard text the dashboards are shown, in characters. */
export const CLIPBOARD_PREVIEW_LIMIT = 256 * 1024;

/**
 * The `clipboardContentUpdate` message carrying server clipboard text to the
 * dashboards.
 *
 * A multi-MB payload structured-clones through `postMessage` and lands in a
 * controlled textarea, freezing the page, while the UI only needs a bounded
 * preview. The `truncated` flag tells the dashboard to render it read-only so
 * a blur cannot echo the cut-down text back over the real server clipboard.
 * @param {string} text The server clipboard text.
 * @returns {{type: string, text: string, truncated: boolean, totalLength: number}}
 */
export function clipboardPreviewMessage(text) {
    const truncated = text.length > CLIPBOARD_PREVIEW_LIMIT;
    return {
        type: 'clipboardContentUpdate',
        text: truncated ? text.slice(0, CLIPBOARD_PREVIEW_LIMIT) : text,
        truncated,
        totalLength: text.length,
    };
}

/**
 * @typedef {object} ClipboardSync
 * @property {(data: string|Uint8Array|ArrayBuffer|Blob, mime?: string) => string} sig
 *     Content signature.
 * @property {(data: string|Uint8Array|ArrayBuffer|Blob, mime?: string) => boolean} shouldSend
 *     Change-only gate.
 * @property {(data: string|Uint8Array|ArrayBuffer|Blob, mime?: string) => void} markSynced
 *     Records content as synced, on transfer success.
 * @property {(text?: string, blob?: Blob, mime?: string, bytes?: Uint8Array) => void} resolveServer
 *     Caches fresh server data and settles pending requests.
 * @property {() => Promise<void>} captureLocalImageSig Records the browser's
 *     re-encoded form of the image just written locally, digesting it through
 *     the worker where the caller supplied one.
 * @property {(wantBinary: boolean) => Promise<string|Blob>} request Requests
 *     the server clipboard.
 * @property {(textPromise: Promise<string>) => Promise<void>} copyViaExecCommand
 *     Last-resort copy through `execCommand`.
 * @property {string} lastText
 * @property {Blob|null} lastBlob
 * @property {string} lastMime
 */

/**
 * Server-clipboard cache, change-only signature, and the Ctrl/Cmd+C request
 * queue with its one-behind guard.
 *
 * The server reads its clipboard the instant REQUEST_CLIPBOARD arrives,
 * racing ahead of the application writing the new selection, so a request
 * stays open until an incoming value differs from the value cached when it
 * was made. The wire protocol carries no request id, so any server push can
 * settle the oldest pending request; the timeout plus the cache bound the
 * impact.
 *
 * Exactly one value is current at a time, the latest synced in either
 * direction: remembering older signatures would suppress legitimately
 * re-copying content copied before an intervening value. Beside it lives the
 * browser's re-encoded form of the latest inbound image, since writing a
 * pushed image recompresses it and the focus read-back would otherwise read
 * as new and echo once; it follows the synced signature's lifetime.
 * @param {object} hooks
 * @param {() => void} hooks.sendRequest Emits REQUEST_CLIPBOARD on the transport.
 * @param {boolean} [hooks.isChromium] Engine flag; the image read-back is Chromium-only.
 * @param {() => boolean} [hooks.canRead] Local-to-server direction enabled.
 * @returns {ClipboardSync}
 */
export function createClipboardSync({ sendRequest, digestBytes, isChromium = true, canRead = () => true }) {
    let lastText = '';
    let lastBlob = null;
    let lastMime = 'text/plain';
    let lastSyncedSig = null;
    let lastReencodeSig = null;
    let pending = [];
    function noteSynced(s) {
        lastSyncedSig = s;
        lastReencodeSig = null;
    }

    function hashBytes(h, u8) {
        for (let i = 0; i < u8.length; i++) h = ((h << 5) + h + u8[i]) | 0;
        return h;
    }

    /**
     * Both signature forms of a value. Text and byte-backed values are
     * content-hashed so two distinct payloads of equal size still differ; a
     * bare Blob, whose bytes are not in hand, gets the size-only `legacy`
     * form, which also rides along with hashed binary signatures so the two
     * can be cross-matched.
     * @returns {{full: string, legacy: string|null}}
     */
    function sigOf(data, mime) {
        if (typeof data === 'string') {
            let h = 5381;
            for (let i = 0; i < data.length; i++) h = ((h << 5) + h + data.charCodeAt(i)) | 0;
            return { full: `t:${data.length}:${h}`, legacy: null };
        }
        if (data && data.__clipDigest) {
            const dm = mime || '';
            return { full: `b:${dm}:${data.byteLength}:${data.hash}`,
                     legacy: `b:${dm}:${data.byteLength}` };
        }
        let parts = null;
        if (data instanceof Uint8Array) parts = [data];
        else if (data instanceof ArrayBuffer) parts = [new Uint8Array(data)];
        else if (Array.isArray(data)) parts = data.map((p) => (p instanceof Uint8Array ? p : new Uint8Array(p)));
        const m = mime || '';
        if (parts) {
            let h = 5381, size = 0;
            for (const p of parts) { size += p.length; h = hashBytes(h, p); }
            return { full: `b:${m}:${size}:${h}`, legacy: `b:${m}:${size}` };
        }
        const size = data && (data.byteLength !== undefined ? data.byteLength : data.size);
        return { full: `b:${m}:${size}`, legacy: null };
    }

    function sig(data, mime) { return sigOf(data, mime).full; }

    /**
     * Change-only gate: true while this content and mime differ from the last
     * synced value. Read-only: the caller marks the content synced through
     * `markSynced` only after the transfer completes, so a failed transfer
     * never permanently suppresses re-sending the same content. The legacy
     * compare suppresses echoes of content whose receive-side signature was
     * stored without bytes.
     */
    function shouldSend(data, mime) {
        const { full, legacy } = sigOf(data, mime);
        if (full === lastSyncedSig || full === lastReencodeSig) return false;
        return !(legacy !== null && (legacy === lastSyncedSig || legacy === lastReencodeSig));
    }

    /** Records content as synced; called on transfer success. */
    function markSynced(data, mime) {
        noteSynced(sig(data, mime));
    }

    /**
     * Caches fresh server data and settles pending requests through the
     * one-behind guard. `bytes`, when the receive path has them, make the
     * stored signature content-hashed so it matches what `shouldSend`
     * computes for the same data.
     */
    function resolveServer(text, blob, mime, bytes) {
        if (typeof text === 'string') { lastText = text; noteSynced(sig(text)); }
        if (blob) { lastBlob = blob; noteSynced(sig(bytes != null ? bytes : blob, mime || blob.type)); }
        if (mime) { lastMime = mime; }
        if (pending.length === 0) return;
        const reqs = pending;
        pending = [];
        for (const req of reqs) {
            if (req.settled) continue;
            try {
                if (req.wantBinary) {
                    if (blob && blob !== req.baselineBlob) req.resolve(blob);
                    else pending.push(req);
                } else {
                    if (typeof text === 'string' && text !== req.baselineText) req.resolve(text);
                    else pending.push(req);
                }
            } catch (_) { /* ignore */ }
        }
    }

    /**
     * After a server image is written to the local clipboard, records the
     * browser's re-encoded representation so the next focus read is
     * recognized as the same content instead of echoed back. Needs clipboard
     * read permission and focus and is silently skipped otherwise; the worst
     * case is one redundant round trip, never a loop. The capture is anchored
     * to the synced signature at entry: a sync in either direction landing
     * mid-read makes it stale, and storing it would suppress a legitimate
     * later copy.
     */
    async function captureLocalImageSig() {
        // Firefox and WebKit raise a paste prompt on a read outside a paste
        // gesture, and this one follows a write, so only Chromium reads back.
        // It reads the local clipboard, so the local-to-server direction has
        // to be enabled for it as much as for the focus read.
        if (!isChromium || !canRead()) return;
        const anchor = lastSyncedSig;
        try {
            const items = await navigator.clipboard.read();
            for (const it of items) {
                const m = it.types.find((t) => t !== 'text/plain');
                if (!m) continue;
                const b = await it.getType(m);
                const buf = await b.arrayBuffer();
                const digest = digestBytes ? await digestBytes(buf) : null;
                const reencoded = sig(digest || new Uint8Array(buf), m);
                if (lastSyncedSig === anchor) {
                    lastReencodeSig = reencoded;
                }
                return;
            }
        } catch (_) { /* unfocused or permission denied */ }
    }

    /**
     * Requests the server clipboard and resolves with the next fresh value.
     *
     * After two seconds the request settles so the ClipboardItem promise, and
     * the browser's transient-activation window, can never hang: with a
     * cached value that differs from the baseline recorded at request time it
     * resolves, otherwise it rejects, since resolving with the baseline-equal
     * cache would settle the copy with stale content exactly when the
     * session-start cache is empty or stale.
     * @param {boolean} wantBinary Whether an image is wanted rather than text.
     * @returns {Promise<string|Blob>}
     */
    function request(wantBinary) {
        try { sendRequest(); } catch (_) { /* transport not ready */ }
        return new Promise((resolve, reject) => {
            const req = { wantBinary: !!wantBinary, resolve, settled: false,
                baselineText: lastText, baselineBlob: lastBlob };
            const settle = (fn, val) => {
                if (req.settled) return;
                req.settled = true;
                const idx = pending.indexOf(req);
                if (idx !== -1) pending.splice(idx, 1);
                fn(val);
            };
            req.resolve = (val) => settle(resolve, val);
            pending.push(req);
            setTimeout(() => {
                if (wantBinary && lastBlob && lastBlob !== req.baselineBlob) {
                    settle(resolve, lastBlob);
                } else if (!wantBinary && lastText && lastText !== req.baselineText) {
                    settle(resolve, lastText);
                } else {
                    settle(reject, new Error('Server clipboard request timed out with no fresh value'));
                }
            }, 2000);
        });
    }

    /**
     * Last-resort copy for browsers that reject `navigator.clipboard.write`
     * (older Firefox and Safari): `execCommand('copy')` from a hidden
     * textarea. Awaiting the promise first can outlive the Ctrl/Cmd+C
     * transient activation, hence last resort. A rejected request or an empty
     * value writes nothing: either would clobber the user's local clipboard
     * with pre-copy content.
     * @param {Promise<string>} textPromise The pending server text.
     */
    async function copyViaExecCommand(textPromise) {
        let text = '';
        try { text = await textPromise; } catch (_) { return; }
        if (typeof text !== 'string') return;
        if (!text) return;
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.setAttribute('readonly', '');
        ta.style.position = 'fixed';
        ta.style.top = '-9999px';
        ta.style.left = '-9999px';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        try {
            ta.focus();
            ta.select();
            ta.setSelectionRange(0, ta.value.length);
            const ok = document.execCommand('copy');
            if (!ok) console.warn('execCommand("copy") fallback returned false.');
        } catch (err) {
            console.warn(`execCommand("copy") fallback threw: ${err && err.name} - ${err && err.message}`);
        } finally {
            document.body.removeChild(ta);
        }
    }

    return {
        sig,
        shouldSend,
        markSynced,
        resolveServer,
        captureLocalImageSig,
        request,
        copyViaExecCommand,
        get lastText() { return lastText; },
        get lastBlob() { return lastBlob; },
        get lastMime() { return lastMime; },
    };
}

/**
 * Keyboard and paste gesture wiring for clipboard sync.
 *
 * Owns the three window-level pieces around the per-transport read and send
 * functions:
 *
 * - Paste-ordering hold: a Ctrl/Cmd+V arriving while the local clipboard is
 *   still being read or sent would depart the ordered channel before the
 *   clipboard content and paste the previous value on the server. The chord's
 *   key events are swallowed, held until the send flushes (bounded), then
 *   replayed in order for the input stack.
 * - Non-Chromium Ctrl/Cmd+C: Safari and Firefox reject `navigator.clipboard`
 *   from focus and message handlers, which have no transient activation, so
 *   the server clipboard is written inside the copy gesture through a
 *   ClipboardItem whose blob is a Promise, with `execCommand('copy')` as last
 *   resort.
 * - Non-Chromium paste-to-server: driven by the `paste` event's synchronous
 *   `clipboardData`. There is deliberately no Ctrl/Cmd+V `navigator.clipboard`
 *   read: WebKit rejects it from keydown, Firefox re-raises its paste prompt,
 *   and it would double-send next to the paste event. While a session value is
 *   still on its way to the local clipboard the local one is older than the
 *   session's, so the paste sends nothing and the chord pastes the session's.
 *
 * Gestures in page form fields (the settings UI) are left alone; the stream's
 * overlay input is exempt. Consumed gestures are never `preventDefault`ed:
 * the chord must still reach the remote session.
 * @param {object} hooks
 * @param {boolean} hooks.isChromium Engine flag.
 * @param {ClipboardSync} hooks.clipboardSync The server-clipboard state.
 * @param {(data: string|ArrayBuffer, mime?: string) => Promise<void>} hooks.sendClipboardData
 *     Transport send.
 * @param {() => boolean} hooks.canSync Clipboard sync enabled.
 * @param {() => boolean} hooks.canRead Local-to-server direction enabled.
 * @param {() => boolean} hooks.canWrite Server-to-local direction enabled.
 * @param {() => boolean} hooks.binaryEnabled Whether images are sent.
 * @param {() => (Promise<*>|null)} hooks.getSendInFlight The local sender's
 *     pending send.
 * @param {(() => (Promise<*>|null))=} hooks.getDeferredWriteLanding The
 *     deferred writer's write whose data are complete, still landing.
 * @param {(() => boolean)=} hooks.hasPendingServerWrite Whether a session
 *     value has yet to land in the local clipboard.
 * @returns {{wire: () => void, unwire: () => void}} Listener registration.
 */
export function createClipboardGestures({
    isChromium,
    clipboardSync,
    sendClipboardData,
    canSync,
    canRead,
    canWrite,
    binaryEnabled,
    getSendInFlight,
    getDeferredWriteLanding,
    hasPendingServerWrite,
}) {
    function inPageFormField() {
        const ae = document.activeElement;
        return !!(ae && ae.id !== 'overlayInput' &&
            (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' ||
             ae.tagName === 'SELECT' || ae.isContentEditable));
    }

    const heldPasteEvents = [];
    let heldPasteReplayPending = false;
    // Outlasts Chromium's first-use clipboard-read prompt, which keeps the read
    // pending well past 2s, yet bounds how long an abandoned prompt can hold V.
    const PASTE_HOLD_MAX_MS = 10000;
    function replayHeldPasteEvents() {
        heldPasteReplayPending = false;
        for (const ev of heldPasteEvents.splice(0)) {
            try {
                const replay = new KeyboardEvent(ev.type, ev);
                Object.defineProperty(replay, '__selkiesClipReplay', { value: true });
                window.dispatchEvent(replay);
            } catch (_) { /* never break the key stream */ }
        }
    }
    /**
     * The in-flight transfer failed or never settled: injecting the held V
     * now would paste stale content, so the held keydowns are dropped. The
     * swallowed keyups (V and the chord's modifiers) are still replayed, as
     * losing a modifier keyup would leave it stuck server-side.
     */
    function dropHeldPasteKeydowns() {
        for (let i = heldPasteEvents.length - 1; i >= 0; i--) {
            if (heldPasteEvents[i].type === 'keydown') heldPasteEvents.splice(i, 1);
        }
        replayHeldPasteEvents();
    }
    const PASTE_MOD_CODES = ['ControlLeft', 'ControlRight', 'MetaLeft', 'MetaRight'];
    /**
     * Capture-phase key listener implementing the paste-ordering hold.
     *
     * A paste chord is held while a send is in flight or a server-to-client
     * local-clipboard write is still landing, since the paste would otherwise
     * read the old value; any KeyV event is held while a replay is queued, so
     * its keyup cannot overtake the held keydown, and so are the chord's
     * modifier keyups, since a Ctrl keyup overtaking the replayed V would
     * break the chord server-side and type a literal `v`. The hold waits for
     * the current read/send and deferred write, then re-checks, as a
     * follow-on transfer may have started meanwhile (the deferred write
     * flushed by this very keydown); replay happens only once nothing is
     * pending, and on failure or an expired bound the paste is dropped rather
     * than injected with stale content.
     * @param {KeyboardEvent} ev
     */
    function holdPasteWhileClipboardInFlight(ev) {
        if (ev.__selkiesClipReplay) return;
        const modHold = heldPasteReplayPending && ev.type === 'keyup' && PASTE_MOD_CODES.includes(ev.code);
        if (ev.code !== 'KeyV' && !modHold) return;
        const chord = (ev.ctrlKey || ev.metaKey) && !ev.altKey;
        const writeInFlight = getDeferredWriteLanding ? getDeferredWriteLanding() : null;
        const hold = modHold || (ev.code === 'KeyV' &&
            ((chord && (getSendInFlight() || writeInFlight)) || heldPasteReplayPending));
        if (!hold) return;
        ev.preventDefault();
        ev.stopImmediatePropagation();
        heldPasteEvents.push(ev);
        if (!heldPasteReplayPending) {
            heldPasteReplayPending = true;
            const holdStart = performance.now();
            const awaitClipboardQuiet = () => {
                const inflight = [];
                const send = getSendInFlight();
                if (send) inflight.push(send);
                const dw = getDeferredWriteLanding ? getDeferredWriteLanding() : null;
                if (dw) inflight.push(dw);
                if (inflight.length === 0) { replayHeldPasteEvents(); return; }
                const remaining = PASTE_HOLD_MAX_MS - (performance.now() - holdStart);
                if (remaining <= 0) { dropHeldPasteKeydowns(); return; }
                Promise.race([
                    Promise.all(inflight).then(() => 'settled', () => 'failed'),
                    new Promise((r) => setTimeout(() => r('timeout'), remaining)),
                ]).then((outcome) => {
                    if (outcome === 'settled') awaitClipboardQuiet();
                    else dropHeldPasteKeydowns();
                });
            };
            awaitClipboardQuiet();
        }
    }

    /**
     * Non-Chromium Ctrl/Cmd+C: writes the server clipboard inside the gesture.
     *
     * Only `text/plain` is advertised: a Ctrl/Cmd+C cannot synchronously know
     * whether the server's current clipboard is an image, and a stale cached
     * MIME type would build a malformed ClipboardItem. Server images are
     * delivered by the push handler instead. Autorepeat is ignored so it
     * cannot spam REQUEST_CLIPBOARD.
     * @param {KeyboardEvent} event
     */
    function onCopyKeydown(event) {
        if (!canSync()) return;
        if (!(event.ctrlKey || event.metaKey) || event.altKey) return;
        if (event.repeat) return;
        if (inPageFormField()) return;
        const key = (event.key || '').toLowerCase();
        if (key === 'c' && canWrite()) {
            const textPromise = clipboardSync.request(false);
            const items = {
                'text/plain': textPromise.then((t) =>
                    new Blob([typeof t === 'string' ? t : (clipboardSync.lastText || '')], { type: 'text/plain' }))
            };
            let writePromise = null;
            try {
                writePromise = navigator.clipboard.write([new ClipboardItem(items)]);
            } catch (err) {
                console.warn(`navigator.clipboard.write unavailable on Ctrl+C, using execCommand: ${err && err.name}`);
                clipboardSync.copyViaExecCommand(textPromise);
            }
            if (writePromise && writePromise.catch) {
                writePromise.catch((err) => {
                    console.warn(`navigator.clipboard.write rejected on Ctrl+C, using execCommand: ${err && err.name} - ${err && err.message}`);
                    clipboardSync.copyViaExecCommand(textPromise);
                });
            }
        }
    }

    /**
     * Non-Chromium paste-to-server from the event's synchronous clipboard
     * data, preferring an image when binary clipboard is on and the payload
     * carries one.
     * @param {ClipboardEvent} event
     */
    function onPaste(event) {
        if (!canSync() || !canRead()) return;
        if (inPageFormField()) return;
        if (hasPendingServerWrite && hasPendingServerWrite()) return;
        const cd = event.clipboardData;
        if (!cd) return;
        if (binaryEnabled() && cd.items) {
            for (let i = 0; i < cd.items.length; i++) {
                const it = cd.items[i];
                if (it.kind === 'file' && it.type && it.type.startsWith('image/')) {
                    const file = it.getAsFile();
                    if (file) {
                        file.arrayBuffer()
                            .then((buf) => sendClipboardData(buf, it.type))
                            .catch((err) => console.warn(`Paste image read failed: ${err && err.name}`));
                        return;
                    }
                }
            }
        }
        const text = cd.getData('text/plain');
        if (text) sendClipboardData(text);
    }

    /** Registers the listeners; called before input attaches so the hold runs first. */
    function wire() {
        window.addEventListener('keydown', holdPasteWhileClipboardInFlight, true);
        window.addEventListener('keyup', holdPasteWhileClipboardInFlight, true);
        if (!isChromium) {
            window.addEventListener('keydown', onCopyKeydown, true);
            window.addEventListener('paste', onPaste, true);
        }
    }

    /** Removes the listeners `wire` registered. */
    function unwire() {
        window.removeEventListener('keydown', holdPasteWhileClipboardInFlight, true);
        window.removeEventListener('keyup', holdPasteWhileClipboardInFlight, true);
        if (!isChromium) {
            window.removeEventListener('keydown', onCopyKeydown, true);
            window.removeEventListener('paste', onPaste, true);
        }
    }

    return { wire, unwire };
}
