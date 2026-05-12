# Selkies Core API 

This document outlines the API for an external dashboard to interact with the client-side Selkies Core application. Interaction primarily occurs via the standard `window.postMessage` mechanism.

## Connection & Authentication Modes

Before interacting with the client via `postMessage`, it must first connect to the server. The client supports multiple modes for establishing its role and permissions, determined by the URL used to access the page.

### 1. Token Authentication Mode (Primary)
*   **URL Format:** `https://<server>/?token=<ACCESS_TOKEN>`
*   **Behavior:** The token is sent to the server during the WebSocket handshake. If valid, the server responds with the client's assigned role (e.g., `controller`, `viewer`), permissions, and controller `slot`. This mode takes precedence over legacy modes.

### 2. Legacy Hash & Multi-Monitor Modes
*   **URL Format:** `https://<server>/#<mode>` (e.g., `/#shared`, `/#player2`, `/#display2-right`)
*   **Behavior:** Used if no `?token=` parameter is present. 
    *   `#shared` or `#playerX`: Assigns a specific controller slot or viewer role.
    *   `#display2-<position>`: Configures the client to act as a secondary monitor extending the primary display.

---

## 1. Window Messaging API (Dashboard -> Client)

The client listens for messages sent via `window.postMessage`. To ensure security, the client **only accepts messages from the same origin** (`event.origin === window.location.origin`).

All messages sent to the client must be JavaScript objects with a `type` property.

### Display & Resolution Controls

*   **`setManualResolution`**
    *   **Payload:** `{ type: 'setManualResolution', width: <number>, height: <number> }`
    *   **Description:** Switches the client to manual resolution mode. Sends the new resolution to the server. *(Note: Ignored in shared/viewer mode).*
*   **`resetResolutionToWindow`**
    *   **Payload:** `{ type: 'resetResolutionToWindow' }`
    *   **Description:** Disables manual resolution mode and reverts to automatic resizing based on the browser window size. *(Note: Ignored in shared/viewer mode).*
*   **`setScaleLocally`**
    *   **Payload:** `{ type: 'setScaleLocally', value: <boolean> }`
    *   **Description:** When in manual resolution mode, `true` scales the video canvas locally to fit the container (letterboxing). `false` renders at exact pixel dimensions. *(Note: Ignored in shared/viewer mode).*
*   **`setUseCssScaling`**
    *   **Payload:** `{ type: 'setUseCssScaling', value: <boolean> }`
    *   **Description:** Toggles between CSS-based scaling and Canvas buffer pixel scaling based on Device Pixel Ratio (DPR).
*   **`setAntiAliasing`**
    *   **Payload:** `{ type: 'setAntiAliasing', value: <boolean> }`
    *   **Description:** Toggles canvas rendering between `auto` (smoothed) and `pixelated` (crisp edges).

### Media & Pipeline Controls

*   **`pipelineControl`**
    *   **Payload:** `{ type: 'pipelineControl', pipeline: <string>, enabled: <boolean> }`
    *   **Description:** Enables or disables specific media pipelines.
        *   `'video'`: Sends `START_VIDEO` / `STOP_VIDEO` to the server. *(Ignored in shared mode).*
        *   `'audio'`: Sends `START_AUDIO` / `STOP_AUDIO`. *(Ignored on secondary displays).*
        *   `'microphone'`: Toggles local microphone capture. *(Ignored in shared mode).*
*   **`setVolume`**
    *   **Payload:** `{ type: 'setVolume', value: <number> }` (0.0 to 1.0)
    *   **Description:** Adjusts the local audio playback volume via the Web Audio API GainNode.
*   **`setMute`**
    *   **Payload:** `{ type: 'setMute', value: <boolean> }`
    *   **Description:** Mutes (`true`) or unmutes (`false`) the local audio playback.
*   **`audioDeviceSelected`**
    *   **Payload:** `{ type: 'audioDeviceSelected', context: <string>, deviceId: <string> }`
    *   **Description:** Sets the preferred audio device. `context` can be `'input'` (microphone) or `'output'` (speakers).

