/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the clipboard worker answers, and the readers that check a reply
 * carries what its request resolves with. Apart from the bridge, so the Node
 * audits read replies with the same checks without loading the worker.
 * @module
 */

/** What the worker answers a request with; the fields beyond `id` and `success` depend on the action. */
export interface WorkerReply {
    /** The request's id. */
    id: number;
    /** Whether the action succeeded. */
    success: boolean;
    /** The encoded or decoded content. */
    result?: string | ArrayBuffer | Blob;
    /** Why the action failed. */
    error?: string;
    /** The content's MIME type. */
    mimeType?: string;
    /** The raw content's size in bytes. */
    byteLength?: number;
    /** The djb2 digest of the raw content. */
    hash?: number;
}

/** A successful reply as a request resolves with it. */
export type WorkerResult = Pick<WorkerReply, 'result' | 'mimeType' | 'byteLength' | 'hash'>;

/** Clipboard content decoded from base64: text for `text/plain`, bytes for anything else. */
export interface DecodedClipboard {
    /** The decoded content. */
    result: string | ArrayBuffer;
    /** The content's MIME type. */
    mimeType: string;
    /** The decoded size in bytes. */
    byteLength: number;
    /** The djb2 digest of the decoded bytes; text carries none. */
    hash?: number;
}

/** Base64 text the worker encoded. */
export interface EncodedClipboard {
    /** The base64 text. */
    result: string;
}

/** A digest the worker computed over bytes it was handed. */
export interface ClipboardDigest {
    /** How many bytes it digested. */
    byteLength: number;
    /** The djb2 digest through those bytes. */
    hash: number;
}

/**
 * Checks a reply carries the fields a request resolves with.
 * @param reply The worker's successful reply.
 * @param what The request, for the error.
 * @returns The reply's MIME type and size.
 * @throws Error where the worker left either out.
 */
export function sized(reply: WorkerResult, what: string): { mimeType: string; byteLength: number } {
    if (typeof reply.mimeType !== 'string' || typeof reply.byteLength !== 'number') {
        throw new Error(`the clipboard worker's ${what} reply carries no type or size`);
    }
    return { mimeType: reply.mimeType, byteLength: reply.byteLength };
}

/**
 * A reply to an encode, checked.
 * @param reply The worker's successful reply.
 * @returns What it encoded to.
 * @throws Error where the reply carries no text.
 */
export function readEncoded(reply: WorkerResult): EncodedClipboard {
    if (typeof reply.result !== 'string') throw new Error("the clipboard worker's encode reply carries no text");
    return { result: reply.result };
}

/**
 * A reply to a digest, checked.
 * @param reply The worker's successful reply.
 * @returns The digest and how many bytes it covers.
 * @throws Error where the reply carries no digest.
 */
export function readDigest(reply: WorkerResult): ClipboardDigest {
    if (typeof reply.byteLength !== 'number' || typeof reply.hash !== 'number') {
        throw new Error("the clipboard worker's digest reply carries no digest");
    }
    return { byteLength: reply.byteLength, hash: reply.hash };
}

/**
 * A reply to a decode, checked.
 * @param reply The worker's successful reply.
 * @returns What it decoded to.
 * @throws Error where the reply carries neither text nor bytes.
 */
export function readDecoded(reply: WorkerResult): DecodedClipboard {
    const { result } = reply;
    if (typeof result !== 'string' && !(result instanceof ArrayBuffer)) {
        throw new Error("the clipboard worker's decode reply carries neither text nor bytes");
    }
    return { result, ...sized(reply, 'decode'), ...(typeof reply.hash === 'number' ? { hash: reply.hash } : {}) };
}
