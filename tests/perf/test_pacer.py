"""Real-life E2E for the WebRTC packet pacer (SELKIES_WEBRTC_PACER off vs on).

Real selkies server (webrtc mode, pixelflux H264, Opus audio, TWCC, the real
vendored ICE/DTLS/SRTP stack) + headless Python WebRTC client on that same
stack. Constrained-link realism comes from a userspace UDP relay with a
deterministic token-bucket queue + delay/jitter: every ICE candidate on BOTH
sides is rewritten to point at a relay endpoint, so all media crosses the
shaper (works without root).

Metrics (all wire-level, pre-jitter-buffer):
  * audio inter-arrival gaps (THE user concern) — gap distribution, >60ms count
  * video per-frame wire span (last-packet minus first-packet per RTP timestamp)
  * video receive-rate shaping: bytes/s, key-frame arrival gaps
  * server CPU (psutil), server log lines (GOP resets, keyframe requests)
"""
import asyncio
import json
import logging
import os
import re
import subprocess
import sys
import time

import aiohttp

from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies.webrtc import (  # noqa: E402
    RTCPeerConnection, RTCConfiguration, RTCSessionDescription,
)
from selkies.webrtc.sdp import candidate_from_sdp  # noqa: E402
import selkies.webrtc.rtcrtpreceiver as rrx_mod  # noqa: E402
from selkies.webrtc.clock import current_ntp_time  # noqa: E402

LOG = logging.getLogger("pacer_e2e")
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s: %(message)s")

RUNNER_DIR = os.path.join(H.WORKDIR, "pacer")
os.makedirs(RUNNER_DIR, exist_ok=True)

# ---------------------------------------------------------------- metrics -----

class Recorder:
    """Wire-level arrival metrics captured by the RTP receive tap.

    Attributes:
        audio_times: Monotonic arrival time per audio packet.
        video_pkts: `(monotonic_time, rtp_timestamp)` per video packet.
        ssrc_kind: SSRC to media-kind cache for tap classification.
        twcc_arrivals: twcc_seq to `(recv_monotonic, wire_size)` for the
            pending feedback window.
        pings: `(recv_time, start_time)` per data-channel ping.
        ow_video: One-way video latency samples in seconds (abs_send_time).
        ow_audio: One-way audio latency samples in seconds (abs_send_time).
    """

    def __init__(self) -> None:
        self.reset()
    def reset(self) -> None:
        self.audio_times = []
        self.video_pkts = []
        self.audio_bytes = 0
        self.video_bytes = 0
        self.n_audio = 0
        self.n_video = 0
        self.ssrc_kind = {}
        self.twcc_arrivals = {}
        self.twcc_fb_count = 0
        self.twcc_emitted = 0
        self.pings = []
        self.ow_video = []
        self.ow_audio = []

REC = Recorder()
_orig_handle_rtp = rrx_mod.RTCRtpReceiver._handle_rtp_packet

async def _wrapped_handle_rtp(self, packet, arrival_time_ms):
    """Tap every received RTP packet into REC before normal handling."""
    kind = REC.ssrc_kind.get(packet.ssrc)
    if kind is None:
        kind = getattr(self, "_RTCRtpReceiver__kind", None)
        if kind is not None:
            REC.ssrc_kind[packet.ssrc] = kind
    if kind == "audio":
        REC.n_audio += 1
        REC.audio_bytes += len(packet.payload) + 40
        REC.audio_times.append(time.monotonic())
    elif kind == "video":
        REC.n_video += 1
        REC.video_bytes += len(packet.payload) + 40
        REC.video_pkts.append((time.monotonic(), packet.timestamp))
    # True one-way latency (send->recv, incl. pacer/queue): RTP abs_send_time
    # is (ntp>>14)&0xFFFFFF — 24-bit, unit 2^-18 s, wraps every 64 s. Client
    # and server share the host clock here, so skew/freerun error is ~0.
    ast = getattr(packet.extensions, "abs_send_time", None)
    if ast:
        diff = (((current_ntp_time() >> 14) & 0xFFFFFF) - ast) & 0xFFFFFF
        # Under 32 s is a plausible latency; bigger diffs are wrap artifacts.
        if diff < 0x800000:
            ow = diff / 262144.0
            if kind == "audio":
                REC.ow_audio.append(ow)
            elif kind == "video":
                REC.ow_video.append(ow)
    seq = getattr(packet.extensions, "transport_sequence_number", None)
    if seq is not None:
        REC.twcc_arrivals[seq] = (time.monotonic(), len(packet.payload) + 60)
    return await _orig_handle_rtp(self, packet, arrival_time_ms)

