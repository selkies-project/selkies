/**
 * Off-main-thread base64 for clipboard payloads and the clipboard send path,
 * shared by both transports.
 *
 * A per-byte `String.fromCharCode` + `btoa` build of a multi-MB clipboard
 * blocks the main thread for seconds, freezing the video presentation and
 * input dispatch that share it, so encode and decode run in
 * clipboard-worker.ts. Both transports emit the identical wire protocol to
 * the same server handler: `cw` / `cb` as a single message, or the multipart
 * `cws`+`cwd`+`cwe` / `cbs`+`cbd`+`cbe` sequence for large payloads. The
 * server decodes each data chunk independently, so each raw chunk is
 * base64-encoded on its own, never the whole payload encoded and then sliced
 * as a string.
 * @module
 */
import ClipboardWorker from '../clipboard-worker.ts?worker&inline';

/**
 * Request/response bridge to the clipboard worker.
 *
 * The worker is created lazily on the first request and every call resolves
 * with `{ result, mimeType, byteLength }` from the worker's reply.
 */
export class ClipboardWorkerBridge {
    worker!: any;
    callbacks!: Map<any, any>;
    msgId!: number;
    constructor() {
        this.worker = null;
        this.callbacks = new Map();
        this.msgId = 0;
    }

    /** Creates the worker when it does not exist yet. */
    init(): void {
        if (!this.worker) {
            this.worker = new ClipboardWorker();
            this.worker.onmessage = (e: any): void => {
                const { id, success, result, error, mimeType, byteLength, hash } = e.data;
                const resolveReject = this.callbacks.get(id);
                if (resolveReject) {
                    this.callbacks.delete(id);
                    if (success) {
                        resolveReject.resolve({ result, mimeType, byteLength, hash });
                    } else {
                        resolveReject.reject(new Error(error));
                    }
                }
            };
            console.log("Clipboard Web Worker initialized.");
        }
    }

    /** Stops the worker and rejects every pending request with an `AbortError`. */
    terminate(): void {
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
     * @param text UTF-8 text to encode.
     * @returns
     */
    async encodeText(text: string): Promise<{ result: string; mimeType: string; byteLength: number }> {
        this.init();
        return new Promise((resolve: any, reject: any): void => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'ENCODE_TEXT_TO_B64', payload: text });
        });
    }

    /**
     * Encodes a buffer with a zero-copy transfer: the buffer is neutered, so
     * callers pass one they own exclusively (a fresh or sliced copy, never a
     * shared view).
     * @param arrayBuffer Bytes to encode; unusable afterwards.
     * @param hash Digest to continue, so a payload sent as several
     *     chunks is digested as the one payload it is.
     * @returns
     */
    async encodeBinary(arrayBuffer: ArrayBuffer, hash: number | undefined): Promise<{ result: string; mimeType: string; byteLength: number; hash: number }> {
        this.init();
        return new Promise((resolve: any, reject: any): void => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage(
                { id, action: 'ENCODE_BINARY_TO_B64', payload: arrayBuffer, hash },
                [arrayBuffer]
            );
        });
    }

    /**
     * Digests bytes that reached the page without passing through a codec.
     * @param arrayBuffer Bytes to digest; unusable afterwards.
     * @returns
     */
    async hashBytes(arrayBuffer: ArrayBuffer): Promise<{ byteLength: number; hash: number }> {
        this.init();
        return new Promise((resolve: any, reject: any): void => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'HASH_BYTES', payload: arrayBuffer }, [arrayBuffer]);
        });
    }

    /**
     * Re-encodes an image as PNG off the main thread.
     * @param blob The image.
     * @returns
     */
    async reencodePng(blob: Blob): Promise<{ result: Blob; mimeType: string; byteLength: number }> {
        this.init();
        return new Promise((resolve: any, reject: any): void => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'REENCODE_PNG', payload: blob });
        });
    }

    /**
     * @param base64String Payload to decode.
     * @param mimeType Type reported back with the decoded bytes.
     * @returns
     */
    async decode(base64String: string, mimeType: string): Promise<{ result: any; mimeType: string; byteLength: number }> {
        this.init();
        return new Promise((resolve: any, reject: any): void => {
            const id = ++this.msgId;
            this.callbacks.set(id, { resolve, reject });
            this.worker.postMessage({ id, action: 'DECODE_FROM_B64', payload: base64String, mimeType });
        });
    }

    /**
     * Opens a multipart download the worker accumulates.
     *
     * Each chunk is handed over as it arrives, so the page never holds the
     * whole base64 payload, let alone joins it into one string and copies
     * that into a message.
     * @param mimeType Type reported back with the decoded bytes.
     * @returns
     */
    decodeStream(mimeType: string): { push: (base64: string) => void; finish: () => Promise<{result: any, mimeType: string, byteLength: number, hash: number}>; abort: () => void } {
        this.init();
        const id = ++this.msgId;
        this.worker.postMessage({ id, action: 'DECODE_BEGIN', mimeType });
        return {
            push: (base64String: any): void => {
                if (this.worker) {
                    this.worker.postMessage({ id, action: 'DECODE_CHUNK', payload: base64String });
                }
            },
            finish: (): Promise<{result: any, mimeType: string, byteLength: number, hash: number}> => new Promise((resolve: any, reject: any): void => {
                if (!this.worker) {
                    reject(new Error('Worker Terminated'));
                    return;
                }
                this.callbacks.set(id, { resolve, reject });
                this.worker.postMessage({ id, action: 'DECODE_END' });
            }),
            abort: (): void => {
                if (this.worker) this.worker.postMessage({ id, action: 'DECODE_ABORT' });
            },
        };
    }
}

