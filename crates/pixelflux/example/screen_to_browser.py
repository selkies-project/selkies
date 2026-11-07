# -*- coding: utf-8 -*-
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""
A multi-client WebSocket and HTTP server for streaming screen captures.

This script demonstrates the pixelflux library's instance-safe capabilities.
It can handle multiple WebSocket clients, each with its own independent
screen capture session. The capture region can be controlled via the URL hash.

The capture callback never blocks and never buffers more than the
bounded per-client queue. When a client cannot drain the stream, the backlog
is dropped and H.264 delivery gates per stripe row until the next keyframe
(requested from the encoder at most once per second), so a slow consumer
costs bounded memory and resumes on a decodable frame.
"""

# Standard library imports
import asyncio
import hmac
import http
import os
import secrets
import mimetypes
import time
import urllib.parse
import websockets
import websockets.asyncio.server as ws_async

# Third-party library imports
from pixelflux import CaptureSettings, ScreenCapture, ensure_wayland_display

# ==============================================================================
# --- BASE CONFIGURATION SETTINGS ---
# These settings are applied to a fresh CaptureSettings for each new
# connection. Modify the parameters below to test different capture and
# encoding options.
# ==============================================================================
HTTP_PORT = 9001
WS_PORT = 9000
# WebSockets are not subject to CORS, so any page a browser visits could otherwise
# open this socket and receive the screen. Only the page this server itself hands
# out may; None keeps non-browser clients, which send no Origin, working.
ALLOWED_ORIGINS = [f"http://localhost:{HTTP_PORT}", f"http://127.0.0.1:{HTTP_PORT}", f"http://[::1]:{HTTP_PORT}", None]
# Any account on the host reaches localhost and can send any Origin: the socket also
# takes only this start's token, which reaches the page in the URL printed below.
TOKEN = secrets.token_urlsafe(16)

# Send-side bounds, mirroring selkies' per-client video relay: the queue holds
# at most ~2 seconds of stream at 60 fps; keyframe requests triggered by
# overflow are rate-limited per client.
VIDEO_QUEUE_MAXSIZE = 120
IDR_REQUEST_FLOOR_SECONDS = 1.0
# A send that makes no progress for this long marks the client abandoned; the
# connection is closed rather than letting a dead socket linger (drops are
# already handled upstream by the bounded queue).
SEND_TIMEOUT_SECONDS = 1.0

# ==============================================================================
# --- ENVIRONMENT OVERRIDES (the same variables selkies accepts) ---
# The library reads no SELKIES_* environment itself — every knob is a
# CaptureSettings field — so this template ingests the variables explicitly,
# with the same precedence selkies uses (SELKIES_* first, then the legacy alias).
# ==============================================================================

def _env(name, legacy=None):
    value = os.environ.get(name)
    if value is None and legacy:
        value = os.environ.get(legacy)
    return value or ""

_wayland = _env("SELKIES_WAYLAND", "PIXELFLUX_WAYLAND").lower()
_render_dri = _env("SELKIES_RENDER_DRI", "DRINODE")
_encode_dri = _env("SELKIES_ENCODE_DRI", "DRI_NODE")
_recording = _env("SELKIES_RECORDING_SOCKET", "PIXELFLUX_RECORDING_SOCKET")
_cursor_size = _env("SELKIES_CURSOR_SIZE", "XCURSOR_SIZE")
_auto_gpu = _env("SELKIES_AUTO_GPU", "AUTO_GPU") or "true"

def build_capture_settings():
    """Return a fresh CaptureSettings for one client.

    Each connection gets its own object: sessions started concurrently must
    not see another client's overrides (e.g. the per-URL capture_x below).
    """
    cs = CaptureSettings()

    # --- Debugging ---
    # Enable/disable the continuous FPS and settings log printed to the console.
    cs.debug_logging = True

    # --- Core Capture ---
    cs.capture_width = 1920
    cs.capture_height = 1080
    cs.capture_x = 0  # This can be overridden by the URL
    cs.capture_y = 0
    cs.target_fps = 60.0
    cs.capture_cursor = False

    # --- Encoding Mode ---
    # Sets the output codec. 0 for JPEG, 1 for H.264.
    cs.codec = "h264"
    # Force CPU encoding and ignore hardware encoders
    cs.use_cpu = False

    # --- H.264 Quality Settings ---
    # Constant Rate Factor (0-51, lower is better quality & higher bitrate).
    # Good values are typically 18-28.
    cs.video_crf = 25
    # CRF for H.264 paintover on static content. Used if lower (better) than video_crf.
    cs.video_paintover_crf = 18
    # Number of high-quality H.264 frames to send in a burst when a paintover is triggered.
    cs.video_paintover_burst_frames = 5
    # Use I444 (full color) instead of I420. Better quality, higher CPU/bandwidth.
    cs.video_fullcolor = False
    # Bits per sample, 8 or 10: 10 where an encoder of the codec carries it, in software where the GPU does not.
    cs.video_bit_depth = 8
    # Flag the stream to be in streaming mode to bypass all vnc logic
    cs.video_streaming_mode = False
    # Encoder device index: -2 = auto-detect (lets AUTO_GPU pick), -1 = software encoding, 0+ = specific GPU.
    cs.encode_node_index = -2
    # Switches to CBR mode and ignores CRF value. Used in conjunction with video_bitrate_kbps.
    cs.video_cbr_mode = False
    # Target bitrate in kbps for CBR mode. Required when video_cbr_mode is enabled.
    cs.video_bitrate_kbps = 4000
    # Optional VBV size as a multiple of one frame's bit budget.
    cs.video_vbv_multiplier = 0.0     # CBR: 0 = auto policy. CRF: 0 = uncapped, a positive value caps software x264.
    # Allow pixelflux to adjust its capture width and height. Overrides provided width and height when enabled.
    cs.auto_adjust_screen_capture_size = True

    # --- Change Detection & Optimization ---
    # Use a higher quality setting for static regions that haven't changed for a while.
    cs.use_paint_over_quality = True
    # Number of frames of no motion in a stripe to trigger a high-quality "paint-over".
    cs.paint_over_trigger_frames = 15
    # Consecutive changes to a stripe to trigger a "damaged" state (uses base quality).
    cs.damage_block_threshold = 10
    # Number of frames a stripe stays "damaged" after being triggered.
    cs.damage_block_duration = 30

    # --- JPEG Quality Settings ---
    # Quality of jpegs under motion
    cs.jpeg_quality = 40
    # Quality of jpegs on static content paintovers
    cs.paint_over_jpeg_quality = 90

    # --- Watermarking ---
    # The path MUST be a byte string (b"") and point to a valid PNG file.
    #cs.watermark_path = b"/path/to/image.png"
    # Sets the watermark location on the screen. Default is 0 (disabled).
    # Options: 0:None, 1:TopLeft, 2:TopRight, 3:BottomLeft, 4:BottomRight, 5:Middle, 6:Animated
    cs.watermark_location_enum = 0

    # --- Recording ---
    # When this is set to a valid path (string) will enable a unix socket for recording
    # i.e. '/tmp/test' can be recorded with "ffmpeg -f h264 -i unix:///tmp/test -c:v copy test.h264"
    # For a clean recording the stream might need a re-encode i.e.:
    # "ffmpeg -f h264 -framerate 60 -i unix:///tmp/test -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p test.mp4"
    # This option enables IDR frames every 30 frames and on socket connection
    cs.recording_socket = None

    # --- Environment overrides ---
    # Backend: force Wayland/X11; left unset, pixelflux follows WAYLAND_DISPLAY.
    if _wayland:
        cs.use_wayland = _wayland in ("1", "true", "yes", "on")
    # Compositor render node: an explicit path wins over auto-GPU selection.
    if _render_dri:
        cs.render_node_path = _render_dri.encode("utf-8")
    # "true" (the default) = first GPU; any other token = first GPU matching a vendor
    # name, kernel driver name, devicetree prefix, or raw PCI vendor id; "false" disables.
    cs.auto_gpu = _auto_gpu
    # Encoder node (VA-API/NVENC device), distinct from the render node.
    if _encode_dri:
        cs.encode_node_path = _encode_dri.encode("utf-8")
        _idx = _encode_dri.rsplit("renderD", 1)[-1]
        if _idx.isdigit():
            cs.encode_node_index = int(_idx) - 128
    if _recording:
        cs.recording_socket = _recording
    # Compositor cursor-theme size in pixels (Wayland backend).
    if _cursor_size.isdigit():
        cs.cursor_size = int(_cursor_size)

    return cs

# Wayland: bring the compositor socket up now, before the capture starts, so apps
# launched alongside this script can already connect to WAYLAND_DISPLAY.
if _wayland in ("1", "true", "yes", "on"):
    _dim = lambda name: int(os.environ.get(name) or 0) if str(os.environ.get(name) or "").isdigit() else 0
    ensure_wayland_display(
        width=_dim("SELKIES_MANUAL_WIDTH"),
        height=_dim("SELKIES_MANUAL_HEIGHT"),
        render_node=_render_dri,
        auto_gpu=_auto_gpu,
        cursor_size=int(_cursor_size) if _cursor_size.isdigit() else -1,
    )

# ==============================================================================
# --- Multi-Client State Management ---
# ==============================================================================
g_loop = None  # The main asyncio event loop.

# This dictionary holds the state for each active client.
# The key is the WebSocket connection object.
# The value is another dictionary containing the client's capture module, queue, and task.
ACTIVE_CLIENTS = {}

async def send_stripes_task(websocket, queue):
    """
    Pulls video stripes from a client-specific queue and sends them.
    This task is canceled when the client disconnects.
    """
    print(f"Send task started for client {websocket.remote_address}.")
    try:
        while True:
            item = await queue.get()
            try:
                # item == {'data': <memoryview>, 'owner': <StripeFrame>}. Keeping
                # `item` (hence the StripeFrame) referenced for the whole send keeps
                # the C buffer alive until the send releases the view, so zero-copy
                # is safe. The timeout is liveness-only: overflow is handled by the
                # bounded queue upstream, so a send stuck this long means the client
                # is gone, not merely slow.
                await asyncio.wait_for(websocket.send(item['data']),
                                       SEND_TIMEOUT_SECONDS)
            except asyncio.TimeoutError:
                print(f"Client {websocket.remote_address} abandoned (send "
                      f"stalled > {SEND_TIMEOUT_SECONDS}s); closing.")
                await websocket.close(code=1001, reason='Send timeout')
                break
            finally:
                queue.task_done()

    except websockets.exceptions.ConnectionClosed:
        # This is the expected, clean way to exit the loop when a client disconnects.
        print(f"Connection closed for {websocket.remote_address}. Send task stopping.")

    except asyncio.CancelledError:
        # This happens when the main handler cancels us during cleanup.
        print(f"Send task was canceled for {websocket.remote_address}.")

    except Exception as e:
        # Catch any other unexpected errors.
        print(f"[ERROR] Send task for client {websocket.remote_address} failed unexpectedly: {e}")

    finally:
        print(f"Send task for {websocket.remote_address} has finished.")

def require_token(connection, request):
    """Refuse an upgrade without this start's token (401)."""
    given = urllib.parse.parse_qs(urllib.parse.urlsplit(request.path).query).get("token", [""])[0]
    if not hmac.compare_digest(given.encode(), TOKEN.encode()):
        return connection.respond(http.HTTPStatus.UNAUTHORIZED, "token required\n")
    return None


