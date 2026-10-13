#!/usr/bin/env python3
"""The WebRTC stats CSV shows what the client reported as text.

Its field names and values come from the browser, and the operator opens the
file in a spreadsheet: a cell or header led by a formula character is written
quoted, a number is written as it came, two reports with the same hostile
names land in one schema, and the Prometheus side keeps the values unchanged.
"""
import csv
import os
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from selkies.metrics import Metrics, csv_cell

FORMULA = ("=", "+", "-", "@", "\t", "\r")


def report() -> list:
    plain = {"type": "inbound-rtp", "id": "IT01", **{f"field{i}": i for i in range(16)}}
    hostile = {"type": "=HYPERLINK(\"http://x.test\")", "id": "H1", "-2+3": "@SUM(A1:A9)",
               "codec": "=cmd|' /C calc'!A0", "jitter": "-0.004", "gain": "+3", "note": "\tTAB"}
    return [plain, hostile]


def main() -> bool:
    res = H.Results("stats-csv")
    res.check("a negative number stays a number", csv_cell("-0.004") == "-0.004")
    res.check("a formula is quoted", csv_cell("=1+1") == "'=1+1")
    metrics = Metrics(using_webrtc_csv=True)
    try:
        with tempfile.TemporaryDirectory(prefix="selkies-stats-") as d:
            path = os.path.join(d, "video.csv")
            stats = metrics.sanitize_json_stats(report())
            metrics.write_webrtc_stats_csv(stats, path)
            metrics.write_webrtc_stats_csv(metrics.sanitize_json_stats(report()), path)
            with open(path, newline="") as f:
                rows = list(csv.reader(f))
            cells = [c for row in rows for c in row]
            led = [c for c in cells if c.startswith(FORMULA) and not _number(c)]
            res.check("no cell or header is led by a formula character", not led, led[:5])
            res.check("one header and both reports", len(rows) == 3, len(rows))
            res.check("the hostile names and values are kept, quoted",
                      "'=cmd|' /C calc'!A0" in cells and "'@SUM(A1:A9)" in cells and "-0.004" in cells,
                      [c for c in cells if "cmd" in c or "SUM" in c])
            res.check("the stats the metrics keep are as reported",
                      any(v == "=cmd|' /C calc'!A0" for v in stats.values()))
    finally:
        metrics.unregister()
    return res.summary()


def _number(text: str) -> bool:
    try:
        float(text)
        return True
    except ValueError:
        return False


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
