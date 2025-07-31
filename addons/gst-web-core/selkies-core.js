/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import webrtc from "./selkies-wr-core";
import websockets from "./selkies-ws-core";

const STREAM_MODE_WEBRTC = "webrtc";
const STREAM_MODE_WEBSOCKETS = "websockets";

// The client streaming mode to use; it'll be assigned a mode webrtc/websockets at container startup,
// essentially making the 'mode' a deployment config. This approach might change in future.
window.streamingMode=null;

// Store the mode so the dashboard can pick it up and render necessary components
window.localStorage.setItem("streamMode", window.streamingMode)

let mode = null;
switch (window.streamingMode) {
    case STREAM_MODE_WEBRTC:
        // TODO: For now there's no option to switch to other modes, as it's a deployment config.
        // Implement the similar approach for websockets and then use cleanup() func appropriately
        // while changing the mode of stream, which enables us to change modes on the fly.
        mode = webrtc();
        mode.initialize();
        break;
    case STREAM_MODE_WEBSOCKETS:
        websockets();
        break;
    default:
        throw new Error(`Invalid client mode: ${window.streamingMode} received, aborting`);
}