rrx_mod.RTCRtpReceiver._handle_rtp_packet = _wrapped_handle_rtp


# ---- receiver-side TWCC feedback emission (what real browsers do) ------------

def _build_twcc_fci(arrivals: dict, fb_count: int) -> Optional[bytes]:
    """Encode a transport-cc FCI block from an arrival window.

    Args:
        arrivals: `{seq: (t_mono, size)}` for the feedback window.
        fb_count: Running feedback packet counter (mod 256 on the wire).

    Returns:
        The FCI bytes as RLE chunks plus 250us receive deltas matching RFC
        transport-cc (smallest truthful encoding), or None when the window is
        empty or too wide to report.
    """
    seqs = sorted(arrivals)
    if not seqs:
        return None
    base = seqs[0]
    statuses = []
    deltas = b""
    # Fill gaps as "not received" so base..end stays contiguous.
    end = seqs[-1]
    ref_t = arrivals[base][0]
    n = ((end - base) & 0xFFFF) + 1
    prev_t = ref_t
    import struct as _st
    for i in range(n):
        s = (base + i) & 0xFFFF
        if s not in arrivals:
            statuses.append(0)
            continue
        t = arrivals[s][0]
        # The RFC encodes per-packet incremental deltas, not offsets from ref.
        dt = t - prev_t
        prev_t = t
        if 0 <= dt < 0.06375:
            statuses.append(1)
            deltas += bytes([min(255, int(dt * 1_000_000 / 250))])
        else:
            statuses.append(2)
            deltas += _st.pack("!h", max(-32768, min(32767, int(dt * 1_000_000 / 250))))
    # Cap the window at 200 statuses to keep feedback messages small.
    if len(statuses) > 200:
        return None
    chunks = b""
    import struct as _st
    i = 0
    while i < len(statuses):
        sym = statuses[i]
        run = 1
        while i + run < len(statuses) and statuses[i + run] == sym and run < 8191:
            run += 1
        chunks += _st.pack("!H", (sym << 13) | run)
        i += run
    ref64ms = int(ref_t * 1000 / 64) & 0xFFFFFF
    fci = _st.pack("!HH3sB", base, len(statuses),
                   ref64ms.to_bytes(3, "big"), fb_count % 256) + chunks + deltas
    if TWCC_DEBUG:
        runs = [arrivals[s][0] for s in sorted(arrivals)]
        LOG.warning("twcc fci: fb=%d statuses=%d window_span_ms=%.0f",
                    fb_count, len(statuses), (max(runs) - min(runs)) * 1000)
    return fci


TWCC_DEBUG = os.environ.get("TWCC_DEBUG", "0") == "1"


# ---------------------------------------------------------------- shaping -----

class Shaper:
    """One-direction token-bucket+FIFO virtual clock, exact departure times,
    with a switch-fidelity tail-drop cap (no unbounded bufferbloat)."""
    CAP_BYTES = 250_000

    def __init__(self, rate_bps: Optional[float] = None, delay_s: float = 0.0) -> None:
        self.rate = rate_bps
        self.delay_s = delay_s
        self.free_at = time.monotonic()
        self.queued_bytes = 0
        self.dropped = 0
        self.shaped = 0
    def push(self, data: bytes, emit) -> None:
        """Schedule `emit` at the datagram's shaped departure time.

        Args:
            data: The datagram, sized against the token bucket.
            emit: Zero-argument callable that actually sends it.
        """
        self.shaped += 1
        loop = asyncio.get_running_loop()
        if self.rate is None:
            loop.call_later(self.delay_s, emit) if self.delay_s else emit()
            return
        if self.queued_bytes + len(data) > self.CAP_BYTES:
            self.dropped += 1
            return
        self.queued_bytes += len(data)
        size = len(data)

        def _emit(n=size):
            self.queued_bytes -= n
            emit()
        now = time.monotonic()
        start = max(now, self.free_at)
        depart = start + len(data) * 8 / self.rate
        self.free_at = depart
        dt = max(depart - now, 0) + self.delay_s
        loop.call_later(dt, _emit)

