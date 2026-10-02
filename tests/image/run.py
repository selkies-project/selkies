#!/usr/bin/env python3
"""The image tier: the maintainer's human-testing checklist against published images.

Each image is brought up by tag or digest (target.py), once per capture
backend, and every item of the checklist runs on every transport in every
engine, each a cell of image x transport x backend x engine. The items are the
modules in items/, one per numbered entry of the checklist (README "Image
tier"); each prints PASS/FAIL/SKIP lines as the other tiers' suites do. The
run ends with a table of the cells, written with the raw results and the
screenshots under `$E2E_WORKDIR/image-tier/<run>/`.

    E2E_IMAGES="trixie=ghcr.io/selkies-project/selkies/desktop:main-debiantrixie" \\
        python3 tests/image/run.py
    python3 tests/image/run.py --items 1,4 --engines chromium --backends x11 \\
        egl=ghcr.io/selkies-project/selkies-egl-desktop@sha256:...

    E2E_IMAGES             label=ref pairs, comma-separated (positional arguments win)
    E2E_IMAGE_TRANSPORTS   websockets,webrtc       E2E_IMAGE_BACKENDS  x11,wayland
    E2E_IMAGE_ENGINES      chromium,firefox,webkit E2E_IMAGE_ITEMS     1-10
    E2E_IMAGE_GPU          none, nvidia (any NVENC GPU, AV1-capable preferred),
                           nvidia-av1 (Ada or newer only), or amd; E2E_IMAGE_GPU_PRODUCTS
                           names the nvidia.com/gpu.product values to pick from instead
    E2E_IMAGE_PRIORITY_CLASS  the pods' priorityClassName
    E2E_IMAGE_ENV          extra container environment, comma-separated KEY=VALUE
    E2E_IMAGE_LABELS       extra pod labels, comma-separated key=value
    E2E_IMAGE_NAME         pod or container name prefix (default selkies-imagetest)
    E2E_IMAGE_KEEP         1 leaves each target up after its run
"""
import argparse
import importlib
import json
import os
import shlex
import sys
import time
import traceback
from typing import Any, Dict, List

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import image_lib as L  # noqa: E402
import target as T  # noqa: E402
from image_lib import H  # noqa: E402

# NVENC-capable products by generation (the compute-only parts have none);
# E2E_IMAGE_GPU_PRODUCTS replaces the list for a cluster that names others.
AV1_GPUS = ["NVIDIA-L4", "NVIDIA-L40", "NVIDIA-L40S", "NVIDIA-RTX-4000-Ada-Generation",
            "NVIDIA-RTX-5000-Ada-Generation", "NVIDIA-GeForce-RTX-4090"]
NVENC_GPUS = AV1_GPUS + ["NVIDIA-GeForce-RTX-3090", "NVIDIA-A10", "NVIDIA-A40", "NVIDIA-RTX-A6000",
                         "NVIDIA-RTX-A5000", "NVIDIA-RTX-A4000", "NVIDIA-GeForce-RTX-2080-Ti",
                         "Quadro-RTX-6000", "Quadro-RTX-8000", "NVIDIA-TITAN-RTX", "Tesla-V100-SXM2-32GB",
                         "Tesla-V100-SXM2-16GB", "Tesla-V100-PCIE-16GB", "NVIDIA-GeForce-GTX-1080-Ti",
                         "NVIDIA-GeForce-GTX-1080", "NVIDIA-TITAN-Xp", "NVIDIA-TITAN-X-Pascal"]
BACKEND_ENV = {"x11": "false", "wayland": "true"}


def gpu_affinity(gpu: str) -> Any:
    """Node affinity for an NVIDIA class: GPUs with NVENC (the compute-only parts have none)."""
    def terms(values: List[str]) -> list:
        return [{"matchExpressions": [{"key": "nvidia.com/gpu.product", "operator": "In", "values": values}]}]
    chosen = [p for p in os.environ.get("E2E_IMAGE_GPU_PRODUCTS", "").split(",") if p]
    if gpu == "nvidia-av1":
        return {"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {
            "nodeSelectorTerms": terms(chosen or AV1_GPUS)}}}
    if gpu == "nvidia":
        return {"nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": terms(chosen or NVENC_GPUS)},
            "preferredDuringSchedulingIgnoredDuringExecution": [{"weight": 100, "preference": terms(AV1_GPUS)[0]}]}}
    return None


def pairs(text: str, sep: str = "=") -> Dict[str, str]:
    out = {}
    for part in filter(None, (p.strip() for p in (text or "").split(","))):
        k, _, v = part.partition(sep)
        out[k] = v
    return out


def item_numbers(text: str) -> List[int]:
    out: List[int] = []
    for part in text.split(","):
        a, _, b = part.partition("-")
        out += list(range(int(a), int(b or a) + 1))
    return out


