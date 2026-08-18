# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Strict-priority packet pacer for the DTLS transport choke point.
#
# One RtpPacer per RTCDtlsTransport. Priority classes, highest first:
# RTCP + audio (which bypass the token bucket entirely: they are protocol
# rate-limited, tiny and latency-critical), data-channel, video (incl. RTX and
# FEC).
#
# Scheduling is an event-driven token bucket. A packet goes straight out when
# nothing is queued and credit covers it; otherwise it queues and the drain
# task sleeps to the exact instant credit covers the highest-priority head. An
# enqueue that credit already covers pokes the drain awake, so neither sleep
# overshoot nor coalescing adds latency to another class.
#
# Invariants:
#   * The burst budget always covers one max-size packet, otherwise a packet
#     could never become affordable and its class would wedge forever.
#   * Video is the only droppable class; every other class is bounded by
#     backpressure on the sender instead.
#   * The video queue budget is max(CAP_MIN_MS of wire time, IDR_FLOOR_FACTOR x
#     the largest of the last IDR_WINDOW keyframes). A budget below one IDR
#     truncates every keyframe under sustained congestion and breaks the
#     reference chain permanently, hence the sliding-window floor.
#   * Overflow drops the whole GOP and requests a throttled keyframe rather
#     than thinning arbitrary packet tails; a timeout resurrects video if no
#     keyframe arrives, so an unbound keyframe callback or a stuck encoder
#     cannot kill the class permanently (natural IDR cadence can be minutes).
#   * Rate control is AIMD driven solely by internal queue overflow: a full
#     queue means injection > wire, so it is the only congestion signal not
#     contaminated by this pacer's own output. Brake to (goodput estimate -
#     BYPASS_RESERVE_BPS) when an estimate exists, else halve, floored at
#     AIMD_FLOOR_FACTOR x the encoder target; recover +25%/s while overflow
#     stays quiet, ceilinged at PACE_FACTOR x the encoder target.
import asyncio
import logging
import os
import time
from typing import Awaitable, Callable, Dict, Deque, Optional
from collections import deque

logger = logging.getLogger("selkies_webrtc_pacer")

# Priority classes (lower value wins). RTCP + audio share a class: both are
# tiny absolute rates and must never queue behind anything else.
CLASS_RTCP = 0
CLASS_AUDIO = 0
# CLASS_DC carries data-channel traffic (input, status, clipboard);
# CLASS_VIDEO carries video RTP, RTX and FEC.
CLASS_DC = 1
CLASS_VIDEO = 2
# Classes that own a queue: class 0 bypasses the bucket and is never enqueued.
_QUEUED_CLASSES = (CLASS_DC, CLASS_VIDEO)

# Pace above the encoder target when the link allows.
PACE_FACTOR = 2.5
# Wire share reserved for bypass classes (audio + RTCP + DC) when braking
# from an estimate.
BYPASS_RESERVE_BPS = 500_000
# AIMD: multiplicative pace recovery (+25%/s) in the absence of internal
# overflow events.
OVERFLOW_RECOVERY_PER_S = 0.25
# Brake depth floor (x encoder target): chained overflow brakes must never
# crater the stream into a starvation valley that +25%/s takes seconds to
# climb out of.
AIMD_FLOOR_FACTOR = 0.35
# Burst credit window: how much wire time the bucket may hoard. Wider windows
# let video bursts land in front of audio.
DEBT_WINDOW_S = 0.005
# Floor under the burst budget. Credit saturates at the budget, so a budget
# below one packet leaves that packet unaffordable forever; DTLS records here
# are MTU-sized and this keeps headroom above them.
BURST_FLOOR_BYTES = 2048
# Video queue budget floor, time-denominated.
CAP_MIN_MS = 120
# IDR-size floor for the video queue budget, over a rolling window of recent
# IDR sizes.
IDR_FLOOR_FACTOR = 2.2
IDR_WINDOW = 4
KEYREQ_MIN_INTERVAL_S = 0.5
# Sanity floor: a 0-goodput feedback window must never stall (or
# divide-by-zero) the scheduler.
MIN_PACE_BPS = 100_000
# Defensive cap on drain sleeps; pokes handle the rest.
DRAIN_MAX_SLEEP_S = 0.05
# Post-enable grace: ignore goodput braking so the first (audio-only/startup)
# feedback windows cannot slam the pace before video even ramps.
GOODPUT_WARMUP_S = 5.0
# If no keyframe resurrects a dead GOP within this of the last keyreq,
# resurrect on queue-drain anyway.
RESURRECT_TIMEOUT_S = 1.0
# Data-channel backpressure: senders block once the backlog passes the high
# water mark and are released again below the low water mark.
DC_HIGH_WATER_BYTES = 2_000_000
DC_LOW_WATER_BYTES = 1_000_000
# Cap on one backpressure wait: a wedged wire must slow senders, never park
# them forever.
DC_BLOCK_TIMEOUT_S = 1.0


