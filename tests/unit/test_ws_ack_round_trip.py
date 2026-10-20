#!/usr/bin/env python3
"""A WebSockets ack yields its frame's round trip, less the time the client held the id.

An ack held longer than the frame it names has been out is an earlier frame's of the
same id (ids start over with a stream): it yields no round trip and leaves the send
for that frame's own ack, where clamped to 0 it would be the round-trip floor for ten
minutes and every later round trip a standing queue to back the rate off from. The
display says so once a minute.
"""
import os
import sys
import unittest
from collections import OrderedDict
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.websockets_mode import _note_ack  # noqa: E402


# The clock the server reads stands still here, so a round trip is what the send's stamp says
# however long the runner takes between the stamp and the ack.
NOW = 1000.0


def state(**sent_ms_ago) -> dict:
    return {"sent_timestamps": OrderedDict((int(k[1:]), (NOW - ms / 1000.0, 1000)) for k, ms in sent_ms_ago.items()),
            "rtt_samples": []}


class AckRoundTripTests(unittest.TestCase):
    def setUp(self) -> None:
        clock = mock.patch("selkies.websockets_mode.time.monotonic", return_value=NOW)
        clock.start()
        self.addCleanup(clock.stop)

    def test_an_ack_yields_the_round_trip_less_its_hold(self) -> None:
        ds = state(f7=80.0)
        _note_ack(ds, 7, 10.0)
        self.assertAlmostEqual(ds["rtt_floor_ms"], 70.0, delta=5.0)
        self.assertNotIn(7, ds["sent_timestamps"])

    def test_an_ack_held_past_its_send_is_an_earlier_frames(self) -> None:
        ds = state(f7=20.0)
        with self.assertLogs("ws", level="INFO") as logged:
            _note_ack(ds, 7, 5000.0)
            _note_ack(ds, 7, 6000.0)
        self.assertEqual(len(logged.output), 1, "said once a minute")
        self.assertIn("Ack of frame 7 held 5000 ms", logged.output[0])
        self.assertNotIn("rtt_floor_ms", ds, "no round trip of 0")
        self.assertIn(7, ds["sent_timestamps"], "the send waits for its own ack")
        self.assertNotIn("acked_at", ds)
        self.assertEqual(ds["acknowledged_frame_id"], 7, "the client is alive")
        _note_ack(ds, 7, 2.0)
        self.assertAlmostEqual(ds["rtt_floor_ms"], 18.0, delta=5.0)

    def test_a_hold_rounded_past_a_loopback_round_trip_still_counts(self) -> None:
        ds = state(f3=0.2)
        _note_ack(ds, 3, 1.0)
        self.assertEqual(ds["rtt_floor_ms"], 0.0)


if __name__ == "__main__":
    unittest.main()
