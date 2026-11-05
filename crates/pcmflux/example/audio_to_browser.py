# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import asyncio
import hmac
import http
import mimetypes
import os
import secrets
import urllib.parse
import websockets
import websockets.asyncio.server as ws_async

from pcmflux import AudioCapture, AudioCaptureSettings

# --- Global Shared Context ---
# These variables manage the server's shared state across different asynchronous
# tasks and threads:
#   g_loop         the main asyncio event loop
#   g_settings     the audio capture configuration
#   g_callback     the C-compatible callback function pointer
#   g_module       the pcmflux.AudioCapture module instance
#   g_clients      ws -> {'queue': Queue, 'task': Task} per-client relay
#   g_is_capturing a flag to track the audio capture state
#   g_status_task  the asyncio.Task that periodically logs queue/drop stats
#   g_dropped      audio frames dropped because a client queue was full
g_loop = None
g_settings = None
g_callback = None
g_module = None
g_clients = {}
g_is_capturing = False
g_status_task = None
g_dropped = 0
# --- End Global Context ---

# Delivery: the capture callback never blocks, each client has its own bounded
# queue (~4s of 20ms frames), and a client that cannot drain the stream loses
# the oldest frames, not the server's memory.
AUDIO_QUEUE_MAXSIZE = 200
# A send that makes no progress for this long marks the client abandoned; the
# connection is closed rather than letting a dead socket linger (drops are
# already handled upstream by the bounded queue).
SEND_TIMEOUT_SECONDS = 1.0
# WebSockets are not subject to CORS, so any page a browser visits could otherwise
# open this socket and listen in. Only the page this server itself hands out may;
# None keeps non-browser clients, which send no Origin, working.
ALLOWED_ORIGINS = ["http://localhost:9001", "http://127.0.0.1:9001", "http://[::1]:9001", None]
# Any account on the host reaches localhost and can send any Origin: the socket also
# takes only this start's token, which reaches the page in the URL printed below.
TOKEN = secrets.token_urlsafe(16)

async def send_audio_chunks(websocket, queue):
    """Per-client sender: forwards this client's queue to its socket.

    Each queued item is a memoryview pinning one pooled native buffer, released
    exactly once — after the send, on drop, or when this task drains the queue
    on exit — so buffers always recycle promptly.
    """
    try:
        while True:
            view = await queue.get()
            try:
                await asyncio.wait_for(websocket.send(view),
                                       SEND_TIMEOUT_SECONDS)
            except asyncio.TimeoutError:
                print(f"Client {websocket.remote_address} abandoned (send "
                      f"stalled > {SEND_TIMEOUT_SECONDS}s); closing.")
                await websocket.close(code=1001, reason='Send timeout')
                break
            finally:
                view.release()
                queue.task_done()
    except websockets.exceptions.ConnectionClosed:
        pass
    except asyncio.CancelledError:
        pass
    finally:
        # Unpin anything still queued so the pooled buffers recycle.
        while True:
            try:
                leftover = queue.get_nowait()
            except asyncio.QueueEmpty:
                break
            leftover.release()
            queue.task_done()

async def status_logger():
    """Periodically logs the capture state, queue depth, and dropped-frame count
    so backpressure drops are visible rather than silent.

    `start_capture` returns while the native thread may still be retrying the
    PulseAudio connection, and a capture can also die mid-run once its reconnect
    budget is spent; polling `state` / `last_error` is how either is noticed.
    """
    try:
        while True:
            await asyncio.sleep(5)
            depths = [c['queue'].qsize() for c in g_clients.values()]
            print(f"[server] capture={g_module.state}, clients={len(depths)}, "
                  f"queued(max)={max(depths) if depths else 0}, "
                  f"dropped={g_dropped}")
            if g_module.last_error:
                print(f"[server] audio capture failed: {g_module.last_error}")
    except asyncio.CancelledError:
        pass

