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

let lastSessionMode = localStorage.getItem(getPrefixedKey('streamMode'));
window.streamingMode = lastSessionMode ? lastSessionMode : STREAM_MODE_WEBSOCKETS;
console.log(`Streaming mode set to: ${window.streamingMode}`);

let mode = null;
function switchStreamingMode(newMode) {
    localStorage.setItem(getPrefixedKey('streamMode'), newMode);
    switch (newMode) {
        case STREAM_MODE_WEBRTC:
            mode = webrtc();
            mode.initialize();
            break;
        case STREAM_MODE_WEBSOCKETS:
            mode = websockets();
            break;
        default:
            throw new Error(`Invalid client mode: ${window.streamingMode} received, aborting`);
    }
}

window.addEventListener("message", handleMessage)
function handleMessage(event) {
    let message = event.data;
    if (message.type === "mode" && message.mode !== undefined) {
        console.log(`Switching streaming mode to: ${message.mode}`);
        localStorage.setItem(getPrefixedKey('streamMode'), message.mode);

        // wait for a few seconds to let the server switch modes
        setTimeout(() => {
            // TODO: for now going with a page reload rather than interchaning the modes
            window.location.reload();
        }, 3000)
    }
}

switchStreamingMode(window.streamingMode);