def load_items(numbers: List[int]) -> list:
    mods = []
    for name in sorted(os.listdir(os.path.join(HERE, "items"))):
        if name.startswith("i") and name.endswith(".py"):
            mod = importlib.import_module(f"items.{name[:-3]}")
            if mod.ITEM in numbers:
                mods.append(mod)
    return mods


# The selkies, pixelflux, and pcmflux an image runs, and the wheel each came from.
PACKAGES_PY = """
import importlib.metadata as m, json
for p in ("selkies", "pixelflux", "pcmflux"):
    try:
        d = m.distribution(p)
        url = json.loads(d.read_text("direct_url.json") or "{}")
        print(p, d.version, (url.get("archive_info", {}).get("hash") or url.get("url") or "index")[:23])
    except Exception as e:
        print(p, "absent", type(e).__name__)
"""


def facts_of(target: Any) -> dict:
    """What an image brings, read once per target."""
    tools = target.out("for t in xdotool xclip xrandr wmctrl wl-copy wl-paste ffmpeg google-chrome chromium firefox "
                       "fcitx5 ibus-daemon pw-record parec pactl kwin_x11 kwin_wayland labwc; do "
                       "command -v $t >/dev/null && printf '%s ' $t; done")
    return {"digest": target.digest, "node": getattr(target, "node", ""), "tools": tools.split(),
            "status": target.api("/api/status"),
            "packages": target.out(f"py=$(head -1 \"$(command -v selkies)\" | sed 's/^#!//'); "
                                   f"${{py:-python3}} -c {shlex.quote(PACKAGES_PY)}"),
            "gpu": target.out("nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null | head -1; "
                              "ls /dev/dri 2>/dev/null | tr '\\n' ' '")}


def switch(target: Any, transport: str, timeout: float = 60) -> bool:
    """The streaming mode the dashboard's switch picks, asked of the server the same way."""
    status = target.api("/api/status") or {}
    if status.get("current_mode") == transport:
        return True
    target.api("/api/switch", {"mode": transport}, timeout=30)
    deadline = time.time() + timeout
    while time.time() < deadline:
        if (target.api("/api/status") or {}).get("current_mode") == transport:
            return True
        time.sleep(1)
    return False


def run_cell(mods: list, target: Any, label: str, transport: str, backend: str, engine: str,
             facts: dict, outdir: str, rows: list, save: Any = lambda: None) -> None:
    from playwright.sync_api import sync_playwright
    with sync_playwright() as pw:
        try:
            closer, ctx = L.launch_client(pw, engine)
        except Exception as e:
            for mod in mods:
                rows.append({"cell": [label, transport, backend, engine], "item": mod.ITEM,
                             "pass": 0, "fail": 0, "skip": 1, "failed": [], "note": f"browser: {e}"[:200]})
            return
        try:
            # Firefox keeps its profile between cells: start each from what a new visitor stores.
            fresh = ctx.new_page()
            try:
                fresh.goto(target.url + "/api/status", timeout=30000)
                fresh.evaluate("localStorage.clear()")
            except Exception:
                pass
            fresh.close()
            for mod in mods:
                cell = L.Cell(target, label, transport, backend, engine, ctx, facts, outdir)
                cell.res = H.Results(f"{cell.id} {mod.ITEM}")
                started = time.time()
                if engine not in getattr(mod, "ENGINES", ("chromium", "firefox", "webkit")):
                    cell.res.skip(mod.TITLE, getattr(mod, "ENGINE_NOTE", f"not an item for {engine}"))
                else:
                    try:
                        mod.run(cell)
                    except Exception as e:
                        cell.res.check(f"{mod.TITLE}: ran to the end", False,
                                       f"{type(e).__name__}: {e}"[:300])
                        traceback.print_exc()
                    finally:
                        for page in list(ctx.pages):
                            try:
                                with H.answers_within(30, "the page"):
                                    page.close()
                            except Exception:
                                pass
                cell.res.summary()
                rows.append({"cell": [label, transport, backend, engine], "item": mod.ITEM,
                             "pass": len(cell.res.items) - len(cell.res.failed()),
                             "fail": len(cell.res.failed()), "skip": len(cell.res.skipped),
                             "failed": [f"{n}: {d}" for n, _, d in cell.res.failed()],
                             "skipped": [f"{n}: {r}" for n, r in cell.res.skipped],
                             "secs": round(time.time() - started)})
                # Kept item by item: a browser that takes its driver down loses nothing already checked.
                save()
        finally:
            L.C.close_browser(closer)


