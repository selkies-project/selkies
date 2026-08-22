"""Virtual webcam plumbing for the Selkies V4L2 interposer.

The browser captures its camera, encodes frames as MJPEG, and sends them to the
server over the WebSocket or WebRTC data channel. This module stages each frame
into a shared-memory ring and notifies the ``selkies_v4l2_interposer.so``
``LD_PRELOAD`` library over a Unix domain socket, which presents the frames as a
virtual ``/dev/videoN`` capture device inside the container.

Frame pixels never cross the socket. The staging ring lives in an anonymous
``memfd`` whose file descriptor is handed to each interposer client once via
``SCM_RIGHTS``; the socket then carries only a one-byte doorbell per staged
frame. The shared-memory layout and the on-connect configuration struct are
kept byte-for-byte identical to the constants in
``addons/v4l2-interposer/v4l2_interposer.c``.
"""

import asyncio
import logging
import mmap
import os
import socket
import struct
import time
from typing import Optional, Set

logger = logging.getLogger("webcam")

# V4L2 FourCC for Motion-JPEG, the single format the virtual device advertises.
V4L2_PIX_FMT_MJPEG = (
    ord("M") | (ord("J") << 8) | (ord("P") << 16) | (ord("G") << 24)
)

# Shared-memory layout constants; must match the interposer's WC_SHM_* defines.
SHM_MAGIC = 0x434B5753  # 'SKWC'
SHM_VERSION = 1
SHM_CTRL_OFFSET = 128
SHM_CTRL_STRIDE = 64
SHM_DATA_OFFSET = 4096
MAX_SLOTS = 4

# Header field byte offsets within the staging memfd (little-endian).
_HDR_LATEST_SLOT = 40
_HDR_LATEST_FRAME_SEQ = 48

# The on-connect configuration struct: 12 * uint32 followed by 16 reserved
# bytes, matching webcam_config_t.
_CONFIG_FMT = "<12I16s"
_CONFIG_SIZE = struct.calcsize(_CONFIG_FMT)

_MIN_SLOT_SIZE = 256 * 1024
_MAX_SLOT_SIZE = 4 * 1024 * 1024


def _clamp_slot_size(width: int, height: int) -> int:
    """Returns a per-slot byte budget large enough for one MJPEG frame."""
    estimate = max(1, width) * max(1, height)
    return max(_MIN_SLOT_SIZE, min(estimate, _MAX_SLOT_SIZE))