class UDPRelay:
    """Forwards datagrams between the NEAR peer (whose candidate we replaced)
    and the real FAR peer, enforcing one shaped leg each way.

    vis transport: the address we advertise to the near side. All datagrams
    arriving there come from the near peer; after the up-shaper they go out on
    `far` toward the real destination. Replies arriving on `far` pass the
    down-shaper and are delivered to the recorded near address."""
    def __init__(self, far_transport, shaper_up: Shaper, shaper_down: Shaper, name: str) -> None:
        self.far_tr = far_transport
        self.up, self.down = shaper_up, shaper_down
        self.name = name
        self.near_addr: Optional[tuple] = None
        self.vis_tr = None
        self.n_up = self.n_down = 0

    def from_near(self, data: bytes, addr: tuple) -> None:
        self.near_addr = addr
        self.n_up += 1
        self.up.push(data, lambda d=data: self.far_tr.sendto(d))

    def from_far(self, data: bytes) -> None:
        self.n_down += 1
        addr = self.near_addr
        if addr is None or self.vis_tr is None:
            return
        self.down.push(data, lambda d=data, a=addr: self.vis_tr.sendto(d, a))

async def make_relay(dest_addr: tuple, up: Shaper, down: Shaper, name: str) -> tuple:
    """Build a UDPRelay toward `dest_addr` and return (relay, advertised_addr)."""
    loop = asyncio.get_running_loop()

    class _Far(asyncio.DatagramProtocol):
        def datagram_received(self, data, addr):
            relay.from_far(data)

    far_tr, _ = await loop.create_datagram_endpoint(
        _Far, remote_addr=dest_addr)
    relay = UDPRelay(far_tr, up, down, name)

    class _Vis(asyncio.DatagramProtocol):
        def datagram_received(self, data, addr):
            relay.from_near(data, addr)

    vis_tr, vis_proto = await loop.create_datagram_endpoint(
        _Vis, local_addr=("127.0.0.1", 0))
    relay.vis_tr = vis_tr
    return relay, vis_tr.get_extra_info("sockname")

# ---------------------------------------------------------------- ICE rewrite -

CAND_RE = re.compile(r"a=candidate:(\S+) (\d+) (\S+) (\d+) (\S+) (\d+) typ (\S+)(.*)")

async def rewrite_candidates(sdp_text: str, up: Shaper, down: Shaper, tag: str) -> str:
    """Replace every host candidate with a fresh relay endpoint; drop non-host."""
    out_lines = []
    for line in sdp_text.split("\r\n"):
        m = CAND_RE.match(line)
        if not m:
            out_lines.append(line)
            continue
        found, comp, proto, prio, host, port, typ, rest = m.groups()
        if typ != "host" or proto.upper() != "UDP":
            # Drop srflx/relay candidates entirely: only the rewritten host
            # candidates are allowed to carry media, or traffic escapes the shaper.
            continue
        relay, addr = await make_relay((host, int(port)), up, down, tag)
        (rh, rp) = addr
        out_lines.append(
            f"a=candidate:{found} {comp} UDP {prio} {rh} {rp} typ host{rest}")
        LOG.info("%s relay: %s:%s -> %s:%s", tag, rh, rp, host, port)
    return "\r\n".join(out_lines)

# ---------------------------------------------------------------- client ------

