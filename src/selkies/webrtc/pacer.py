# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Strict-priority packet pacer for the DTLS transport choke point.
#
# Architecture selected by empirical A/B/C/E/F/G candidate search over a
# deterministic bottleneck model with real asyncio senders (see
# /home/ubuntu/build/pacer_bench, results_round{1,2,3}.json):
#
#   * Event-driven token-bucket scheduling (wake at the exact next affordable
#     instant) with an immediate fast path when the queue is empty and credit
#     covers the packet. Fixed-timer polling variants lose ~8% goodput to
#     timer-jitter x debt-cap clipping and add ~100 ms video lag; in-sender
#     sleeping cannot protect other classes at all.
#   * Priority classes: RTCP/audio > interactive data-channel > video
#     (incl. RTX/FEC) > bulk data-channel. Sharing audio's class with DC puts
#     audio behind clipboard storms; bulk DC must sit BELOW video.
#   * RTCP and audio bypass the bucket entirely. They are protocol rate-
#     limited and tiny; queuing them behind video credit debt only adds wake
#     latency to exactly the traffic this pacer exists to protect.
#   * Video-only, IDR-aware queue budget. A fixed queue budget smaller than
#     one variable-size IDR truncates every keyframe under sustained
#     congestion and breaks the reference chain permanently, so the floor
#     tracks the max of the last few observed IDR sizes (a sliding window,
#     NOT time-decay: per-keyframe decay collapses precisely when congestion
#     forces frequent keyframes, which is when the floor matters most).
#   * GOP-reset posture on overflow: drop the damaged GOP wholesale and ask
#     the encoder for a fresh keyframe (throttled), rather than thinning
#     arbitrary packet tails. Deadline walls and IDR smoothing were tried and
#     rejected by measurement.
#   * Wake-on-affordable: packets enqueued while the drain sleeps poke it if
#     credit already covers them, so neither sleep overshoot nor coalescing
#     adds latency to other classes.
#
# Anti-spiral lessons from LIVE E2E (a simulator cannot produce these):
#   * A goodput estimate measures THIS pacer's own output once it throttles.
#     Braking fractionally/subtractively on that signal is a feedback loop
#     that always drifts (saturated wire = link limit, not capacity;
#     unsaturated wire = own injection — three variants were tried and each
#     failed differently live). The stable control law is classic AIMD: the
#     internal queue overflow is the ONLY trustworthy congestion signal
#     (like TCP's packet loss), so the pace is braked at overflow events
#     (to est - bypass-reserve when an estimate exists, halved otherwise)
#     and recovers multiplicatively (+25%/s) while overflow stays quiet.
#   * A GOP-dead state must have a timeout resurrect: if the keyframe
#     callback is unbound or the encoder ignores it, no IDR will ever come
#     to lift the dead state (natural IDR cadence can be minutes).
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
CLASS_DC = 1        # interactive data-channel traffic (input, status, small msgs)
CLASS_VIDEO = 2     # video RTP, RTX and FEC
CLASS_BULK_DC = 3   # bulk data-channel traffic (clipboard, file transfer)
_NUM_CLASSES = 4

PACE_FACTOR = 2.5          # pace above encoder target when the link allows
BYPASS_RESERVE_BPS = 500_000  # wire share reserved for bypass classes (audio
                           # + RTCP + DC) when braking from an estimate
OVERFLOW_RECOVERY_PER_S = 0.25  # AIMD: multiplicative pace recovery (+25%/s)
                           # in the absence of internal overflow events
AIMD_FLOOR_FACTOR = 0.35   # brake depth floor (x encoder target): chained
                           # overflow brakes must never crater the stream into
                           # a starvation valley that +25%/s takes seconds to
                           # escape (measured live: one osc run collapsed to
                           # ~1/14 of frames without a floor)
DEBT_WINDOW_S = 0.005      # burst credit window (5 ms; the 20 ms variant hurt audio)
CAP_MIN_MS = 120           # video queue budget floor, time-denominated
IDR_FLOOR_FACTOR = 2.2     # IDR-size floor for the video queue budget
IDR_WINDOW = 4             # rolling window of recent IDR sizes for the floor
KEYREQ_MIN_INTERVAL_S = 0.5
MIN_PACE_BPS = 100_000     # sanity floor: a 0-goodput feedback window must
                           # never stall (or divide-by-zero) the scheduler
