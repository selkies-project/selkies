# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Probe PNG16 decoding and canvas precision using a known ten-bit RGB fixture.

This measures a synthetic transport and browser-memory boundary, not PixelFlux
capture or physical display precision. The standard-library fixture has every
ten-bit code, independent color ramps, adjacent one-LSB steps, and dark values.
Readback compares against the original integer codes instead of another canvas.

Run with the existing Playwright test dependency. A browser endpoint file selects
an already prepared isolated test browser; otherwise Playwright launches one.
"""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import struct
import zlib
from typing import Any, Dict, Tuple


def png_chunk(kind: bytes, data: bytes) -> bytes:
    """Encode a PNG chunk with its CRC."""
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data))
    )


def make_fixture() -> Tuple[bytes, Dict[str, Any]]:
    """Return RGB PNG16 and original ten-bit codes with a reversible mapping.

    Each component is stored as ``(v << 6) | (v >> 4)``. The PNG contains
    ``sBIT=10`` and an sRGB intent. The top ten bits recover the source exactly.
    """
    width, height = 1024, 4
    rows = [
        [(x, x, x) for x in range(width)],
        [(x, 1023 - x, (73 * x) % 1024) for x in range(width)],
        [(x ^ 1,) * 3 for x in range(width)],
        [(x % 64,) * 3 for x in range(width)],
    ]
    codes = [value for row in rows for pixel in row for value in pixel]
    raw = b"".join(
        b"\0"
        + b"".join(
            struct.pack(">H", (value << 6) | (value >> 4))
            for pixel in row
            for value in pixel
        )
        for row in rows
    )
    png = b"\x89PNG\r\n\x1a\n" + b"".join(
        [
            png_chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 16, 2, 0, 0, 0)),
            png_chunk(b"sBIT", bytes([10, 10, 10])),
            png_chunk(b"sRGB", bytes([0])),
            png_chunk(b"IDAT", zlib.compress(raw, 9)),
            png_chunk(b"IEND", b""),
        ]
    )
    return png, {"width": width, "height": height, "codes": codes}


def verify_fixture(png: bytes, reference: Dict[str, Any]) -> Dict[str, Any]:
    """Verify CRCs and all original codes in this restricted unfiltered PNG.

    This is a structural check of the generated fixture, not a general-purpose
    PNG decoder. Browser precision is always checked against ``reference``.
    """
    assert png[:8] == b"\x89PNG\r\n\x1a\n"
    offset, compressed = 8, bytearray()
    significant = None
    while offset < len(png):
        length = struct.unpack_from(">I", png, offset)[0]
        kind = png[offset + 4 : offset + 8]
        data = png[offset + 8 : offset + 8 + length]
        crc = struct.unpack_from(">I", png, offset + 8 + length)[0]
        assert zlib.crc32(kind + data) == crc
        if kind == b"IHDR":
            assert struct.unpack(">IIBBBBB", data) == (
                reference["width"], reference["height"], 16, 2, 0, 0, 0
            )
        if kind == b"sBIT":
            significant = list(data)
        if kind == b"IDAT":
            compressed.extend(data)
        offset += length + 12
    assert significant == [10, 10, 10]
    raw = zlib.decompress(compressed)
    stride = reference["width"] * 6 + 1
    assert len(raw) == stride * reference["height"]
    decoded = []
    for y in range(reference["height"]):
        row = raw[y * stride : (y + 1) * stride]
        assert row[0] == 0
        decoded.extend(value[0] >> 6 for value in struct.iter_unpack(">H", row[1:]))
    assert decoded == reference["codes"]
    assert len(set(decoded[: reference["width"] * 3])) == 1024
    return {
        "samples": len(decoded), "exact_samples": len(decoded),
        "container_bits": 16, "significant_bits": significant,
        "max_error_10bit": 0, "gray_levels": 1024,
        "png_sha256": hashlib.sha256(png).hexdigest(),
    }


BROWSER_PROBE = r"""async ({width, height, codes, pngBase64}) => {
    const bytes = Uint8Array.from(atob(pngBase64), c => c.charCodeAt(0));
    const blob = new Blob([bytes], {type: 'image/png'});
    const result = {userAgent: navigator.userAgent, dpr: devicePixelRatio, cases: {}};

    /** Measure recovered integer codes, preserving explicit unsupported states. */
    const measure = data => {
        const is8 = data instanceof Uint8ClampedArray;
        if (data.length !== width * height * 4) throw new Error('Readback sample count differs');
        const rgb = Array.from(data).filter((_, i) => i % 4 !== 3);
        const recovered = rgb.map(v => Math.round(v * (is8 ? 1023 / 255 : 1023)));
        const errors = recovered.map((v, i) => Math.abs(v - codes[i]));
        const gray = Array.from({length: width}, (_, x) => data[x * 4]);
        const exact = errors.filter(v => v === 0).length;
        return {
            status: exact === codes.length ? 'preserved' : 'measured-loss',
            arrayType: data.constructor.name, samples: rgb.length, exact_recovered_10bit_samples: exact,
            max_error_10bit: Math.max(...errors),
            mae_10bit: errors.reduce((a, b) => a + b, 0) / errors.length,
            gray_levels: new Set(gray).size,
            distinct_adjacent_gray_pairs: gray.slice(1).filter((v, i) => v !== gray[i]).length,
        };
    };

    /** Allocate a new canvas and test the actual backing and readback types. */
    const allocate = high => {
        const canvas = document.createElement('canvas');
        canvas.width = width; canvas.height = height;
        const context = canvas.getContext('2d', high
            ? {alpha: false, colorSpace: 'srgb', colorType: 'float16'} : {alpha: false});
        const attributes = context?.getContextAttributes?.() ?? null;
        if (!context || (high && attributes?.colorType !== 'float16')) {
            return {unsupported: 'Requested canvas backing unavailable', attributes};
        }
        return {canvas, context, attributes};
    };

    /** Classify unavailable precision separately from an executed lossy route. */
    const read = (target, high) => {
        if (target.unsupported) return {status: 'unsupported', reason: target.unsupported, attributes: target.attributes};
        if (high && !globalThis.Float16Array) return {status: 'unsupported', reason: 'Float16Array unavailable'};
        let data;
        try {
            data = target.context.getImageData(0, 0, width, height,
                high ? {pixelFormat: 'rgba-float16'} : {}).data;
        } catch (error) {
            if (error.name === 'NotSupportedError' || error.name === 'TypeError') {
                return {status: 'unsupported', reason: 'Requested readback unavailable', error: error.name};
            }
            throw error;
        }
        if (high && !(data instanceof Float16Array)) {
            return {status: 'unsupported', reason: 'Float16 readback not honored', arrayType: data.constructor.name};
        }
        return {...measure(data), attributes: target.attributes};
    };

    const bitmap = await createImageBitmap(blob, {colorSpaceConversion: 'none'});
    if (bitmap.width !== width || bitmap.height !== height) throw new Error('Decoded dimensions differ');
    const standard = allocate(false);
    if (standard.unsupported) throw new Error('Default 2D canvas unavailable');
    standard.context.drawImage(bitmap, 0, 0);
    result.cases.blob_bitmap_default8 = read(standard, false);
    result.cases.default8_float16_readback = read(standard, true);
    const secondBitmap = await createImageBitmap(blob, {colorSpaceConversion: 'none'});
    const other = allocate(false); other.context.drawImage(secondBitmap, 0, 0);
    const a = standard.context.getImageData(0, 0, width, height).data;
    const b = other.context.getImageData(0, 0, width, height).data;
    result.shared8_equality = a.every((v, i) => v === b[i]);
    secondBitmap.close();

    const paths = ['blob_bitmap_none', 'blob_bitmap_default', 'html_image',
        'html_image_bitmap_none', 'html_image_bitmap_default'];
    for (const name of paths) {
        let source, objectUrl;
        try {
            const target = allocate(true);
            if (target.unsupported) {
                result.cases[name] = read(target, true);
                continue;
            }
            if (name === 'blob_bitmap_none') source = bitmap;
            if (name === 'blob_bitmap_default') source = await createImageBitmap(blob);
            if (name.startsWith('html_image')) {
                objectUrl = URL.createObjectURL(blob);
                source = new Image(); source.src = objectUrl; await source.decode();
                if (name === 'html_image_bitmap_none') source = await createImageBitmap(source, {colorSpaceConversion: 'none'});
                if (name === 'html_image_bitmap_default') source = await createImageBitmap(source);
            }
            target.context.drawImage(source, 0, 0);
            result.cases[name] = read(target, true);
        } catch (error) {
            result.cases[name] = {status: 'error', error: error.name};
        } finally {
            if (source !== bitmap) source?.close?.();
            if (objectUrl) URL.revokeObjectURL(objectUrl);
        }
    }
    bitmap.close();
    return result;
}"""


def main() -> None:
    """Write the precision fixture and results from a private browser context."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--browser-endpoint-file", type=Path)
    parser.add_argument("--executable-path")
    parser.add_argument("--require-high-precision", action="store_true",
                        help="Fail unless one complete decode/render route preserves every ten-bit code")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    png, reference = make_fixture()
    results = {
        "scope": "Synthetic PNG transport and browser canvas memory; not capture or monitor proof",
        "fixture": verify_fixture(png, reference),
    }
    (args.output / "ramp10-rgb16.png").write_bytes(png)
    (args.output / "reference.json").write_text(json.dumps(reference) + "\n")
    from playwright.sync_api import sync_playwright

    with sync_playwright() as playwright:
        try:
            if args.browser_endpoint_file:
                launch_options = {
                    "headless": True,
                    "args": ["--no-sandbox", "--disable-dev-shm-usage", "--use-gl=swiftshader"],
                }
                browser = playwright.chromium.connect(
                    args.browser_endpoint_file.read_text().strip(),
                    headers={
                        "x-playwright-browser": "chromium",
                        "x-playwright-launch-options": json.dumps(launch_options),
                    },
                )
                results["launch"] = {"mode": "remote-run-server", **launch_options}
            else:
                options = {"headless": True}
                if args.executable_path:
                    options["executable_path"] = args.executable_path
                browser = playwright.chromium.launch(**options)
                results["launch"] = {"mode": "local", "headless": True, "extra_args": []}
        except Exception:
            raise RuntimeError("Isolated browser connection or launch failed") from None
        try:
            context = browser.new_context(
                viewport={"width": 1100, "height": 200}, device_scale_factor=1
            )
            try:
                page = context.new_page()
                page.set_content("<!doctype html><title>Synthetic precision fixture</title>")
                results["browser_version"] = browser.version
                results["browser"] = page.evaluate(
                    BROWSER_PROBE, {**reference, "pngBase64": base64.b64encode(png).decode("ascii")}
                )
            finally:
                context.close()
        finally:
            browser.close()
    cases = results["browser"]["cases"]
    verified = [
        name for name, case in cases.items()
        if case.get("gray_levels") == 1024
        and case.get("exact_recovered_10bit_samples") == len(reference["codes"])
        and case.get("attributes", {}).get("colorType") == "float16"
        and case.get("arrayType") == "Float16Array"
    ]
    results["precision_gate"] = {
        "verified_canvas_paths": verified,
        "unsupported_paths": [name for name, case in cases.items() if case["status"] == "unsupported"],
        "measured_loss_paths": [name for name, case in cases.items() if case["status"] == "measured-loss"],
        "native_capture_verified": False,
        "physical_display_verified": False,
        "exactness_definition": "All original ten-bit codes recover via round(float_sample * 1023)",
    }
    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    assert results["browser"]["shared8_equality"]
    assert cases["blob_bitmap_default8"]["status"] == "measured-loss"
    assert cases["blob_bitmap_default8"]["gray_levels"] <= 256
    assert all(case["status"] != "error" for case in cases.values())
    if args.require_high_precision:
        assert verified, "No complete ten-bit decode/render route was verified"
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
