# Selkies Server

This document outlines the fundamental responsibilities and architecture of this server component, designed for real-time interactive streaming.

## Overview

This server provides the backend infrastructure for establishing and managing interactive streaming sessions. It is engineered with a dual-mode architecture to cater to different client requirements and network environments, offering both WebRTC-based peer-to-peer connections and direct WebSocket-based streaming.

## Core Responsibilities

1.  **Dual-Mode Streaming Architecture:**
    *   **WebRTC Mode:** Facilitates full peer-to-peer (P2P) interactive streaming. This mode manages the complete WebRTC lifecycle, including signaling, media transport negotiation (SRTP), and data channel communication for low-latency interaction.
    *   **WebSockets Streaming Mode:** Offers an alternative, server-relayed streaming approach. Media is streamed directly over WebSockets, supporting specialized, efficient protocols such as `jpeg-striped` and `x264enc-striped` (akin to the `pixelflux` methodology). This mode is advantageous in scenarios where P2P WebRTC is constrained or a simpler, direct stream is preferred.

2.  **WebRTC Connection Management (WebRTC Mode):**
    *   Handles Session Description Protocol (SDP) offer/answer exchanges.
    *   Manages Interactive Connectivity Establishment (ICE) candidate exchange for NAT traversal.
    *   Establishes secure media transport (SRTP) for audio and video.

3.  **Audio/Video Processing and Delivery:**
    *   Captures, encodes, and streams audio and video from the server.
    *   Utilizes GStreamer for flexible and high-performance media pipeline construction, supporting various encoders (e.g., `x264enc`).
    *   Delivers media via SRTP in WebRTC mode or custom WebSocket protocols (`jpeg-striped`, `x264enc-striped`) in WebSockets mode.
    *   Supports dynamic adjustments to streaming parameters such as bitrate and framerate.

4.  **Remote Input Handling:**
    *   Receives and processes user input events (e.g., mouse, keyboard, clipboard) from the client, primarily transmitted over WebRTC data channels.
    *   Injects these inputs into the server's environment to enable remote control and interaction.

5.  **Dynamic RTC Configuration (WebRTC Mode):**
    *   Manages STUN/TURN server configurations crucial for robust NAT traversal in WebRTC.
    *   Supports fetching RTC configurations from multiple sources:
        *   Static JSON configuration files.
        *   External REST APIs.
        *   Dynamically generated credentials (e.g., HMAC-based for TURN).
        *   Cloud-provider TURN services.
    *   Includes mechanisms for periodic monitoring and updating of these configurations.

6.  **Session and Display Adaptation:**
    *   Supports dynamic resizing of the remote display to match the client's viewport dimensions.
    *   Manages DPI scaling and remote cursor state updates to ensure a consistent user experience.

7.  **Integrated Web Server & Signaling:**
    *   Provides an embedded HTTP/HTTPS server to:
        *   Serve client-side web application assets.
        *   Host the WebSocket endpoint necessary for WebRTC signaling.
    *   Supports basic authentication for access control to the web server and signaling endpoints.

8.  **System Monitoring and Metrics:**
    *   Collects and exposes key performance indicators (KPIs) and metrics, including:
        *   System resource utilization (CPU, memory).
        *   GPU performance (if applicable).
        *   WebRTC and streaming connection statistics.

9.  **Token-Based Authentication & Authorization:**
    *   Features an optional secure mode, enabled by setting a `master_token`.
    *   When enabled, it exposes a control plane API on a separate port (`control_port`) to manage temporary user access tokens.
    *   Clients must connect with a valid token (`?token=...`) to establish a WebSocket connection.
    *   Assigns roles (e.g., `controller`, `viewer`) and properties (e.g., gamepad `slot`) to clients based on their token.
    *   Enforces permissions on the server-side, restricting actions that viewers can perform.
    *   Automatically disconnects clients if their token is revoked or their permissions change.

### Control Plane API for Token Management

When secure mode is enabled (`SELKIES_MASTER_TOKEN` is set), the server runs a control plane API on the `control_port` (default: 8083). This API is used to dynamically set and update the access tokens that clients can use to connect.

**Endpoint:** `POST /tokens`

**Authentication:** The request must include an `Authorization` header with the master token: `Authorization: Bearer <your-master-token>`

**Request Body:** A JSON object where each key is a unique access token string you create, and the value is a permissions object defining that token's capabilities.

