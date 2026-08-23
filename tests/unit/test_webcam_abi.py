#!/usr/bin/env python3
"""The V4L2 interposer reads the staging ring that pixelflux's virtual camera
writes, so the constants and structs in ``v4l2_interposer.c`` must match the
writer byte for byte. This compiles the interposer's own struct definitions and
compares their layout against the layout the writer publishes, the wire codec
ids of ``selkies.webcam`` against pixelflux's, and the source against the
compiler. pixelflux is consulted when importable; the layout it is expected to
report is pinned here as well, so the source-only run still guards the C side.
"""
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))
INTERPOSER = os.path.join(ROOT, "addons", "v4l2-interposer", "v4l2_interposer.c")

# The writer's layout (pixelflux/src/webcam/ring.rs); pixelflux reports the same
# through VirtualCamera.shm_layout() when it is installed.
EXPECTED = {
    "magic": 0x434B5753,
    "version": 1,
    "ctrl_offset": 128,
    "ctrl_stride": 64,
    "data_offset": 4096,
    "max_slots": 4,
    "config_size": 64,
    "config_fields": [
        "magic", "version", "width", "height", "fourcc", "fps_num", "fps_den", "n_slots",
        "slot_size", "data_offset", "ctrl_offset", "ctrl_stride", "bytesperline", "sizeimage",
    ],
    "header_fields": [
        "magic", "version", "width", "height", "fourcc", "fps_num", "fps_den", "n_slots",
        "slot_size", "data_offset", "bytesperline", "sizeimage", "latest_slot", "_pad",
    ],
    "header_latest_frame_seq_offset": 56,
    "ctrl_fields": [("seq", 0, 4), ("bytesused", 4, 4), ("frame_seq", 8, 8), ("ts_ns", 16, 8)],
}
EXPECTED_CODECS = {"mjpeg": 0, "h264": 1, "vp8": 2, "vp9": 3, "av1": 4, "hevc": 5}

fails = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(f"{'PASS' if ok else 'FAIL'}  {name}{(': ' + detail) if detail else ''}")
    if not ok:
        fails.append(name)


def c_defines(src: str) -> dict:
    out = {}
    for m in re.finditer(r"^#define\s+(WC_SHM_[A-Z_]+|WC_MAX_SLOTS)\s+(0x[0-9A-Fa-f]+|\d+)u?", src, re.M):
        out[m.group(1)] = int(m.group(2), 0)
    return out


def c_struct(src: str, name: str) -> str:
    m = re.search(r"typedef struct \{([^}]*)\} " + re.escape(name) + r";", src, re.S)
    if not m:
        raise SystemExit(f"struct {name} not found in interposer source")
    return m.group(1)


def member_names(body: str) -> list:
    names = []
    for line in body.splitlines():
        line = line.split("/*")[0].strip()
        m = re.match(r"(?:uint8_t|uint32_t|uint64_t)\s+(\w+)(\[\d+\])?;", line)
        if m:
            names.append(m.group(1))
    return names


def compile_layout(src: str) -> dict:
    """offsetof/sizeof of the interposer's structs, from the compiler."""
    structs = {n: c_struct(src, n) for n in ("webcam_config_t", "wc_shm_header_t", "wc_shm_ctrl_t")}
    prog = ["#include <stdio.h>", "#include <stdint.h>", "#include <stddef.h>"]
    for n, body in structs.items():
        prog.append("typedef struct {" + body + "} " + n + ";")
    prog.append("int main(void) {")
    for n, body in structs.items():
        prog.append(f'printf("sizeof {n} %zu\\n", sizeof({n}));')
        for member in member_names(body):
            prog.append(f'printf("offsetof {n} {member} %zu\\n", offsetof({n}, {member}));')
    prog.append("return 0; }")
    with tempfile.TemporaryDirectory() as td:
        c = os.path.join(td, "layout.c")
        exe = os.path.join(td, "layout")
        with open(c, "w") as f:
            f.write("\n".join(prog) + "\n")
        subprocess.run(["gcc", "-o", exe, c], check=True)
        out = subprocess.run([exe], capture_output=True, text=True, check=True).stdout
    layout = {}
    for line in out.splitlines():
        parts = line.split()
        if parts[0] == "sizeof":
            layout[("sizeof", parts[1])] = int(parts[2])
        else:
            layout[("offsetof", parts[1], parts[2])] = int(parts[3])
    return layout