async def health_check(connection, request):
    """
    A pre-processor for incoming connections to the WebSocket port.

    This function intercepts plain HTTP requests, which browsers often send
    (e.g., for /favicon.ico), and handles them gracefully. This prevents
    WebSocket handshake errors from cluttering the console.
    """
    if request.path == "/favicon.ico":
        # Return a "204 No Content" response for favicon requests.
        return connection.respond(http.HTTPStatus.NO_CONTENT, "")
    given = urllib.parse.parse_qs(urllib.parse.urlsplit(request.path).query).get("token", [""])[0]
    if not hmac.compare_digest(given.encode(), TOKEN.encode()):
        return connection.respond(http.HTTPStatus.UNAUTHORIZED, "token required\n")
    # Allow all other requests to proceed to the WebSocket handler.
    return None

async def ws_handler(websocket):
    """
    Handles the lifecycle of each WebSocket client connection.

    This function is responsible for starting the audio capture when the first
    client connects and stopping it when the last client disconnects, ensuring
    that system resources are only used when needed.
    """
    global g_is_capturing, g_status_task

    # Register the new client with its own bounded relay.
    client_queue = asyncio.Queue(maxsize=AUDIO_QUEUE_MAXSIZE)
    send_task = asyncio.create_task(send_audio_chunks(websocket, client_queue))
    g_clients[websocket] = {'queue': client_queue, 'task': send_task}
    print(f"Client connected: {websocket.remote_address}. "
          f"Total clients: {len(g_clients)}")

    # If this is the first client, start the audio capture process. start_capture
    # blocks until the native thread reports it is running (up to ~2s), so it runs
    # on a worker thread: the loop keeps feeding every other client meanwhile.
    if not g_is_capturing and g_module:
        print("First client connected. Starting audio capture...")
        await asyncio.to_thread(g_module.start_capture, g_settings, g_callback)
        g_is_capturing = True
        if g_status_task is None or g_status_task.done():
            g_status_task = asyncio.create_task(status_logger())
        # "running" once PulseAudio is connected; "starting" while the native
        # thread is still retrying (the status logger reports how it ends).
        print(f"Audio capture process initiated (state: {g_module.state}).")

    try:
        # Wait for messages from the client. In this demo, we don't expect
        # any, so this loop effectively just waits for the client to close.
        async for _ in websocket:
            pass
    except websockets.exceptions.ConnectionClosed:
        pass
    finally:
        # Unregister first so the fan-out stops targeting this client, then
        # stop its sender (which drains and unpins any queued buffers).
        g_clients.pop(websocket, None)
        if not send_task.done():
            send_task.cancel()
            try:
                await send_task
            except asyncio.CancelledError:
                pass
        print(f"Client disconnected. Remaining clients: {len(g_clients)}")

        # If this was the last client, stop the audio capture to save resources.
        # The stop joins the capture thread through PulseAudio teardown, so it also
        # runs on a worker thread rather than stalling the loop.
        if g_is_capturing and not g_clients and g_module:
            print("Last client disconnected. Stopping audio capture...")
            await asyncio.to_thread(g_module.stop_capture)
            g_is_capturing = False
            if g_status_task:
                g_status_task.cancel()
                g_status_task = None
            print("Audio capture process stopped.")

def py_audio_callback(frame):
    """Per-chunk callback invoked from the native capture thread.

    `frame` is an AudioFrame that OWNS its encoded payload (buffer protocol +
    `.pts`): it is handed to the loop thread as-is, and the fan-out there makes
    one memoryview per client, each pinning the frame — and therefore the
    native buffer — alive until that client's send releases it. Dropping the
    last reference recycles the buffer back to the capture's pool.
    Silence-gated chunks are never delivered (the callback is skipped for
    them), so `frame` always carries an encoded payload — no empty-frame check
    is needed.
    """
    if g_is_capturing and frame is not None:
        try:
            if g_loop and not g_loop.is_closed():
                g_loop.call_soon_threadsafe(_fanout_audio, frame)
            # Without a running loop the frame reference is simply dropped,
            # which recycles the pooled buffer.
        except RuntimeError:
            # Loop closed between the check and the call (teardown race): the
            # frame was never handed off, so dropping it recycles the buffer.
            pass

def _fanout_audio(frame):
    """Fan one chunk out to every client's bounded queue on the loop thread,
    dropping each stalled client's oldest frame first, so a slow consumer
    costs bounded memory and never delays the others (each queued view pins
    one pooled native buffer until it is sent or dropped)."""
    global g_dropped
    for entry in g_clients.values():
        queue = entry['queue']
        # Loop-thread serialized with the senders: full()/get_nowait()/
        # put_nowait() cannot race, so no defensive except is needed.
        if queue.full():
            stale = queue.get_nowait()
            queue.task_done()
            stale.release()
            g_dropped += 1
        queue.put_nowait(memoryview(frame))

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
        # e.g. embedded NUL byte ("%00")
        return None
    if requested != root and not requested.startswith(root + os.sep):
        return None
    return requested