async def run_client(ws_url: str, measure_s: float, warmup_s: float,
                     client_type: str = "controller", client_slot: int = -1) -> dict:
    """Run one headless WebRTC client session and measure a settled window.

    Args:
        ws_url: Signaling websocket URL of the server under test.
        measure_s: Length of the measurement window after warmup.
        warmup_s: Maximum time to wait for media before measuring.
        client_type: Role announced in the HELLO message.
        client_slot: Slot announced in the HELLO message.

    Returns:
        The measure_window() summary for the measurement window.
    """
    recorder = REC
    recorder.reset()

    pc = RTCPeerConnection(RTCConfiguration(iceServers=[]))
    state = {"server_puid": None}

    @pc.on("track")
    def on_track(track):
        LOG.info("track: %s", track.kind)

    @pc.on("datachannel")
    def on_dc(channel):
        LOG.info("datachannel: %s", channel.label)
        @channel.on("message")
        def on_msg(msg):
            if isinstance(msg, str) and '"ping"' in msg:
                try:
                    data = json.loads(msg)
                    if data.get("type") == "ping":
                        REC.pings.append((time.time(), float(data["data"]["start_time"])))
                except Exception:
                    pass

    up = CURRENT_SHAPER["up"]
    down = CURRENT_SHAPER["down"]

    session = aiohttp.ClientSession()
    ws = await session.ws_connect(ws_url, heartbeat=5.0)

    async def rx_loop():
        await ws.send_str(f'HELLO client {{"client_type":"{client_type}","client_slot":{client_slot}}}')
        async for msg in ws:
            if msg.type != aiohttp.WSMsgType.TEXT:
                continue
            data = msg.data
            if data == "HELLO":
                await ws.send_str("SESSION server")
            elif data.startswith(("SESSION_OK", "SESSION_END", "ERROR")):
                LOG.info("signaling: %s", data)
            elif data.startswith("SESSION"):
                pass
            else:
                try:
                    peer, payload = data.split(" ", 1)
                    obj = json.loads(payload)
                except ValueError:
                    continue
                state["server_puid"] = state["server_puid"] or peer
                if isinstance(obj.get("sdp"), dict):
                    t, sdp = obj["sdp"].get("type"), obj["sdp"].get("sdp")
                    if t == "offer":
                        LOG.info("offer received (%d bytes)", len(sdp or ""))
                        sdp = await rewrite_candidates(sdp, up, down, "offer")
                        await pc.setRemoteDescription(
                            RTCSessionDescription(sdp=sdp, type=t))
                        answer = await pc.createAnswer()
                        await pc.setLocalDescription(answer)
                        answer_sdp = await rewrite_candidates(
                            pc.localDescription.sdp, up, down, "answer")
                        await ws.send_str(peer + " " + json.dumps(
                            {"sdp": {"type": "answer", "sdp": answer_sdp}}))
                        LOG.info("answer sent")
                    else:
                        await pc.setRemoteDescription(
                            RTCSessionDescription(sdp=sdp, type=t))
                elif isinstance(obj.get("ice"), dict):
                    cand = obj["ice"].get("candidate", "")
                    if cand:
                        ic = candidate_from_sdp(
                            cand.split(":", 1)[1]
                            if cand.startswith("candidate") else cand)
                        ic.sdpMLineIndex = obj["ice"].get("sdpMLineIndex", 0)
                        await pc.addIceCandidate(ic)

    # TWCC feedback emitter: mirrors what Chrome/Firefox receivers emit, so the
    # server's pacer + GCC loop get real goodput feedback over the shaped link.
    async def twcc_loop():
        receiver = None
        while True:
            if receiver is None:
                trx = pc.getTransceivers() or []
                at = [t for t in trx if getattr(t, "kind", "") == "audio"]
                if at:
                    receiver = at[0].receiver
            fci = None
            if REC.twcc_arrivals:
                arrivals = REC.twcc_arrivals
                REC.twcc_arrivals = {}
                fci = _build_twcc_fci(arrivals, REC.twcc_fb_count)
                REC.twcc_fb_count += 1
            if fci is not None and receiver is not None:
                try:
                    # RtcpRtpfbPacket.__bytes__ drops fci (NACK-only), so build the
                    # RTCP packet bytes directly: RTPFB(205)/fmt=15.
                    import struct as _st
                    from selkies.webrtc.rtp import pack_rtcp_packet, RTCP_RTPFB, RTCP_RTPFB_TWCC
                    payload = _st.pack("!LL", 0, 0) + fci
                    raw = pack_rtcp_packet(RTCP_RTPFB, RTCP_RTPFB_TWCC, payload)
                    REC.twcc_emitted += 1
                    await receiver.transport._send_rtp(raw)
                except Exception:
                    LOG.debug("twcc emit failed", exc_info=True)
            await asyncio.sleep(0.1)

    rx_task = asyncio.create_task(rx_loop())
    tw_task = asyncio.create_task(twcc_loop())
    window = {}
    try:
        # Warmup: wait for media to start flowing, then let it settle.
        t0 = time.monotonic()
        while time.monotonic() - t0 < warmup_s:
            await asyncio.sleep(0.5)
            if REC.n_video > 0 and REC.n_audio > 0:
                break
        if REC.n_video == 0 and REC.n_audio == 0:
            LOG.warning("no media within warmup; connecting trace follows")
        await asyncio.sleep(2.0)
        LOG.info("warmup end: video pkts=%d audio pkts=%d", REC.n_video, REC.n_audio)
        recorder.reset()
        await asyncio.sleep(measure_s)
        window = measure_window(recorder)
        window["video_saw"] = REC.n_video
        window["audio_saw"] = REC.n_audio
    finally:
        rx_task.cancel()
        tw_task.cancel()
        try:
            await rx_task
        except (asyncio.CancelledError, Exception):
            pass
        try:
            await tw_task
        except (asyncio.CancelledError, Exception):
            pass
        try:
            if state["server_puid"]:
                # No explicit goodbye message: peer cleanup happens on ws close.
                pass
        finally:
            try:
                await ws.close()
            except Exception:
                pass
            await session.close()
            try:
                await pc.close()
            except Exception:
                pass
    return window

