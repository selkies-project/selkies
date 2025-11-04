# Selkies Core API 

This document outlines the API for an external dashboard to interact with the client-side Selkies Core application. Interaction primarily occurs via the standard `window.postMessage` mechanism and by observing specific global variables for statistics.


## Connection & Authentication Modes

Before interacting with the client via `postMessage`, it must first connect to the server. The client supports two primary modes for establishing its role and permissions, determined by the URL used to access the page.

### 1. Token Authentication Mode

This is the primary mode when connecting to a server running in its secure configuration. The client's role and capabilities are determined by a temporary token provided as a URL query parameter.

*   **URL Format:** `https://<server>/?token=<ACCESS_TOKEN>`
*   **Behavior:** The token is sent to the server during the WebSocket handshake. If valid, the server responds with the client's assigned role (e.g., `controller`, `viewer`) and properties (e.g., a gamepad `slot`). This mode takes precedence over the legacy hash mode.

### 2. Legacy Hash Mode
*   **URL Format:** `https://<server>/#<mode>` (e.g., `/#shared`, `/#player2`)
*   **Behavior:** This mode is used only if no `?token=` parameter is present in the URL. The fragment (`#...`) determines the client's role. This is intended for simpler, unsecured deployments.

## 1. Window Messaging API (Dashboard -> Client)

The client listens for messages sent via `window.postMessage`. To ensure security, the client **only accepts messages from the same origin** (`event.origin === window.location.origin`).

All messages sent to the client should be JavaScript objects with a `type` property indicating the action to perform.

### Supported Messages:

---

**Type:** `setScaleLocally`

*   **Payload:** `{ type: 'setScaleLocally', value: <boolean> }`
*   **Description:** Sets the client-side preference for how video is scaled when using manual resolution.
    *   `true`: Scales the video canvas locally to fit within the container while maintaining the aspect ratio set by the manual resolution (letterboxing/pillarboxing may occur).
    *   `false`: Renders the canvas at the exact manual resolution, potentially overflowing the container or appearing smaller.
    *   This setting is persisted in `localStorage` (`appName_scaleLocallyManual`). It only takes visual effect when `isManualResolutionMode` is active.

---

**Type:** `showVirtualKeyboard`

*   **Payload:** `{ type: 'showVirtualKeyboard' }`
*   **Description:** Attempts to focus a hidden input element (`#keyboard-input-assist`) on the page. This is intended as a workaround on mobile devices or touch environments to bring up the operating system's virtual keyboard for text input, which is then captured and forwarded by the client's input handler.

---

**Type:** `setManualResolution`

*   **Payload:** `{ type: 'setManualResolution', width: <number>, height: <number> }`
*   **Description:** Switches the client to manual resolution mode.
    *   Disables automatic resizing based on the window/container size.
    *   Sends the specified `width` and `height` (rounded down to the nearest even number) to the server via the active connection (WebRTC DataChannel or WebSocket message `r,WIDTHxHEIGHT`).
    *   Applies local canvas styling based on the `scaleLocallyManual` setting (see `setScaleLocally`).

---

**Type:** `resetResolutionToWindow`

*   **Payload:** `{ type: 'resetResolutionToWindow' }`
*   **Description:** Disables manual resolution mode and reverts to automatic resizing.
    *   Enables the window resize listener.
    *   Calculates the current container size, rounds it down to even numbers, and sends it to the server.
    *   Resets the canvas CSS styles to fill the container (`100%` width/height, `object-fit: contain`).

---

**Type:** `settings`

*   **Payload:** `{ type: 'settings', settings: <object> }`
*   **Description:** Applies one or more client-side settings and attempts to propagate them to the server. Settings are persisted in `localStorage`.
*   **Supported `settings` object properties:**
    *   `videoBitRate`: (Number) Target video bitrate in KBs (e.g., `8000`). Sends `vb,VALUE` (WebRTC) or `SET_VIDEO_BITRATE,VALUE` (WebSocket).
    *   `videoFramerate`: (Number) Target video framerate (e.g., `60`). Sends `_arg_fps,VALUE` (WebRTC) or `SET_FRAMERATE,VALUE` (WebSocket).
    *   `audioBitRate`: (Number) Target audio bitrate in kbit/s (e.g., `320000`). Sends `ab,VALUE` (WebRTC) or `SET_AUDIO_BITRATE,VALUE` (WebSocket).
    *   `encoder`: (String) Preferred video encoder name (e.g., `'x264enc'`). Sends `enc,VALUE` (WebRTC) or `SET_ENCODER,VALUE` (WebSocket).
    *   `videoBufferSize`: (Number) Target number of video frames to buffer on the client before rendering (0 = immediate). Affects client-side rendering latency.
    *   `resizeRemote`: (Boolean) *WebRTC Only*. If `true`, sends resolution updates to the server when the window resizes. Sends `_arg_resize,VALUE,WIDTHxHEIGHT`.
    *   `scaleLocal`: (Boolean) *WebRTC Only (Legacy?)*. Toggles a 'scale' CSS class on the video element.
    *   `turnSwitch`: (Boolean) *WebRTC Only*. If `true`, forces the use of TURN servers for the connection. Requires page reload to take effect.
    *   `debug`: (Boolean) *WebRTC Only*. Enables verbose debug logging. Requires page reload to take effect.

