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