# ------------------------------------------------------------- summarizing ----

def measure_window(rec: Optional[Recorder] = None) -> dict:
    """Summarize a Recorder window into the metrics dict a cell reports."""
    rec = rec or REC
    gaps = [b - a for a, b in zip(rec.audio_times, rec.audio_times[1:])]
    gaps.sort()
    over60 = sum(1 for g in gaps if g > 0.060)
    frames = {}
    for t, ts in rec.video_pkts:
        d = frames.get(ts)
        if d is None:
            frames[ts] = [t, t, 1]
        else:
            d[0] = min(d[0], t); d[1] = max(d[1], t); d[2] += 1
    spans = sorted(v[1] - v[0] for v in frames.values())

    def pct(v: list, p: float) -> Optional[float]:
        """Percentile of an already-sorted list, reported in milliseconds."""
        if not v: return None
        return round(1000 * v[min(len(v) - 1, int(p / 100 * len(v)))], 2)

    def pct_s(v: list, p: float) -> Optional[float]:
        """Percentile of an unsorted seconds list (sorts a copy), in milliseconds."""
        if not v: return None
        s = sorted(v)
        return round(1000 * s[min(len(s) - 1, int(p / 100 * len(s)))], 2)

    ping_ow = sorted(b - a for a, b in rec.pings)
    return {
        "ping_ms": {
            "n": len(rec.pings),
            "p50": pct(ping_ow, 50),
            "p99": pct(ping_ow, 99),
            "max": round(1000 * max(ping_ow), 2) if ping_ow else None,
        },
        "audio_pkts": rec.n_audio,
        "audio_gap_ms": {"p50": pct(gaps, 50), "p99": pct(gaps, 99),
                         "max": round(1000 * max(gaps), 2) if gaps else None},
        "audio_gaps_over_60ms": over60,
        "video_pkts": rec.n_video,
        "video_frames": len(frames),
        "frame_span_ms": {"p50": pct(spans, 50), "p99": pct(spans, 99),
                          "max": round(1000 * max(spans), 2) if spans else None},
        "ow_video_ms": {"p50": pct_s(rec.ow_video, 50), "p99": pct_s(rec.ow_video, 99),
                        "max": round(1000 * max(rec.ow_video), 2) if rec.ow_video else None},
        "ow_audio_ms": {"p50": pct_s(rec.ow_audio, 50), "p99": pct_s(rec.ow_audio, 99),
                        "max": round(1000 * max(rec.ow_audio), 2) if rec.ow_audio else None},
        "audio_mbps": round(rec.audio_bytes * 8 / 1e6 / (rec.audio_times[-1] - rec.audio_times[0]), 3) if len(rec.audio_times) > 2 else None,
        "video_mbps": round(rec.video_bytes * 8 / 1e6 / (rec.video_pkts[-1][0] - rec.video_pkts[0][0]), 3) if len(rec.video_pkts) > 2 else None,
    }

# ------------------------------------------------------------ server-side -----

def server_log_delta(path: str, before_bytes: int) -> str:
    """The server log's contents past the `before_bytes` offset."""
    size = os.path.getsize(path)
    with open(path, "rb") as f:
        f.seek(min(before_bytes, size))
        return f.read().decode(errors="replace")

def server_cpu(pid: int) -> Optional[float]:
    """Instantaneous CPU percent of the server process, or None if gone."""
    import psutil
    try:
        p = psutil.Process(pid)
        return round(p.cpu_percent(interval=None), 1)
    except Exception:
        return None

# ------------------------------------------------------------------ cells -----

LOAD_GEN = os.environ.get("PACER_E2E_LOAD", "1") != "0"
CURRENT_SHAPER = {"up": None, "down": None}

