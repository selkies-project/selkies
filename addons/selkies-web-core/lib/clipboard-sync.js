/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Client<->server clipboard synchronization state, shared by both transports.
 *
 * Owns the server-clipboard cache (text/blob/mime), the change-only signature
 * (unchanged content never re-crosses the transport in either direction), and
 * the Ctrl/Cmd+C request queue with its one-behind guard: the server reads its
 * clipboard the instant REQUEST_CLIPBOARD arrives, racing ahead of the app
 * writing the new selection, so a request stays open until an incoming value
 * DIFFERS from the value cached when it was made. The wire protocol carries no
 * request id, so any server push can settle the oldest pending request; the
 * timeout plus cache bound the impact.
 *
 * `sendRequest` is the transport hook that emits REQUEST_CLIPBOARD.
 */
/**
 * Write a server image to the local clipboard. Chromium's async clipboard
 * accepts ONLY image/png on write, but the X selection owner may offer only
 * JPEG/BMP/WebP — decode any non-PNG raster with the browser's own decoders
 * and re-encode as PNG first. Rejects (for the caller's error path) when the
 * mime is undecodable (e.g. dimensionless SVG) or the clipboard write fails.
 */
export async function writeImageToLocalClipboard(blob, mime) {
    let outBlob = blob;
    if (mime !== 'image/png') {
        const bmp = await createImageBitmap(blob);
        try {
            const canvas = document.createElement('canvas');
            canvas.width = bmp.width;
            canvas.height = bmp.height;
            canvas.getContext('2d').drawImage(bmp, 0, 0);
            outBlob = await new Promise((resolve, reject) =>
                canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('PNG encode failed'))), 'image/png'));
        } finally {
            bmp.close();
        }
    }
    await navigator.clipboard.write([new ClipboardItem({ 'image/png': outBlob })]);
}

/**
 * Read the local clipboard for the focus/gesture send path, shared by both
 * transports (websockets-canonical). Returns {kind:'text', text} |
 * {kind:'image', blob, mime} | null. Chromium's advanced read()/getType()
 * throws DataError on large text (and some images); readText() still returns
 * the text, so every failure path falls back to it rather than dropping the
 * sync. Throws only genuinely unexpected errors for the caller to log.
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
 * Deferred local-clipboard writer for server pushes. Firefox (and WebKit)
 * reject navigator.clipboard writes outside a transient user activation, and a
 * server push handler never has one — so on an activation/focus rejection the
 * write is stashed and retried on the next real gesture instead of being lost.
 * Only the newest pending write is kept: the clipboard is last-value-wins.
 */
