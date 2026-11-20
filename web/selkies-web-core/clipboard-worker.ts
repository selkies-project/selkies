/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Worker entry for the clipboard codec: runs `lib/clipboard-codec.ts` off the
 * main thread, so a multi-MB base64 build never blocks video presentation or
 * input dispatch. `lib/clipboard-worker-bridge.ts` posts the requests and
 * receives the replies; the protocol is documented with the codec.
 * @module
 */
import { createClipboardCodec, type WorkerRequest } from './lib/clipboard-codec.ts';

const dispatch = createClipboardCodec((msg, transfer) => {
    if (transfer) self.postMessage(msg, transfer);
    else self.postMessage(msg);
});

/** Dispatches one bridge request and posts its reply. */
self.onmessage = function(e: MessageEvent<WorkerRequest>): void {
    dispatch(e.data);
};