async def run_cell(pacer_on: bool, regime: str) -> dict:
    """Run one benchmark cell: a server boot plus one measured client session.

    Args:
        pacer_on: Whether the server runs with SELKIES_WEBRTC_PACER enabled.
        regime: Link shape: ``flat`` (pass-through), ``shaped`` (constant
            constrained), or ``osc`` (rate oscillates between two levels).

    Returns:
        The cell's metrics row, including server CPU and log-derived counts.
    """
    # Pass-through by default; the regimes below replace both legs.
    up = Shaper()
    down = Shaper()
    if regime == "shaped":
        # Client -> server carries input; server -> client carries media.
        up = Shaper(rate_bps=5.5e6, delay_s=0.015)
        down = Shaper(rate_bps=5.5e6, delay_s=0.015)
    elif regime == "osc":
        up = Shaper(rate_bps=8e6, delay_s=0.015)
        down = Shaper(rate_bps=8e6, delay_s=0.015)
        async def _osc():
            while True:
                await asyncio.sleep(4.0)
                up.rate = down.rate = 2.8e6
                await asyncio.sleep(4.0)
                up.rate = down.rate = 8e6
        osc_task = asyncio.create_task(_osc())
    else:
        osc_task = None
    if regime != "osc":
        osc_task = None
    CURRENT_SHAPER["up"], CURRENT_SHAPER["down"] = up, down
    log_before = 0
    env = {
        "SELKIES_WEBRTC_PACER": "true" if pacer_on else "false",
        "SELKIES_MODE": "webrtc",
        "SELKIES_TURN_REST_URI": "",
        "SELKIES_STUN_HOST": "",
        "SELKIES_VIDEO_BITRATE": "8000",
        "SELKIES_DEBUG": "true",
    }
    # Forward pacer tuning knobs from our own env to the server under test.
    for k in os.environ:
        if k.startswith("SELKIES_WEBRTC_PACER_") or k in ("SELKIES_CONGESTION_CONTROL", "SELKIES_ENCODER"):
            env[k] = os.environ[k]
    log = os.path.join(RUNNER_DIR, "server.log")
    proc = H.server_start(mode="webrtc", extra_env=env, log=log)
    from psutil import Process
    pr = Process(proc.pid)
    # The first cpu_percent() call primes psutil's delta counter.
    pr.cpu_percent()
    window = {}
    load_proc = None
    try:
        if LOAD_GEN:
            load_proc = subprocess.Popen(
                [os.path.join(H.TOOLS, "pacer_load_gen.sh")],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        log_before = os.path.getsize(log)
        url = f"ws://127.0.0.1:{H.PORT}/api/ws"
        ms = 32.0 if regime == "osc" else 25.0
        window = await run_client(url, measure_s=ms, warmup_s=8.0)
    finally:
        if osc_task is not None:
            osc_task.cancel()
        if load_proc is not None:
            load_proc.terminate()
            subprocess.run(["pkill", "-9", "-f", "pacer-load"], capture_output=True)
        cell_log = server_log_delta(log, log_before)
        # Second cpu_percent() reads the average since the priming call.
        cpu = pr.cpu_percent()
        H.server_stop()
    return {
        "pacer": pacer_on, "regime": regime,
        **window,
        "server_cpu_pct": cpu,
        "server_resets": cell_log.count("GOP reset"),
        "server_keyreq_lines": cell_log.count("requestKeyframe")
                             + cell_log.count("keyframe requested"),
    }

async def main() -> bool:
    """Run every pacer/regime cell and require media in each."""
    cells = [
        (False, "osc"), (True, "osc"),
        (False, "osc2"), (True, "osc2"),
        (False, "flat"), (True, "flat"),
        (False, "shaped"), (True, "shaped"),
    ]
    rows = []
    for pacer_on, reg in cells:
        regime = "osc" if reg.startswith("osc") else ("shaped" if reg.startswith("shaped") else "flat")
        LOG.info("=== cell pacer=%s regime=%s", pacer_on, reg)
        row = await run_cell(pacer_on, regime)
        row["cell"] = reg
        rows.append(row)
        print(json.dumps(row), flush=True)
    with open(os.path.join(RUNNER_DIR, "pacer_e2e_results.json"), "w") as f:
        json.dump(rows, f, indent=1)
    print("DONE ->", os.path.join(RUNNER_DIR, "pacer_e2e_results.json"))
    # The numbers are the point of this benchmark and are left to the reader;
    # what it asserts is that every cell completed with media actually flowing,
    # which is what breaks when the pacer or the ICE path regresses.
    empty = [f"pacer={r['pacer']} {r['cell']}" for r in rows
             if not r.get("video_pkts") or not r.get("audio_pkts")]
    print("RESULT", "all cells carried media" if not empty else f"FAILED: {empty}")
    return not empty

if __name__ == "__main__":
    sys.exit(0 if asyncio.run(main()) else 1)
