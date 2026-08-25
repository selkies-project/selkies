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
 * never re-crosses the transport in either direction), `createMultipartClipboardState`
 * reassembles multipart server pushes, `createTaggedClipboardFetch` marks the
 * connect-time cache-only fetch, `createLocalClipboardSender` is the
 * focus-driven local-to-server path, `createDeferredClipboardWriter` lands
 * server pushes on engines that reject clipboard writes outside a user
 * activation, and `createClipboardGestures` wires the copy and paste
 * keystrokes. The transports differ only in the hooks they inject: how a
 * request or payload is sent and the enablement gates, which are closures
 * re-read per event so runtime settings changes apply immediately.
 * @module
 */

/**
 * @typedef {{kind: 'text', text: string}|{kind: 'image', blob: Blob, mime: string}} LocalClipboardContent
 */

/**
 * Re-encodes a raster blob as PNG.
 *
 * Chromium's async clipboard accepts only `image/png` on write, but a source
 * may offer only JPEG, BMP or WebP, so the blob is decoded with the browser's
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
 * Writes a server image to the local clipboard, PNG-normalized through
 * `reencodeBlobAsPng`.
 * @param {Blob} blob The image.
 * @param {string} mime Its type.
 * @throws When the type is undecodable or the clipboard write fails.
 */