async def websocket_handler(websocket):
    """
    Manages a single WebSocket connection and its dedicated screen capture lifecycle.
    """
    path = urllib.parse.urlsplit(websocket.request.path).path
    client_id = id(websocket)
    print(f"New client connected from {websocket.remote_address} with path '{path}' (ID: {client_id}).")

    client_module = None
    send_task = None

    try:
        # --- 1. Configure Capture for this Specific Client ---
        client_settings = build_capture_settings()
        try:
            x_offset = int(path.strip('/'))
            client_settings.capture_x = x_offset
            print(f"Client {client_id} requested custom capture at x={x_offset}.")
        except (ValueError, TypeError):
            print(f"Client {client_id} using default capture at x=0.")

        # --- 2. Create Resources for this Client ---
        client_module = ScreenCapture()
        client_queue = asyncio.Queue(maxsize=VIDEO_QUEUE_MAXSIZE)
        # live_rows: stripe rows (wire y_start) whose H.264 chain is intact
        # for this client. A row's deltas are deliverable only after its IDR;
        # an overflow drop clears the set, gating every row until a keyframe
        # re-anchors it. Video frames are whole, so they always arrive at row 0.
        relay_state = {'live_rows': set(), 'last_idr_req': 0.0}

        # --- 3. Create a unique callback (closure) for this client ---
        # Runs on the native capture thread: it must never block, so it hands
        # the frame to the loop and the loop-side enqueue applies the bounded
        # relay rules. The memoryview aliases the C buffer and pins the
        # StripeFrame alive until the send (or a drop) releases the item.
        def on_stripe(frame):
            """Callback invoked by pixelflux with a StripeFrame per video stripe."""
            if not (len(frame) > 0 and g_loop and not g_loop.is_closed()):
                return

            item_to_queue = {'data': memoryview(frame), 'owner': frame}

            def request_idr_throttled():
                now = time.monotonic()
                if now - relay_state['last_idr_req'] >= IDR_REQUEST_FLOOR_SECONDS:
                    relay_state['last_idr_req'] = now
                    client_module.request_idr_frame()

            def enqueue():
                data = item_to_queue['data']
                # Wire prefixes: 0x04 = video (byte 1 = frame kind in the low
                # nibble, 0x1 = key frame, and the codec id in the high nibble;
                # bytes 4:6 = stripe y_start big-endian), 0x03 = JPEG. JPEG is
                # never gated: every frame is independently decodable, a
                # dropped one is simply repainted by the next.
                is_h264 = len(data) >= 10 and data[0] == 0x04
                is_idr = is_h264 and (data[1] & 0x0f) == 0x01
                row = ((data[4] << 8) | data[5]) if is_h264 else 0
                if is_h264 and not is_idr and row not in relay_state['live_rows']:
                    # Undecodable until this row re-anchors; ask for a
                    # keyframe in case none is already on the way.
                    request_idr_throttled()
                    return
                try:
                    client_queue.put_nowait(item_to_queue)
                    if is_idr:
                        relay_state['live_rows'].add(row)
                except asyncio.QueueFull:
                    # The socket is slower than the stream: drop the whole
                    # backlog rather than buffer it, then recover via a
                    # keyframe. Dropping the item references releases the
                    # underlying StripeFrame buffers.
                    while not client_queue.empty():
                        try:
                            client_queue.get_nowait()
                            client_queue.task_done()
                        except asyncio.QueueEmpty:
                            break
                    relay_state['live_rows'].clear()
                    if is_h264 and not is_idr:
                        request_idr_throttled()
                    else:
                        # Keyframes and JPEG frames go into the just-cleared
                        # queue so the client resumes immediately.
                        client_queue.put_nowait(item_to_queue)
                        if is_idr:
                            relay_state['live_rows'].add(row)

            try:
                g_loop.call_soon_threadsafe(enqueue)
            except RuntimeError:
                # Loop closed between the check and the call (teardown race);
                # dropping the item's references recycles the frame buffer.
                pass

        # --- 4. Register and Start Resources for this Client ---
        send_task = asyncio.create_task(send_stripes_task(websocket, client_queue))
        ACTIVE_CLIENTS[websocket] = {
            "module": client_module,
            "queue": client_queue,
            "task": send_task,
            "callback": on_stripe # Store reference to prevent GC
        }

        # --- 5. Start the Capture with the callback and settings ---
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(
            None, client_module.start_capture, on_stripe, client_settings
        )
        print(f"Capture started for client {client_id}.")

        # --- 6. Wait for the Client to Disconnect ---
        async for _ in websocket:
            pass # Keep the connection alive

    except websockets.exceptions.ConnectionClosed:
        print(f"Client {client_id} disconnected normally.")
    except Exception as e:
        print(f"[ERROR] WebSocket handler for client {client_id} error: {e}")
    finally:
        # --- 7. Clean Up Resources for this Specific Client ---
        print(f"Cleaning up resources for client {client_id}...")

        if send_task and not send_task.done():
            send_task.cancel()
            try: await send_task
            except asyncio.CancelledError: pass

        if client_module:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, client_module.stop_capture)

        ACTIVE_CLIENTS.pop(websocket, None)
        print(f"Cleanup complete for client {client_id}. Active clients: {len(ACTIVE_CLIENTS)}")

