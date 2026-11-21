/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Base64 codec for clipboard payloads as a dispatcher that answers through a
 * callback, so one implementation runs in the clipboard worker and, where a
 * Content-Security-Policy refuses workers, on the main thread.
 *
 * A request is `{id, action, payload, mimeType}`, a reply `{id, success, result,
 * mimeType, byteLength, hash}` or `{id, success: false, error}`. A multipart
 * download is `DECODE_BEGIN`, one `DECODE_CHUNK` per message, and `DECODE_END`,
 * which alone answers, so the payload is never joined into one base64 string.
 * Replies carrying bytes carry `hash`, their djb2 digest, computed where the
 * bytes are already walked.
 * @module
 */
import { errorMessage } from './narrow.ts';
import type { WorkerReply } from './clipboard-replies.ts';

/** Bytes per `String.fromCharCode` call, keeping its argument list bounded. */
const CHUNK_SIZE = 0x8000;

/** Seed of the djb2 digest the clipboard compares payloads by. */
const HASH_SEED = 5381;

/** One bridge request handed to the codec. */
export interface WorkerRequest {
    id: number;
    action: string;
    payload?: ArrayBuffer | string | Blob;
    mimeType?: string;
    hash?: number;
}

/** A multipart download in progress: decoded parts, the undecodable tail, and the running digest. */
interface DecodeStream {
    mimeType?: string;
    parts: Uint8Array[];
    remainder: string;
    bytes: number;
    hash: number;
}

/** Sends one reply, handing over the buffers listed with it. */
type Reply = (msg: WorkerReply, transfer?: Transferable[]) => void;

/** A request's payload as text; throws where it is none, which the dispatcher replies with. */
function textOf(request: WorkerRequest): string {
    if (typeof request.payload !== 'string') throw new Error(`${request.action} takes text`);
    return request.payload;
}

/** A request's payload as bytes; throws where it is no ArrayBuffer. */
function bufferOf(request: WorkerRequest): ArrayBuffer {
    if (!(request.payload instanceof ArrayBuffer)) throw new Error(`${request.action} takes an ArrayBuffer`);
    return request.payload;
}

/** A request's payload as an image; throws where it is no Blob. */
function blobOf(request: WorkerRequest): Blob {
    if (!(request.payload instanceof Blob)) throw new Error(`${request.action} takes a Blob`);
    return request.payload;
}

/** djb2 over `bytes`, continuing from the digest `h` so one digest can span several chunks. */
function hashBytes(h: number, bytes: Uint8Array): number {
    for (let i = 0; i < bytes.length; i++) h = ((h << 5) + h + bytes[i]) | 0;
    return h;
}

/** Base64 of `bytes`, built in bounded `String.fromCharCode` argument lists. */
function bytesToBase64(bytes: Uint8Array): string {
    let binString = '';
    for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        binString += String.fromCharCode(...bytes.subarray(i, i + CHUNK_SIZE));
    }
    return btoa(binString);
}

/** The bytes `base64` decodes to. */
function base64ToBytes(base64: string): Uint8Array<ArrayBuffer> {
    const binString = atob(base64);
    const bytes = new Uint8Array(binString.length);
    for (let i = 0; i < binString.length; i++) bytes[i] = binString.charCodeAt(i);
    return bytes;
}

/** Appends decoded bytes to `stream`, continuing its digest. */
function appendBytes(stream: DecodeStream, bytes: Uint8Array): void {
    stream.parts.push(bytes);
    stream.bytes += bytes.length;
    stream.hash = hashBytes(stream.hash, bytes);
}

/**
 * Decodes as much of `text` as forms whole base64 quartets onto `stream`,
 * keeping the rest for the next chunk: a sender may split its base64 anywhere.
 * @param stream The transfer to append to.
 * @param text The next base64 chunk.
 */
function feedStream(stream: DecodeStream, text: string): void {
    const pending = stream.remainder + text;
    const whole = pending.length - (pending.length % 4);
    stream.remainder = pending.slice(whole);
    if (whole) appendBytes(stream, base64ToBytes(pending.slice(0, whole)));
}

/** The stream's bytes as one buffer, remainder included. */
function drainStream(stream: DecodeStream): Uint8Array<ArrayBuffer> {
    if (stream.remainder) appendBytes(stream, base64ToBytes(stream.remainder));
    stream.remainder = '';
    const all = new Uint8Array(stream.bytes);
    let at = 0;
    for (const part of stream.parts) {
        all.set(part, at);
        at += part.length;
    }
    stream.parts = [];
    return all;
}