export function createDeferredClipboardWriter() {
    let pending = null;
    // Resolves when the most recent write attempt (immediate or flushed) settles.
    // The paste-ordering hold awaits it so a server->client write LANDS before a
    // paste reads the local clipboard — otherwise the stashed write flushes on the
    // paste's own keydown and lands just after the read, so the first paste is
    // one-behind and the user has to paste twice.
    let inFlight = null;

    function isActivationError(err) {
        return !!err && (err.name === 'NotAllowedError' || err.name === 'SecurityError');
    }

    function track(promise) {
        inFlight = promise;
        promise.finally(() => { if (inFlight === promise) inFlight = null; });
    }

    function attemptOnce(w) {
        return w.attempt().then(
            () => { if (w.onSuccess) w.onSuccess(); return true; },
            (err) => {
                // Still no focus/activation (e.g. synthetic event, or the tab is
                // blurred — Chromium rejects clipboard writes from an unfocused
                // document): keep it for the next gesture/focus unless something
                // newer replaced it meanwhile.
                if (isActivationError(err)) { if (!pending) pending = w; return false; }
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

    // keydown/pointerdown carry a user activation; focus/visibilitychange land the
    // write the instant Chromium will accept it (it rejects writes from an
    // unfocused document), so a push that arrived while the tab was blurred is
    // current well before the user's next paste instead of waiting for a stray
    // keystroke. All are cheap no-ops while nothing is stashed.
    for (const type of ['pointerdown', 'keydown', 'focus']) {
        window.addEventListener(type, flush, true);
    }
    document.addEventListener('visibilitychange', () => { if (!document.hidden) flush(); }, true);

    /**
     * Run `attempt` (an async clipboard write) now; on an activation/focus
     * rejection queue it for the next gesture/focus. onSuccess fires whenever the
     * write eventually lands; onFailure only for non-activation errors.
     */
    function write(attempt, { onSuccess, onFailure } = {}) {
        const p = attemptOnce({ attempt, onSuccess, onFailure });
        track(p);
        return p;
    }

    return { write, flush, getInFlight: () => inFlight };
}

/**
 * Preview form of server clipboard text for the dashboard UI. Multi-MB
 * payloads structured-clone through postMessage and land in a controlled
 * textarea, freezing the page; the UI only needs a bounded preview. The
 * `truncated` flag tells the dashboard to render it read-only so a blur
 * can't echo the cut-down text back over the real server clipboard.
 */
export const CLIPBOARD_PREVIEW_LIMIT = 256 * 1024;

export function clipboardPreviewMessage(text) {
    const truncated = text.length > CLIPBOARD_PREVIEW_LIMIT;
    return {
        type: 'clipboardContentUpdate',
        text: truncated ? text.slice(0, CLIPBOARD_PREVIEW_LIMIT) : text,
        truncated,
        totalLength: text.length,
    };
}

export function createClipboardSync({ sendRequest }) {
    let lastText = '';
    let lastBlob = null;
    let lastMime = 'text/plain';
    let lastSyncedSig = null;
    let pending = [];

    function hashBytes(h, u8) {
        for (let i = 0; i < u8.length; i++) h = ((h << 5) + h + u8[i]) | 0;
        return h;
    }

    /**
     * Signature forms: text and byte-backed values are content-hashed so two
     * distinct payloads of equal size still differ; a bare Blob (bytes not in
     * hand) gets the size-only `legacy` form. `legacy` also rides along with
     * hashed binary signatures so the two forms can be cross-matched.
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

    /** Change-only gate: true exactly once per distinct content+mime. */
    function shouldSend(data, mime) {
        const { full, legacy } = sigOf(data, mime);
        // The legacy compare suppresses echoes of content whose receive-side
        // signature was stored without bytes (size-only form).
        if (full === lastSyncedSig || (legacy !== null && legacy === lastSyncedSig)) {
            lastSyncedSig = full;
            return false;
        }
        lastSyncedSig = full;
        return true;
    }

    /**
     * Cache fresh server data and settle pending requests (one-behind guard).
     * `bytes` (when the receive path has them) makes the stored signature
     * content-hashed so it matches what shouldSend computes for the same data.
     */
    function resolveServer(text, blob, mime, bytes) {
        if (typeof text === 'string') { lastText = text; lastSyncedSig = sig(text); }
        if (blob) { lastBlob = blob; lastSyncedSig = sig(bytes != null ? bytes : blob, mime || blob.type); }
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
     * After a server image is written to the local clipboard, record the
     * browser's re-encoded representation (browsers recompress on write) so the
     * next focus-read is recognized as the same content instead of echoed back.
     * Needs clipboard-read permission and focus; silently skipped otherwise —
     * worst case is one redundant round-trip, never a loop.
     */
    async function captureLocalImageSig() {
        try {
            const items = await navigator.clipboard.read();
            for (const it of items) {
                const m = it.types.find((t) => t !== 'text/plain');
                if (!m) continue;
                const b = await it.getType(m);
                lastSyncedSig = sig(new Uint8Array(await b.arrayBuffer()), m);
                return;
            }
        } catch (_) { /* unfocused or permission denied */ }
    }

    /**
     * Request the server clipboard; resolves with the next FRESH value, falling
     * back to the cached value after 2s so the ClipboardItem promise (and the
     * browser's transient-activation window) can never hang.
     */
    function request(wantBinary) {
        try { sendRequest(); } catch (_) { /* transport not ready */ }
        return new Promise((resolve) => {
            const req = { wantBinary: !!wantBinary, resolve, settled: false,
                baselineText: lastText, baselineBlob: lastBlob };
            const done = (val) => {
                if (req.settled) return;
                req.settled = true;
                const idx = pending.indexOf(req);
                if (idx !== -1) pending.splice(idx, 1);
                resolve(val);
            };
            req.resolve = done;
            pending.push(req);
            setTimeout(() => {
                if (wantBinary) {
                    done(lastBlob || new Blob([lastText || ''], { type: 'text/plain' }));
                } else {
                    done(lastText || '');
                }
            }, 2000);
        });
    }

    /**
     * Last-resort copy for browsers that reject navigator.clipboard.write (older
     * Firefox/Safari): execCommand('copy') from a hidden textarea. Awaiting the
     * promise first can outlive the Ctrl/Cmd+C transient activation, hence last resort.
     */
    async function copyViaExecCommand(textPromise) {
        let text = '';
        try { text = await textPromise; } catch (_) { text = lastText || ''; }
        if (typeof text !== 'string') text = lastText || '';
        // Don't clobber the user's local clipboard with empty content (slow/empty
        // server response on the first copy of a session).
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
 * Keyboard/paste gesture wiring for clipboard sync, shared by both transports.
 *
 * Owns the three window-level pieces around the per-transport read/send
 * functions:
 *
 * - Paste-ordering hold: a Ctrl/Cmd+V arriving while the local clipboard is
 *   still being read/sent would depart the ordered channel BEFORE the
 *   clipboard content and paste the previous value on the server. The chord's
 *   key events are swallowed, held until the send flushes (bounded), then
 *   replayed in order for the input stack.
 * - Non-Chromium Ctrl/Cmd+C: Safari/Firefox reject navigator.clipboard from
 *   focus/message handlers (no transient activation), so the server clipboard
 *   is written inside the copy gesture via a ClipboardItem whose blob is a
 *   Promise, with execCommand('copy') as last resort.
 * - Non-Chromium paste-to-server: driven by the 'paste' event's synchronous
 *   event.clipboardData. There is deliberately NO Ctrl/Cmd+V
 *   navigator.clipboard read: WebKit rejects it from keydown, Firefox
 *   re-raises its paste prompt, and it would double-send next to the paste
 *   event.
 *
 * Gates are callbacks because the two cores keep their enablement state in
 * different variables; every gate is re-read per event so runtime settings
 * changes apply immediately. Never preventDefault on consumed gestures: the
 * chord must still reach the remote session.
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
    // Only drive remote-clipboard sync from the stream; don't hijack
    // copy/paste in page form fields (settings UI, etc.). The stream's
    // overlay input is exempt.
    function inPageFormField() {
        const ae = document.activeElement;
        return !!(ae && ae.id !== 'overlayInput' &&
            (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' ||
             ae.tagName === 'SELECT' || ae.isContentEditable));
    }

    const heldPasteEvents = [];
    let heldPasteReplayPending = false;
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
    const PASTE_MOD_CODES = ['ControlLeft', 'ControlRight', 'MetaLeft', 'MetaRight'];
    function holdPasteWhileClipboardInFlight(ev) {
        if (ev.__selkiesClipReplay) return;
        // While a replay is queued, the chord's modifier keyups must be held
        // too — a Ctrl keyup overtaking the replayed V would break the chord
        // server-side (V would arrive unmodified and type a literal 'v').
        const modHold = heldPasteReplayPending && ev.type === 'keyup' && PASTE_MOD_CODES.includes(ev.code);
        if (ev.code !== 'KeyV' && !modHold) return;
        const chord = (ev.ctrlKey || ev.metaKey) && !ev.altKey;
        // Hold a paste chord while a send is in flight OR a server->client
        // local-clipboard write is still landing (else the paste reads the old
        // value — "paste twice"); also hold ANY KeyV event while a replay is
        // queued (its keyup must not overtake the held keydown, even if Ctrl was
        // already released).
        const writeInFlight = getDeferredWriteInFlight ? getDeferredWriteInFlight() : null;
        const hold = modHold || (ev.code === 'KeyV' &&
            ((chord && (getSendInFlight() || writeInFlight)) || heldPasteReplayPending));
        if (!hold) return;
        ev.preventDefault();
        ev.stopImmediatePropagation();
        heldPasteEvents.push(ev);
        if (!heldPasteReplayPending) {
            heldPasteReplayPending = true;
            const waitFor = [getSendInFlight() || Promise.resolve()];
            if (getDeferredWriteInFlight) waitFor.push(getDeferredWriteInFlight() || Promise.resolve());
            Promise.race([
                Promise.all(waitFor),
                new Promise((r) => setTimeout(r, 2000)),
            ]).then(replayHeldPasteEvents, replayHeldPasteEvents);
        }
    }

    function onCopyKeydown(event) {
        if (!canSync()) return;
        if (!(event.ctrlKey || event.metaKey) || event.altKey) return;
        // Once per physical keypress: autorepeat must not spam REQUEST_CLIPBOARD.
        if (event.repeat) return;
        if (inPageFormField()) return;
        const key = (event.key || '').toLowerCase();
        // Read (Ctrl/Cmd+V) is handled by the 'paste' listener via
        // event.clipboardData: synchronous, no Firefox paste-prompt, and no
        // double-send. Reading here through navigator.clipboard would re-raise
        // the prompt and send twice.
        if (key === 'c' && canWrite()) {
            // Advertise text/plain ONLY: a Ctrl/Cmd+C can't synchronously know
            // whether the server's CURRENT clipboard is an image, and a stale
            // cached MIME type would build a malformed ClipboardItem (image
            // entry holding text). Server images are delivered by the push
            // handler instead.
            const textPromise = clipboardSync.request(false);
            const items = {
                'text/plain': textPromise.then((t) =>
                    new Blob([typeof t === 'string' ? t : (clipboardSync.lastText || '')], { type: 'text/plain' }))
            };
            let writePromise = null;
            try {
                writePromise = navigator.clipboard.write([new ClipboardItem(items)]);
            } catch (err) {
                // Synchronous throw (e.g. ClipboardItem/clipboard.write unsupported).
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

    function onPaste(event) {
        if (!canSync() || !canRead()) return;
        if (inPageFormField()) return;
        const cd = event.clipboardData;
        if (!cd) return;
        // Prefer an image when binary clipboard is on and the payload carries one.
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

    function wire() {
        // Registered before input attaches (both capture on window), so the
        // hold runs first.
        window.addEventListener('keydown', holdPasteWhileClipboardInFlight, true);
        window.addEventListener('keyup', holdPasteWhileClipboardInFlight, true);
        if (!isChromium) {
            window.addEventListener('keydown', onCopyKeydown, true);
            window.addEventListener('paste', onPaste, true);
        }
    }

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