# The static root, resolved once: the directory this script lives in.
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))


def _resolve_static_path(script_dir, request_path):
    """Return the real path of request_path under script_dir, or None if it
    escapes. realpath canonicalizes '..'/symlinks and the root+os.sep boundary
    rejects sibling-prefix dirs; the path is URL-decoded first."""
    decoded = urllib.parse.unquote(request_path)
    root = os.path.realpath(script_dir)
    try:
        requested = os.path.realpath(os.path.join(root, decoded.lstrip('/')))
    except ValueError:
        return None  # e.g. embedded NUL byte ("%00")
    if requested != root and not requested.startswith(root + os.sep):
        return None
    return requested


def _serve_static(script_dir, request_path):
    """Resolve request_path under script_dir and read what it names, as
    (status, body, path): 403 when it escapes, 404 when no file is there, 200 with
    the bytes otherwise. Every filesystem call a request needs is here, so the
    handler makes one hop off the loop for all of them and no slow resolve or read
    can stall the stream.
    """
    full_path = _resolve_static_path(script_dir, request_path)
    if full_path is None:
        return 403, None, None
    if not os.path.isfile(full_path):
        return 404, None, None
    with open(full_path, 'rb') as handle:
        return 200, handle.read(), full_path


async def handle_http_request(reader, writer):
    """Handle HTTP requests by serving static files from the script directory."""
    try:
        request_line = await reader.readline()
        if not request_line:
            return

        parts = request_line.split()
        if len(parts) < 2 or parts[0] != b'GET':
            writer.write(b'HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return

        try:
            path = parts[1].decode().split('#')[0].split('?')[0]  # Ignore hash and query
        except UnicodeDecodeError:
            writer.write(b'HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return
        if path == '/':
            path = '/index.html'

        status, content, full_path = await asyncio.to_thread(_serve_static, SCRIPT_DIR, path)
        if status == 403:
            # Directory traversal, or an escape outside SCRIPT_DIR.
            writer.write(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
        elif status == 404:
            writer.write(b'HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
        else:
            content_type = mimetypes.guess_type(full_path)[0] or 'application/octet-stream'
            headers = f'HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {len(content)}\r\nConnection: close\r\n\r\n'
            writer.write(headers.encode())
            writer.write(content)

    except Exception as e:
        print(f"[HTTP Error] {e}")
        writer.write(b'HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
    finally:
        # The client may already be gone; draining/closing a dead connection
        # must not raise out of the handler.
        try:
            await writer.drain()
        except (ConnectionError, OSError):
            pass
        writer.close()


async def main():
    """Initializes and starts the WebSocket and HTTP servers."""
    global g_loop
    g_loop = asyncio.get_running_loop()

    http_server = await asyncio.start_server(handle_http_request, 'localhost', HTTP_PORT)
    print(f"HTTP server serving on http://localhost:{HTTP_PORT}/")
    print(f"-> Open http://localhost:{HTTP_PORT}/?token={TOKEN} to start a capture at (0,0).")
    print(f"-> Open http://localhost:{HTTP_PORT}/?token={TOKEN}#10 to start a capture at (10,0).")

    ws_server = None
    try:
        ws_server = await ws_async.serve(websocket_handler, 'localhost', WS_PORT, process_request=require_token,
                                         compression=None, origins=ALLOWED_ORIGINS)
        print(f"WebSocket server started on ws://localhost:{WS_PORT}")
        print("Waiting for client connections... Press Ctrl+C to stop.")
        await asyncio.Event().wait()
    except OSError as e:
        print(f"[FATAL] Could not start server (is port {WS_PORT} in use?): {e}")
    finally:
        print("Shutting down all client connections...")
        # Closing each websocket connection triggers its handler's finally block.
        cleanup_tasks = [ws.close(code=1001, reason='Server shutting down')
                         for ws in list(ACTIVE_CLIENTS.keys())]
        if cleanup_tasks:
            await asyncio.gather(*cleanup_tasks, return_exceptions=True)

        if ws_server:
            ws_server.close()
            await ws_server.wait_closed()

        http_server.close()
        await http_server.wait_closed()
        print("All servers and connections closed. Goodbye.")

if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\nApplication exiting.")
