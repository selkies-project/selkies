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
import { createClipboardCodec, type WorkerRequest } from './clipboard-codec.ts';
import { readDecoded, readDigest, readEncoded, sized, type ClipboardDigest, type DecodedClipboard, type EncodedClipboard, type WorkerReply, type WorkerResult } from './clipboard-replies.ts';

export type { ClipboardDigest, DecodedClipboard, EncodedClipboard, WorkerResult } from './clipboard-replies.ts';

/**
 * Request/response bridge to the clipboard worker.
 *
 * The worker is created lazily on the first request and every call resolves
 * with `{ result, mimeType, byteLength }` from the worker's reply. Where the
 * worker cannot run (`new ClipboardWorker()` throws, or its `onerror` fires, as
 * under `Content-Security-Policy: worker-src 'none'`), the bridge swaps in the
 * same codec on the main thread and replays what the worker never answered.
 * Until the worker has answered once, requests go to it without a transfer
 * list, so their buffers stay usable for that replay.
 */
/** A request to the worker, less the id the bridge gives it. */
interface BridgeRequest {
    action: string;
    payload?: string | ArrayBuffer | Blob;
    mimeType?: string;
    hash?: number;
}

/** A multipart decode in progress (`ClipboardWorkerBridge.decodeStream`). */
export interface ClipboardDecodeStream {
    /** Feeds one base64 chunk. */
    push: (base64: string) => void;
    /** Ends the transfer and resolves with what it decoded to. */
    finish: () => Promise<DecodedClipboard>;
    /** Drops the transfer. */
    abort: () => void;
}

export class ClipboardWorkerBridge {
    worker!: Worker | null;
    callbacks!: Map<number, { resolve: (value: WorkerResult) => void; reject: (reason: Error) => void }>;
    msgId!: number;
    /** Whether the worker has answered, proving its script loaded. */
    workerReady!: boolean;
    /** The main-thread codec, once the worker proved unusable. */
    inProcess!: ((req: WorkerRequest) => void) | null;
    /** Requests posted to a worker that has not answered yet, for replay. */
    pendingRequests!: Map<number, WorkerRequest>;
    /** Multipart downloads begun on a worker that has not answered yet, for replay. */
    pendingStreams!: Map<number, { mimeType: string; chunks: string[]; ended: boolean }>;
    constructor() {
        this.worker = null;
        this.callbacks = new Map();
        this.msgId = 0;
        this.workerReady = false;
        this.inProcess = null;
        this.pendingRequests = new Map();
        this.pendingStreams = new Map();
    }

    /** Creates the worker when there is neither it nor the main-thread codec yet. */
    init(): void {
        if (this.worker || this.inProcess) return;
        try {
            this.worker = new ClipboardWorker();
        } catch {
            this.fallBackToMainThread();
            return;
        }
        this.worker.onmessage = (e: MessageEvent<WorkerReply>): void => {
            this.workerReady = true;
            this.onReply(e.data);
        };
        this.worker.onerror = (): void => {
            this.fallBackToMainThread();
        };
        console.log("Clipboard Web Worker initialized.");
    }

    /**
     * Settles the request a reply answers, whether it came from the worker or
     * the main-thread codec.
     * @param data The reply.
     */
    private onReply(data: WorkerReply): void {
        const { id, success, result, error, mimeType, byteLength, hash } = data;
        this.pendingRequests.delete(id);
        this.pendingStreams.delete(id);
        const resolveReject = this.callbacks.get(id);
        if (!resolveReject) return;
        this.callbacks.delete(id);
        if (success) resolveReject.resolve({ result, mimeType, byteLength, hash });
        else resolveReject.reject(new Error(error));
    }

    /** Drops the worker for the main-thread codec and replays what the worker never answered. */
    private fallBackToMainThread(): void {
        if (this.worker) {
            try { this.worker.terminate(); } catch { /* gone */ }
            this.worker = null;
        }
        this.workerReady = false;
        console.warn("Clipboard Web Worker unavailable; running the clipboard codec on the main thread.");
        this.inProcess ??= createClipboardCodec((reply) => this.onReply(reply));
        const codec = this.inProcess;
        const requests = [...this.pendingRequests.values()];
        const streams = [...this.pendingStreams.entries()];
        this.pendingRequests.clear();
        this.pendingStreams.clear();
        for (const req of requests) codec(req);
        for (const [id, s] of streams) {
            codec({ id, action: 'DECODE_BEGIN', mimeType: s.mimeType });
            for (const chunk of s.chunks) codec({ id, action: 'DECODE_CHUNK', payload: chunk });
            if (s.ended) codec({ id, action: 'DECODE_END' });
        }
    }