def table(rows: list, numbers: List[int]) -> str:
    cells: Dict[tuple, dict] = {}
    for r in rows:
        cells.setdefault(tuple(r["cell"]), {})[r["item"]] = r
    head = "| image | transport | backend | engine | " + " | ".join(str(n) for n in numbers) + " |"
    lines = [head, "|" + " --- |" * (4 + len(numbers))]
    for key, items in cells.items():
        marks = []
        for n in numbers:
            r = items.get(n)
            if not r:
                marks.append("")
            elif r["pass"] + r["fail"] == 0:
                marks.append("skip")
            else:
                marks.append(f"{r['pass']}/{r['pass'] + r['fail']}" + (" FAIL" if r["fail"] else ""))
        lines.append("| " + " | ".join(key) + " | " + " | ".join(marks) + " |")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("images", nargs="*", help="label=ref")
    ap.add_argument("--transports", default=os.environ.get("E2E_IMAGE_TRANSPORTS", "websockets,webrtc"))
    ap.add_argument("--backends", default=os.environ.get("E2E_IMAGE_BACKENDS", "x11,wayland"))
    ap.add_argument("--engines", default=os.environ.get("E2E_IMAGE_ENGINES", "chromium,firefox,webkit"))
    ap.add_argument("--items", default=os.environ.get("E2E_IMAGE_ITEMS", "1-10"))
    args = ap.parse_args()
    images = pairs(",".join(args.images)) if args.images else pairs(os.environ.get("E2E_IMAGES", ""))
    if not images:
        H.skip_suite("no image named: set E2E_IMAGES=label=ref[,label=ref]")
    H.require_display()
    numbers = item_numbers(args.items)
    mods = load_items(numbers)
    run_id = time.strftime("%Y%m%d-%H%M%S")
    outdir = os.path.join(H.WORKDIR, "image-tier", run_id)
    os.makedirs(outdir, exist_ok=True)
    gpu = os.environ.get("E2E_IMAGE_GPU", "none")
    prefix = os.environ.get("E2E_IMAGE_NAME", "selkies-imagetest")
    rows: list = []
    images_seen: dict = {}

    def save() -> None:
        with open(os.path.join(outdir, "results.json"), "w") as f:
            json.dump({"argv": sys.argv[1:], "images": images_seen, "rows": rows}, f, indent=1)
    for label, ref in images.items():
        for backend in args.backends.split(","):
            env = {"TZ": "UTC", "SELKIES_ENABLE_BASIC_AUTH": "false", "SELKIES_WAYLAND": BACKEND_ENV[backend],
                   **pairs(os.environ.get("E2E_IMAGE_ENV", ""))}
            name = f"{prefix}-{label}-{backend}".lower().replace("_", "-")[:63]
            target = T.make(ref, name, env, labels=pairs(os.environ.get("E2E_IMAGE_LABELS", "")), gpu=gpu,
                            affinity=gpu_affinity(gpu))
            print(f"=== {label} {backend}: bringing up {ref}", flush=True)
            try:
                target.up()
            except Exception as e:
                print(f"FAIL  [{label}/{backend}] the image came up  {e}", flush=True)
                rows.append({"cell": [label, "-", backend, "-"], "item": 0, "pass": 0, "fail": 1, "skip": 0,
                             "failed": [f"bring-up: {e}"[:300]]})
                target.down()
                continue
            try:
                facts = facts_of(target)
                facts["site"] = L.install_site(target)
                facts["session_browser"] = L.session_browser(target) and bool(
                    L.Cell(target, label, "", backend, "", None, facts, outdir).report("pattern", 60))
                images_seen[f"{label}/{backend}"] = {"ref": ref, **facts}
                print(f"=== {label} {backend}: {json.dumps(facts)[:600]}", flush=True)
                for transport in args.transports.split(","):
                    if not target.alive():
                        print(f"=== {label} {backend}: the target went away (preempted?); bringing it up again",
                              flush=True)
                        target.down()
                        target.up()
                        facts.update(facts_of(target), site=L.install_site(target))
                        L.session_browser(target)
                    if not switch(target, transport):
                        print(f"FAIL  [{label}/{backend}] switched to {transport}", flush=True)
                        rows.append({"cell": [label, transport, backend, "-"], "item": 0, "pass": 0, "fail": 1,
                                     "skip": 0, "failed": [f"no switch to {transport}"]})
                        continue
                    for engine in args.engines.split(","):
                        run_cell(mods, target, label, transport, backend, engine, facts, outdir, rows, save)
                with open(os.path.join(outdir, f"{label}-{backend}-server.log"), "w") as f:
                    f.write(target.selkies_log(20000))
            finally:
                if os.environ.get("E2E_IMAGE_KEEP") != "1":
                    target.down()
    text = table(rows, numbers)
    with open(os.path.join(outdir, "table.md"), "w") as f:
        f.write(text + "\n")
    save()
    print("\n" + text + f"\n\nresults: {outdir}", flush=True)
    failed = sum(r["fail"] for r in rows)
    print(f"\n=== IMAGE TIER: {'FAIL' if failed else 'PASS'} ({failed} failed checks) ===", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