**Permissions Object Fields:**
*   `"role"`: (String, required) Can be one of the following:
    *   `"controller"`: Full access. Can send keyboard, mouse, and all other input events (unless overridden by `mk_control`).
    *   `"viewer"`: Restricted access. Primarily for viewing the stream. Can be granted specific input rights via the `slot` or `mk_control` properties.
*   `"slot"`: (Integer or `null`, required) Assigns an input slot, primarily for gamepads.
    *   `null`: No specific input slot.
    *   `1` - `4`: Grants the user control over the specific virtual gamepad slot (Player 1 through Player 4).
*   `"mk_control"`: (Boolean, optional) Exclusive override for Mouse & Keyboard input.
    *   If `true` on **any** active token in the set, only that specific client processes mouse and keyboard events.
    *   If `false` or omitted on **all** active tokens, mouse and keyboard access defaults to clients with the `"controller"` role.

**Behavior:** When a valid request is received, the server replaces its entire set of active tokens with the new set provided in the payload. It then runs a reconciliation process:
1.  Clients with tokens not present in the new set are disconnected.
2.  Clients with tokens that remain valid but have changed permissions (`role`, `slot`, or `mk_control`) receive an immediate state update without disconnection.

**Example `curl` Command:**
```bash
curl -X POST http://localhost:8083/tokens \
-H "Authorization: Bearer my-secret-master-token" \
-H "Content-Type: application/json" \
-d '{
  "token-1": {"role": "controller", "slot": null, "mk_control": false},
  "token-2": {"role": "viewer", "slot": 1, "mk_control": true}
}'
```

## Technical Foundation

*   **Primary Language/Runtime:** Python, leveraging `asyncio` for efficient asynchronous operations and I/O handling.
*   **Media Framework:** GStreamer is extensively used for all media capture, encoding, and streaming pipeline management.
*   **Communication Protocols:**
    *   WebRTC (SDP, ICE, SRTP, Data Channels) for P2P mode.
    *   WebSockets for signaling (WebRTC mode) and as a direct media transport (WebSockets streaming mode with custom protocols).
    *   HTTP/HTTPS for asset delivery and signaling endpoint.

Of course. Here is a complete markdown section for your `README.md` based on the provided settings file. It explains the precedence, setting methods, special value types, and includes a comprehensive table of all available settings.

Of course. Here is the updated introductory text for your "Server Settings" section with the requested additions.

## Server Settings

The server's behavior can be extensively customized through command-line arguments or environment variables. This section details how to configure these settings.

### How Settings Work

#### Precedence Order
Settings are applied in the following order of precedence, with the first value found being used:
1.  **Command-Line (CLI) Arguments**: The highest precedence (e.g., `--port 9000`).
2.  **Standard Environment Variables**: The primary method for containerized environments (e.g., `export SELKIES_PORT=9000`).
3.  **Legacy Environment Variables**: Used as a fallback if a standard variable is not set (e.g., `export CUSTOM_WS_PORT=8888`). These are noted in the table where applicable.
4.  **Default Values**: The hardcoded default in the server code is used if no other value is provided.

#### Naming Convention
Settings are automatically named based on their variable name (e.g., `audio_enabled`):
*   **CLI Flag**: The name is converted to kebab-case: `--audio-enabled`
*   **Standard Environment Variable**: The name is prefixed with `SELKIES_` and converted to uppercase: `SELKIES_AUDIO_ENABLED`

### Setting Types and UI Customization

Certain setting types have special syntax for advanced control over the client-side UI and available options. A key concept is that **any setting that is locked to a single value will not be rendered in the UI**, giving the user no option to change it. This, combined with the various `ui_` visibility settings, allows administrators to completely customize the client interface.

#### Booleans and Locking
Boolean settings accept `true` or `false`. You can also prevent the user from changing a boolean setting in the UI by appending `|locked`. The UI toggle for this setting will be hidden.

*   **Example**: To force CPU encoding on and prevent the user from disabling it:
    ```bash
    export SELKIES_USE_CPU="true|locked"
    ```

#### Enums and Lists
These settings accept a comma-separated list of values. Their behavior depends on the number of items provided:

*   **Multiple Values**: The first item in the list becomes the default selection, and all items in the list become the available options in the UI dropdown.
*   **Single Value**: The provided value becomes the default, and the UI dropdown is hidden because the choice is locked.

*   **Example**: Force the encoder to be `jpeg` with no other options available to the user:
    ```bash
    export SELKIES_ENCODER="jpeg"
    ```

#### Ranges
Range settings define a minimum and maximum for a value (e.g., framerate).

*   **To set a range**: Use a hyphen-separated `min-max` format. The UI will show a slider.
*   **To set a fixed value**: Provide a single number. This will lock the value and hide the UI slider.