/** Seed of the digest the clipboard compares payloads by; matches the worker's. */
export const CLIPBOARD_HASH_SEED = 5381;

/**
 * Base64-encodes one clipboard byte run off the main thread, digesting it on
 * the way through.
 *
 * A fresh slice gives the worker a buffer it can neuter through zero-copy
 * transfer; on worker failure it degrades to a chunked main-thread encode,
 * still far cheaper than a per-byte `String.fromCharCode` build.
 * @param worker The bridge to encode through.
 * @param bytes The run to encode; left untouched.
 * @param hash Digest to continue across chunks.
 * @returns The base64 text and
 *     the digest through this run, `undefined` once any chunk fell back.
 */
export async function encodeClipboardChunk(worker: ClipboardWorkerBridge, bytes: Uint8Array, hash: number | undefined): Promise<{ b64: string; hash: number | undefined }> {
    try {
        const copy = bytes.slice();
        const out = await worker.encodeBinary(copy.buffer, hash);
        return { b64: out.result, hash: out.hash };
    } catch (e: any) {
        console.warn('Clipboard worker encode failed; falling back to main thread:', e);
        let s = '';
        for (let i = 0; i < bytes.length; i += 0x8000) {
            s += (String.fromCharCode as any).apply(null, bytes.subarray(i, i + 0x8000));
        }
        return { b64: btoa(s), hash: undefined };
    }
}

/**
 * Sends a clipboard payload, as one message when it fits a chunk and as a
 * multipart sequence otherwise.
 *
 * Each chunk is encoded off the main thread, so a multi-MB clipboard never
 * blocks video presentation or input dispatch, and the round trip to the
 * worker is itself the yield between chunks. The transports differ only in
 * the injected `send` and `waitDrain`, which is what keeps a transfer from
 * queueing ahead of the audio and input sharing the connection.
 * @param bytes The payload.
 * @param mimeType `text/plain` selects the text messages, anything
 *     else the binary ones.
 * @param io Transport hooks.
 * @param io.worker The bridge to encode through.
 * @param io.send Sends one wire message.
 * @param io.waitDrain Awaited before every
 *     chunk for backpressure; resolving `false` aborts the transfer (the
 *     channel closed).
 * @param io.chunkRawBytes Raw bytes per chunk.
 * @param io.nextTid Allocates the multipart transfer id.
 * @returns The payload's digest, or `undefined`
 *     where the worker was unavailable or the transfer was cut short.
 */
export async function sendClipboardChunked(bytes: Uint8Array, mimeType: string, { worker, send, waitDrain, chunkRawBytes, nextTid }: { "worker": any; "send": any; "waitDrain": any; "chunkRawBytes": any; "nextTid": any }): Promise<number | undefined> {
    const isText = mimeType === 'text/plain';
    const total = bytes.byteLength;
    if (total < chunkRawBytes) {
        const { b64, hash } = await encodeClipboardChunk(worker, bytes, CLIPBOARD_HASH_SEED);
        send(isText ? `cw,${b64}` : `cb,${mimeType},${b64}`);
        return hash;
    }
    const tid = nextTid();
    send(isText ? `cws,${tid},${total}` : `cbs,${tid},${mimeType},${total}`);
    let hash: number | undefined = CLIPBOARD_HASH_SEED;
    for (let off = 0; off < total; off += chunkRawBytes) {
        if (waitDrain) {
            const ok = await waitDrain();
            if (ok === false) return undefined;
        }
        const chunk = bytes.subarray(off, off + chunkRawBytes);
        const out = await encodeClipboardChunk(worker, chunk, hash);
        hash = out.hash;
        send(isText ? `cwd,${tid},${out.b64}` : `cbd,${tid},${out.b64}`);
    }
    send(isText ? `cwe,${tid}` : `cbe,${tid}`);
    return hash;
}
