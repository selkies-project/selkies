import ClipboardWorker from '../clipboard-worker.js?worker&inline';

// Off-main-thread base64 for clipboard payloads, shared by both transports. The
// per-byte String.fromCharCode + btoa build of a multi-MB clipboard blocks the
// main thread for seconds (freezing video presentation and input dispatch that
// share it); this offloads encode/decode to clipboard-worker.js.
export class ClipboardWorkerBridge {
    constructor() {
        this.worker = null;
        this.callbacks = new Map();
        this.msgId = 0;
    }

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

    async encodeText(text) {
        this.init();
        return new Promise((resolve, reject) => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'ENCODE_TEXT_TO_B64', payload: text });
        });
    }

    // Zero-copy transfer: the passed ArrayBuffer is neutered, so callers pass a
    // buffer they own exclusively (a fresh/sliced copy, never a shared view).
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

    async decode(base64String, mimeType) {
        this.init();
        return new Promise((resolve, reject) => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'DECODE_FROM_B64', payload: base64String, mimeType });
        });
    }
}

// Base64 one clipboard byte-run off the main thread. A fresh slice gives the
// worker a buffer it can neuter via zero-copy transfer; on worker failure it
// degrades to a chunked main-thread encode (still far cheaper than a per-byte
// String.fromCharCode build).
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

// Shared WS/WebRTC clipboard SEND. Both transports emit the identical wire
// protocol (cw / cb single message; cws+cwd+cwe / cbs+cbd+cbe multipart) to the
// same server handler, which decodes each data chunk INDEPENDENTLY — so each raw
// chunk is base64'd on its own (never base64-whole-then-slice-the-string). Base64
// runs off the main thread per chunk with a yield between, so a multi-MB clipboard
// never blocks video presentation or input dispatch. Transport differences are
// only the injected send() and waitDrain() (backpressure); returning false from
// waitDrain aborts the transfer (channel closed).
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