def _stale_deadline_s() -> float:
    """Read SELKIES_WEBRTC_PACER_STALE_MS as seconds; a malformed value disables
    the deadline instead of killing the import."""
    raw = os.environ.get("SELKIES_WEBRTC_PACER_STALE_MS", "0")
    try:
        return max(0.0, float(raw)) / 1000.0
    except ValueError:
        logger.warning("pacer: ignoring malformed "
                       "SELKIES_WEBRTC_PACER_STALE_MS=%r", raw)
        return 0.0


# Whole-GOP deadline for queued video, never tighter than one IDR's wire time.
# Off by default: purging the backlog stalls the receiver's jitter buffer on the
# resulting gap, which costs more delivered-video latency than the freshness it
# buys on most links. Set SELKIES_WEBRTC_PACER_STALE_MS to enable.
VIDEO_STALE_S = _stale_deadline_s()
# Near-empty windows carry no rate signal.
MIN_GOODPUT_SAMPLE_BYTES = 2048

SendNow = Callable[[bytes], Awaitable[None]]


def h264_payloads_suggest_idr(payloads) -> bool:
    """Cheap keyframe hint from the first few payload bytes. Handles raw NAL
    (IDR=5/SPS=7 at byte 0), STAP-A aggregation (type 24: first inner NAL at
    offset 3) and FU-A fragmentation (type 28+start bit: original NAL type in
    byte 1) — pixelflux/libx264 keyframes announce themselves as leading
    SPS(7)/IDR(5) inside one of those wrappings. False negatives only
    under-inflate the IDR floor temporarily; false positives just age out
    of the window."""
    for payload in payloads[:3]:
        if not payload:
            continue
        t = payload[0] & 0x1F
        if t in (5, 7):
            return True
        if t == 24 and len(payload) > 3 and (payload[3] & 0x1F) in (5, 7):
            return True
        if t == 28 and len(payload) > 1 and (payload[1] & 0x80) \
                and (payload[1] & 0x1F) in (5, 7):
            return True
    return False