---

**Type:** `clipboardUpdateFromUI`

*   **Payload:** `{ type: 'clipboardUpdateFromUI', text: <string> }`
*   **Description:** Sends the provided `text` to the server as the new client clipboard content. The text is Base64 encoded before sending (`cw,BASE64_TEXT`). This is typically triggered when the user modifies the clipboard textarea in the sidebar.

---

**Type:** `pipelineControl`

*   **Payload:** `{ type: 'pipelineControl', pipeline: <string>, enabled: <boolean> }`
*   **Description:** Attempts to enable or disable specific media pipelines.
*   **Supported `pipeline` values:**
    *   `'video'`: (WebSocket Mode Only) Sends `START_VIDEO` or `STOP_VIDEO` message to the server. Updates internal state `isVideoPipelineActive`.
    *   `'audio'`: (WebSocket Mode Only) Sends `START_AUDIO` or `STOP_AUDIO` message to the server. Updates internal state `isAudioPipelineActive`.
    *   `'microphone'`: Toggles microphone capture locally using `startMicrophoneCapture()` or `stopMicrophoneCapture()`. Updates internal state `isMicrophoneActive`. Captured audio (if enabled) is sent over the WebSocket connection.

---

**Type:** `audioDeviceSelected`

*   **Payload:** `{ type: 'audioDeviceSelected', context: <string>, deviceId: <string> }`
*   **Description:** Sets the preferred audio device for input or output.
*   **Supported `context` values:**
    *   `'input'`: Sets the `preferredInputDeviceId`. If the microphone is currently active, it will be restarted to use the new device.
    *   `'output'`: Sets the `preferredOutputDeviceId`. Attempts to apply this preference to the playback `AudioContext` and `<audio>` element using `setSinkId()` (if supported by the browser).

---

**Type:** `gamepadControl`

*   **Payload:** `{ type: 'gamepadControl', enabled: <boolean> }`
*   **Description:** Enables or disables the client's gamepad input processing and forwarding. Updates internal state `isGamepadEnabled` and calls `enable()` or `disable()` on the `GamepadManager`.

---

**Type:** `requestFullscreen`

*   **Payload:** `{ type: 'requestFullscreen' }`
*   **Description:** Triggers the client's internal `enterFullscreen()` function, which attempts to make the video container fullscreen using the browser's Fullscreen API.

## 2. Client State & Statistics (Client -> Dashboard)

The client exposes certain state information and statistics directly through global JavaScript variables on the `window` object or as properties of globally accessible objects. An external dashboard can read these variables to get real-time information. The client also sends other status updates (not primary statistics) via `window.postMessage`.

### Key Global Variables:

*   `window.fps`: (Number) Calculated client-side rendering frames per second.
*   `window.currentAudioBufferSize`: (Number) Number of audio buffers currently queued in the playback AudioWorklet.
*   `videoFrameBuffer.length`: (Number) Number of video frames currently buffered client-side before rendering. This global array's length can be read directly.
*   `connectionStat`: (Object) Contains WebRTC connection statistics (latency, bitrate, packets lost, codec, resolution, etc.). This object is globally accessible and updated by the client. Structure:
    ```javascript
    {
      connectionStatType: 'unknown' | 'webrtc' | 'websocket',
      connectionLatency: 0, // Round trip time (WebRTC specific)
      connectionVideoLatency: 0, // Video specific latency (WebRTC)
      connectionAudioLatency: 0, // Audio specific latency (WebRTC)
      connectionAudioCodecName: 'NA',
      connectionAudioBitrate: 0,
      connectionPacketsReceived: 0,
      connectionPacketsLost: 0,
      connectionBytesReceived: 0,
      connectionBytesSent: 0,
      connectionCodec: 'unknown', // Video Codec
      connectionVideoDecoder: 'unknown',
      connectionResolution: '', // e.g., "1920x1080"
      connectionFrameRate: 0,
      connectionVideoBitrate: 0,
      connectionAvailableBandwidth: 0 // WebRTC specific
    }
    ```
*   `gpuStat`: (Object) Server-reported GPU statistics (if available). This object is globally accessible and updated by the client. Structure: `{ gpuLoad: 0, gpuMemoryTotal: 0, gpuMemoryUsed: 0 }`.
*   `cpuStat`: (Object) Server-reported CPU/Memory statistics (if available via WebSocket `system_stats`). This object is globally accessible and updated by the client. (Note: Code shows `window.system_stats` receives this, and `cpuStat` is then updated). Structure: `{ serverCPUUsage: 0, serverMemoryTotal: 0, serverMemoryUsed: 0 }`.
*   `serverClipboardContent`: (String) The last known clipboard content received from the server. Globally accessible.
*   `isVideoPipelineActive`: (Boolean) Client's belief about whether the video pipeline (receiving/decoding/rendering) is active. Globally accessible.
*   `isAudioPipelineActive`: (Boolean) Client's belief about whether the audio pipeline (receiving/decoding/playback) is active. Globally accessible.
*   `isMicrophoneActive`: (Boolean) Whether the client is currently capturing microphone audio. Globally accessible.
*   `isGamepadEnabled`: (Boolean) Whether gamepad input processing is enabled. Globally accessible.