    /** Stops the worker and rejects every pending request with an `AbortError`. */
    terminate(): void {
        if (!this.worker && !this.inProcess) return;
        if (this.worker) this.worker.terminate();
        this.worker = null;
        this.inProcess = null;
        this.pendingRequests.clear();
        this.pendingStreams.clear();
        this.workerReady = false;
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
     * Posts one request and waits for its reply.
     * @param message The request; its `id` is filled in.
     * @param transfer Buffers handed over with it.
     * @returns The successful reply.
     */
    private request(message: BridgeRequest, transfer: Transferable[] = []): Promise<WorkerResult> {
        this.init();
        return new Promise((resolve: (value: WorkerResult) => void, reject: (reason: Error) => void): void => {
            const id = ++this.msgId;
            const req: WorkerRequest = { id, ...message };
            this.callbacks.set(id, { resolve, reject });
            if (this.inProcess) {
                this.inProcess(req);
            } else if (this.workerReady) {
                this.worker?.postMessage(req, transfer);
            } else {
                // No transfer: a buffer detached now could not be replayed if the worker never loads.
                this.pendingRequests.set(id, req);
                this.worker?.postMessage(req);
            }
        });
    }

    /**
     * @param text UTF-8 text to encode.
     * @returns
     */
    async encodeText(text: string): Promise<EncodedClipboard> {
        return readEncoded(await this.request({ action: 'ENCODE_TEXT_TO_B64', payload: text }));
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
    async encodeBinary(arrayBuffer: ArrayBuffer, hash: number | undefined): Promise<EncodedClipboard & ClipboardDigest> {
        const reply = await this.request({ action: 'ENCODE_BINARY_TO_B64', payload: arrayBuffer, hash }, [arrayBuffer]);
        return { ...readEncoded(reply), ...readDigest(reply) };
    }

    /**
     * Digests bytes that reached the page without passing through a codec.
     * @param arrayBuffer Bytes to digest; unusable afterwards.
     * @returns
     */
    async hashBytes(arrayBuffer: ArrayBuffer): Promise<ClipboardDigest> {
        return readDigest(await this.request({ action: 'HASH_BYTES', payload: arrayBuffer }, [arrayBuffer]));
    }

    /**
     * Re-encodes an image as PNG off the main thread.
     * @param blob The image.
     * @returns
     */
    async reencodePng(blob: Blob): Promise<{ result: Blob; mimeType: string; byteLength: number }> {
        const reply = await this.request({ action: 'REENCODE_PNG', payload: blob });
        if (!(reply.result instanceof Blob)) throw new Error("the clipboard worker's re-encode reply carries no image");
        return { result: reply.result, ...sized(reply, 're-encode') };
    }

    /**
     * @param base64String Payload to decode.
     * @param mimeType Type reported back with the decoded bytes.
     * @returns
     */
    async decode(base64String: string, mimeType: string): Promise<DecodedClipboard> {
        return readDecoded(await this.request({ action: 'DECODE_FROM_B64', payload: base64String, mimeType }));
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
    decodeStream(mimeType: string): ClipboardDecodeStream {
        this.init();
        const id = ++this.msgId;
        if (this.inProcess) {
            this.inProcess({ id, action: 'DECODE_BEGIN', mimeType });
        } else if (this.worker) {
            if (!this.workerReady) this.pendingStreams.set(id, { mimeType, chunks: [], ended: false });
            this.worker.postMessage({ id, action: 'DECODE_BEGIN', mimeType });
        }
        return {
            push: (base64String: string): void => {
                if (this.inProcess) {
                    this.inProcess({ id, action: 'DECODE_CHUNK', payload: base64String });
                } else if (this.worker) {
                    this.pendingStreams.get(id)?.chunks.push(base64String);
                    this.worker.postMessage({ id, action: 'DECODE_CHUNK', payload: base64String });
                }
            },
            finish: (): Promise<DecodedClipboard> => new Promise((resolve: (value: WorkerResult) => void, reject: (reason: Error) => void): void => {
                if (this.inProcess) {
                    this.callbacks.set(id, { resolve, reject });
                    this.inProcess({ id, action: 'DECODE_END' });
                } else if (this.worker) {
                    const ps = this.pendingStreams.get(id);
                    if (ps) ps.ended = true;
                    this.callbacks.set(id, { resolve, reject });
                    this.worker.postMessage({ id, action: 'DECODE_END' });
                } else {
                    reject(new Error('Worker Terminated'));
                }
            }).then(readDecoded),
            abort: (): void => {
                this.pendingStreams.delete(id);
                if (this.inProcess) this.inProcess({ id, action: 'DECODE_ABORT' });
                else if (this.worker) this.worker.postMessage({ id, action: 'DECODE_ABORT' });
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
    } catch (e) {
        console.warn('Clipboard worker encode failed; falling back to main thread:', e);
        let s = '';
        for (let i = 0; i < bytes.length; i += 0x8000) {
            s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
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
export async function sendClipboardChunked(bytes: Uint8Array, mimeType: string, { worker, send, waitDrain, chunkRawBytes, nextTid }: { worker: ClipboardWorkerBridge; send: (message: string) => void; waitDrain?: (() => Promise<boolean | void>) | null; chunkRawBytes: number; nextTid: () => number }): Promise<number | undefined> {
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