class RtpPacer:
    """Token-bucket pacer with strict priority classes for one RTP/DTLS flow.

    Owned by one RTCDtlsTransport. All classes share a single credit window.
    Video packets that cannot fit the queue budget trigger a GOP reset: queued
    video is purged, subsequent video is dropped until a fresh keyframe
    resurrects the stream, and a throttled keyframe request is emitted.
    Video is the only class ever dropped; data-channel senders are throttled by
    backpressure instead, since their traffic is reliable.
    """

    def __init__(
        self,
        encoder_bps: int,
        send_now: SendNow,
        send_now_data: Optional[SendNow] = None,
        request_keyframe: Optional[Callable[[], None]] = None,
        loop: Optional[asyncio.AbstractEventLoop] = None,
    ) -> None:
        self._encoder_bps = max(int(encoder_bps), 100_000)
        self._goodput_bps: Optional[int] = None
        self._send_now = send_now
        self._send_now_data = send_now_data or send_now
        self._request_keyframe = request_keyframe
        self._loop = loop or asyncio.get_running_loop()

        self._queues: Dict[int, Deque[bytes]] = {c: deque() for c in _QUEUED_CLASSES}
        self._poke = asyncio.Event()
        # Set while the backlog is below the low-water mark: data-channel
        # senders wait on it once the backlog crosses the high-water mark.
        self._drained = asyncio.Event()
        self._drained.set()
        self._make_class_table()
        self._bytes_queued = 0
        self._video_bytes = 0
        # Enqueue-time mirror of the video queue: head age drives the
        # latency-first stale-GOP deadline. Kept in lockstep at every
        # mutation site (enqueue / trim / drain / wholesale clears).
        self._video_ts: Deque[float] = deque()
        self.credit = 0.0
        self._debt_cap = 0.0
        self._pace_bps = MIN_PACE_BPS
        # Sentinel for "AIMD owns the pace now"; distinct from the recovery
        # clock, which ticks on every pace update.
        self._ever_overflowed = False
        self._last_pace_update_at = 0.0
        self._refresh_windows()
        self._last = time.monotonic()
        self._drain_task: Optional[asyncio.Task] = None
        self._stopped = False
        self._idr_sizes: Deque[int] = deque(maxlen=IDR_WINDOW)
        self._idr_floor_bytes = 0
        self._gop_dead = False
        self._last_keyreq = 0.0
        self._enabled_at = self._last
        self._gop_dead_at = 0.0
        self._oversize_warned = False
        self.stats = {
            "video_dropped": 0, "keyreqs": 0, "gop_resets": 0,
            "idr_resurrects": 0, "timeout_resurrects": 0, "stale_resets": 0,
            "paced_bytes": 0, "queue_max_bytes": 0, "fastpath_bytes": 0,
        }
        self._stats_prev: dict = {}
        self._stats_timer: Optional[asyncio.TimerHandle] = None
        self._arm_stats_log()

    def _arm_stats_log(self) -> None:
        if self._stopped:
            return
        self._stats_timer = self._loop.call_later(10.0, self._maybe_log_stats)

    def _maybe_log_stats(self) -> None:
        """Periodic counter deltas, at debug: a streaming link moves them every
        interval, so this is one line per pacer per interval for as long as the
        session lasts. The same numbers are exported as webrtc_pacer_* gauges,
        which is where a running deployment should read them."""
        changed = {
            k: self.stats[k] - self._stats_prev.get(k, 0)
            for k in self.stats
            if self.stats[k] != self._stats_prev.get(k, 0)
        }
        self._stats_prev = dict(self.stats)
        if changed:
            logger.debug(
                "pacer: +%s | total %s", changed, self.snapshot())
        self._arm_stats_log()

    # ------------------------------------------------------------------ rates
    def set_encoder_bps(self, bps: int) -> None:
        bps = max(int(bps), 100_000)
        if bps != self._encoder_bps:
            self._encoder_bps = bps
            self._refresh_windows()

    def set_goodput_bps(self, bps: Optional[float]) -> None:
        # The estimate is CONTROL INPUT ONLY AT OVERFLOW EVENTS: braking on
        # own-output measurement continuously is a feedback loop that always
        # drifts (saturated wire = link, not capacity; unsaturated wire = own
        # injection). AIMD: estimates merely size the step taken on overflow.
        if bps is None:
            return
        if time.monotonic() - self._enabled_at < GOODPUT_WARMUP_S:
            # The first feedback windows after enabling carry audio-only (or
            # no) traffic: their throughput measures the offered load, not the
            # link, and would size an early brake far below capacity.
            return
        bps = int(bps)
        if bps > 0:
            self._goodput_bps = bps

    def _on_overflow(self) -> None:
        """AIMD brake, fired by internal queue overflow — the ONLY trustworthy
        congestion signal: a full queue means injection > wire, so the last
        measured wire rate bounds capacity. Without an estimate yet, halve
        (classic TCP-style response to a loss event)."""
        now = time.monotonic()
        self._ever_overflowed = True
        self._last_pace_update_at = now
        est = self._goodput_bps
        if est:
            target = max(MIN_PACE_BPS, est - BYPASS_RESERVE_BPS)
        else:
            target = max(MIN_PACE_BPS, self._pace_bps // 2)
        # Depth floor: never crater below a usable video share of the user's
        # encoder target (starvation valleys take seconds to recover from).
        target = max(target, int(AIMD_FLOOR_FACTOR * self._encoder_bps))
        if target < self._pace_bps:
            self._pace_bps = target
            self._apply_pace()

    def _maybe_recover_pace(self) -> None:
        # +25%/s multiplicative recovery toward the encoder ceiling,
        # accumulating only while overflow stays quiet.
        now = time.monotonic()
        dt = now - self._last_pace_update_at
        if dt < 1.0:
            return
        ceiling = PACE_FACTOR * self._encoder_bps
        self._last_pace_update_at = now
        if self._pace_bps >= ceiling:
            return
        grown = int(self._pace_bps * (1.0 + OVERFLOW_RECOVERY_PER_S) ** min(dt, 4.0))
        self._pace_bps = min(int(ceiling), max(grown, MIN_PACE_BPS))
        self._apply_pace()

    def _make_class_table(self) -> None:
        """Precomputed per-class (class, queue, sender) triples in priority
        order: per-packet work must not re-derive what never changes."""
        self._class_table = [
            (CLASS_DC, self._queues[CLASS_DC], self._send_now_data),
            (CLASS_VIDEO, self._queues[CLASS_VIDEO], self._send_now),
        ]

    def _apply_pace(self) -> None:
        """Re-derive the burst budget from the current pace and clamp credit to
        it. Credit saturates at the budget, so the budget is floored at
        BURST_FLOOR_BYTES: below one packet's size, that packet could never
        become affordable and its class would wedge."""
        self._debt_cap = max(self._pace_bps / 8.0 * DEBT_WINDOW_S,
                             float(BURST_FLOOR_BYTES))
        if self.credit > self._debt_cap:
            self.credit = self._debt_cap

    def _refresh_windows(self) -> None:
        ceiling = PACE_FACTOR * self._encoder_bps
        # Until the first overflow the encoder setting owns the pace (config
        # tracking); afterwards it can only CLAMP the AIMD-controlled pace.
        if not self._ever_overflowed:
            self._pace_bps = int(ceiling)
        else:
            self._pace_bps = min(self._pace_bps, int(ceiling))
        self._pace_bps = max(self._pace_bps, MIN_PACE_BPS)
        self._apply_pace()

    def _video_cap_bytes(self) -> int:
        base = int(self._pace_bps / 8.0 * CAP_MIN_MS / 1000)
        return max(base, self._idr_floor_bytes)

    # ----------------------------------------------------------------- inputs
    def note_keyframe(self, total_payload_bytes: int, natural: bool = True) -> None:
        """Feed the sliding-window IDR floor and resurrect the stream after a
        GOP reset.

        Only NATURAL keyframes enter the window: a forced keyframe is emitted at
        the collapsed bitrate that made us ask for it, so letting it evict a
        window entry would shrink the floor, shrink the cap and trigger the next
        reset — a self-reinforcing keyframe-churn cycle. A forced keyframe may
        still RAISE the floor, so a cap too small to hold one IDR still grows out
        of the churn.

        A keyframe arriving within KEYREQ_MIN_INTERVAL_S of our own request is
        treated as forced whatever the caller says: on the pre-encoded pack()
        path the sender packetizes frames it did not encode, so it cannot tell
        the two apart.
        """
        size = int(total_payload_bytes)
        if natural and time.monotonic() - self._last_keyreq < KEYREQ_MIN_INTERVAL_S:
            natural = False
        if natural:
            self._idr_sizes.append(size)
            self._idr_floor_bytes = int(max(self._idr_sizes) * IDR_FLOOR_FACTOR)
        else:
            self._idr_floor_bytes = max(self._idr_floor_bytes,
                                        int(size * IDR_FLOOR_FACTOR))
        if self._gop_dead:
            self._gop_dead = False
            self.stats["idr_resurrects"] += 1

    def request_keyframe_once(self) -> None:
        now = time.monotonic()
        if now - self._last_keyreq < KEYREQ_MIN_INTERVAL_S:
            return
        self._last_keyreq = now
        if self._request_keyframe is None:
            logger.warning("pacer: no keyframe callback bound; GOP reset "
                           "relies on the %.1fs timeout resurrect", RESURRECT_TIMEOUT_S)
            return
        self.stats["keyreqs"] += 1
        try:
            self._request_keyframe()
        except Exception:
            logger.debug("pacer keyframe request failed", exc_info=True)

    # ------------------------------------------------------------------- send
    def _accrue(self) -> None:
        now = time.monotonic()
        self.credit = min(self._debt_cap,
                          self.credit + (now - self._last) * self._pace_bps / 8.0)
        self._last = now

    async def send(self, data: bytes, cls: int) -> None:
        if self._stopped:
            raise ConnectionError("pacer stopped")
        n = len(data)

        # RTCP/audio bypass: protocol rate-limited and latency-critical, so
        # they never queue and never consume video's credit.
        if cls == CLASS_RTCP:
            self.stats["fastpath_bytes"] += n
            await self._send_now(data)
            return

        self._maybe_recover_pace()

        if cls != CLASS_VIDEO and self._bytes_queued >= DC_HIGH_WATER_BYTES:
            await self._await_backlog_drain()
            if self._stopped:
                raise ConnectionError("pacer stopped")

        if cls == CLASS_VIDEO and self._gop_dead:
            # Safety net: if no keyframe resurrected us within the timeout of
            # the last keyreq (broken/unbound callback, encoder stuck), let
            # video flow again anyway. The decoder recovers from the reference
            # break at the next keyframe; a permanently dead class does not.
            if time.monotonic() - self._gop_dead_at >= RESURRECT_TIMEOUT_S:
                self._gop_dead = False
                self.stats["timeout_resurrects"] += 1
                logger.info("pacer: no keyframe within %.1fs of reset; "
                            "resurrecting video optimistically", RESURRECT_TIMEOUT_S)
            else:
                self.stats["video_dropped"] += 1
                return

        # Latency-first stale deadline: video sitting in queue longer than
        # the deadline is delivered-too-late by definition, so the whole GOP
        # is purged and a fresh keyframe requested (fewer stale frames > more
        # stale frames). The deadline is never tighter than one IDR's wire
        # time — otherwise reset churn would deliver zero video.
        if VIDEO_STALE_S > 0 and cls == CLASS_VIDEO and self._video_ts:
            deadline = VIDEO_STALE_S
            if self._idr_floor_bytes:
                idr_time = self._idr_floor_bytes * 8.0 / max(self._pace_bps, 1)
                if idr_time > deadline:
                    deadline = idr_time
            now = time.monotonic()
            if now - self._video_ts[0] > deadline:
                self._stale_reset(deadline)
                self.stats["video_dropped"] += 1
                return

        # Fast path when nothing is buffered: anything credit covers goes
        # straight out (~0 added delay below the rate).
        if not self._bytes_queued:
            self._accrue()
            if n <= self.credit:
                self.credit -= n
                self.stats["fastpath_bytes"] += n
                sender = self._send_now_data if cls == CLASS_DC else self._send_now
                await sender(data)
                return

        # Video queue budget: purge oldest video to fit, then GOP-reset.
        # cap is video-only: audio/DC can never push an IDR out.
        if cls == CLASS_VIDEO:
            cap = self._video_cap_bytes()
            if self._video_bytes + n > cap:
                self._reset_gop()
                while self._queues[CLASS_VIDEO] and self._video_bytes + n > cap:
                    old = self._queues[CLASS_VIDEO].popleft()
                    self._video_ts.popleft()
                    self._video_bytes -= len(old)
                    self._bytes_queued -= len(old)
                    self.stats["video_dropped"] += 1
                if self._video_bytes + n > cap:
                    self.stats["video_dropped"] += 1
                    return

        self._queues[cls].append(data)
        if cls == CLASS_VIDEO:
            self._video_bytes += n
            self._video_ts.append(time.monotonic())
        self._bytes_queued += n
        if self._bytes_queued > self.stats["queue_max_bytes"]:
            self.stats["queue_max_bytes"] = self._bytes_queued
        # Wake-on-affordable: if the drain is sleeping through a big head
        # packet but credit already covers THIS newcomer, don't make it wait
        # for the head's deadline.
        self._accrue()
        if n <= self.credit:
            self._poke.set()
        self._kick()

    def _stale_reset(self, deadline_s: float) -> None:
        """Latency-first branch of GOP reset used when queued video outlives
        its usefulness: unlike a cap overflow (which trims only what doesn't
        fit), everything in the video queue belongs to the same late GOP, so
        the whole queue is purged to make room for the fresh keyframe."""
        n = len(self._queues[CLASS_VIDEO])
        self._reset_gop("video backlog stale (>%.0fms, %d pkts purged)"
                        % (deadline_s * 1000, n))
        self._purge_video()
        self.stats["stale_resets"] += 1

    def _purge_video(self) -> None:
        """Drop the whole video queue, keeping the byte counters and the
        enqueue-time mirror in lockstep with it."""
        dq = self._queues[CLASS_VIDEO]
        if dq:
            self.stats["video_dropped"] += len(dq)
            dq.clear()
        self._video_ts.clear()
        self._bytes_queued -= self._video_bytes
        self._video_bytes = 0

    def _reset_gop(self, reason: str = "video queue overflow") -> None:
        if not self._gop_dead:
            self._gop_dead = True
            self._gop_dead_at = time.monotonic()
            self._on_overflow()
            self.stats["gop_resets"] += 1
            logger.info("pacer: %s => GOP reset, keyframe requested", reason)
            self.request_keyframe_once()

    # ------------------------------------------------------------------ drain
    def _kick(self) -> None:
        task = self._drain_task
        if self._stopped or (task is not None and not task.done()):
            return
        self._drain_task = self._loop.create_task(self._drain())

    async def _await_backlog_drain(self) -> None:
        """Backpressure for the reliable classes: hold the caller until the
        backlog falls back to the low-water mark. Bounded so a wedged wire can
        only slow a sender, never park it."""
        self._drained.clear()
        self._kick()
        try:
            await asyncio.wait_for(self._drained.wait(), DC_BLOCK_TIMEOUT_S)
        except asyncio.TimeoutError:
            logger.warning("pacer: %d bytes still queued after %.1fs of "
                           "backpressure; letting the sender through",
                           self._bytes_queued, DC_BLOCK_TIMEOUT_S)

    def _release_senders(self) -> None:
        if not self._drained.is_set():
            self._drained.set()

    async def _drain(self) -> None:
        try:
            while not self._stopped:
                # Clear the poke BEFORE the pass, not right before the wait: a
                # poke arriving mid-pass must survive into the wait below,
                # otherwise a newly affordable packet sleeps until the head's
                # deadline.
                self._poke.clear()
                self._maybe_recover_pace()
                self._accrue()
                for cls, dq, sender in self._class_table:
                    while dq:
                        size = len(dq[0])
                        if size > self.credit:
                            # A packet wider than the whole burst budget can
                            # never be covered by credit; release it once the
                            # bucket is full so its class cannot wedge.
                            if size <= self._debt_cap or self.credit < self._debt_cap:
                                break
                            if not self._oversize_warned:
                                self._oversize_warned = True
                                logger.warning(
                                    "pacer: %d-byte packet exceeds the %d-byte burst "
                                    "budget; releasing oversized packets on a full "
                                    "bucket", size, int(self._debt_cap))
                        data = dq.popleft()
                        self._bytes_queued -= size
                        if cls == CLASS_VIDEO:
                            self._video_bytes -= size
                            self._video_ts.popleft()
                        self.credit -= size
                        try:
                            await sender(data)
                        except Exception:
                            logger.warning("pacer: send failed; dropping queue",
                                           exc_info=True)
                            # The receiver's reference chain dies with the
                            # purged packets: mark video dead so nothing that
                            # depends on them is sent, and ask for a keyframe.
                            self._reset_gop("send failed")
                            self._purge_video()
                            self._queues = {c: deque() for c in _QUEUED_CLASSES}
                            self._make_class_table()
                            self._bytes_queued = self._video_bytes = 0
                            self._release_senders()
                            return
                        self.stats["paced_bytes"] += size
                if self._bytes_queued <= DC_LOW_WATER_BYTES:
                    self._release_senders()
                if not self._bytes_queued:
                    return
                head = 0
                for _, dq, _sender in self._class_table:
                    if dq:
                        head = len(dq[0])
                        break
                if not head:
                    return
                # Nothing more is affordable: sleep to the exact instant
                # credit covers the highest-priority queued packet — but wake
                # early if an enqueue pokes us (newly affordable packet) and
                # never sleep beyond the defensive cap.
                delay = min(
                    max((head - self.credit) * 8.0 / self._pace_bps, 0.0005),
                    DRAIN_MAX_SLEEP_S,
                )
                try:
                    await asyncio.wait_for(self._poke.wait(), delay)
                except asyncio.TimeoutError:
                    pass
        finally:
            self._drain_task = None

    async def close(self) -> None:
        self._stopped = True
        # Waiters re-check _stopped and raise; nothing may stay parked here.
        self._release_senders()
        if self._stats_timer is not None:
            self._stats_timer.cancel()
            self._stats_timer = None
        task = self._drain_task
        if task is not None and not task.done():
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            except Exception:
                pass
        self._drain_task = None
        logger.info("pacer closed: %s", self.snapshot())

    def snapshot(self) -> Dict[str, int]:
        out = dict(self.stats)
        out["pace_bps"] = int(self._pace_bps)
        out["queued_bytes"] = self._bytes_queued
        out["video_bytes"] = self._video_bytes
        out["idr_floor_bytes"] = int(self._idr_floor_bytes)
        out["gop_dead"] = int(self._gop_dead)
        return out