*   **Example**: Lock the framerate to exactly 60 FPS.
    ```bash
    export SELKIES_FRAMERATE="60"
    ```

#### Manual Resolution Mode
The server can be forced to use a single, fixed resolution for all connecting clients. This mode is automatically activated if `manual_width`, `manual_height`, or `is_manual_resolution_mode` is set.

*   If `manual_width` and/or `manual_height` are set, the resolution is locked to those values.
*   If `is_manual_resolution_mode` is set to `true` without specifying width or height, the resolution defaults to **1024x768**.
*   When this mode is active, the client UI for changing resolution is disabled.

### Available Settings

The table below lists all available server settings.

| Environment Variable | CLI Flag | Default Value | Description |
| -------------------- | -------- | ------------- | ----------- |
| `SELKIES_UI_TITLE` | `--ui-title` | `'Selkies'` | Title in top left corner of sidebar. |
| `SELKIES_UI_SHOW_LOGO` | `--ui-show-logo` | `True` | Show the Selkies logo in the sidebar. |
| `SELKIES_UI_SHOW_SIDEBAR` | `--ui-show-sidebar` | `True` | Show the main sidebar UI. |
| `SELKIES_UI_SHOW_CORE_BUTTONS` | `--ui-show-core-buttons` | `True` | Show the core components buttons display, audio, microphone, and gamepad. |
| `SELKIES_UI_SIDEBAR_SHOW_VIDEO_SETTINGS` | `--ui-sidebar-show-video-settings` | `True` | Show the video settings section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_SCREEN_SETTINGS` | `--ui-sidebar-show-screen-settings` | `True` | Show the screen settings section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_AUDIO_SETTINGS` | `--ui-sidebar-show-audio-settings` | `True` | Show the audio settings section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_STATS` | `--ui-sidebar-show-stats` | `True` | Show the stats section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_CLIPBOARD` | `--ui-sidebar-show-clipboard` | `True` | Show the clipboard section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_FILES` | `--ui-sidebar-show-files` | `True` | Show the file transfer section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_APPS` | `--ui-sidebar-show-apps` | `True` | Show the applications section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_SHARING` | `--ui-sidebar-show-sharing` | `True` | Show the sharing section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_GAMEPADS` | `--ui-sidebar-show-gamepads` | `True` | Show the gamepads section in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_FULLSCREEN` | `--ui-sidebar-show-fullscreen` | `True` | Show the fullscreen button in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_GAMING_MODE` | `--ui-sidebar-show-gaming-mode` | `True` | Show the gaming mode button in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_TRACKPAD` | `--ui-sidebar-show-trackpad` | `True` | Show the virtual trackpad button in the sidebar. |
| `SELKIES_UI_SIDEBAR_SHOW_KEYBOARD_BUTTON` | `--ui-sidebar-show-keyboard-button` | `True` | Show the on-screen keyboard button in the display area. |
| `SELKIES_UI_SIDEBAR_SHOW_SOFT_BUTTONS` | `--ui-sidebar-show-soft-buttons` | `True` | Show the soft buttons section in the sidebar. |
| `SELKIES_AUDIO_ENABLED` | `--audio-enabled` | `True` | Enable server-to-client audio streaming. |
| `SELKIES_MICROPHONE_ENABLED` | `--microphone-enabled` | `True` | Enable client-to-server microphone forwarding. |
| `SELKIES_GAMEPAD_ENABLED` | `--gamepad-enabled` | `True` | Enable gamepad support. |
| `SELKIES_CLIPBOARD_ENABLED` | `--clipboard-enabled` | `True` | Enable clipboard synchronization. |
| `SELKIES_COMMAND_ENABLED` | `--command-enabled` | `True` | Enable parsing of command websocket messages. |
| `SELKIES_FILE_TRANSFERS` | `--file-transfers` | `'upload,download'` | Allowed file transfer directions (comma-separated: "upload,download"). Set to "" or "none" to disable. |
| `SELKIES_ENCODER` | `--encoder` | `'x264enc'` | The default video encoder. |
| `SELKIES_FRAMERATE` | `--framerate` | `'8-120'` | Allowed framerate range or a fixed value. |
| `SELKIES_H264_CRF` | `--h264-crf` | `'5-50'` | Allowed H.264 CRF range or a fixed value. |
| `SELKIES_JPEG_QUALITY` | `--jpeg-quality` | `'1-100'` | Allowed JPEG quality range or a fixed value. |
| `SELKIES_H264_FULLCOLOR` | `--h264-fullcolor` | `False` | Enable H.264 full color range for pixelflux encoders. |
| `SELKIES_H264_STREAMING_MODE` | `--h264-streaming-mode` | `False` | Enable H.264 streaming mode for pixelflux encoders. |
| `SELKIES_USE_CPU` | `--use-cpu` | `False` | Force CPU-based encoding for pixelflux. |
| `SELKIES_USE_PAINT_OVER_QUALITY` | `--use-paint-over-quality` | `True` | Enable high-quality paint-over for static scenes. |
| `SELKIES_PAINT_OVER_JPEG_QUALITY` | `--paint-over-jpeg-quality` | `'1-100'` | Allowed JPEG paint-over quality range or a fixed value. |
| `SELKIES_H264_PAINTOVER_CRF` | `--h264-paintover-crf` | `'5-50'` | Allowed H.264 paint-over CRF range or a fixed value. |
| `SELKIES_H264_PAINTOVER_BURST_FRAMES` | `--h264-paintover-burst-frames` | `'1-30'` | Allowed H.264 paint-over burst frames range or a fixed value. |
| `SELKIES_SECOND_SCREEN` | `--second-screen` | `True` | Enable support for a second monitor/display. |
| `SELKIES_AUDIO_BITRATE` | `--audio-bitrate` | `'320000'` | The default audio bitrate. |
| `SELKIES_IS_MANUAL_RESOLUTION_MODE` | `--is-manual-resolution-mode` | `False` | Lock the resolution to the manual width/height values. |
| `SELKIES_MANUAL_WIDTH` | `--manual-width` | `0` | Lock width to a fixed value. Setting this forces manual resolution mode. |
| `SELKIES_MANUAL_HEIGHT` | `--manual-height` | `0` | Lock height to a fixed value. Setting this forces manual resolution mode. |
| `SELKIES_SCALING_DPI` | `--scaling-dpi` | `'96'` | The default DPI for UI scaling. |
| `SELKIES_ENABLE_BINARY_CLIPBOARD` | `--enable-binary-clipboard` | `False` | Allow binary data on the clipboard. |
| `SELKIES_USE_BROWSER_CURSORS` | `--use-browser-cursors` | `False` | Use browser CSS cursors instead of rendering to canvas. |
| `SELKIES_USE_CSS_SCALING` | `--use-css-scaling` | `False` | HiDPI when false, if true a lower resolution is sent from the client and the canvas is stretched. |
| `SELKIES_PORT` (or `CUSTOM_WS_PORT`) | `--port` | `8082` | Port for the data websocket server. |
| `SELKIES_CONTROL_PORT` | `--control-port` | `8083` | Port for the internal control plane API, used for managing access tokens when in secure mode. |
| `SELKIES_MASTER_TOKEN` | `--master-token` | `''` | Master token to enable secure mode. If set, clients must authenticate using tokens provided via the control plane API. |
| `SELKIES_DRI_NODE` (or `DRI_NODE`) | `--dri-node` | `''` | Path to the DRI render node for VA-API. |
| `SELKIES_AUDIO_DEVICE_NAME` | `--audio-device-name` | `'output.monitor'` | Audio device name for pcmflux capture. |
| `SELKIES_WATERMARK_PATH` (or `WATERMARK_PNG`) | `--watermark-path` | `''` | Absolute path to the watermark PNG file. |
| `SELKIES_WATERMARK_LOCATION` (or `WATERMARK_LOCATION`) | `--watermark-location` | `-1` | Watermark location enum (0-6). |
| `SELKIES_DEBUG` | `--debug` | `False` | Enable debug logging. |
| `SELKIES_WAYLAND_SOCKET_INDEX` | `--wayland-socket-index` | `0` | Index for the Wayland command socket (e.g. 0 for wayland-0). |
| `SELKIES_ENABLE_SHARING` | `--enable-sharing` | `True` | Master toggle for all sharing features. |
| `SELKIES_ENABLE_COLLAB` | `--enable-collab` | `True` | Enable collaborative (read-write) sharing link. |
| `SELKIES_ENABLE_SHARED` | `--enable-shared` | `True` | Enable view-only sharing links. |
| `SELKIES_ENABLE_PLAYER2` | `--enable-player2` | `True` | Enable sharing link for gamepad player 2. |
| `SELKIES_ENABLE_PLAYER3` | `--enable-player3` | `True` | Enable sharing link for gamepad player 3. |
| `SELKIES_ENABLE_PLAYER4` | `--enable-player4` | `True` | Enable sharing link for gamepad player 4. |
