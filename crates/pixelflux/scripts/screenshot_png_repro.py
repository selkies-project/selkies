#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Reproduce screenshot admission and compositor blocking on a private blank desktop.

Requires only PixelFlux and Python 3.9+. This small reproducer has no client or
video, so its timings are not a streaming benchmark or a pixel-fidelity test.
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import struct
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--width", type=int, default=3840)
    parser.add_argument("--height", type=int, default=2160)
    parser.add_argument("--render-node", default="")
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()
    if args.width <= 0 or args.height <= 0:
        parser.error("Dimensions must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    result = {"status": "incomplete", "requests": [], "geometry_ms": []}
    result_path = args.output / "results.json"
    result_path.write_text(json.dumps(result) + "\n")
    with tempfile.TemporaryDirectory(prefix="pf-repro-", dir="/tmp") as runtime:
        os.environ["XDG_RUNTIME_DIR"] = runtime
        for name in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"):
            path = Path(runtime) / name.lower()
            path.mkdir()
            os.environ[name] = str(path)
        for name in ("DISPLAY", "WAYLAND_DISPLAY", "PIXELFLUX_CU", "PIXELFLUX_RECORD"):
            os.environ.pop(name, None)
        try:
            import pixelflux
            module = Path(pixelflux.__file__)
            native = [module] if module.suffix == ".so" else list(module.parent.glob("*.so"))
            result["installed_module_hashes"] = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in native}
            result["script_sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
            result["arguments"] = {key: str(value) if isinstance(value, Path) else value
                                   for key, value in vars(args).items()}
            result["socket"] = pixelflux.ensure_wayland_display(width=args.width, height=args.height,
                render_node=args.render_node, auto_gpu="")
            control = pixelflux.ScreenCapture()
            if not result["socket"] or not control.create_output(2, args.width, args.height, args.width, 0):
                raise RuntimeError("Private outputs did not start")

            def screenshot(display, barrier=None):
                if barrier:
                    barrier.wait(timeout=10)
                start = time.monotonic_ns()
                row = {"display": display}
                try:
                    png = bytes(pixelflux.screenshot_png(display))
                    if len(png) < 33 or png[:16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR":
                        raise ValueError("Response lacks a PNG signature and IHDR")
                    row.update(status="ok", size=list(struct.unpack(">II", png[16:24])),
                               bytes=len(png), sha256=hashlib.sha256(png).hexdigest())
                except Exception as error:
                    row.update(status="error", error=str(error))
                row["latency_ms"] = (time.monotonic_ns() - start) / 1e6
                return row

            view_size = [min(args.width, 256), min(args.height, 128)]
            if not control.create_view(7, 0, 0, 0, *view_size):
                raise RuntimeError("Private view did not start")
            result["view_without_capture"] = screenshot(7)
            result["view_expected_size"] = view_size
            result["view_geometry_correct"] = (result["view_without_capture"]["status"] == "ok" and
                                               result["view_without_capture"]["size"] == view_size)
            for _ in range(3):
                barrier = threading.Barrier(2)
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                    calls = [executor.submit(screenshot, display, barrier) for display in (0, 2)]
                    result["requests"].append([call.result(timeout=15) for call in calls])
            def load():
                return [screenshot(0) for _ in range(8)]
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                pending = executor.submit(load)
                while not pending.done():
                    start = time.monotonic_ns()
                    geometry = control.get_realized_geometry(0)
                    result["geometry_ms"].append({"ms": (time.monotonic_ns() - start) / 1e6,
                                                  "value": geometry})
                    time.sleep(0.01)
                result["compression_load"] = pending.result(timeout=15)
            result["distinct_outputs_both_answer"] = all(
                row["status"] == "ok" and row["size"] == [args.width, args.height]
                for wave in result["requests"] for row in wave)
            result["compression_calls_answer"] = all(row["status"] == "ok" and
                row["size"] == [args.width, args.height] for row in result["compression_load"])
            result["geometry_answers_correct"] = bool(result["geometry_ms"]) and all(
                row["value"] == (args.width, args.height, 1.0) for row in result["geometry_ms"])
            result["checks_passed"] = all(result[name] for name in (
                "view_geometry_correct", "distinct_outputs_both_answer", "compression_calls_answer", "geometry_answers_correct"))
            result["status"] = "completed" if result["checks_passed"] else "completed_with_failures"
        except Exception as error:
            result["fatal_error"] = repr(error)
            raise
        finally:
            result_path.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    if args.strict and not result["checks_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