def _serve_static(script_dir, request_path):
    """Resolve request_path under script_dir and read what it names, as
    (status, body, path): 403 when it escapes, 404 when no file is there, 200 with
    the bytes otherwise. Every filesystem call a request needs is here, so the
    handler makes one hop off the loop for all of them and no slow resolve or read
    can stall the audio.
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
            path = parts[1].decode()
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

async def main_async():
    """The main routine to initialize and run the servers."""
    global g_loop, g_settings, g_callback, g_module, g_status_task

    g_loop = asyncio.get_running_loop()

    # --- Configure Audio Capture Parameters ---
    g_settings = AudioCaptureSettings()
    # To capture desktop audio on Linux with PulseAudio, you may need to find
    # the name of your output's ".monitor" source. Use `pactl list sources`
    # in a terminal to find available source names.
    # To use the system's default microphone, set device_name to None or b''.
    # The same variables selkies accepts override the template values here (the
    # library reads no SELKIES_* environment itself — each knob is a settings field).
    g_settings.device_name = os.environ.get(
        "SELKIES_AUDIO_DEVICE_NAME",
        "alsa_output.pci-0000_2b_00.1.hdmi-stereo.monitor",
    ).encode("utf-8")
    g_settings.sample_rate = 48000
    g_settings.channels = int(os.environ.get("SELKIES_AUDIO_CHANNELS", "2"))
    g_settings.opus_bitrate = int(os.environ.get("SELKIES_AUDIO_BITRATE", "128000"))
    g_settings.frame_duration_ms = int(
        os.environ.get("SELKIES_AUDIO_FRAME_DURATION_MS", "20")
    )
    g_settings.use_vbr = True
    g_settings.use_silence_gate = False
    g_settings.debug_logging = True
    # Emit the 2-byte audio header [0x01, 0x00] natively in the extension (the default).
    # Set True only for a raw-Opus/WebRTC transport that adds no header.
    g_settings.omit_audio_header = False
    # --- End Configuration ---

    # Pass the plain Python callback; the wrapper marshals it once internally.
    g_callback = py_audio_callback
    g_module = AudioCapture()
    print("pcmflux audio capture module initialized.")

    # Start HTTP server using asyncio
    http_server = await asyncio.start_server(
        handle_http_request, 'localhost', 9001
    )
    print("HTTP server is serving files from current directory")
    print(f"-> Open http://localhost:9001/index.html#token={TOKEN} in your browser.")

    # Start the WebSocket server. Opus is already compressed, so
    # permessage-deflate would only burn CPU and per-connection memory.
    ws_server = await ws_async.serve(
        ws_handler,
        'localhost',
        9000,
        process_request=health_check,
        compression=None,
        origins=ALLOWED_ORIGINS,
    )
    print("WebSocket server started on ws://localhost:9000")

    try:
        # Keep the main coroutine running indefinitely.
        await asyncio.Event().wait()
    finally:
        # Stop capture first (joins the native capture thread, stops enqueuing),
        # then close the client connections (each handler's finally block stops
        # its sender), then close the servers.
        print("\nShutting down...")
        if g_module:
            await asyncio.to_thread(g_module.stop_capture)
        close_tasks = [ws.close(code=1001, reason='Server shutting down')
                       for ws in list(g_clients.keys())]
        if close_tasks:
            await asyncio.gather(*close_tasks, return_exceptions=True)
        if g_status_task:
            g_status_task.cancel()
            await asyncio.gather(g_status_task, return_exceptions=True)
            g_status_task = None
        for server in (ws_server, http_server):
            if server:
                server.close()
                await server.wait_closed()
        # Drop the last reference so AudioCapture.__del__ releases the native capture resources.
        if g_module:
            g_module = None
        print("Cleanup complete.")

if __name__ == "__main__":
    try:
        asyncio.run(main_async())
    except KeyboardInterrupt:
        print("\nApplication exiting.")