class WebcamRing:
    """A shared-memory ring of MJPEG frame slots backed by an anonymous memfd.

    A single writer (the server) publishes frames; any number of interposer
    processes map the ring read-only and read the newest frame. Each slot
    carries a seqlock so a reader can detect and retry a torn read without any
    cross-process lock.
    """

    def __init__(self, width: int, height: int, fps_num: int, fps_den: int,
                 n_slots: int = 3) -> None:
        """Allocates and zero-initializes the staging ring.

        Args:
            width: Advertised frame width in pixels.
            height: Advertised frame height in pixels.
            fps_num: Frame-rate numerator (frames per ``fps_den`` seconds).
            fps_den: Frame-rate denominator.
            n_slots: Number of frame slots in the ring (clamped to ``MAX_SLOTS``).
        """
        self.width = width
        self.height = height
        self.fps_num = fps_num
        self.fps_den = fps_den
        self.n_slots = max(2, min(n_slots, MAX_SLOTS))
        self.slot_size = _clamp_slot_size(width, height)
        self.total_size = SHM_DATA_OFFSET + self.n_slots * self.slot_size

        self._fd = os.memfd_create("selkies-webcam-staging", os.MFD_CLOEXEC)
        os.ftruncate(self._fd, self.total_size)
        self._map = mmap.mmap(self._fd, self.total_size, mmap.MAP_SHARED,
                              mmap.PROT_READ | mmap.PROT_WRITE)
        self._view = memoryview(self._map)

        self._next_slot = 0
        self._slot_seq = [0] * self.n_slots
        self._frame_seq = 0

        self._write_header()

    @property
    def fd(self) -> int:
        """The memfd file descriptor to pass to interposer clients."""
        return self._fd

    def config_bytes(self) -> bytes:
        """Serializes the on-connect configuration struct for the interposer."""
        return struct.pack(
            _CONFIG_FMT,
            SHM_MAGIC, SHM_VERSION,
            self.width, self.height, V4L2_PIX_FMT_MJPEG,
            self.fps_num, self.fps_den,
            self.n_slots, self.slot_size, SHM_DATA_OFFSET,
            SHM_CTRL_OFFSET, SHM_CTRL_STRIDE,
            b"\x00" * 16,
        )

    def _write_header(self) -> None:
        struct.pack_into(
            "<12I", self._map, 0,
            SHM_MAGIC, SHM_VERSION,
            self.width, self.height, V4L2_PIX_FMT_MJPEG,
            self.fps_num, self.fps_den,
            self.n_slots, self.slot_size, SHM_DATA_OFFSET,
            0, 0,  # latest_slot, _pad
        )
        struct.pack_into("<Q", self._map, _HDR_LATEST_FRAME_SEQ, 0)

    def write_frame(self, data: bytes) -> None:
        """Publishes one MJPEG frame into the next slot.

        The slot's seqlock is bumped to odd before the copy and back to even
        after, and the header's ``latest_slot``/``latest_frame_seq`` are
        published last, so a reader either sees the previous frame or the new
        one, never a mix.
        """
        slot = self._next_slot
        base = SHM_CTRL_OFFSET + slot * SHM_CTRL_STRIDE
        data_off = SHM_DATA_OFFSET + slot * self.slot_size
        n = min(len(data), self.slot_size)

        seq = self._slot_seq[slot]
        struct.pack_into("<I", self._map, base, seq + 1)  # begin write (odd)
        self._view[data_off:data_off + n] = data[:n]
        self._frame_seq += 1
        struct.pack_into("<IQQ", self._map, base + 4,
                         n, self._frame_seq, time.monotonic_ns())
        struct.pack_into("<I", self._map, base, seq + 2)  # end write (even)
        self._slot_seq[slot] = seq + 2

        struct.pack_into("<I", self._map, _HDR_LATEST_SLOT, slot)
        struct.pack_into("<Q", self._map, _HDR_LATEST_FRAME_SEQ, self._frame_seq)

        self._next_slot = (slot + 1) % self.n_slots

    def close(self) -> None:
        """Releases the mapping and the backing memfd."""
        try:
            self._view.release()
            self._map.close()
        finally:
            if self._fd >= 0:
                os.close(self._fd)
                self._fd = -1