### Messages Sent from Client to Dashboard:

*   **Type:** `pipelineStatusUpdate`
    *   **Payload:** `{ type: 'pipelineStatusUpdate', video?: <boolean>, audio?: <boolean>, microphone?: <boolean>, gamepad?: <boolean> }`
    *   **Description:** Sent when the client's internal state for pipelines changes (e.g., after receiving confirmation from the server in WebSocket mode, or toggling locally). Used to keep the dashboard UI (like toggle buttons) in sync.

*   **Type:** `sidebarButtonStatusUpdate`
    *   **Payload:** `{ type: 'sidebarButtonStatusUpdate', video: <boolean>, audio: <boolean>, microphone: <boolean>, gamepad: <boolean> }`
    *   **Description:** Sent *by* the client *to itself* after a state change to trigger UI updates.

*   **Type:** `clipboardContentUpdate`
    *   **Payload:** `{ type: 'clipboardContentUpdate', text: <string> }`
    *   **Description:** Sent when the client receives new clipboard content from the server (via WebSocket `clipboard,...` or WebRTC datachannel).

*   **Type:** `fileUpload`
    *   **Payload:** `{ type: 'fileUpload', payload: <object> }`
    *   **Description:** Sent during file uploads initiated via drag-and-drop or the file input. The `payload` object indicates the status.
    *   **Payload `status` values:**
        *   `'start'`: `{ status: 'start', fileName: <string>, fileSize: <number> }`
        *   `'progress'`: `{ status: 'progress', fileName: <string>, progress: <number (0-100)>, fileSize: <number> }`
        *   `'end'`: `{ status: 'end', fileName: <string>, fileSize: <number> }`
        *   `'error'`: `{ status: 'error', fileName: <string>, message: <string> }`

*   **Type:** `gamepadButtonUpdate` / `gamepadAxisUpdate`
    *   **Payload:** `{ type: 'gamepadButtonUpdate', gamepadIndex: <number>, buttonIndex: <number>, value: <number> }` or `{ type: 'gamepadAxisUpdate', gamepadIndex: <number>, axisIndex: <number>, value: <number> }`
    *   **Description:** Sent *by* the client *to itself* when gamepad input is detected.

## 3. Replicating UI Interactions

An external dashboard needs to implement the following:

1.  **Settings Controls:** Use the `settings` message type to send changes for bitrate, framerate, encoder, etc.
2.  **Pipeline Toggles:** Use the `pipelineControl` message to toggle Video, Audio (WebSocket only), and Microphone pipelines. Listen for `pipelineStatusUpdate` to update the button states.
3.  **Gamepad Toggle & Visualization:** Use `gamepadControl` to toggle gamepad input. Listen for `gamepadButtonUpdate` and `gamepadAxisUpdate` messages to update a custom gamepad visualizer.
4.  **Resolution Control:**
    *   Implement inputs/dropdowns for manual width/height.
    *   Send `setManualResolution` on apply.
    *   Implement a checkbox for "Scale Locally" and send `setScaleLocally`.
    *   Implement a "Reset" button sending `resetResolutionToWindow`.
5.  **Fullscreen:** Implement a button sending the `requestFullscreen` message.
6.  **Stats Display:** Periodically or on demand, directly read the relevant global variables from the client's `window` object (e.g., `window.fps`, `window.currentAudioBufferSize`, `videoFrameBuffer.length`, `connectionStat`, `gpuStat`, `cpuStat`) and display the information.
7.  **Server Clipboard:**
    *   Display clipboard content received via the `clipboardContentUpdate` message.
    *   Allow editing and send changes back using the `clipboardUpdateFromUI` message.
8.  **File Upload:**
    *   Implement a file input button. When clicked, dispatch a `CustomEvent('requestFileUpload')` on the client's `window` object (`window.dispatchEvent(new CustomEvent('requestFileUpload'))`). This triggers the client's hidden file input.
    *   *(Alternative/DragDrop)*: Implement drag-and-drop handling. The client overlay already handles this, sending files via WebSocket. Replicating this fully externally might be complex, but triggering the file input is the standard sidebar approach.
    *   Listen for `fileUpload` messages to display upload progress and status.
9.  **Virtual Keyboard:** Implement a button sending the `showVirtualKeyboard` message for environments needing the OSK.
10. **Audio Device Selection:**
    *   Query `navigator.mediaDevices.enumerateDevices()` (requires user permission first, often obtained via a temporary `getUserMedia({audio: true})` call).
    *   Populate dropdowns for audio input and output devices.
    *   On selection change, send the `audioDeviceSelected` message with the appropriate `context` ('input' or 'output') and `deviceId`.

Remember to handle the origin check when sending messages and potentially when receiving them if the dashboard itself needs security.