### Input & Interaction Controls

*   **`gamepadControl`**
    *   **Payload:** `{ type: 'gamepadControl', enabled: <boolean> }`
    *   **Description:** Enables or disables gamepad input polling.
*   **`showVirtualKeyboard`**
    *   **Payload:** `{ type: 'showVirtualKeyboard' }`
    *   **Description:** Focuses a hidden input element to force mobile devices to display their OS virtual keyboard. *(Ignored in shared mode).*
*   **`setUseBrowserCursors`**
    *   **Payload:** `{ type: 'setUseBrowserCursors', value: <boolean> }`
    *   **Description:** Toggles between using local browser cursors vs. server-rendered video cursors.
*   **`touchinput:trackpad` / `touchinput:touch`**
    *   **Payload:** `{ type: 'touchinput:trackpad' }` or `{ type: 'touchinput:touch' }`
    *   **Description:** Switches touch interaction between relative trackpad mode and absolute touch mode.

### System & Settings

*   **`settings`**
    *   **Payload:** `{ type: 'settings', settings: <object> }`
    *   **Description:** Applies core stream settings and propagates them to the server via WebSocket.
    *   **Supported `settings` properties:**
        *   `framerate` (Number): Target FPS (e.g., 60).
        *   `rate_control_mode` (String): `'crf'` or `'cbr'`.
        *   `video_bitrate` (Number): Target bitrate in Mbps (used if CBR).
        *   `h264_crf` (Number): Constant Rate Factor for H.264 (used if CRF).
        *   `encoder` (String): Video encoder (e.g., `'x264enc'`, `'jpeg'`, `'x264enc-striped'`).
        *   `audio_bitrate` (Number): Audio bitrate in bps (e.g., 320000).
        *   `scaling_dpi` (Number): Custom DPI scaling for the remote desktop.
        *   `enable_binary_clipboard` (Boolean): Enables image copy/pasting.
        *   *Advanced Toggles:* `use_cpu`, `h264_fullcolor`, `h264_streaming_mode`, `jpeg_quality`, `use_paint_over_quality`.
*   **`clipboardUpdateFromUI`**
    *   **Payload:** `{ type: 'clipboardUpdateFromUI', text: <string> }`
    *   **Description:** Sends text from the local UI to the remote server's clipboard. *(Ignored in shared mode).*
*   **`command`**
    *   **Payload:** `{ type: 'command', value: <string> }`
    *   **Description:** Sends a raw arbitrary command string to the server via WebSocket. *(Ignored in shared mode).*
*   **`getStats`**
    *   **Payload:** `{ type: 'getStats' }`
    *   **Description:** Requests the client to immediately compile and post a `stats` message back to the dashboard.
*   **`sidebarVisibilityChanged`**
    *   **Payload:** `{ type: 'sidebarVisibilityChanged', isOpen: <boolean> }`
    *   **Description:** Notifies the client if the dashboard sidebar is open (used to throttle/enable stat calculations to save CPU).

---

## 2. Client State & Statistics (Client -> Dashboard)

The client pushes state changes and telemetry back to the parent window (`window.parent.postMessage`). 

### Core Telemetry & Status Messages

*   **`stats`**
    *   **Payload:** `{ type: 'stats', data: <object> }`
    *   **Description:** Sent in response to a `getStats` message. Contains complete telemetry:
        *   `clientFps`: Client-side rendering frames per second.
        *   `audioBuffer` / `videoBuffer`: Current frames queued in the WebCodecs/AudioWorklet buffers.
        *   `gpu` / `cpu` / `network`: Server-reported hardware and network statistics.
        *   `isVideoPipelineActive`, `isAudioPipelineActive`, `isMicrophoneActive`: Current pipeline states.
        *   `encoderName`: The actively negotiated video encoder.
*   **`pipelineStatusUpdate`**
    *   **Payload:** `{ type: 'pipelineStatusUpdate', video?: <boolean>, audio?: <boolean>, microphone?: <boolean>, gamepad?: <boolean> }`
    *   **Description:** Sent whenever a media pipeline starts, stops, or errors out. Use this to keep UI toggle buttons in sync.
