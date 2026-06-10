/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import webrtc from "./selkies-wr-core";
import websockets from "./selkies-ws-core";

const STREAM_MODE_WEBRTC = "webrtc";
const STREAM_MODE_WEBSOCKETS = "websockets";

// Set storage key based on URL
const urlForKey = window.location.href.split('#')[0];
const storageAppName = urlForKey.replace(/[^a-zA-Z0-9.-_]/g, '_');
const getPrefixedKey = (key) => {return `${storageAppName}_${key}`}

let mode = null;

function determineStreamingMode() {
    // Check for runtime injected mode
    const runtimeMode = (typeof window !== 'undefined' && window.__SELKIES_STREAMING_MODE__) ? window.__SELKIES_STREAMING_MODE__ : undefined;
    let lastSessionMode = localStorage.getItem(getPrefixedKey('stream_mode'));
    // Precedence: runtime mode > last session mode > default mode
    const finalMode = runtimeMode ? runtimeMode : (lastSessionMode ? lastSessionMode : STREAM_MODE_WEBSOCKETS);
    console.log(`Streaming mode determined to be: ${finalMode}`);
    return finalMode;
}

function handleMessage(event) {
    let message = event.data;
    if (message.mode !== undefined && message.type === "mode") {
        console.log(`Switching streaming mode to: ${message.mode}`);
        localStorage.setItem(getPrefixedKey('stream_mode'), message.mode);

        // wait for a few seconds to let the server switch modes
        setTimeout(() => {
            // FIXME: for now going with a page reload rather than interchaning the modes without reload
            window.location.reload();
        }, 2000)
    }
}

function switchStreamingMode(newMode) {
    localStorage.setItem(getPrefixedKey('stream_mode'), newMode);
    switch (newMode) {
        case STREAM_MODE_WEBRTC:
            mode = webrtc();
            mode.initialize();
            break;
        case STREAM_MODE_WEBSOCKETS:
            mode = websockets();
            break;
        default:
            throw new Error(`Invalid client mode: ${newMode} received, aborting`);
    }
}

if (typeof window !== 'undefined') {
    window.addEventListener("message", handleMessage)
    window.selkiesCoreInitialize = function() {
        const streamingMode = determineStreamingMode();
        switchStreamingMode(streamingMode);
    };
}

// Auto-initialize for backward compatibility when script is loaded directly
// This preserves existing behavior for non-dashboard usage
if (typeof window !== 'undefined' && !window.__SELKIES_DEFER_INITIALIZATION) {
    window.selkiesCoreInitialize();
}
