#!/usr/bin/env python3
"""Transport feedback counts each known packet once, including late arrivals.

A missing status is provisional until the send history expires. A late positive
restores its bytes and retracts a negative only from an interval not yet read by
the controller. Replayed feedback must not steer either congestion or burst size.
"""
import copy
import os
import struct
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import rtcdtlstransport as dtls  # noqa: E402
from selkies.webrtc.rtp import pack_twcc_fci  # noqa: E402


def transport() -> dtls.RTCDtlsTransport:
    """Initialize the real feedback state without starting ICE or DTLS."""
    tr = dtls.RTCDtlsTransport(SimpleNamespace(), [SimpleNamespace()])
    tr._twcc_history = {100: (1000, 0.0), 101: (1100, 0.001), 102: (1200, 0.002)}
    return tr


def feedback(tr: dtls.RTCDtlsTransport, base: int, times: list) -> None:
    tr._twcc_process_feedback(pack_twcc_fci(base, times, 0))


def missing(base: int, count: int) -> bytes:
    """A valid all-missing run, which the uplink feedback builder never emits."""
    return struct.pack("!HH", base, count) + bytes(4) + struct.pack("!H", count)


class AccountingTests(unittest.TestCase):
    """Independent feedback lifecycles with exact byte and observation oracles."""

    def test_late_ack_in_open_window(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, None, 20])
        feedback(tr, 101, [60, 20])
        self.assertEqual(tr._twcc_window["bytes_acked"], 3300)
        self.assertEqual((tr._twcc_window["received"], tr._twcc_window["lost"]), (3, 0))
        self.assertEqual(tr._twcc_history, {})

    def test_late_ack_after_window_read(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, None, 20])
        first = tr.take_twcc_window()
        saved = copy.deepcopy(first)
        feedback(tr, 101, [60, 20])
        second = tr.take_twcc_window()
        self.assertEqual(first, saved)
        self.assertEqual((first["received"], first["lost"]), (2, 1))
        self.assertEqual((second["received"], second["lost"], second["bytes_acked"]), (1, 0, 1100))

    def test_duplicate_positive_is_inert(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, 11, 12])
        saved = copy.deepcopy(tr._twcc_window)
        feedback(tr, 100, [10, 11, 12])
        self.assertEqual(tr._twcc_window, saved)
        tr.take_twcc_window()
        feedback(tr, 100, [10, 11, 12])
        self.assertIsNone(tr.take_twcc_window())

    def test_repeated_negative_is_inert(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, None, 20])
        saved = copy.deepcopy(tr._twcc_window)
        feedback(tr, 100, [10, None, 20])
        self.assertEqual(tr._twcc_window, saved)
        tr.take_twcc_window()
        feedback(tr, 100, [10, None, 20])
        self.assertIsNone(tr.take_twcc_window())

    def test_stale_negative_after_positive_is_inert(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, None, 20])
        feedback(tr, 101, [60, 20])
        saved = copy.deepcopy(tr._twcc_window)
        feedback(tr, 100, [10, None, 20])
        self.assertEqual(tr._twcc_window, saved)

    def test_unknown_feedback_is_not_network_evidence(self) -> None:
        tr = transport()
        feedback(tr, 90, [10, None, 20])
        self.assertIsNone(tr.take_twcc_window())
        self.assertEqual(len(tr._twcc_history), 3)

    def test_local_drop_is_neither_loss_nor_delivery(self) -> None:
        tr = transport()
        tr._twcc_dropped(101)
        feedback(tr, 100, [10, None, 20])
        feedback(tr, 100, [10, None, 20])
        feedback(tr, 101, [60, 20])
        self.assertEqual((tr._twcc_window["received"], tr._twcc_window["lost"]), (2, 0))
        self.assertEqual(tr._twcc_window["bytes_acked"], 2200)

    def test_local_drop_retracts_only_open_negative(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, None, 20])
        tr._twcc_dropped(101)
        self.assertEqual(tr._twcc_window["lost"], 0)
        tr._twcc_dropped(101)
        self.assertEqual(tr._twcc_window["lost"], 0)
        closed = transport()
        feedback(closed, 100, [10, None, 20])
        first = closed.take_twcc_window()
        closed._twcc_dropped(101)
        self.assertEqual(first["lost"], 1)
        self.assertIsNone(closed.take_twcc_window())

    def test_wire_loss_still_counts(self) -> None:
        tr = transport()
        tr._twcc_process_feedback(missing(100, 3))
        window = tr.take_twcc_window()
        self.assertEqual((window["lost"], window["received"], window["loss_fraction"]), (3, 0, 1.0))

    def test_sequence_wrap_and_reuse(self) -> None:
        tr = transport()
        tr._twcc_history = {65535: (1000, 0.0), 0: (1100, 0.001), 1: (1200, 0.002)}
        feedback(tr, 65535, [10, None, 20])
        feedback(tr, 0, [60, 20])
        self.assertEqual((tr._twcc_window["received"], tr._twcc_window["lost"], tr._twcc_window["bytes_acked"]), (3, 0, 3300))
        tr._twcc_seq = 65535
        self.assertEqual(tr._twcc_next(900), 65535)
        self.assertEqual(tr._twcc_next(1000), 0)
        tr._twcc_process_feedback(missing(65535, 2))
        self.assertEqual(tr._twcc_window["lost"], 2)

    def test_reallocation_does_not_reuse_a_negative_marker(self) -> None:
        tr = transport()
        feedback(tr, 100, [10, None, 20])
        tr.take_twcc_window()
        tr._twcc_seq = 101
        self.assertEqual(tr._twcc_next(900), 101)
        tr._twcc_process_feedback(missing(101, 1))
        self.assertEqual(tr._twcc_window["lost"], 1)
        feedback(tr, 101, [100])
        self.assertEqual((tr._twcc_window["lost"], tr._twcc_window["bytes_acked"]), (0, 900))

    def test_history_expiry_bounds_auxiliary_state_after_reuse(self) -> None:
        tr = transport()
        tr._twcc_history.clear()
        with patch.object(dtls.time, "monotonic", return_value=1.0):
            for _ in range(2050):
                tr._twcc_next(100)
        tr._twcc_process_feedback(missing(0, 2050))
        tr.take_twcc_window()
        tr._twcc_seq = 0
        with patch.object(dtls.time, "monotonic", return_value=1.0 + dtls.TWCC_HISTORY_S + 1):
            tr._twcc_next(100)
        self.assertEqual(set(tr._twcc_history), {0})
        self.assertEqual(getattr(tr, "_twcc_missing", {}), {})
        feedback(tr, 1, [100])
        self.assertIsNone(tr.take_twcc_window())

    def test_malformed_feedback_is_atomic(self) -> None:
        for fci in (b"", struct.pack("!HH", 100, 3) + bytes(4),
                    pack_twcc_fci(100, [10, None, 20], 0)[:10],
                    struct.pack("!HH", 100, 3) + bytes(4) + struct.pack("!H", (3 << 13) | 3)):
            with self.subTest(fci=fci.hex()):
                tr = transport()
                before = copy.deepcopy(tr._twcc_history)
                tr._twcc_process_feedback(fci)
                self.assertEqual(tr._twcc_history, before)
                self.assertIsNone(tr.twcc_estimate)
                self.assertIsNone(tr.take_twcc_window())
                self.assertEqual(getattr(tr, "_twcc_missing", {}), {})

    def test_burst_estimator_sees_unique_known_arrivals(self) -> None:
        tr = transport()
        links, brakes = [], []
        tr._pacer = SimpleNamespace(set_link_bps=links.append, set_goodput_bps=brakes.append, burst_probe_bytes=12500)
        with patch.object(dtls, "burst_delivery_bps", wraps=dtls.burst_delivery_bps) as burst:
            feedback(tr, 100, [10, None, 20])
            self.assertEqual(links, [0.0])
            self.assertEqual(burst.call_count, 0)
            feedback(tr, 101, [60, 20])
            self.assertEqual(burst.call_args.args[0], [(0.001, 60000.0, 1100)])
            saved = (len(links), len(brakes), burst.call_count)
            feedback(tr, 101, [60, 20])
            feedback(tr, 90, [10, None, 20])
            self.assertEqual((len(links), len(brakes), burst.call_count), saved)


if __name__ == "__main__":
    unittest.main()
