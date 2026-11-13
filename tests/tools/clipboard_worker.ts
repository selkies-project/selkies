/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The types of the clipboard worker's stand-in scope. The clipboard audits
 * load `clipboard-worker.ts` in process: each installs a `self` that takes
 * the worker's replies, and calls the handler the worker leaves on it with
 * the requests the bridge would post. Types only: nothing here exists at run
 * time.
 * @module
 */
import type { DecodedClipboard } from '../../web/selkies-web-core/lib/clipboard-worker-bridge.ts';

/** One request of the bridge, as the worker reads it. */
export interface WorkerRequest {
    id: number;
    action: string;
    payload?: ArrayBuffer | string | Blob;
    mimeType?: string;
    hash?: number;
}

/** One reply of the worker: what it decoded, coded or digested, or why it could not. */
export interface WorkerReply extends Partial<DecodedClipboard> {
    id: number;
    success: boolean;
    error?: string;
}

/** The worker's global scope, as the audits stand in for it. */
export interface WorkerScope {
    /** Takes one reply of the worker. */
    postMessage: (reply: WorkerReply) => void;
    /** The worker's handler, there once the worker has loaded. */
    onmessage?: (event: Pick<MessageEvent<WorkerRequest>, 'data'>) => void;
}

/** A reply awaited under its request's id. */
export interface Waiter {
    resolve: (reply: WorkerReply) => void;
    reject: (error: Error) => void;
}