def main() -> int:
    src = open(INTERPOSER).read()
    syntax = subprocess.run(["gcc", "-fsyntax-only", "-Wall", "-Werror=implicit-function-declaration", INTERPOSER],
                            capture_output=True, text=True)
    check("interposer compiles", syntax.returncode == 0, syntax.stderr.strip().splitlines()[-1] if syntax.returncode else "")

    try:
        import pixelflux
        layout = dict(pixelflux.VirtualCamera.shm_layout())
        layout["ctrl_fields"] = [tuple(x) for x in layout["ctrl_fields"]]
        check("pixelflux layout matches the pinned expectation", layout == EXPECTED,
              "" if layout == EXPECTED else f"pixelflux reports {layout}")
        codecs = {n: getattr(pixelflux.VirtualCamera, "CODEC_" + n.upper()) for n in EXPECTED_CODECS}
        check("pixelflux codec ids match the pinned expectation", codecs == EXPECTED_CODECS, str(codecs))
    except ImportError:
        print("SKIP  pixelflux not importable; checking the C side against the pinned layout only")

    from selkies import webcam as wc
    ids = {"mjpeg": wc.CODEC_MJPEG, "h264": wc.CODEC_H264, "vp8": wc.CODEC_VP8, "vp9": wc.CODEC_VP9,
           "av1": wc.CODEC_AV1, "hevc": wc.CODEC_HEVC}
    check("selkies.webcam codec ids", ids == EXPECTED_CODECS, str(ids))
    check("selkies.webcam codec names map onto the ids",
          all(wc.CODEC_BY_NAME[n] == v for n, v in EXPECTED_CODECS.items()))

    defines = c_defines(src)
    check("WC_SHM_MAGIC", defines.get("WC_SHM_MAGIC") == EXPECTED["magic"], hex(defines.get("WC_SHM_MAGIC", 0)))
    check("WC_SHM_VERSION", defines.get("WC_SHM_VERSION") == EXPECTED["version"], str(defines.get("WC_SHM_VERSION")))
    check("WC_SHM_CTRL_OFFSET", defines.get("WC_SHM_CTRL_OFFSET") == EXPECTED["ctrl_offset"])
    check("WC_SHM_CTRL_STRIDE", defines.get("WC_SHM_CTRL_STRIDE") == EXPECTED["ctrl_stride"])
    check("WC_SHM_DATA_OFFSET", defines.get("WC_SHM_DATA_OFFSET") == EXPECTED["data_offset"])
    check("WC_MAX_SLOTS", defines.get("WC_MAX_SLOTS") == EXPECTED["max_slots"])

    layout = compile_layout(src)
    check("sizeof(webcam_config_t)", layout[("sizeof", "webcam_config_t")] == EXPECTED["config_size"],
          str(layout[("sizeof", "webcam_config_t")]))
    for i, name in enumerate(EXPECTED["config_fields"]):
        off = layout.get(("offsetof", "webcam_config_t", name))
        check(f"webcam_config_t.{name} at {i * 4}", off == i * 4, str(off))
    for i, name in enumerate(EXPECTED["header_fields"]):
        off = layout.get(("offsetof", "wc_shm_header_t", name))
        check(f"wc_shm_header_t.{name} at {i * 4}", off == i * 4, str(off))
    check("wc_shm_header_t.latest_frame_seq",
          layout.get(("offsetof", "wc_shm_header_t", "latest_frame_seq")) == EXPECTED["header_latest_frame_seq_offset"])
    for name, off, _size in EXPECTED["ctrl_fields"]:
        check(f"wc_shm_ctrl_t.{name} at {off}", layout.get(("offsetof", "wc_shm_ctrl_t", name)) == off)
    check("sizeof(wc_shm_ctrl_t) fits the control stride",
          layout[("sizeof", "wc_shm_ctrl_t")] <= EXPECTED["ctrl_stride"])
    check("header fits before the control blocks",
          layout[("sizeof", "wc_shm_header_t")] <= EXPECTED["ctrl_offset"])
    check("control blocks fit before the data",
          EXPECTED["ctrl_offset"] + EXPECTED["max_slots"] * EXPECTED["ctrl_stride"] <= EXPECTED["data_offset"])

    print(f"\n{len(fails)} failure(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