*   **`sidebarButtonStatusUpdate`**
    *   **Payload:** `{ type: 'sidebarButtonStatusUpdate', video: <boolean>, audio: <boolean>, microphone: <boolean>, gamepad: <boolean> }`
    *   **Description:** Similar to `pipelineStatusUpdate`, explicitly formatted for updating external UI buttons.
*   **`clientRoleUpdate`**
    *   **Payload:** `{ type: 'clientRoleUpdate', role: <string> }`
    *   **Description:** Sent when the server authenticates the user and assigns a role (`'controller'`, `'viewer'`).
*   **`trackpadModeUpdate`**
    *   **Payload:** `{ type: 'trackpadModeUpdate', enabled: <boolean> }`
    *   **Description:** Sent on connection to sync the UI with the client's current trackpad mode state.

### Server Config & Clipboard

*   **`serverSettings`**
    *   **Payload:** `{ type: 'serverSettings', payload: <object> }`
    *   **Description:** Fired when the server pushes its configuration/settings constraints down to the client.
*   **`systemApps`**
    *   **Payload:** `{ type: 'systemApps', apps: <array> }`
    *   **Description:** Fired when the server sends a list of available applications.
*   **`clipboardContentUpdate`**
    *   **Payload:** `{ type: 'clipboardContentUpdate', text: <string> }`
    *   **Description:** Sent when the client receives new clipboard content from the server. If the payload was an image (binary clipboard), the text will read `"Image (mime/type) received from session and copied to clipboard."`

### File Uploads
*   **`fileUpload`**
    *   **Description:** Sent during file uploads (drag-and-drop or file input) to report upload progress.
    *   **Payload Variants:**
        *   **Start:** `{ type: 'fileUpload', payload: { status: 'start', fileName: <string>, fileSize: <number> } }`
        *   **Progress:** `{ type: 'fileUpload', payload: { status: 'progress', fileName: <string>, progress: <number (0-100)>, fileSize: <number> } }`
        *   **End:** `{ type: 'fileUpload', payload: { status: 'end', fileName: <string>, fileSize: <number> } }`
        *   **Error:** `{ type: 'fileUpload', payload: { status: 'error', fileName: <string>, message: <string> } }`

---

## 3. Replicating UI Interactions

An external dashboard needs to implement the following to fully control the iframe:

1.  **Settings Controls:** Use the `settings` message type to send changes for bitrate, framerate, encoder, etc.
2.  **Pipeline Toggles:** Use the `pipelineControl` message to toggle Video, Audio, and Microphone pipelines. Listen for `pipelineStatusUpdate` to ensure buttons reflect the actual state.
3.  **Gamepad Toggle:** Use `gamepadControl` to toggle gamepad input polling.
4.  **Resolution Control:**
    *   Implement inputs for manual width/height and send `setManualResolution`.
    *   Implement a checkbox for "Scale Locally" and send `setScaleLocally`.
    *   Implement a "Reset" button sending `resetResolutionToWindow`.
5.  **Stats Display:** Send a `getStats` message on a timer (e.g., every 1 second) and listen for the `stats` response event to populate your dashboard charts/metrics. Send `sidebarVisibilityChanged` to tell the client when to bother calculating these.
6.  **Server Clipboard:**
    *   Display text received via the `clipboardContentUpdate` message.
    *   Allow editing and send changes back using the `clipboardUpdateFromUI` message.
7.  **File Upload:**
    *   Implement a file input button. When clicked, dispatch a `CustomEvent('requestFileUpload')` on the client's `window` object (`window.dispatchEvent(new CustomEvent('requestFileUpload'))`). This triggers the client's hidden file input.
    *   Listen for `fileUpload` messages to display upload progress bars.
8.  **Audio Device Selection:**
    *   Query `navigator.mediaDevices.enumerateDevices()` in the dashboard.
    *   Populate dropdowns for audio input and output devices.
    *   On selection change, send the `audioDeviceSelected` message with the appropriate `context` ('input' or 'output') and `deviceId`.