export async function writeImageToLocalClipboard(blob, mime) {
    const outBlob = mime === 'image/png' ? blob : await reencodeBlobAsPng(blob);
    await navigator.clipboard.write([new ClipboardItem({ 'image/png': outBlob })]);
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
 * @property {(b64: string) => void} push Accumulates one base64 chunk.
 * @property {() => ({base64: string, mimeType: string, totalSize: number}|null)} assemble
 *     Joins the chunks and resets; `null` when no transfer is in progress.
 * @property {() => void} reset Drops the transfer.
 * @property {boolean} inProgress
 * @property {string|null} mimeType
 * @property {number} totalSize Declared size in bytes.
 * @property {number} receivedSize Decoded bytes accumulated so far.
 */

/**
 * Multipart server-to-client clipboard download state.
 *
 * The decoded byte count is tracked incrementally from the base64 lengths, so
 * nothing decodes on the main thread until the caller assembles. A truncated
 * stream must never be delivered as content: callers compare `receivedSize`
 * against `totalSize` before assembling, or the decoded byte length after,
 * and discard on a mismatch.
 * @returns {MultipartClipboardState}
 */
export function createMultipartClipboardState() {
    let chunks = [];
    let mimeType = null;
    let totalSize = 0;
    let receivedSize = 0;
    let inProgress = false;

    function base64DecodedSize(b64) {
        if (!b64) return 0;
        const pad = b64.endsWith('==') ? 2 : (b64.endsWith('=') ? 1 : 0);
        return (b64.length / 4) * 3 - pad;
    }

    return {
        begin(mime, total) {
            chunks = [];
            mimeType = mime;
            totalSize = total;
            receivedSize = 0;
            inProgress = true;
        },
        push(b64) {
            if (!inProgress) return;
            chunks.push(b64);
            receivedSize += base64DecodedSize(b64);
        },
        assemble() {
            if (!inProgress) return null;
            const result = { base64: chunks.join(''), mimeType, totalSize };
            this.reset();
            return result;
        },
        reset() {
            chunks = [];
            mimeType = null;
            totalSize = 0;
            receivedSize = 0;
            inProgress = false;
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
 * @typedef {object} LocalClipboardSender
 * @property {() => Promise<void>} readAndSend Reads the local clipboard and
 *     pushes any content to the server.
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
 * @param {object} hooks
 * @param {boolean} hooks.isChromium Engine flag.
 * @param {() => boolean} hooks.isSharedMode Viewer sessions never send.
 * @param {() => boolean} hooks.canSync Clipboard sync enabled.
 * @param {() => boolean} hooks.canRead Local-to-server direction enabled.
 * @param {() => boolean} hooks.binaryEnabled Whether images are sent.
 * @param {(data: string|ArrayBuffer, mime?: string) => Promise<void>} hooks.sendClipboardData
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

    /**
     * A server push still settling through the deferred writer must land
     * before this read: reading around the flush returns the pre-push
     * content, which then reads as a change and bounces the stale value back
     * to the server.
     */
    async function readAndSend() {
        // navigator.clipboard is undefined on insecure origins.
        if (!window.isSecureContext || !navigator.clipboard) return;
        if (isSharedMode() || !canSync() || !canRead()) return;

        if (getDeferredWriteInFlight) {
            for (let i = 0; i < 2; i++) {
                const w = getDeferredWriteInFlight();
                if (!w) break;
                try { await w; } catch (_) { /* write-side errors surface there */ }
                if (!getDeferredWriteInFlight()) break;
            }
        }

        const work = (async () => {
            try {
                const res = await readLocalClipboard(binaryEnabled());
                if (!res) return;
                if (res.kind === 'image') {
                    const arrayBuffer = await res.blob.arrayBuffer();
                    await sendClipboardData(arrayBuffer, res.mime);
                    console.log(`Sent binary clipboard: ${res.mime}, size: ${res.blob.size} bytes`);
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

    return { readAndSend, maybeInitial, getSendInFlight: () => sendInFlight };
}

/**
 * @typedef {object} DeferredClipboardWriter
 * @property {(attempt: () => Promise<void>, callbacks?: {onSuccess?: () => void, onFailure?: (err: Error) => void}) => Promise<boolean>} write
 *     Runs an async clipboard write now, stashing it for the next gesture on
 *     an activation rejection.
 * @property {() => void} flush Retries the stashed write.
 * @property {() => (Promise<boolean>|null)} getInFlight The most recent
 *     attempt, immediate or flushed, or `null`.
 */

/**
 * Deferred local-clipboard writer for server pushes.
 *
 * Firefox and WebKit reject `navigator.clipboard` writes outside a transient
 * user activation, and a server push handler never has one, so on an
 * activation or focus rejection the write is stashed and retried on the next
 * real gesture instead of being lost. Only the newest pending write is kept,
 * since the clipboard is last-value-wins: a monotonic sequence lets a failed
 * newer write replace an older stash while a flushed stash that fails again
 * can never clobber a write that arrived during its attempt. The
 * paste-ordering hold awaits the in-flight attempt so a server-to-client
 * write lands before a paste reads the local clipboard; otherwise the stash
 * flushes on the paste's own keydown and lands just after the read, and the
 * first paste is one behind. The flush rides keydown and pointerdown, which
 * carry a user activation, and focus and visibilitychange, which land the
 * write the instant Chromium accepts it again (it rejects writes from an
 * unfocused document), well before the user's next paste.
 * @returns {DeferredClipboardWriter}
 */
export function createDeferredClipboardWriter() {
    let pending = null;
    let writeSeq = 0;
    let inFlight = null;

    function isActivationError(err) {
        return !!err && (err.name === 'NotAllowedError' || err.name === 'SecurityError');
    }

    function track(promise) {
        inFlight = promise;
        promise.finally(() => { if (inFlight === promise) inFlight = null; });
    }

    /**
     * Runs one write. An activation rejection (a synthetic event, or a
     * blurred tab) stashes it for the next gesture unless something newer
     * replaced it; any other error reaches `onFailure`.
     */
    function attemptOnce(w) {
        return w.attempt().then(
            () => { if (w.onSuccess) w.onSuccess(); return true; },
            (err) => {
                if (isActivationError(err)) {
                    if (!pending || pending.seq < w.seq) pending = w;
                    return false;
                }
                if (w.onFailure) w.onFailure(err);
                return false;
            });
    }

    function flush() {
        const w = pending;
        if (!w) return;
        pending = null;
        track(attemptOnce(w));
    }

    for (const type of ['pointerdown', 'keydown', 'focus']) {
        window.addEventListener(type, flush, true);
    }
    document.addEventListener('visibilitychange', () => { if (!document.hidden) flush(); }, true);

    /**
     * Runs `attempt` now; on an activation or focus rejection queues it for
     * the next gesture. `onSuccess` fires whenever the write eventually lands,
     * `onFailure` only for non-activation errors.
     */
    function write(attempt, { onSuccess, onFailure } = {}) {
        const p = attemptOnce({ attempt, onSuccess, onFailure, seq: ++writeSeq });
        track(p);
        return p;
    }

    return { write, flush, getInFlight: () => inFlight };
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
 *     re-encoded form of the image just written locally.
 * @property {(wantBinary: boolean) => Promise<string|Blob>} request Requests
 *     the server clipboard.
 * @property {(textPromise: Promise<string>) => Promise<void>} copyViaExecCommand
 *     Last-resort copy through `execCommand`.
 * @property {string} lastText
 * @property {Blob|null} lastBlob
 * @property {string} lastMime
 */

/**
 * Server-clipboard cache, change-only signature and the Ctrl/Cmd+C request
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
 * @returns {ClipboardSync}
 */
export function createClipboardSync({ sendRequest }) {
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
        const anchor = lastSyncedSig;
        try {
            const items = await navigator.clipboard.read();
            for (const it of items) {
                const m = it.types.find((t) => t !== 'text/plain');
                if (!m) continue;
                const b = await it.getType(m);
                const reencoded = sig(new Uint8Array(await b.arrayBuffer()), m);
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
 *   and it would double-send next to the paste event.
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
 * @param {(() => (Promise<*>|null))=} hooks.getDeferredWriteInFlight The
 *     deferred writer's pending write.
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
    getDeferredWriteInFlight,
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
        const writeInFlight = getDeferredWriteInFlight ? getDeferredWriteInFlight() : null;
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
                const dw = getDeferredWriteInFlight ? getDeferredWriteInFlight() : null;
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
