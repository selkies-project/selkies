# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Exercise real opt-in UI and natural rendering routes in a private session.

The visible refinement canvas is compared to an independently known SHM source.
Track-generator checks also observe the original live video and identify two
post-Off source changes with a lossy-video margin, separate from PNG exactness.
No renderer/worker/codec override, no HTTP screenshot stand-in, and no physical
monitor or input-to-photon claim. Unsupported routes are explicitly classified.
"""
import argparse
import base64
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import time
from typing import Any, Callable, Optional


def read_producer_commits(path: Path) -> tuple:
    """Read bounded producer diagnostics without replacing a probe failure."""
    records, warnings = [], []
    try:
        for index, line in enumerate(path.read_text().splitlines(), 1):
            if not line.startswith("{"):
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                warnings.append("Line {}: {}".format(index, error))
                continue
            if isinstance(record, dict) and record.get("kind") == "committed":
                if len(records) < 32:
                    records.append(record)
                else:
                    warnings.append("Commit record exceeds the 32-image fixture bound")
    except (OSError, UnicodeError) as error:
        warnings.append("Could not read producer diagnostics: " + repr(error))
    return records, warnings


def is_lossless_status_error(text: str) -> bool:
    """Identify lossless protocol errors even when the logger uses console.log."""
    return text.strip().endswith(("Unhandled message received: lossless_status",
                                  "Invalid lossless status for WebRTC"))


def main() -> None:
    """Run a bounded private session and retain observations on failure."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--runtime", required=True, type=Path)
    parser.add_argument("--selkies-repo", required=True, type=Path)
    parser.add_argument("--web-root", required=True, type=Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--dashboard", choices=("default", "wish"), required=True)
    parser.add_argument("--require-supported", action="store_true")
    parser.add_argument("--require-sink", choices=("track-generator", "worker-canvas", "page-canvas"))
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=720)
    parser.add_argument("--server-python", type=Path, default=Path(sys.executable))
    parser.add_argument("--backend", choices=("wayland", "x11"), required=True)
    parser.add_argument("--transport", choices=("websockets", "webrtc"), required=True)
    parser.add_argument("--browser", choices=("chromium", "firefox", "webkit"), default="chromium")
    parser.add_argument("--browser-endpoint-file", type=Path)
    parser.add_argument("--xvfb", default="Xvfb")
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()
    if any(v < 128 or v > 8192 or v % 2 for v in (args.width, args.height)) or args.width * args.height > 16777216:
        parser.error("Even dimensions 128..8192 and at most 16,777,216 pixels required")
    args.output.mkdir(parents=True, exist_ok=False)
    result = {"status": "incomplete", "run_completed": False, "checks": [],
              "revision_requested": args.revision, "requests": [],
              "arguments": {key: str(value) if isinstance(value, Path) else value
                            for key, value in vars(args).items()},
              "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    result_path = args.output / "results.json"
    result_path.write_text(json.dumps(result, indent=2) + "\n")
    processes, logs = [], []
    connection, browser, page, playwright_runtime = None, None, None, None
    try:
        args.runtime.mkdir(parents=True, mode=0o700, exist_ok=False)
        private_server_home = args.runtime / "server-home"
        private_server_home.mkdir(mode=0o700)
        for name, directory in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                                ("XDG_DATA_HOME", "data")):
            path = args.runtime / directory
            path.mkdir()
            os.environ[name] = str(path)
        os.environ["XDG_RUNTIME_DIR"] = str(args.runtime)
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "PIXELFLUX_CU", "PIXELFLUX_RECORD", "PULSE_SERVER"):
            os.environ.pop(key, None)
        with socket.socket() as available:
            available.bind(("127.0.0.1", 0))
            port = available.getsockname()[1]
        os.environ.update(SELKIES_REPO=str(args.selkies_repo), E2E_WORKDIR=str(args.output), E2E_PORT=str(port))
        sys.path[:0] = [str(args.selkies_repo / "tests"), str(args.selkies_repo / "src" / "selkies")]
        from PIL import Image
        import numpy as np
        import core_lib as C
        from playwright.sync_api import sync_playwright
        from refinement_fixture import source_pixels

        identity_code = """import hashlib, importlib.metadata, json, sys
from pathlib import Path
import pixelflux
module = Path(pixelflux.__file__)
native = [module] if module.suffix == '.so' else list(module.parent.glob('*.so'))
packages = {}
for name in ('pixelflux', 'pcmflux', 'numpy', 'Pillow', 'pywayland'):
    try:
        packages[name] = importlib.metadata.version(name)
    except importlib.metadata.PackageNotFoundError:
        packages[name] = 'unavailable'
print(json.dumps({'python': sys.version, 'module': str(module), 'packages': packages,
    'installed_module_hashes': {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in native}}))
"""
        def server_environment() -> dict:
            """Allow only runtime paths and device selection into the owned server."""
            kept = ("PATH", "HOME", "LD_LIBRARY_PATH", "TMPDIR", "LANG", "LC_ALL", "PYTHONWARNINGS",
                    "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                    "CUDA_VISIBLE_DEVICES", "CUDA_DEVICE_ORDER", "NVIDIA_VISIBLE_DEVICES",
                    "NVIDIA_DRIVER_CAPABILITIES", "__EGL_VENDOR_LIBRARY_FILENAMES",
                    "LIBGL_DRIVERS_PATH", "GBM_BACKENDS_PATH", "GBM_BACKEND", "VK_ICD_FILENAMES")
            environment = {key: os.environ[key] for key in kept if key in os.environ}
            environment["PYTHONPATH"] = str(args.selkies_repo / "src")
            environment["HOME"] = str(private_server_home)
            return environment

        identity_env = server_environment()
        identity = subprocess.run([str(args.server_python), "-c", identity_code],
                                  env=identity_env, capture_output=True, text=True, check=True)
        result["server_runtime"] = json.loads(identity.stdout.strip().splitlines()[-1])
        result["server_runtime_stderr"] = identity.stderr
        result["installed_module_hashes"] = result["server_runtime"]["installed_module_hashes"]
        result["selkies_head"] = subprocess.check_output(["git", "-C", str(args.selkies_repo), "rev-parse", "HEAD"], text=True).strip()
        result["selkies_dirty"] = subprocess.check_output(["git", "-C", str(args.selkies_repo), "status", "--porcelain"], text=True)
        web_files = {str(path.relative_to(args.web_root)): hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in sorted(args.web_root.rglob("*")) if path.is_file()}
        if not web_files:
            raise RuntimeError("The selected webroot has no built files")
        result["web_root_files_sha256"] = web_files
        result["web_root_manifest_sha256"] = hashlib.sha256(
            json.dumps(web_files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        result["fixture_sha256"] = hashlib.sha256(Path(__file__).with_name("refinement_fixture.py").read_bytes()).hexdigest()
        gate_script = Path(__file__).with_name("png_decode_gate.js")
        result["decode_gate_sha256"] = hashlib.sha256(gate_script.read_bytes()).hexdigest()

        def check(name: str, passed: bool, detail: Any = None) -> None:
            """Record a diagnostic gate; strict mode rejects any failed gate."""
            row = {"name": name, "passed": bool(passed), "detail": detail}
            result["checks"].append(row)
            print(json.dumps(row), flush=True)

        def spawn(command: list, tag: str, env: Optional[dict] = None,
                  pass_fds: tuple = (), commands: bool = False) -> subprocess.Popen:
            """Start an attributed child, keeping its log and optional command pipe."""
            log = open(args.output / (tag + ".log"), "w")
            logs.append(log)
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                       env=env, pass_fds=pass_fds,
                                       stdin=subprocess.PIPE if commands else subprocess.DEVNULL, text=True)
            processes.append(process)
            return process

        if args.backend == "x11":
            read_fd, write_fd = os.pipe()
            try:
                spawn([args.xvfb, "-displayfd", str(write_fd), "-screen", "0", "{}x{}x24".format(args.width, args.height),
                       "-extension", "GLX", "-nolisten", "tcp", "-ac", "-noreset", "+extension", "XFIXES"],
                      "xvfb", pass_fds=(write_fd,))
                os.close(write_fd)
                write_fd = None
                if not select.select([read_fd], [], [], 10)[0]:
                    raise RuntimeError("Private X server did not allocate a display")
                number = os.read(read_fd, 64).decode().strip()
                if not number.isdigit():
                    raise RuntimeError("Invalid private display number")
                os.environ["DISPLAY"] = ":" + number
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
        env = server_environment()
        if args.backend == "x11":
            env["DISPLAY"] = os.environ["DISPLAY"]
        private_files = args.output / "files"
        private_files.mkdir()
        env.update(PYTHONPATH=str(args.selkies_repo / "src"), SELKIES_ADDR="127.0.0.1",
                   SELKIES_PORT=str(port), SELKIES_MODE=args.transport,
                   SELKIES_WAYLAND="true" if args.backend == "wayland" else "false",
                   SELKIES_ENABLE_BASIC_AUTH="false", SELKIES_ENABLE_HTTPS="false",
                   SELKIES_WEB_ROOT=str(args.web_root), SELKIES_AUDIO_ENABLED="false",
                   SELKIES_MICROPHONE_ENABLED="false", SELKIES_GAMEPAD_ENABLED="false",
                   SELKIES_VIDEO_STREAMING_MODE="true", SELKIES_FRAMERATE="30",
                   FILE_MANAGER_PATH=str(private_files), SELKIES_FILE_MANAGER_PATH=str(private_files),
                   SELKIES_WAYLAND_HOST_DISPLAY="", SELKIES_MASTER_TOKEN="",
                   SELKIES_COMPUTER_USE_BIND="", SELKIES_RECORDING_SOCKET="",
                   PIXELFLUX_RECORDING_SOCKET="", WATERMARK_PNG="")
        result["server_environment_keys"] = sorted(env)
        server = spawn([str(args.server_python), "-m", "selkies"], "selkies-server", env=env)
        result["owned_server_pid"] = server.pid

        def request(path: str = "/api/status") -> dict:
            """Read readiness only; this probe never substitutes HTTP screenshots."""
            started = time.monotonic_ns()
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
            try:
                conn.request("GET", path)
                response = conn.getresponse()
                body = response.read()
                return {"status": response.status, "body": body,
                        "latency_ms": (time.monotonic_ns() - started) / 1e6}
            except Exception as error:
                return {"status": 0, "error": repr(error), "body": b"",
                        "latency_ms": (time.monotonic_ns() - started) / 1e6}
            finally:
                conn.close()

        deadline = time.monotonic() + 75
        while time.monotonic() < deadline:
            if request("/api/status")["status"] == 200:
                break
            if server.poll() is not None:
                raise RuntimeError("Selkies exited during startup; inspect server log")
            time.sleep(0.25)
        else:
            raise RuntimeError("Selkies did not become ready")

        playwright_runtime = sync_playwright().start()
        pw = playwright_runtime
        browser_type = getattr(pw, args.browser)
        options = {"headless": True}
        if args.browser == "chromium":
            options["args"] = ["--no-sandbox", "--disable-dev-shm-usage", "--use-gl=swiftshader",
                               "--autoplay-policy=no-user-gesture-required"]
        result["browser_launch"] = {"engine": args.browser, "options": options,
            "executable_selection": "broker-selected" if args.browser_endpoint_file else "Playwright bundled",
            "server_url": "http://127.0.0.1:{}/".format(port)}
        if args.browser_endpoint_file:
            browser = browser_type.connect(args.browser_endpoint_file.read_text().strip(),
                headers={"x-playwright-browser": args.browser,
                         "x-playwright-launch-options": json.dumps(options)}, timeout=60000)
        else:
            browser = browser_type.launch(**options)
        result["browser_version"] = browser.version
        context = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1, locale="en-US")
        context.add_init_script("window.__SELKIES_STREAMING_MODE__ = " + json.dumps(args.transport) + ";" + C.PC_TAP_JS)
        context.add_init_script("window.__probeGeometry="+json.dumps([args.width,args.height])+";"+"""(() => {
            const app=(location.origin+location.pathname).replace(/[^a-zA-Z0-9._-]/g,'_');
            if(localStorage.getItem(app+'_manual_resolution')===null)
            for (const [key,value] of Object.entries({manual_resolution:'true',manual_width:String(window.__probeGeometry[0]),
              manual_height:String(window.__probeGeometry[1]),video_streaming_mode:'true',audio_enabled:'false'}))
              localStorage.setItem(app+'_'+key,value);
        })();""")
        context.add_init_script("window.__losslessWire=[];window.__losslessStates=[];" + C.wire_hook_js("""
            if(typeof data==='string' && data.startsWith('LOSSLESS ')) {
              const row=JSON.parse(data.slice(9));
              window.__losslessWire.push({at:performance.now(),...row});
            }
        """))
        context.add_init_script("""window.__probeServerSettings=null;addEventListener('message',e=>{
          if(e.source===window && e.origin===location.origin && e.data?.type==='serverSettings')
            window.__probeServerSettings=e.data.payload;
          if(e.source===window && e.origin===location.origin && e.data?.type==='losslessRefinementStatus') {
            window.__losslessStates.push({at:performance.now(),...e.data.status});
            if(window.__losslessStates.length>512)window.__losslessStates.shift();
          }
        });""")
        context.add_init_script(gate_script.read_text())
        page = context.new_page()
        console = open(args.output / "browser-console.log", "w")
        logs.append(console)
        def record_console(message: Any) -> None:
            """Record diagnostics and classify lossless protocol dispatch errors."""
            entry = {"type": message.type, "text": message.text}
            console.write(json.dumps(entry) + "\n")
            console.flush()
            if is_lossless_status_error(message.text):
                result.setdefault("lossless_status_errors", []).append(message.text)
            if message.type == "error":
                result.setdefault("console_errors", []).append(message.text)
        page.on("console", record_console)
        page.on("pageerror", lambda error: result.setdefault("page_errors", []).append(str(error)))
        page.goto("http://127.0.0.1:{}/".format(port), wait_until="load")
        video = C.wait_wr_video(page, timeout=45) if args.transport == "webrtc" else C.wait_ws_video(page, timeout=30)
        check("browser-stream-started", video is not None, video)
        if video is None:
            raise RuntimeError("Browser did not receive video")
        expected = source_pixels(args.width, args.height, 711)
        if args.backend == "wayland":
            server_log = (args.output / "selkies-server.log").read_text()
            lines = [line for line in server_log.splitlines() if "Socket listening on:" in line]
            if not lines:
                raise RuntimeError("No compositor socket found in owned server log")
            wayland_socket = lines[-1].split('"')[1]
            fixture = spawn([str(args.server_python), str(Path(__file__).with_name("refinement_fixture.py")),
                   "--socket", wayland_socket, "--output-x", "0", "--seed", "711", "--title", "refinement-fixture"],
                  "refinement-fixture", commands=True)
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                if '"kind": "committed"' in (args.output / "refinement-fixture.log").read_text():
                    break
                time.sleep(0.02)
            else:
                raise RuntimeError("Wayland fixture did not commit")
        else:
            from Xlib import X
            from Xlib.display import Display
            connection = Display(os.environ["DISPLAY"])
            screen = connection.screen()
            root = screen.root
            window = root.create_window(0, 0, args.width, args.height, 0, screen.root_depth,
                                        override_redirect=True, background_pixel=0)
            window.map()
            gc = window.create_gc()
            bgra = expected[:, :, [2, 1, 0, 3]].copy()
            for y in range(args.height):
                window.put_image(gc, 0, y, args.width, 1, X.ZPixmap, screen.root_depth, 0, bgra[y:y + 1].tobytes())
            mask = root.create_pixmap(1, 1, 1)
            mask.fill_rectangle(mask.create_gc(foreground=0), 0, 0, 1, 1)
            invisible = mask.create_cursor(mask, (0, 0, 0), (0, 0, 0), 0, 0)
            window.change_attributes(cursor=invisible)
            root.warp_pointer(64, 64)
            connection.sync()
        time.sleep(1)

        def state() -> Optional[dict]:
            return page.evaluate("window.losslessRefinementStatus || null")

        def wait_for(test: Callable[[], Any], timeout: float = 15) -> Any:
            """Return the first truthy observation or raise after the bounded wait."""
            end = time.monotonic() + timeout
            last = None
            while time.monotonic() < end:
                last = test()
                if last:
                    return last
                page.wait_for_timeout(50)
            raise RuntimeError("Bounded wait did not complete: " + repr(last))

        def requests() -> list:
            return page.evaluate("(window.__losslessWire||[]).filter(m=>m.op==='request')")

        def toggled(locator: Any) -> bool:
            return locator.get_attribute("aria-pressed") == "true" or locator.get_attribute("aria-checked") == "true" or locator.get_attribute("data-state") == "checked"

        def dashboard_switch(control_id: str) -> Any:
            """Select the visible switch; Wish assigns its ID to a hidden input."""
            if args.dashboard == "default":
                return page.locator("#" + control_id)
            return page.locator('div.justify-between:has(label[for="' + control_id +
                                '"]) [role="switch"]')

        def open_settings(tab: str = "Video", panel_open: bool = False) -> None:
            """Open settings with normal clicks in the selected dashboard."""
            if args.dashboard == "default":
                if not panel_open:
                    page.locator('.toggle-handle').first.click()
                target = '#usePaintOverQualityToggle' if tab == "Video" else '#manualWidthInput'
                if not page.locator(target).is_visible():
                    page.locator('.sidebar-section-header:has-text("' + tab + '")').first.click()
            else:
                if not panel_open:
                    page.locator('button:has(svg.lucide-settings-2)').first.click()
                page.locator('[role="tab"]:has-text("' + tab + '")').first.click()

        def collect_segment(label: str) -> None:
            """Preserve one connection timeline before reload resets page globals."""
            result.setdefault("connection_segments", []).append({"label": label,
                "wire_messages": page.evaluate("window.__losslessWire || []"),
                "state_timeline": page.evaluate("window.__losslessStates || []"),
                "decode_gate_events": page.evaluate("window.__refinementProbeDecodes || []"),
                "state": state(), "progress": page.evaluate(progress_js)})

        progress_js = """async () => {
          let decoded=0;for(const pc of window.__pcs||[])for(const row of(await pc.getStats()).values())
            if(row.type==='inbound-rtp'&&row.kind==='video')decoded+=row.framesDecoded||0;
          const video=document.querySelector('video');
          return {chunks:window.videoChunksReceived||0,decoded,
            frames:video?.getVideoPlaybackQuality?.().totalVideoFrames||0,
            client:window.stream_client||null};
        }"""
        result["video_before"] = page.evaluate(progress_js)
        wait_for(lambda: state() and state().get("sink") not in (None, "unsettled"))
        result["natural_route"] = state()
        if args.require_sink:
            check("required-natural-sink", state().get("sink") == args.require_sink, state())
        track_segment = {"ids": None}
        def track_phase(tag: str, new_connection: bool = False) -> Optional[dict]:
            """Observe live video without treating a delayed rVFC as a commit gate."""
            current_sink = state().get("sink")
            if result["natural_route"].get("sink") == "track-generator":
                check(tag + "-natural-sink-retained", current_sink == "track-generator", state())
            if current_sink != "track-generator":
                return None
            row = page.evaluate("""async () => {
              const video=document.getElementById('videoStream');
              if(!video)return {missing:true};
              const rect=e=>{const r=e.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height};};
              const visible=e=>{const s=getComputedStyle(e),r=e.getBoundingClientRect();
                return s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity)>0&&r.width>0&&r.height>0;};
              const canvas=document.getElementById('losslessVideoCanvas');
              let frame=null,timer=null,callback=null;
              if(video.requestVideoFrameCallback)await new Promise(resolve=>{
                callback=video.requestVideoFrameCallback((now,m)=>{clearTimeout(timer);
                  frame={callbackNow:now,mediaTime:m.mediaTime,presentationTime:m.presentationTime,
                    expectedDisplayTime:m.expectedDisplayTime,presentedFrames:m.presentedFrames,
                    width:m.width,height:m.height};resolve();});
                timer=setTimeout(()=>{video.cancelVideoFrameCallback(callback);resolve();},1500);
              });
              const tracks=[...(video.srcObject?.getVideoTracks?.()||[])].map(t=>({id:t.id,
                readyState:t.readyState,enabled:t.enabled,muted:t.muted,settings:t.getSettings()}));
              return {at:performance.now(),sink:window.losslessRefinementStatus?.sink,
                visible:visible(video),videoId:video.id,rect:rect(video),width:video.videoWidth,
                height:video.videoHeight,readyState:video.readyState,paused:video.paused,
                tracks,frame,overlay:canvas?{visible:visible(canvas),rect:rect(canvas),
                  width:canvas.width,height:canvas.height,pointerEvents:getComputedStyle(canvas).pointerEvents}:null};
            }""")
            row["tag"] = tag
            result.setdefault("track_video_phases", []).append(row)
            tracks = row.get("tracks", [])
            ids = sorted(track["id"] for track in tracks)
            check(tag + "-original-video-live", row.get("visible") and not row.get("paused")
                  and row.get("readyState", 0) >= 2 and tracks
                  and all(track["readyState"] == "live" and track["enabled"] for track in tracks), row)
            row["rvfc_scope"] = "One bounded observational callback; absence on a quiet healthy sink is not failure"
            if new_connection or track_segment["ids"] is None:
                track_segment["ids"] = ids
            check(tag + "-same-live-track", ids == track_segment["ids"], row)
            overlay = row.get("overlay")
            if overlay and overlay["visible"]:
                aligned = all(abs(overlay["rect"][key] - row["rect"][key]) <= 1
                              for key in ("x", "y", "width", "height"))
                check(tag + "-overlay-bounds-match-video", aligned and overlay["pointerEvents"] == "none", row)
            return row

        track_phase("before-opt-in")
        page.wait_for_timeout(1200)
        check("default-off-without-requests", state().get("requested") is False and not requests(), state())
        open_settings()
        parent = dashboard_switch('usePaintOverQualityToggle')
        parent.wait_for(state="visible", timeout=10000)
        if not toggled(parent):
            parent.click()
        child = dashboard_switch('losslessStaticRefinementToggle')
        child.wait_for(state="visible", timeout=10000)
        result["visible_switch_elements"] = {"parent": parent.evaluate("e=>e.outerHTML"),
                                             "child": child.evaluate("e=>e.outerHTML")}
        page.wait_for_timeout(800)
        result["capability_after_parent"] = state()
        supported = bool(state().get("supported"))
        result["feature_exercised"] = supported
        check("capability-matches-user-control", child.is_enabled() == supported, state())
        if args.require_supported:
            check("required-natural-refinement-route", supported, state())
        if not supported:
            check("unsupported-reason-visible", bool(page.locator('#losslessStaticRefinementStatus').inner_text())
                  and state().get("reason") not in (None, "disabled", "ready"), state())
            page.wait_for_timeout(1000)
            check("unsupported-route-does-not-request-png", not requests(), requests())
            result["capability_limitation"] = state()
        else:
            if args.backend != "wayland" or args.transport != "websockets":
                raise RuntimeError("Unexpected supported backend/transport")
            result["canvas_samples"] = []
            read_canvas = """async () => {
              const sink=window.losslessRefinementStatus?.sink;
              const source=document.getElementById(sink==='track-generator'?'losslessVideoCanvas':
                sink==='worker-canvas'?'videoWorkerCanvas':'videoCanvas');
              if(!source || getComputedStyle(source).display==='none'||getComputedStyle(source).visibility==='hidden')return null;
              const copy=document.createElement('canvas');copy.width=source.width;copy.height=source.height;
              const ctx=copy.getContext('2d',{willReadFrequently:true});ctx.drawImage(source,0,0);
              const rgba=ctx.getImageData(0,0,copy.width,copy.height).data;
              const digest=await crypto.subtle.digest('SHA-256',rgba);
              return {sink,canvasId:source.id,width:copy.width,height:copy.height,
                sha256:Array.from(new Uint8Array(digest),v=>v.toString(16).padStart(2,'0')).join(''),
                png:copy.toDataURL('image/png').split(',')[1]};
            }"""
            active_geometry = [args.width, args.height]
            def exact_canvas(tag: str, seed: int, new_connection: bool = False) -> dict:
                """Compare visible RGBA8 to the known source after bounded presentation."""
                wait_for(lambda: state().get("presentation", {}).get("shown"), 20)
                # Read from the same visible canvas, independent of the protocol's claim.
                attempts = []
                # Offscreen placeholder commits can follow its worker's status message.
                # The gate is bounded eventual equality, not the first readback timing.
                deadline = time.monotonic() + 5
                while True:
                    row = page.evaluate(read_canvas)
                    if not row:
                        raise RuntimeError("Supported route has no visible canvas")
                    oracle = source_pixels(row["width"], row["height"], seed)
                    expected_hash = hashlib.sha256(oracle.tobytes()).hexdigest()
                    attempts.append({"monotonic_ns": time.monotonic_ns(), "sha256": row["sha256"],
                                     "width": row["width"], "height": row["height"]})
                    if row["sha256"] == expected_hash or time.monotonic() >= deadline:
                        break
                    page.wait_for_timeout(250)
                raw = base64.b64decode(row.pop("png"))
                decoded = np.array(Image.open(io.BytesIO(raw)).convert("RGBA"))
                producer_commits, producer_warnings = read_producer_commits(args.output / "refinement-fixture.log")
                matching_commits = [record for record in producer_commits
                                    if record.get("seed") == seed and record.get("size") == active_geometry]
                producer_commit = matching_commits[-1] if matching_commits else None
                row.update(tag=tag, seed=seed, attempts=attempts, expected_sha256=expected_hash,
                           decoded_png_exact=bool(np.array_equal(decoded, oracle)), state=state(),
                           producer_commit=producer_commit, producer_diagnostic_warnings=producer_warnings)
                (args.output / (tag + ".png")).write_bytes(raw)
                check(tag + "-producer-physical-source", producer_commit is not None, producer_commit)
                check(tag + "-expected-geometry", [row["width"], row["height"]] == active_geometry, row)
                result["canvas_samples"].append(row)
                check(tag + "-visible-rgba-exact", row["sha256"] == row["expected_sha256"]
                      and row["decoded_png_exact"], row)
                track_phase(tag, new_connection=new_connection)
                return row

            def change_source(seed: int) -> None:
                """Ask the independent producer to commit a new source image."""
                fixture.stdin.write(json.dumps({"seed": seed}) + "\n")
                fixture.stdin.flush()
                wait_for(lambda: any(json.loads(line).get("kind") == "committed"
                                     and json.loads(line).get("seed") == seed
                                     for line in (args.output / "refinement-fixture.log").read_text().splitlines()
                                     if line.startswith('{')), 5)

            def off_video_recovery(tag: str, seed: int, previous_seed: int) -> None:
                """Recognize a changed lossy video source while the overlay is absent.

                A five-code-value mean RGB error margin identifies this deterministic
                noise source against the preceding seed. It is not a quality or
                pixel-exact gate and does not observe every intervening frame.
                """
                if state().get("sink") != "track-generator":
                    return
                change_source(seed)
                rows = []
                deadline = time.monotonic() + 5
                while True:
                    row = page.evaluate("""() => {
                      const video=document.getElementById('videoStream');
                      const canvas=document.createElement('canvas');
                      canvas.width=video.videoWidth;canvas.height=video.videoHeight;
                      const ctx=canvas.getContext('2d',{willReadFrequently:true});
                      ctx.drawImage(video,0,0);return {at:performance.now(),width:canvas.width,
                        height:canvas.height,png:canvas.toDataURL('image/png').split(',')[1]};
                    }""")
                    raw = base64.b64decode(row.pop("png"))
                    decoded = np.array(Image.open(io.BytesIO(raw)).convert("RGBA"))
                    rgb = decoded[:, :, :3].astype(np.int16)
                    errors = {str(value): float(np.abs(rgb - source_pixels(row["width"], row["height"], value)
                                                      [:, :, :3].astype(np.int16)).mean())
                              for value in (seed, previous_seed)}
                    row.update(rgb_mae=errors, seed=seed, previous_seed=previous_seed,
                               nonblack_fraction=float(np.any(rgb != 0, axis=2).mean()),
                               sha256=hashlib.sha256(decoded.tobytes()).hexdigest())
                    rows.append(row)
                    recognized = errors[str(seed)] + 5 < errors[str(previous_seed)] and row["nonblack_fraction"] > 0.99
                    if recognized or time.monotonic() >= deadline:
                        break
                    page.wait_for_timeout(250)
                (args.output / (tag + ".png")).write_bytes(raw)
                result.setdefault("off_video_samples", []).append({"tag": tag, "attempts": rows,
                    "criterion": "Target mean RGB error at least5 below preceding-source error; >99% nonblack pixels; not PNG exactness"})
                check(tag + "-changed-video-source-visible", recognized and
                      [row["width"], row["height"]] == active_geometry, rows)
                phase = track_phase(tag)
                overlay = phase.get("overlay") if phase else None
                check(tag + "-overlay-absent", phase is not None and not (overlay and overlay["visible"]), phase)

            child.click()
            wait_for(lambda: state().get("requested") and state().get("effective"))
            first = exact_canvas("first-refined", 711)
            first_scene = first["state"]["presentation"]["scene"]
            page.wait_for_timeout(1200)
            held = state().get("presentation", {})
            result["retention_while_visible"] = held
            check("bounded-video-frame-retention", held.get("retainedVideoFrames") in (0, 1)
                  and held.get("backupBytes", 0) <= args.width * args.height * 4, held)
            before_scene = len(requests())
            change_source(712)
            wait_for(lambda: state().get("presentation", {}).get("scene") not in (None, first_scene))
            second = exact_canvas("changed-scene-refined", 712)
            check("new-scene-has-distinct-request", len(requests()) > before_scene
                  and second["state"]["presentation"]["scene"] != first_scene, requests())
            child.click()
            wait_for(lambda: not state().get("requested") and not state().get("presentation", {}).get("shown"))
            off = state()
            check("off-releases-refinement-objects", off.get("presentation", {}).get("retainedVideoFrames") == 0
                  and off.get("presentation", {}).get("backupBytes") == 0, off)
            track_phase("first-off")
            off_requests = len(requests())
            change_source(713)
            page.wait_for_timeout(1300)
            check("off-does-not-request-new-scene", len(requests()) == off_requests, requests())
            child.click()
            exact_canvas("reenabled-refined", 713)
            parent.click()
            wait_for(lambda: not state().get("effective") and not state().get("presentation", {}).get("shown"))
            check("parent-off-preserves-child-preference", state().get("requested") is True, state())
            track_phase("parent-off")
            parent.click()
            wait_for(lambda: state().get("effective"))
            exact_canvas("parent-reenabled-refined", 713)
            # A real resolution edit must retire the old epoch and refine only
            # the independently reconfigured fixture's complete new geometry.
            old_epoch = state()["epoch"]
            resize_settings = page.evaluate("""() => {const s=window.__probeServerSettings||{};
              return {manual_resolution:s.manual_resolution||null,enable_resize:s.enable_resize||null,
                manual_width:s.manual_width||null,manual_height:s.manual_height||null};}""")
            result["resize_server_settings"] = resize_settings
            editable = (resize_settings["manual_resolution"] is not None and
                resize_settings["manual_resolution"].get("locked") is False and
                resize_settings["enable_resize"] is not None and
                resize_settings["enable_resize"].get("value") is not False)
            check("user-resolution-edit-is-available", editable, resize_settings)
            if not editable:
                raise RuntimeError("The server settings prohibit a user resolution change")
            open_settings("Screen" if args.dashboard == "default" else "Resolution", panel_open=True)
            active_geometry = [max(128, args.width // 2 // 2 * 2), max(128, args.height // 2 // 2 * 2)]
            if args.dashboard == "default":
                page.locator('#manualWidthInput').fill(str(active_geometry[0]))
                page.locator('#manualHeightInput').fill(str(active_geometry[1]))
                page.locator('.resolution-action-buttons .resolution-button').first.click()
            else:
                page.get_by_placeholder("e.g., 1920", exact=True).fill(str(active_geometry[0]))
                page.get_by_placeholder("e.g., 1080", exact=True).fill(str(active_geometry[1]))
                page.get_by_role("button", name="Set", exact=True).click()
            wait_for(lambda: state()["epoch"] > old_epoch)
            exact_canvas("resized-refined", 713, new_connection=True)
            result["resized_geometry"] = active_geometry
            open_settings("Video", panel_open=True)

            # Keep this separate from natural selection: no API is disabled,
            # but one PNG decode waits while actual UI OFF cancels its commit.
            child = dashboard_switch('losslessStaticRefinementToggle')
            child.click()
            wait_for(lambda: not state().get("requested") and not state().get("effective"))
            page.evaluate("dims => window.__setRefinementPngHold(true, ...dims)", active_geometry)
            gate_before = page.evaluate("window.__refinementProbeDecodes.filter(e=>e.kind==='decode-held').length")
            child.click()
            wait_for(lambda: page.evaluate("window.__refinementProbeDecodes.filter(e=>e.kind==='decode-held').length") > gate_before, 20)
            child.click()
            wait_for(lambda: not state().get("requested") and not state().get("effective"))
            canceled_at = page.evaluate("performance.now()")
            page.evaluate("window.__setRefinementPngHold(false)")
            page.wait_for_timeout(1000)
            late_shown = page.evaluate("at => window.__losslessStates.filter(s=>s.at>at&&s.presentation?.shown)", canceled_at)
            check("real-browser-off-during-png-decode-prevents-late-commit", not late_shown
                  and not state().get("presentation", {}).get("shown"), late_shown)
            result["adversarial_scope"] = "Only PNG createImageBitmap delayed by harness; normal video sink/decoder unchanged"
            child.click()
            exact_canvas("after-decode-cancel-recovery", 713)

            # A held PNG for A must not paint after video for B has been shown.
            # Positive B paint tracking prevents an inert observation hook passing.
            previous_scene = state()["presentation"]["scene"]
            scene_gate_before = page.evaluate("window.__refinementProbeDecodes.length")
            scene_requests_before = len(requests())
            page.evaluate("dims => window.__setRefinementPngHold(true, ...dims, true)", active_geometry)
            change_source(714)
            wait_for(lambda: len(requests()) > scene_requests_before and
                page.evaluate("offset => window.__refinementProbeDecodes.slice(offset).some(e=>e.kind==='decode-held')",
                              scene_gate_before), 20)
            held_a = page.evaluate("offset => window.__refinementProbeDecodes.slice(offset).find(e=>e.kind==='decode-held')",
                                   scene_gate_before)
            stamp_a = requests()[-1]
            check("adversarial-a-is-new-presented-scene", stamp_a["scene"] != previous_scene and
                  state().get("presentation", {}).get("scene") == stamp_a["scene"], stamp_a)
            change_source(715)
            wait_for(lambda: requests()[-1]["scene"] != stamp_a["scene"] and
                state().get("presentation", {}).get("scene") == requests()[-1]["scene"] and
                state().get("presentation", {}).get("queuedPngBytes", 0) > 0, 20)
            stamp_b = requests()[-1]
            same_layout = all(stamp_a[key] == stamp_b[key] for key in
                              ("epoch", "run", "source", "width", "height"))
            check("adversarial-b-changes-only-scene-with-a-decode-pending", same_layout and
                  stamp_a["scene"] != stamp_b["scene"], {"a": stamp_a, "b": stamp_b, "state": state()})
            b_presented_at = page.evaluate("performance.now()")
            page.evaluate("dims => window.__setRefinementPngHold(false, ...dims, true)", active_geometry)
            exact_canvas("new-scene-survives-old-png-decode", 715)
            wait_for(lambda: page.evaluate("held => window.__refinementProbeDecodes.some(e=>e.kind==='bitmap-closed'&&e.id===held.id&&e.thread===held.thread)", held_a), 5)
            race_events = page.evaluate("offset => window.__refinementProbeDecodes.slice(offset)", scene_gate_before)
            paints_a = [event for event in race_events if event["kind"] == "bitmap-painted" and
                        event["id"] == held_a["id"] and event["thread"] == held_a["thread"]]
            observed = {(event["thread"], event["id"]) for event in race_events
                        if event["kind"] == "decode-observed"}
            paints_b = [event for event in race_events if event["kind"] == "bitmap-painted" and
                        (event["thread"], event["id"]) in observed and
                        (event["thread"], event["id"]) != (held_a["thread"], held_a["id"])]
            late_a = page.evaluate("args => window.__losslessStates.filter(s=>s.at>=args.at&&s.presentation?.shown&&s.presentation.scene===args.scene)",
                                   {"at": b_presented_at, "scene": stamp_a["scene"]})
            result["adversarial_scene_race"] = {"a": stamp_a, "b": stamp_b, "held_a": held_a,
                "b_presented_at": b_presented_at, "events": race_events, "old_a_paints": paints_a,
                "positive_b_paints": paints_b, "late_a_states": late_a}
            check("old-scene-png-never-drawn-after-new-video", not paints_a and not late_a,
                  result["adversarial_scene_race"])
            check("png-paint-observer-positive-new-scene-control", bool(paints_b), paints_b)
            page.evaluate("window.__setRefinementPngHold(false)")
            result["adversarial_scope"] = ("PNG createImageBitmap delayed; PNG bitmap drawImage/close observed "
                "without suppressing original calls. Natural video sink and decoder preserved. "
                "Off cancellation and an older scene decode crossing a newer presented scene exercised. "
                "Not a latency or physical monitor measurement.")

            # Reload exercises a new real socket while preserving an explicit
            # child preference; initial settings must resend its boolean value.
            collect_segment("before-reload")
            page.reload(wait_until="load")
            wait_for(lambda: state() and state().get("effective"), 30)
            # Resolution and opt-in survive this ordinary same-origin reload.
            exact_canvas("reconnected-refined", 715, new_connection=True)
            check("reload-preserves-explicit-opt-in", state().get("requested") is True, state())
            result["video_before"] = page.evaluate(progress_js)
            open_settings()
            child = dashboard_switch('losslessStaticRefinementToggle')
            child.click()
            wait_for(lambda: not state().get("requested") and not state().get("effective"))
            page.wait_for_timeout(1100)
            final_off_requests = len(requests())
            off_video_recovery("final-off-source-716", 716, 715)
            off_video_recovery("final-off-source-717", 717, 716)
            check("final-off-source-changes-do-not-request-png", len(requests()) == final_off_requests, requests())

        track_phase("final-video")
        collect_segment("final")
        result["video_after"] = page.evaluate(progress_js)
        counter = "decoded" if args.transport == "webrtc" else "chunks"
        check("video-receipt-or-decode-progress", result["video_after"][counter] > result["video_before"][counter],
              {"counter": counter, "before": result["video_before"], "after": result["video_after"]})
        result["wire_messages"] = page.evaluate("window.__losslessWire || []")
        result["state_timeline"] = page.evaluate("window.__losslessStates || []")
        result["final_state"] = state()
        result["scope"] = ("Real UI opt-in and native sink selection; PNG8 exactness from visible canvas "
            "readback where supported. This is not a performance, physical presentation, native10 or "
            "all-frames stale-overlay proof; instrumented decode races are reported separately. "
            "Track-generator rVFC is observational, not a presentation fence; post-Off video source "
            "recognition uses an explicit lossy margin, separate from exact refinement PNG checks.")
        page.screenshot(path=str(args.output / "browser-after.png"))
        check("browser-no-uncaught-page-errors", not result.get("page_errors"), result.get("page_errors", []))
        browser.close()
        browser = None
        result["run_completed"] = True
    except Exception as error:
        result["fatal_error"] = repr(error)
        raise
    finally:
        if browser is not None and page is not None:
            try:
                result["last_browser_observation"] = page.evaluate("""() => ({
                  state:window.losslessRefinementStatus||null, wire:window.__losslessWire||[],
                  timeline:window.__losslessStates||[], decodes:window.__refinementProbeDecodes||[],
                switches:[...document.querySelectorAll('[role="switch"], #usePaintOverQualityToggle, #losslessStaticRefinementToggle')].map(e=>e.outerHTML),
                bodyText:document.body.innerText})""")
                page.screenshot(path=str(args.output / "browser-at-cleanup.png"))
                page.evaluate("window.__setRefinementPngHold?.(false)")
            except Exception as error:
                result["browser_observation_error"] = repr(error)
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if playwright_runtime is not None:
            try:
                playwright_runtime.stop()
            except Exception as error:
                result["playwright_cleanup_error"] = repr(error)
        if connection is not None:
            connection.close()
        result["owned_process_cleanup"] = []
        for process in reversed(processes):
            if process.stdin:
                process.stdin.close()
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
            result["owned_process_cleanup"].append({"pid": process.pid, "returncode": process.returncode,
                                                    "leader_reaped": process.poll() is not None})
        result["cleanup_scope"] = "Popen-owned leaders reaped; descendant process-group cleanup requires an external sandbox supervisor"
        for log in logs:
            log.close()
        producer_log = args.output / "refinement-fixture.log"
        if producer_log.exists():
            result["producer_commits"], result["producer_diagnostic_warnings"] = read_producer_commits(producer_log)
        if page is not None:
            result["checks"].append({"name": "browser-handles-lossless-status",
                                     "passed": not result.get("lossless_status_errors"),
                                     "detail": result.get("lossless_status_errors", [])})
        if result.get("playwright_cleanup_error"):
            result["checks"].append({"name": "playwright-runtime-cleanup", "passed": False,
                                     "detail": result["playwright_cleanup_error"]})
        result["failed_checks"] = [row for row in result["checks"] if not row["passed"]]
        result["status"] = ("completed_with_failures" if result["failed_checks"] else
                            "completed_with_limitations" if result.get("capability_limitation") else "passed") if result["run_completed"] else "incomplete"
        result_path.write_text(json.dumps(result, indent=2) + "\n")
    if args.strict and (result["failed_checks"] or (args.require_supported and result.get("capability_limitation"))):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
