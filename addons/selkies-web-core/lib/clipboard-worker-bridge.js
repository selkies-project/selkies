/**
 * Off-main-thread base64 for clipboard payloads and the clipboard send path,
 * shared by both transports.
 *
 * A per-byte `String.fromCharCode` + `btoa` build of a multi-MB clipboard
 * blocks the main thread for seconds, freezing the video presentation and
 * input dispatch that share it, so encode and decode run in
 * clipboard-worker.js. Both transports emit the identical wire protocol to
 * the same server handler: `cw` / `cb` as a single message, or the multipart
 * `cws`+`cwd`+`cwe` / `cbs`+`cbd`+`cbe` sequence for large payloads. The
 * server decodes each data chunk independently, so each raw chunk is
 * base64-encoded on its own, never the whole payload encoded and then sliced
 * as a string.
 * @module
 */
import ClipboardWorker from '../clipboard-worker.js?worker&inline';

/**
 * Request/response bridge to the clipboard worker.
 *
 * The worker is created lazily on the first request and every call resolves
 * with `{ result, mimeType, byteLength }` from the worker's reply.
 */
export class ClipboardWorkerBridge {
    constructor() {
        this.worker = null;
        this.callbacks = new Map();
        this.msgId = 0;
    }

    /** Creates the worker when it does not exist yet. */
    init() {
        if (!this.worker) {
            this.worker = new ClipboardWorker();
            this.worker.onmessage = (e) => {
                const { id, success, result, error, mimeType, byteLength } = e.data;
                const resolveReject = this.callbacks.get(id);
                if (resolveReject) {
                    this.callbacks.delete(id);
                    if (success) {
                        resolveReject.resolve({ result, mimeType, byteLength });
                    } else {
                        resolveReject.reject(new Error(error));
                    }
                }
            };
            console.log("Clipboard Web Worker initialized.");
        }
    }

    /** Stops the worker and rejects every pending request with an `AbortError`. */
    terminate() {
        if (!this.worker) return;
        this.worker.terminate();
        this.worker = null;
        const pendingCallbacks = Array.from(this.callbacks.values());
        this.callbacks.clear();
        for (const { reject } of pendingCallbacks) {
            const err = new Error("Worker Terminated");
            err.name = "AbortError";
            reject(err);
        }
        console.log("Clipboard Web Worker terminated and pending operations aborted.");
    }

    /**
     * @param {string} text UTF-8 text to encode.
     * @returns {Promise<{result: string, mimeType: string, byteLength: number}>}
     */
    async encodeText(text) {
        this.init();
        return new Promise((resolve, reject) => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'ENCODE_TEXT_TO_B64', payload: text });
        });
    }

    /**
     * Encodes a buffer with a zero-copy transfer: the buffer is neutered, so
     * callers pass one they own exclusively (a fresh or sliced copy, never a
     * shared view).
     * @param {ArrayBuffer} arrayBuffer Bytes to encode; unusable afterwards.
     * @returns {Promise<{result: string, mimeType: string, byteLength: number}>}
     */
    async encodeBinary(arrayBuffer) {
        this.init();
        return new Promise((resolve, reject) => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage(
                { id, action: 'ENCODE_BINARY_TO_B64', payload: arrayBuffer },
                [arrayBuffer]
            );
        });
    }

    /**
     * @param {string} base64String Payload to decode.
     * @param {string} mimeType Type reported back with the decoded bytes.
     * @returns {Promise<{result: *, mimeType: string, byteLength: number}>}
     */
    async decode(base64String, mimeType) {
        this.init();
        return new Promise((resolve, reject) => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'DECODE_FROM_B64', payload: base64String, mimeType });
        });
    }
}

/**
 * Base64-encodes one clipboard byte run off the main thread.
 *
 * A fresh slice gives the worker a buffer it can neuter through zero-copy
 * transfer; on worker failure it degrades to a chunked main-thread encode,
 * still far cheaper than a per-byte `String.fromCharCode` build.
 * @param {ClipboardWorkerBridge} worker The bridge to encode through.
 * @param {Uint8Array} bytes The run to encode; left untouched.
 * @returns {Promise<string>} The base64 text.
 */
export async function encodeClipboardChunk(worker, bytes) {
    try {
        const copy = bytes.slice();
        const { result } = await worker.encodeBinary(copy.buffer);
        return result;
    } catch (e) {
        console.warn('Clipboard worker encode failed; falling back to main thread:', e);
        let s = '';
        for (let i = 0; i < bytes.length; i += 0x8000) {
            s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
        }
        return btoa(s);
    }
}

/**
 * Sends a clipboard payload, as one message when it fits a chunk and as a
 * multipart sequence otherwise.
 *
 * Each chunk is encoded off the main thread with a yield between chunks, so
 * a multi-MB clipboard never blocks video presentation or input dispatch.
 * The transports differ only in the injected `send` and `waitDrain`.
 * @param {Uint8Array} bytes The payload.
 * @param {string} mimeType `text/plain` selects the text messages, anything
 *     else the binary ones.
 * @param {object} io Transport hooks.
 * @param {ClipboardWorkerBridge} io.worker The bridge to encode through.
 * @param {(message: string) => void} io.send Sends one wire message.
 * @param {(() => Promise<boolean|void>)=} io.waitDrain Awaited before every
 *     chunk for backpressure; resolving `false` aborts the transfer (the
 *     channel closed).
 * @param {number} io.chunkRawBytes Raw bytes per chunk.
 * @param {() => number|string} io.nextTid Allocates the multipart transfer id.
 */
export async function sendClipboardChunked(bytes, mimeType, { worker, send, waitDrain, chunkRawBytes, nextTid }) {
    const isText = mimeType === 'text/plain';
    const total = bytes.byteLength;
    if (total < chunkRawBytes) {
        const b64 = await encodeClipboardChunk(worker, bytes);
        send(isText ? `cw,${b64}` : `cb,${mimeType},${b64}`);
        return;
    }
    const tid = nextTid();
    send(isText ? `cws,${tid},${total}` : `cbs,${tid},${mimeType},${total}`);
    for (let off = 0; off < total; off += chunkRawBytes) {
        if (waitDrain) {
            const ok = await waitDrain();
            if (ok === false) return;
        }
        const chunk = bytes.subarray(off, off + chunkRawBytes);
        const b64 = await encodeClipboardChunk(worker, chunk);
        send(isText ? `cwd,${tid},${b64}` : `cbd,${tid},${b64}`);
        await new Promise(resolve => setTimeout(resolve, 0));
    }
    send(isText ? `cwe,${tid}` : `cbe,${tid}`);
}