/** The image as PNG, drawn on an OffscreenCanvas where there is one, else on a page canvas; throws where none can. */
async function drawPng(bmp: ImageBitmap): Promise<Blob> {
    if (typeof OffscreenCanvas !== 'undefined') {
        const canvas = new OffscreenCanvas(bmp.width, bmp.height);
        const ctx = canvas.getContext('2d');
        if (!ctx) throw new Error('no 2d context');
        ctx.drawImage(bmp, 0, 0);
        return await canvas.convertToBlob({ type: 'image/png' });
    }
    if (typeof document === 'undefined') throw new Error('no canvas available');
    const canvas = document.createElement('canvas');
    canvas.width = bmp.width;
    canvas.height = bmp.height;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('no 2d context');
    ctx.drawImage(bmp, 0, 0);
    return await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('PNG encode failed'))), 'image/png');
    });
}

/** Normalizes the image in `blob` to PNG and answers request `id` through `reply`. */
async function reencodeToPng(id: number, blob: Blob, reply: Reply): Promise<void> {
    let bmp: ImageBitmap | null = null;
    try {
        bmp = await createImageBitmap(blob);
        const png = await drawPng(bmp);
        reply({ id, success: true, result: png, mimeType: 'image/png', byteLength: png.size });
    } catch (err) {
        reply({ id, success: false, error: errorMessage(err) });
    } finally {
        if (bmp) bmp.close();
    }
}

/**
 * Creates a codec whose multipart downloads in progress are its own.
 * @param reply Receives every answer, with the buffers it hands over.
 * @returns The dispatcher for one request; failures are answered, never thrown.
 */
export function createClipboardCodec(reply: Reply): (data: WorkerRequest) => void {
    const streams = new Map<number, DecodeStream>();

    /** Answers with decoded bytes: text for `text/plain`, else the buffer, transferred. */
    const replyBytes = (id: number, bytes: Uint8Array<ArrayBuffer>, mimeType: string | undefined, hash: number | undefined): void => {
        const digest = hash === undefined ? {} : { hash };
        const { byteLength } = bytes;
        if (mimeType === 'text/plain') {
            reply({ id, success: true, mimeType, result: new TextDecoder().decode(bytes), byteLength, ...digest });
        } else {
            reply({ id, success: true, mimeType, result: bytes.buffer, byteLength, ...digest }, [bytes.buffer]);
        }
    };

    return (data: WorkerRequest): void => {
        const { id, action, mimeType } = data;
        try {
            if (action === 'DECODE_BEGIN') {
                streams.set(id, { mimeType, parts: [], remainder: '', bytes: 0, hash: HASH_SEED });
            } else if (action === 'DECODE_CHUNK') {
                const stream = streams.get(id);
                if (stream) feedStream(stream, textOf(data));
            } else if (action === 'DECODE_ABORT') {
                streams.delete(id);
            } else if (action === 'DECODE_END') {
                const stream = streams.get(id);
                streams.delete(id);
                if (!stream) {
                    reply({ id, success: false, error: 'no transfer in progress' });
                    return;
                }
                const bytes = drainStream(stream);
                replyBytes(id, bytes, stream.mimeType, stream.hash);
            } else if (action === 'HASH_BYTES') {
                const bytes = new Uint8Array(bufferOf(data));
                reply({ id, success: true, byteLength: bytes.byteLength, hash: hashBytes(HASH_SEED, bytes) });
            } else if (action === 'REENCODE_PNG') {
                void reencodeToPng(id, blobOf(data), reply);
            } else if (action === 'ENCODE_BINARY_TO_B64') {
                const bytes = new Uint8Array(bufferOf(data));
                reply({ id, success: true, result: bytesToBase64(bytes), byteLength: bytes.byteLength,
                        hash: hashBytes(data.hash === undefined ? HASH_SEED : data.hash, bytes) });
            } else if (action === 'ENCODE_TEXT_TO_B64') {
                reply({ id, success: true, result: bytesToBase64(new TextEncoder().encode(textOf(data))) });
            } else if (action === 'DECODE_FROM_B64') {
                const bytes = base64ToBytes(textOf(data));
                replyBytes(id, bytes, mimeType, mimeType === 'text/plain' ? undefined : hashBytes(HASH_SEED, bytes));
            } else {
                reply({ id, success: false, error: `Unknown action: ${action}` });
            }
        } catch (err) {
            reply({ id, success: false, error: errorMessage(err) });
        }
    };
}
