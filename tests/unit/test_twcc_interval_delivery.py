#!/usr/bin/env python3
"""Feedback partitioning cannot change aggregate delivered throughput.

The same unique arrivals span the same time whether they reach the transport
in one report, many reports, or reordered reports. The first packet opens the
interval; its bytes precede that interval. Per-report pacer estimates retain
their separate burst and idle-exclusion behavior.
"""
import asyncio
import os
import sys
from types import SimpleNamespace
from typing import Optional
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.rtcdtlstransport import RTCDtlsTransport  # noqa: E402
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402
from selkies.webrtc.rtp import RTCP_RTPFB_TWCC, RtcpRtpfbPacket, pack_twcc_fci  # noqa: E402


def transport(count: int, sizes: Optional[dict] = None, base: int = 0) -> RTCDtlsTransport:
    """Create real feedback state with deterministic, known send history."""
    tr = RTCDtlsTransport(SimpleNamespace(), [SimpleNamespace()])
    tr._twcc_history = {(base + i) & 0xFFFF: ((sizes or {}).get(i, 1000), 10 + i * .00125)
                        for i in range(count)}
    return tr


def measure(times: list, partition: int = 20, sizes: Optional[dict] = None,
            reverse: bool = False, duplicate: bool = False, base: int = 0) -> dict:
    tr = transport(len(times), sizes, base)
    starts = list(range(0, len(times), partition))
    if reverse:
        starts.reverse()
    for i in starts:
        fci = pack_twcc_fci((base + i) & 0xFFFF, times[i:i + partition], 0)
        tr._twcc_process_feedback(fci)
        if duplicate:
            tr._twcc_process_feedback(fci)
    return tr.take_twcc_window()


