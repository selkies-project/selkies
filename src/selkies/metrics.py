# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Prometheus gauges and histograms both transports feed, plus an optional
per-connection CSV dump of client-reported WebRTC statistics. CSV writes run
on a dedicated single-worker thread pool so they preserve row order, never
block the event loop, and can be drained deterministically at teardown.
"""
import asyncio
import csv
import json
import logging
import os
import threading
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from typing import Any, Dict, List, Optional, Tuple

from prometheus_client import REGISTRY
from prometheus_client import Gauge, Histogram, Info

logger_metrics = logging.getLogger("metrics")
logger_metrics.setLevel(logging.INFO)

FPS_HIST_BUCKETS = (0, 20, 40, 60)

# Bound the diagnostic stats CSV: field names come from the untrusted client, so cap
# header width and retained rows so it can't grow the file unbounded.
WEBRTC_CSV_MAX_HEADERS = 2048
WEBRTC_CSV_MAX_RETAINED_ROWS = 100000

class Metrics:
    """Prometheus metrics plus optional CSV capture of client WebRTC stats.

    Registers gauges/histograms in the global Prometheus registry at
    construction; `unregister()` must release every one of them or the next
    `Metrics()` raises DuplicateTimeseries. When `using_webrtc_csv` is set,
    client-reported stat dictionaries are also appended to per-connection CSV
    files whose column schema follows the (untrusted) client's field set with
    bounded width and row count.

    Attributes:
        webrtc_pacer_pace_bps: Pacer gauges are per display and exist only
            while a pacer is attached; the event counters are cumulative
            since transport start.
        webrtc_bridge_dropped_frames: Per display, cumulative since the graph
            was built; unlike the pacer counters it is published whether or not
            a pacer is attached. `webrtc_bridge_invalidated_frames` counts the
            share of those the encoder was told to predict past, the rest being
            frames that predicted from one already dropped.
        prev_stats_video_header_names: Header names of the video CSV (and
            `prev_stats_audio_header_names` for audio), tracked alongside the
            lengths so a same-count field swap still triggers a remap.
        stats_video_row_count: On-disk data rows (excluding the header) of the
            video CSV (`stats_audio_row_count` for audio), so the append path
            bounds file growth without re-reading the file each write.
        _csv_lock: Serializes CSV writes, which run in worker threads, so
            concurrent stat messages cannot interleave rows or race the
            `prev_stats_*` state.
        _csv_executor: Single-worker executor for CSV writes, so `unregister`
            can drain them with `shutdown(wait=True)` (the shared default
            executor must not be shut down) and rows keep their order.
        _csv_tasks: Strong references to in-flight write futures so they are
            not collected before completion and their exceptions stay
            observed.
    """

    def __init__(self, using_webrtc_csv: bool = False):
        self.using_webrtc_csv = using_webrtc_csv

        self.fps = Gauge('fps', 'Frames per second observed by client')
        self.fps_hist = Histogram('fps_hist', 'Histogram of FPS observed by client', buckets=FPS_HIST_BUCKETS)
        self.gpu_utilization = Gauge('gpu_utilization', 'Utilization percentage reported by GPU')
        self.latency = Gauge('latency', 'Latency observed by client')
        self.webrtc_statistics = Info('webrtc_statistics', 'WebRTC Statistics from the client')
        self.webrtc_pacer_pace_bps = Gauge(
            'webrtc_pacer_pace_bps', 'Current pacer rate in bits per second', ['display'])
        self.webrtc_pacer_queue_bytes = Gauge(
            'webrtc_pacer_queue_bytes', 'Bytes queued in the pacer', ['display', 'kind'])
        self.webrtc_pacer_idr_floor_bytes = Gauge(
            'webrtc_pacer_idr_floor_bytes', 'IDR floor of the pacer video queue budget in bytes', ['display'])
        self.webrtc_pacer_events = Gauge(
            'webrtc_pacer_events', 'Cumulative pacer event counter', ['display', 'event'])
        self.webrtc_bridge_dropped_frames = Gauge(
            'webrtc_bridge_dropped_frames',
            'Encoded frames dropped before packetization, per display', ['display'])
        self.webrtc_bridge_invalidated_frames = Gauge(
            'webrtc_bridge_invalidated_frames',
            'Dropped frames the encoder was told to predict past, per display', ['display'])
        self.stats_video_file_path: Optional[str] = None
        self.stats_audio_file_path: Optional[str] = None
        self.prev_stats_video_header_len: Optional[int]  = None
        self.prev_stats_audio_header_len: Optional[int]  = None
        self.prev_stats_video_header_names: Optional[Tuple[str, ...]] = None
        self.prev_stats_audio_header_names: Optional[Tuple[str, ...]] = None
        self.stats_video_row_count: int = 0
        self.stats_audio_row_count: int = 0
        self._csv_lock = threading.Lock()
        self._csv_executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="webrtc-csv")
        self._csv_tasks: set = set()

    def set_fps(self, fps: float) -> None:
        """Records the client-observed FPS in both the gauge and histogram."""
        self.fps.set(fps)
        self.fps_hist.observe(fps)

    def set_pacer_snapshot(self, display: str, snap: Optional[dict]) -> None:
        """Publish one pacer snapshot per display; no-ops when there is no pacer."""
        if snap is None:
            return
        display = display or "primary"
        self.webrtc_pacer_pace_bps.labels(display).set(snap.get("pace_bps", 0))
        self.webrtc_pacer_queue_bytes.labels(display, "total").set(snap.get("queued_bytes", 0))
        self.webrtc_pacer_queue_bytes.labels(display, "video").set(snap.get("video_bytes", 0))
        self.webrtc_pacer_idr_floor_bytes.labels(display).set(snap.get("idr_floor_bytes", 0))
        for event in ("video_dropped", "gop_resets", "keyreqs",
                      "idr_resurrects", "timeout_resurrects", "stale_resets"):
            self.webrtc_pacer_events.labels(display, event).set(snap.get(event, 0))

    def set_bridge_drops(self, drops: Dict[str, tuple]) -> None:
        """Publish each display's bridge drop and invalidation counts (see
        `RTCApp.bridge_drops`)."""
        for display, (dropped, invalidated) in drops.items():
            self.webrtc_bridge_dropped_frames.labels(display or "primary").set(dropped)
            self.webrtc_bridge_invalidated_frames.labels(display or "primary").set(invalidated)

    def set_gpu_utilization(self, utilization: float) -> None:
        self.gpu_utilization.set(utilization)

    def set_latency(self, latency_ms: float) -> None:
        self.latency.set(latency_ms)

    def unregister(self) -> None:
        """Unregisters all metrics from the global registry and drains CSV writers.

        Not-yet-started CSV futures are canceled and the executor shut down
        with `wait=True` first, so no writer thread is still running (or about
        to take the lock) after teardown; draining the lock alone would leave
        that window open. Every collector built in `__init__` is then released,
        each independently so an already-released one does not strand the
        rest: any left behind makes the next `Metrics()` raise
        DuplicateTimeseries and a mode switch back into metrics-enabled
        streaming fails to start.
        """
        for fut in list(self._csv_tasks):
            fut.cancel()
        self._csv_tasks.clear()
        self._csv_executor.shutdown(wait=True)
        for collector in (self.fps, self.fps_hist, self.gpu_utilization,
                          self.latency, self.webrtc_statistics,
                          self.webrtc_pacer_pace_bps, self.webrtc_pacer_queue_bytes,
                          self.webrtc_pacer_idr_floor_bytes, self.webrtc_pacer_events,
                          self.webrtc_bridge_dropped_frames):
            try:
                REGISTRY.unregister(collector)
            except KeyError:
                pass

    async def set_webrtc_stats(self, webrtc_stat_type: str, webrtc_stats: str) -> None:
        """Publishes a client stats report to Prometheus and, optionally, CSV.

        The CSV write is submitted to the dedicated executor rather than
        `asyncio.to_thread`, whose shared executor cannot be drained, so
        `unregister` can join it; a write refused by an already shut-down
        executor (teardown in progress) is dropped. The Prometheus Info
        update is a cheap dict copy and stays inline.

        Args:
            webrtc_stat_type: `_stats_audio` for the audio stream; anything
                else is treated as video.
            webrtc_stats: Raw JSON list of RTCStats-shaped objects from the
                client. Parsing/sanitizing runs in a worker thread to keep
                large reports off the event loop.
        """
        sanitized_stats = await asyncio.to_thread(self._parse_and_sanitize_stats, webrtc_stats)
        if self.using_webrtc_csv:
            is_audio = webrtc_stat_type == "_stats_audio"
            csv_path = self.stats_audio_file_path if is_audio else self.stats_video_file_path
            try:
                fut = self._csv_executor.submit(self.write_webrtc_stats_csv, sanitized_stats, csv_path, is_audio)
            except RuntimeError:
                fut = None
            if fut is not None:
                self._csv_tasks.add(fut)
                fut.add_done_callback(self._csv_tasks.discard)
        self.webrtc_statistics.info(sanitized_stats)

    def _parse_and_sanitize_stats(self, webrtc_stats: str) -> OrderedDict:
        return self.sanitize_json_stats(json.loads(webrtc_stats))

    def sanitize_json_stats(self, obj_list: List[Dict[str, Any]]) -> OrderedDict:
        """Flattens a list of RTCStats objects into `reportName.fieldName` keys.

        The first entry of each stat type gets the bare type as its report
        name; later same-type entries get a `-id` suffix, or `-n` (the
        per-type occurrence index) without an id, plus a collision counter.
        Both stay stable across reorders and inserts, unlike a global list
        index, which would shift every column name whenever the browser
        reordered the list and churn the CSV schema (full rewrites, an
        unbounded union header). Entries are sorted by `(type, id)` first for
        the same reason: the browser may emit same-type stats (two
        `inbound-rtp` for distinct SSRCs) in a different order each message,
        and without a fixed order the bare-named first occurrence could be a
        different SSRC on each row; `sorted` is stable, so entries sharing a
        key keep their input order. All values are stringified. Entries that
        are not dicts are skipped and a missing/non-string `type` defaults to
        `unknown`, since the list comes from the untrusted browser client.
        """
        obj_type = set()
        sanitized_stats = OrderedDict()
        type_counts: Dict[str, int] = {}

        def _identity(entry: Any) -> Tuple[str, str]:
            """Stable `(type, id)` sort key; non-dict entries sort first."""
            if not isinstance(entry, dict):
                return ("", "")
            t = entry.get('type')
            t = t if isinstance(t, str) else "unknown"
            i = entry.get('id')
            i = i if isinstance(i, str) else ""
            return (t, i)

        for entry in sorted(obj_list, key=_identity) if isinstance(obj_list, list) else obj_list:
            if not isinstance(entry, dict):
                continue
            base_key = entry.get('type')
            if not isinstance(base_key, str):
                base_key = "unknown"
            occurrence = type_counts.get(base_key, 0)
            type_counts[base_key] = occurrence + 1
            curr_key = base_key
            if curr_key in obj_type:
                entry_id = entry.get('id')
                if isinstance(entry_id, str) and entry_id:
                    suffix = entry_id
                else:
                    suffix = str(occurrence)
                candidate_key = curr_key + "-" + suffix
                collision = 0
                while candidate_key in obj_type:
                    collision += 1
                    candidate_key = curr_key + "-" + suffix + "-" + str(collision)
                curr_key = candidate_key
            obj_type.add(curr_key)

            for key, val in entry.items():
                unique_type = curr_key + "." + str(key)
                if not isinstance(val, str):
                    sanitized_stats[unique_type] = str(val)
                else:
                    sanitized_stats[unique_type] = val

        return sanitized_stats

    def _bump_and_cap_rows(self, file_path: str, is_audio: bool) -> None:
        """Counts one appended data row, trimming the file once over the cap.

        Tracking the count avoids re-reading the file on every write; the
        O(N) trim runs only when the count exceeds
        WEBRTC_CSV_MAX_RETAINED_ROWS. Caller must hold `self._csv_lock`.
        """
        if is_audio:
            self.stats_audio_row_count += 1
            if self.stats_audio_row_count > WEBRTC_CSV_MAX_RETAINED_ROWS:
                self.stats_audio_row_count = self._trim_csv_to_cap(file_path)
        else:
            self.stats_video_row_count += 1
            if self.stats_video_row_count > WEBRTC_CSV_MAX_RETAINED_ROWS:
                self.stats_video_row_count = self._trim_csv_to_cap(file_path)

    def _trim_csv_to_cap(self, file_path: str) -> int:
        """Drops the oldest data rows so the file stays within the row cap.

        Keeps at most WEBRTC_CSV_MAX_RETAINED_ROWS rows plus the header,
        bounding on-disk growth on the steady-state append path. Rewrites via
        a temp file and atomic replace so an interrupted trim cannot corrupt
        the stats. Caller must hold `self._csv_lock`.

        Returns:
            The resulting on-disk data-row count.
        """
        with open(file_path, 'r', newline='') as stats_file:
            rows = list(csv.reader(stats_file, delimiter=','))
        if not rows:
            return 0
        header, data = rows[0], rows[1:]
        if len(data) <= WEBRTC_CSV_MAX_RETAINED_ROWS:
            return len(data)
        data = data[-WEBRTC_CSV_MAX_RETAINED_ROWS:]
        tmp_path = file_path + ".tmp"
        with open(tmp_path, 'w', newline='') as stats_file:
            csv_writer = csv.writer(stats_file)
            csv_writer.writerow(header)
            csv_writer.writerows(data)
        os.replace(tmp_path, file_path)
        return len(data)

    def write_webrtc_stats_csv(self, obj: dict, file_path: str, is_audio: bool = False) -> None:
        """Appends one sanitized stats report to the CSV file.

        Runs on the dedicated CSV executor thread. Handles three schema cases:
        the same field set in a different order (single-row remap, no
        rewrite), a changed field set (full union-schema rewrite via
        `update_webrtc_stats_csv`), and a fresh file (header plus first row).

        Args:
            obj: Flattened `reportName.fieldName` stats mapping from
                `sanitize_json_stats`.
            file_path: Destination CSV path.
            is_audio: Whether this is the audio stream, passed by the caller
                rather than re-derived from the file path; selects which
                header/row-count state to use.
        """

        dt = datetime.now()
        timestamp = dt.strftime("%d/%B/%Y:%H:%M:%S")
        with self._csv_lock:
            try:
                headers = ["timestamp"]
                headers += obj.keys()

                # Reconnecting clients send near-empty reports; too few fields to
                # be a real stats sample.
                if len(headers) < 15:
                    return

                values = [timestamp]
                values.extend(obj.values())

                header_names = tuple(headers)
                prev_len = self.prev_stats_audio_header_len if is_audio else self.prev_stats_video_header_len
                prev_names = self.prev_stats_audio_header_names if is_audio else self.prev_stats_video_header_names

                if prev_len is not None and prev_names != header_names:
                    if prev_names is not None and frozenset(prev_names) == frozenset(header_names):
                        value_by_name = dict(zip(headers, values))
                        remapped = [value_by_name.get(name, "NaN") for name in prev_names]
                        with open(file_path, 'a+', newline='') as stats_file:
                            csv.writer(stats_file, quotechar='"').writerow(remapped)
                        self._bump_and_cap_rows(file_path, is_audio)
                        return

                    # Outside any open handle: os.replace() onto an open file fails on Windows.
                    new_len, new_names, new_rows = self.update_webrtc_stats_csv(file_path, headers, values, is_audio)
                    if is_audio:
                        self.prev_stats_audio_header_len = new_len
                        self.prev_stats_audio_header_names = new_names
                        self.stats_audio_row_count = new_rows
                    else:
                        self.prev_stats_video_header_len = new_len
                        self.prev_stats_video_header_names = new_names
                        self.stats_video_row_count = new_rows
                    return

                with open(file_path, 'a+', newline='') as stats_file:
                    csv_writer = csv.writer(stats_file, quotechar='"')
                    if prev_len is None:
                        csv_writer.writerow(headers)
                        csv_writer.writerow(values)
                        if is_audio:
                            self.prev_stats_audio_header_len = len(headers)
                            self.prev_stats_audio_header_names = header_names
                            self.stats_audio_row_count = 1
                        else:
                            self.prev_stats_video_header_len = len(headers)
                            self.prev_stats_video_header_names = header_names
                            self.stats_video_row_count = 1
                    else:
                        csv_writer.writerow(values)
                if prev_len is not None:
                    self._bump_and_cap_rows(file_path, is_audio)

            except Exception as e:
                logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))

    def update_webrtc_stats_csv(self, file_path: str, headers: List[str], values: List[Any], is_audio: bool = False) -> Tuple[Optional[int], Optional[Tuple[str, ...]], int]:
        """Rewrites the CSV when the set of stat fields changes.

        The stored rows are aligned by field name onto the union header (prior
        order, new fields appended, width capped at `WEBRTC_CSV_MAX_HEADERS`
        because the names come from the client), gaps filled with "NaN", and
        only the most recent `WEBRTC_CSV_MAX_RETAINED_ROWS` rows are carried
        forward. The rewrite goes through a temp file and an atomic replace so
        an interrupted one cannot truncate the stats; a file deleted since the
        last write, or holding only a header, is recreated with the current
        schema. Caller must hold `self._csv_lock`.

        Returns:
            A tuple of the new header length, the new header-name tuple, and
            the resulting on-disk data-row count — or the previous values on
            failure so the caller's state stays consistent with the file.
        """
        prev_len = self.prev_stats_audio_header_len if is_audio else self.prev_stats_video_header_len
        prev_names = self.prev_stats_audio_header_names if is_audio else self.prev_stats_video_header_names
        prev_rows = self.stats_audio_row_count if is_audio else self.stats_video_row_count

        try:
            prev_headers = None
            prev_values = []
            try:
                with open(file_path, 'r', newline='') as stats_file:
                    csv_reader = csv.reader(stats_file, delimiter=',')
                    for idx, row in enumerate(csv_reader):
                        if idx == 0:
                            prev_headers = row
                        else:
                            prev_values.append(row)
            except FileNotFoundError:
                pass

            if not prev_headers:
                with open(file_path, 'w', newline='') as stats_file:
                    csv_writer = csv.writer(stats_file)
                    csv_writer.writerow(headers)
                    csv_writer.writerow(values)
                return len(headers), tuple(headers), 1

            merged_headers = list(prev_headers)
            seen_names = set(prev_headers)
            for name in headers:
                if name not in seen_names:
                    if len(merged_headers) >= WEBRTC_CSV_MAX_HEADERS:
                        logger_metrics.warning(
                            "WebRTC Statistics header width capped at %d columns; "
                            "dropping additional fields", WEBRTC_CSV_MAX_HEADERS)
                        break
                    merged_headers.append(name)
                    seen_names.add(name)

            if len(prev_values) > WEBRTC_CSV_MAX_RETAINED_ROWS:
                prev_values = prev_values[-WEBRTC_CSV_MAX_RETAINED_ROWS:]

            prev_index = {name: pos for pos, name in enumerate(prev_headers)}
            new_index = {name: pos for pos, name in enumerate(headers)}

            def remap(row_values: List[Any], src_index: Dict[str, int]) -> List[Any]:
                out = []
                for name in merged_headers:
                    pos = src_index.get(name)
                    if pos is not None and pos < len(row_values):
                        out.append(row_values[pos])
                    else:
                        out.append("NaN")
                return out

            remapped_prev = [remap(row, prev_index) for row in prev_values]
            remapped_new = remap(values, new_index)

            tmp_path = file_path + ".tmp"
            with open(tmp_path, 'w', newline='') as stats_file:
                csv_writer = csv.writer(stats_file)
                csv_writer.writerow(merged_headers)
                csv_writer.writerows(remapped_prev)
                csv_writer.writerow(remapped_new)
            os.replace(tmp_path, file_path)

            logger_metrics.debug("WebRTC Statistics file {} rewritten with updated schema".format(file_path))
            return len(merged_headers), tuple(merged_headers), len(remapped_prev) + 1
        except Exception as e:
            logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))
            return prev_len, prev_names, prev_rows

    async def initialize_webrtc_csv_file(self, webrtc_stats_dir: str = '/tmp') -> None:
        """Points CSV capture at fresh timestamped files for a new connection.

        The header state is reset under `_csv_lock` off the loop thread: a CSV
        rewrite in flight on the executor may hold the lock, so taking it here
        would stall the event loop, and without it a worker could read a torn
        `(len, names)` pair and wrongly rewrite.
        """
        dt = datetime.now()
        timestamp = dt.strftime("%Y-%m-%d:%H:%M:%S")
        self.stats_video_file_path = '{}/selkies-stats-video-{}.csv'.format(webrtc_stats_dir, timestamp)
        self.stats_audio_file_path = '{}/selkies-stats-audio-{}.csv'.format(webrtc_stats_dir, timestamp)
        await asyncio.to_thread(self._reset_csv_header_state)

    def _reset_csv_header_state(self) -> None:
        with self._csv_lock:
            self.prev_stats_video_header_len = None
            self.prev_stats_audio_header_len = None
            self.prev_stats_video_header_names = None
            self.prev_stats_audio_header_names = None
            self.stats_video_row_count = 0
            self.stats_audio_row_count = 0