DRAIN_MAX_SLEEP_S = 0.05   # defensive cap on drain sleeps; pokes handle the rest
GOODPUT_WARMUP_S = 5.0     # post-enable grace: ignore goodput braking so the
                           # first (audio-only/startup) feedback windows cannot
                           # slam the pace before video even ramps
RESURRECT_TIMEOUT_S = 1.0  # if no keyframe resurrects a dead GOP within this
                           # of the last keyreq, resurrect on queue-drain anyway
VIDEO_STALE_S = float(os.environ.get("SELKIES_WEBRTC_PACER_STALE_MS", "0")) / 1000.0
# Latency-first: whole-GOP deadline for queued video; never tighter than one
# IDR's wire time. DEFAULT OFF: live A/B at 350 ms showed purging the GOP
# inflates delivered-video one-way p99 (2.2 s vs 1.5 s without it): the
# receiver's jitter buffer stalls on the purge gap and smears the cost into
# later frames, eating the freshness gained. Configurable for links where
# the tradeoff is wanted; unit-tested.
MIN_GOODPUT_SAMPLE_BYTES = 2048  # near-empty windows carry no rate signal

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
    Nothing but video is ever dropped.
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

        self._queues: Dict[int, Deque[bytes]] = {c: deque() for c in range(_NUM_CLASSES)}
        self._poke = asyncio.Event()
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
        self._saturated = False
        self._last_overflow_at = 0.0
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
        """Periodic observability: emit only when activity changed progressfully, so
        a quiet link costs zero log lines."""
        changed = {
            k: self.stats[k] - self._stats_prev.get(k, 0)
            for k in self.stats
            if self.stats[k] != self._stats_prev.get(k, 0)
        }
        self._stats_prev = dict(self.stats)
        if changed:
            logger.info(
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
        bps = int(bps)
        if bps > 0:
            self._goodput_bps = bps

    def _on_overflow(self) -> None:
        """AIMD brake, fired by internal queue overflow — the ONLY trustworthy
        congestion signal: a full queue means injection > wire, so the last
        measured wire rate bounds capacity. Without an estimate yet, halve
        (classic TCP-style response to a loss event)."""
        now = time.monotonic()
        self._last_overflow_at = now
        self._saturated = True
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
            self._debt_cap = self._pace_bps / 8.0 * DEBT_WINDOW_S
            if self.credit > self._debt_cap:
                self.credit = self._debt_cap

    def _maybe_recover_pace(self) -> None:
        # +25%/s multiplicative recovery toward the encoder ceiling,
        # accumulating only while overflow stays quiet.
        now = time.monotonic()
        dt = now - self._last_overflow_at
        if dt < 1.0:
            return
        ceiling = PACE_FACTOR * self._encoder_bps
        if self._pace_bps >= ceiling:
            self._saturated = False
            self._last_overflow_at = now
            return
        grown = int(self._pace_bps * (1.0 + OVERFLOW_RECOVERY_PER_S) ** min(dt, 4.0))
        self._pace_bps = min(int(ceiling), max(grown, MIN_PACE_BPS))
        if self._pace_bps >= ceiling:
            self._saturated = False
        self._debt_cap = self._pace_bps / 8.0 * DEBT_WINDOW_S
        self._last_overflow_at = now

    def _set_goodput_bps_unslewed_for_tests(self, bps: Optional[float]) -> None:
        """Test hook: plant an estimate as-if mid-session."""
        self._enabled_at -= GOODPUT_WARMUP_S + 1.0
        if bps is not None and int(bps) > 0:
            self._goodput_bps = int(bps)

    def _make_class_table(self) -> None:
        """Precomputed per-class (queue, sender) pairs: per-packet work must not
        re-derive what never changes."""
        self._class_table = [
            (self._queues[CLASS_RTCP], self._send_now),
            (self._queues[CLASS_DC], self._send_now_data),
            (self._queues[CLASS_VIDEO], self._send_now),
            (self._queues[CLASS_BULK_DC], self._send_now_data),
        ]

    def _refresh_windows(self) -> None:
        ceiling = PACE_FACTOR * self._encoder_bps
        # Pre-first-overflow the encoder setting owns the pace (config
        # tracking); afterwards it can only CLAMP the AIMD-controlled pace.
        if self._last_overflow_at == 0.0:
            self._pace_bps = int(ceiling)
        else:
            self._pace_bps = min(self._pace_bps, int(ceiling))
        self._pace_bps = max(self._pace_bps, MIN_PACE_BPS)
        self._debt_cap = self._pace_bps / 8.0 * DEBT_WINDOW_S
        if self.credit > self._debt_cap:
            self.credit = self._debt_cap

    def _video_cap_bytes(self) -> int:
        base = int(self._pace_bps / 8.0 * CAP_MIN_MS / 1000)
        return max(base, self._idr_floor_bytes)

    # ----------------------------------------------------------------- inputs
    def note_keyframe(self, total_payload_bytes: int, natural: bool = True) -> None:
        """Feed the sliding-window IDR floor and resurrect the stream after a
        GOP reset. Only NATURAL keyframes update the floor: forced mini-
        keyframes (emitted at collapsed bitrate in response to our own
        congestion signal) would shrink the floor, shrink the cap, and so
        trigger the next reset — a self-reinforcing collapse measured in
        E2E as a permanent keyframe-churn cycle under steady congestion."""
        if natural:
            self._idr_sizes.append(int(total_payload_bytes))
            self._idr_floor_bytes = int(max(self._idr_sizes) * IDR_FLOOR_FACTOR)
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
        # they never queue and never consume video's credit. This removes the
        # wake-wait they previously paid while video debt rebuilt (the exact
        # latency this whole mechanism exists to protect them from).
        if cls == CLASS_RTCP:
            self.stats["fastpath_bytes"] += n
            await self._send_now(data)
            return

        self._maybe_recover_pace()

        if cls == CLASS_VIDEO and self._gop_dead:
            # Safety net: if no keyframe resurrected us within the timeout of
            # the last keyreq (broken/unbound callback, encoder stuck), let
            # video flow again anyway; the decoder's own repair (#287 path)
            # recovers from the reference break, while a permanently dead
            # class is unrecoverable by definition.
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
                sender = self._send_now_data if cls in (CLASS_DC, CLASS_BULK_DC) \
                    else self._send_now
                await sender(data)
                return

        # Video queue budget: purge oldest video to fit, then GOP-reset.
        # cap is video-only: audio/DC bulk can never push an IDR out.
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
        self._reset_gop()
        dq = self._queues[CLASS_VIDEO]
        n = len(dq)
        if n:
            dq.clear()
            self._video_ts.clear()
            self.stats["video_dropped"] += n
            self._bytes_queued -= self._video_bytes
            self._video_bytes = 0
        self.stats["stale_resets"] += 1
        logger.info("pacer: video backlog stale (>%.0fms) => GOP reset + purge (%d pkts)",
                    deadline_s * 1000, n)

    def _reset_gop(self) -> None:
        if not self._gop_dead:
            self._gop_dead = True
            self._gop_dead_at = time.monotonic()
            self._on_overflow()
            self.stats["gop_resets"] += 1
            logger.info("pacer: video queue overflow => GOP reset, keyframe requested")
            self.request_keyframe_once()

    # ------------------------------------------------------------------ drain
    def _kick(self) -> None:
        task = self._drain_task
        if self._stopped or (task is not None and not task.done()):
            return
        self._drain_task = self._loop.create_task(self._drain())

    async def _drain(self) -> None:
        try:
            while not self._stopped:
                # Clear the poke BEFORE the pass: a poke arriving during the
                # pass must survive into the wait below (clear-right-before-
                # sleep racy-deletes pokes and added measured 75 ms of DC
                # wait behind an IDR head).
                self._poke.clear()
                self._maybe_recover_pace()
                self._accrue()
                for cls, (dq, sender) in enumerate(self._class_table):
                    while dq and len(dq[0]) <= self.credit:
                        data = dq.popleft()
                        self._bytes_queued -= len(data)
                        if cls == CLASS_VIDEO:
                            self._video_bytes -= len(data)
                            self._video_ts.popleft()
                        self.credit -= len(data)
                        try:
                            await sender(data)
                        except Exception:
                            logger.warning("pacer: send failed; dropping queue",
                                           exc_info=True)
                            self._queues = {c: deque() for c in range(_NUM_CLASSES)}
                            self._video_ts.clear()
                            self._make_class_table()
                            self._bytes_queued = self._video_bytes = 0
                            return
                        self.stats["paced_bytes"] += len(data)
                if not self._bytes_queued:
                    return
                head = 0
                for dq, _ in self._class_table:
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