def batched() -> list:
    return [1000 + (i // 20) * 25 + (i % 20) * .25 for i in range(400)]


class IntervalDeliveryTests(unittest.TestCase):
    """Assert rates from independently specified bytes and arrival intervals."""

    def test_regular_partition(self) -> None:
        times = [1000 + i * 1.25 for i in range(400)]
        self.assertEqual([measure(times, n)["goodput_bps"] for n in (400, 20, 7)], [6400000] * 3)

    def test_batched_partition(self) -> None:
        self.assertEqual([measure(batched(), n)["goodput_bps"] for n in (400, 20, 7)], [6653465] * 3)

    def test_single_or_empty_interval(self) -> None:
        value = measure([1000])
        self.assertEqual((value["received"], value["bytes_acked"], value["goodput_bps"]), (1, 1000, 0))
        self.assertIsNone(transport(0).take_twcc_window())
        tr = transport(1)
        tr._twcc_process_feedback(pack_twcc_fci(0, [None, 1000], 0))
        self.assertEqual((tr.take_twcc_window()["goodput_bps"]), 0)

    def test_duplicate_reports_are_inert(self) -> None:
        a, b = measure(batched()), measure(batched(), duplicate=True)
        for key in ("goodput_bps", "bytes_acked", "received", "lost"):
            self.assertEqual(a[key], b[key])

    def test_late_positive_extends_open_interval(self) -> None:
        tr = transport(3)
        tr._twcc_process_feedback(pack_twcc_fci(0, [None, 1010, 1020], 0))
        tr._twcc_process_feedback(pack_twcc_fci(0, [1000], 1))
        value = tr.take_twcc_window()
        self.assertEqual((value["received"], value["lost"], value["goodput_bps"]), (3, 0, 800000))

    def test_late_positive_after_drain_starts_new_interval(self) -> None:
        tr = transport(4)
        tr._twcc_process_feedback(pack_twcc_fci(0, [None, 1010], 0))
        first = tr.take_twcc_window()
        tr._twcc_process_feedback(pack_twcc_fci(2, [1020, 1030], 1))
        tr._twcc_process_feedback(pack_twcc_fci(0, [1000], 2))
        second = tr.take_twcc_window()
        self.assertEqual((first["received"], first["lost"], first["goodput_bps"]), (1, 1, 0))
        self.assertEqual((second["received"], second["lost"], second["goodput_bps"]), (3, 0, 533333))

    def test_reordered_reports_preserve_rate(self) -> None:
        self.assertEqual(measure(batched(), reverse=True)["goodput_bps"], 6653465)

    def test_equal_time_earliest_packet_is_deterministic(self) -> None:
        times, sizes = [1000, 1000, 1001, 1002], {0: 500}
        values = [measure(times, 4, sizes), measure(times, 1, sizes, reverse=True)]
        self.assertEqual([v["goodput_bps"] for v in values], [12000000] * 2)

    def test_idle_time_counts_only_in_aggregate_rate(self) -> None:
        times = [1000, 1001, 2000, 2001]
        self.assertEqual([measure(times, n)["goodput_bps"] for n in (4, 2)], [23976] * 2)
        tr = transport(4)
        tr._twcc_process_feedback(pack_twcc_fci(0, times, 0))
        self.assertEqual(tr.twcc_estimate["goodput_bps"], 8000000)

    def test_drain_resets_delivery_envelope(self) -> None:
        tr = transport(4)
        tr._twcc_process_feedback(pack_twcc_fci(0, [1000, 1001], 0))
        self.assertEqual(tr.take_twcc_window()["goodput_bps"], 8000000)
        tr._twcc_process_feedback(pack_twcc_fci(2, [2000, 2002], 1))
        self.assertEqual(tr.take_twcc_window()["goodput_bps"], 4000000)
        self.assertIsNone(tr.take_twcc_window())

    def test_simultaneous_arrivals_do_not_infer_infinite_rate(self) -> None:
        value = measure([1000] * 400)
        self.assertEqual((value["received"], value["goodput_bps"]), (400, 0))

    def test_reference_wrap_in_both_report_orders(self) -> None:
        wrap_ms = (1 << 24) * 64
        times = [wrap_ms - 2, wrap_ms - 1, wrap_ms, wrap_ms + 1]
        values = [measure(times, 2, reverse=reverse)["goodput_bps"] for reverse in (False, True)]
        self.assertEqual(values, [8000000] * 2)

    def test_sequence_wrap_preserves_partition_and_order(self) -> None:
        values = [measure(batched(), n, reverse=reverse, base=65530)["goodput_bps"]
                  for n, reverse in ((400, False), (20, False), (7, True))]
        self.assertEqual(values, [6653465] * 3)

    def test_unknown_and_locally_dropped_arrivals_do_not_extend_span(self) -> None:
        tr = transport(4)
        tr._twcc_dropped(0)
        tr._twcc_process_feedback(pack_twcc_fci(0, [900, 1000, 1001, 1002, 2000], 0))
        value = tr.take_twcc_window()
        self.assertEqual((value["received"], value["bytes_acked"], value["goodput_bps"]), (3, 3000, 8000000))

    def test_single_packet_reports_keep_pacer_input(self) -> None:
        tr = transport(4)
        links, brakes = [], []
        tr._pacer = SimpleNamespace(set_link_bps=links.append, set_goodput_bps=brakes.append,
                                    burst_probe_bytes=12500)
        for i in range(4):
            tr._twcc_process_feedback(pack_twcc_fci(i, [1000 + i], 0))
            self.assertEqual(tr.twcc_estimate["goodput_bps"], 0)
        self.assertEqual(links, [None] * 4)
        self.assertEqual(brakes, [])
        self.assertEqual(tr.take_twcc_window()["goodput_bps"], 8000000)

    def test_rtcp_routing_reaches_aggregate_measurement(self) -> None:
        async def route() -> dict:
            tr = transport(400)
            sender = RTCRtpSender("video", tr)
            tr._rtp_router.register_sender(sender, sender._ssrc)
            times = batched()
            for i in range(0, 400, 20):
                packet = RtcpRtpfbPacket(fmt=RTCP_RTPFB_TWCC, ssrc=1, media_ssrc=0,
                                        fci=pack_twcc_fci(i, times[i:i + 20], 0))
                await tr._handle_rtcp_data(bytes(packet))
            return tr.take_twcc_window()
        value = asyncio.run(route())
        self.assertEqual((value["received"], value["goodput_bps"]), (400, 6653465))


if __name__ == "__main__":
    unittest.main()