class WebcamServer:
    """Serves the staging ring to interposer clients over a Unix socket.

    Lifetime is process-wide, like the gamepad interposer servers: applications
    open ``/dev/videoN`` once at their own startup and must survive a transport
    mode switch. A single ring is shared by all clients; every client is handed
    the ring fd on connect and a one-byte doorbell per staged frame.
    """

    def __init__(self, socket_path: str, width: int = 1280, height: int = 720,
                 fps_num: int = 30, fps_den: int = 1) -> None:
        self.socket_path = socket_path
        self.ring = WebcamRing(width, height, fps_num, fps_den)
        self._config = self.ring.config_bytes()
        self._server: Optional[asyncio.AbstractServer] = None
        self._clients: Set[socket.socket] = set()
        self._active = False

    async def start(self) -> None:
        """Binds the Unix socket and begins accepting interposer clients."""
        os.makedirs(os.path.dirname(self.socket_path) or ".", exist_ok=True)
        try:
            os.unlink(self.socket_path)
        except FileNotFoundError:
            pass
        self._server = await asyncio.start_unix_server(
            self._handle_client, path=self.socket_path)
        self._active = True
        logger.info("Webcam socket serving at %s (%dx%d @ %d/%d)",
                    self.socket_path, self.ring.width, self.ring.height,
                    self.ring.fps_num, self.ring.fps_den)

    async def _handle_client(self, reader: asyncio.StreamReader,
                             writer: asyncio.StreamWriter) -> None:
        # asyncio wraps the connection in a TransportSocket that blocks sendmsg,
        # so the SCM_RIGHTS handoff and the doorbells go through an independent
        # socket dup'd from the same underlying connection. The reader/writer
        # pair is kept only to detect the client closing.
        transport_sock = writer.get_extra_info("socket")
        try:
            raw = socket.socket(transport_sock.family, transport_sock.type,
                                fileno=os.dup(transport_sock.fileno()))
        except OSError as exc:
            logger.warning("Webcam client socket dup failed: %s", exc)
            writer.close()
            return
        try:
            raw.setblocking(True)
            socket.send_fds(raw, [self._config], [self.ring.fd])
            raw.setblocking(False)
        except OSError as exc:
            logger.warning("Webcam client fd handoff failed: %s", exc)
            raw.close()
            writer.close()
            return

        # The interposer replies with a one-byte architecture specifier and then
        # only reads doorbells; the read loop below blocks until the client
        # closes, at which point the connection is retired.
        try:
            await reader.readexactly(1)
        except (asyncio.IncompleteReadError, ConnectionError):
            raw.close()
            writer.close()
            return

        self._clients.add(raw)
        logger.info("Webcam client connected (%d active)", len(self._clients))
        try:
            while True:
                chunk = await reader.read(64)
                if not chunk:
                    break
        except (ConnectionError, OSError):
            pass
        finally:
            self._clients.discard(raw)
            raw.close()
            writer.close()
            logger.info("Webcam client disconnected (%d active)",
                        len(self._clients))

    def has_clients(self) -> bool:
        """Whether any interposer is currently connected."""
        return bool(self._clients)

    def feed(self, data: bytes) -> None:
        """Stages one MJPEG frame and rings the doorbell for every client."""
        if not self._active:
            return
        self.ring.write_frame(data)
        for sock in list(self._clients):
            try:
                sock.send(b"\x01", socket.MSG_DONTWAIT)
            except (BlockingIOError, InterruptedError):
                # Doorbell backlog already pending; the client will still wake.
                pass
            except OSError:
                self._clients.discard(sock)

    async def stop(self) -> None:
        """Stops accepting clients, closes connections and frees the ring."""
        self._active = False
        for sock in list(self._clients):
            try:
                sock.close()
            except OSError:
                pass
        self._clients.clear()
        if self._server is not None:
            self._server.close()
            try:
                await self._server.wait_closed()
            except Exception:
                pass
            self._server = None
        self.ring.close()
        try:
            os.unlink(self.socket_path)
        except FileNotFoundError:
            pass


_shared_server: Optional[WebcamServer] = None
_shared_lock: Optional[asyncio.Lock] = None


async def get_shared_webcam_server(socket_dir: str, width: int = 1280,
                                   height: int = 720, fps_num: int = 30,
                                   fps_den: int = 1) -> WebcamServer:
    """Returns the process-wide webcam server, starting it on first use.

    The whole Selkies process runs on a single event loop, so one shared server
    survives transport-mode switches and individual client reconnects: an
    application opens ``/dev/videoN`` once and keeps that connection while
    browser clients come and go. ``socket_dir`` mirrors the joystick interposer
    convention; the socket basename is fixed at ``selkies_webcam0.sock``.
    """
    global _shared_server, _shared_lock
    if _shared_lock is None:
        _shared_lock = asyncio.Lock()
    async with _shared_lock:
        if _shared_server is None:
            path = os.path.join(socket_dir or "/tmp", "selkies_webcam0.sock")
            server = WebcamServer(path, width, height, fps_num, fps_den)
            await server.start()
            _shared_server = server
    return _shared_server


async def stop_shared_webcam_server() -> None:
    """Stops and clears the process-wide webcam server, if any."""
    global _shared_server
    if _shared_server is not None:
        await _shared_server.stop()
        _shared_server = None
