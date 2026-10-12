#!/usr/bin/env python3
"""Exercise the experimental still overlay in Chromium, without installing it.

The default is a deterministic browser fixture: a manually advanced canvas
stream supplies video, and PNG captures supply the original RGB pixels. It
checks presentation, cancellation, stale responses, geometry, and cleanup.

With --url, the probe connects to an explicitly chosen isolated Selkies test
session and exposes manual Refine and Video buttons. --token-file contains a
session token, never a master credential. --api-path resolves relative to that
page and must stay on its origin. The existing API includes the cursor; use a
single-display session and move the cursor outside the region being measured.
An active stream invalidates every still, so turn off continuous streaming
through the test session's normal controls before refining.

Usage:
    python tests/tools/static_refinement_probe.py --output /tmp/refinement
    python tests/tools/static_refinement_probe.py --url http://localhost:8080/ \
        --mode websockets --headed --output /tmp/refinement
"""

import argparse
import json
from pathlib import Path
import time
from typing import Any
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

from playwright.sync_api import sync_playwright


def install(page: Any) -> None:
    """Load the actual module as an ES module into the isolated probe page."""
    source = Path(__file__).with_name("static_refinement.mjs").read_text()
    page.add_script_tag(type="module", content=source + "\nwindow.createStaticRefinement = createStaticRefinement;")
    page.wait_for_function("typeof window.createStaticRefinement === 'function'")


def self_test(browser: Any, output: Path) -> list:
    """Assert RGB equality and lifecycle behavior using real browser media APIs."""
    context = browser.new_context(viewport={"width": 800, "height": 600}, device_scale_factor=2)
    page = context.new_page()
    checks = []

    def check(label: str, passed: bool) -> None:
        checks.append({"check": label, "passed": bool(passed)})
        print(f"{'PASS' if passed else 'FAIL'} {label}", flush=True)

    try:
        page.set_content('<video autoplay muted playsinline style="position:absolute;left:16px;top:24px;'
                         'width:128px;height:96px;image-rendering:pixelated"></video>')
        install(page)
        page.evaluate("""async () => {
          const source = document.createElement('canvas'); source.width=128; source.height=96;
          const ctx=source.getContext('2d'); const data=ctx.createImageData(128,96);
          for(let i=0;i<data.data.length;i+=4){const p=i/4;
            data.data.set([(p*37)%256,(p*71)%256,(p*13)%256,255],i);}
          ctx.putImageData(data,0,0);
          window.source=source; window.original=data.data;
          window.video=document.querySelector('video'); video.srcObject=source.captureStream(0);
          window.track=video.srcObject.getVideoTracks()[0]; track.requestFrame(); await video.play();
          let tick=0;
          window.advance=()=>{ctx.fillStyle=(++tick%2)?'#123456':'#abcdef';
            ctx.fillRect(0,0,16,16);track.requestFrame();};
          window.png=()=>new Promise(resolve=>source.toBlob(resolve,'image/png'));
          window.capture=()=>png();
          window.probe=createStaticRefinement(video,signal=>capture(signal));
        }""")
        page.wait_for_timeout(300)
        result = page.evaluate("probe.refine()")
        check("a settled video accepts a lossless snapshot", result.get("accepted"))
        check("every source RGB value survives the overlay", page.evaluate("""() => {
          const got=probe.canvas.getContext('2d').getImageData(0,0,128,96).data;
          return got.every((v,i)=>v===original[i]);
        }"""))
        page.screenshot(path=str(output / "self-test-refined.png"))
        page.evaluate("advance()")
        page.wait_for_function("!probe.status().shown")
        check("a new presented frame immediately reveals video", not page.evaluate("probe.status().shown"))
        page.evaluate("""() => {window.release=null; capture=()=>new Promise(resolve=>{release=resolve});
          window.delayed=probe.refine();}""")
        page.wait_for_function("typeof release === 'function'")
        page.evaluate("advance()")
        page.wait_for_function("probe.status().reason === 'new-video-frame'")
        page.evaluate("async () => release(await png())")
        check("a late snapshot cannot cover a newer frame", not page.evaluate("delayed").get("accepted"))
        page.evaluate("""() => {capture=()=>new Promise(resolve=>{release=resolve});
          window.older=probe.refine();}""")
        result = page.evaluate("() => {capture=()=>png(); return probe.refine();}")
        page.evaluate("async () => release(await png())")
        check("a newer request supersedes an older response", result.get("accepted")
              and not page.evaluate("older").get("accepted") and page.evaluate("probe.status().shown"))
        page.mouse.move(300, 200)
        check("pointer movement invalidates the baked-cursor snapshot", not page.evaluate("probe.status().shown"))
        page.evaluate("""() => {video.style.width='111px';video.style.height='83.25px';
          video.style.imageRendering='auto';}""")
        page.wait_for_timeout(100)
        page.evaluate("probe.refine()")
        check("fractional geometry and smoothing match at DPR 2", page.evaluate("""() => {
          const a=video.getBoundingClientRect(), b=probe.canvas.getBoundingClientRect();
          return ['x','y','width','height'].every(k=>a[k]===b[k]) && devicePixelRatio===2
            && getComputedStyle(probe.canvas).imageRendering===getComputedStyle(video).imageRendering;
        }"""))
        page.set_viewport_size({"width": 700, "height": 500})
        page.wait_for_function("!probe.status().shown")
        check("viewport resize removes the old overlay", not page.evaluate("probe.status().shown"))
        page.evaluate("""() => {capture=()=>new Promise(resolve=>{release=resolve});
          window.resizing=probe.refine();video.style.width='96px';}""")
        page.wait_for_timeout(100)
        page.evaluate("async () => release(await png())")
        check("geometry changed during capture rejects the response", not page.evaluate("resizing").get("accepted"))
        result = page.evaluate("""() => {capture=()=>{throw new Error('fixture unavailable')};return probe.refine();}""")
        check("capture errors retain video and release pending work", not result.get("accepted")
              and not page.evaluate("probe.status().pending || probe.status().shown"))
        result = page.evaluate("""() => {capture=()=>Promise.resolve(new Blob(['invalid'],{type:'image/png'}));
          return probe.refine();}""")
        check("invalid image bytes never hide the video", not result.get("accepted") and not page.evaluate("probe.status().shown"))
        result = page.evaluate("""() => {capture=()=>new Promise(resolve=>{const c=document.createElement('canvas');
          c.width=2;c.height=2;c.toBlob(resolve,'image/png')});return probe.refine();}""")
        check("a snapshot for a different display size is rejected", result.get("reason") == "snapshot-size-mismatch")
        page.evaluate("""() => {capture=()=>new Promise(resolve=>{release=resolve});
          window.ending=probe.refine();probe.dispose();}""")
        page.evaluate("async () => release(await png())")
        check("disposal rejects pending work and removes the overlay", not page.evaluate("ending").get("accepted")
              and page.evaluate("!probe.canvas.isConnected && !probe.status().pending"))
        check("a disposed probe cannot request another capture", page.evaluate("probe.refine()").get("reason") == "disposed")
    finally:
        context.close()
    return checks


def live_probe(browser: Any, args: Any, output: Path) -> list:
    """Open a temporary probe UI in the supplied isolated Selkies session."""
    parts = urlsplit(args.url)
    if parts.scheme not in ("http", "https") or parts.username or parts.password:
        raise ValueError("Use an HTTP(S) test-session URL without embedded credentials")
    token = Path(args.token_file).read_text().strip() if args.token_file else ""
    query = dict(parse_qsl(parts.query))
    if token:
        query["token"] = token
    url = urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(query), parts.fragment))
    context = browser.new_context(viewport={"width": 1920, "height": 1080}, device_scale_factor=1)
    context.add_init_script("window.__SELKIES_STREAMING_MODE__ = " + json.dumps(args.mode))
    page = context.new_page()
    try:
        try:
            page.goto(url, wait_until="load")
        except Exception:
            raise RuntimeError("Could not open the test session; check its reachability and authentication") from None
        page.wait_for_function("[...document.querySelectorAll('video')].some(v=>v.videoWidth && getComputedStyle(v).display!=='none')",
                               timeout=60000)
        install(page)
        page.evaluate("""({token,path}) => {
          const video=[...document.querySelectorAll('video')].find(v=>v.videoWidth && getComputedStyle(v).display!=='none');
          const endpoint=new URL(path,location.href);
          if(endpoint.origin!==location.origin)throw new Error('Snapshot endpoint must be same-origin');
          window.probe=createStaticRefinement(video,async signal=>{
            const response=await fetch(endpoint,{signal,cache:'no-store',
              headers:token?{Authorization:'Bearer '+token}:{}});
            if(!response.ok)throw new Error('Snapshot HTTP '+response.status);
            if(!response.headers.get('Content-Type')?.startsWith('image/png'))throw new Error('Expected a PNG snapshot');
            const blob=await response.blob();window.probeSource=blob;return blob;
          });
          const panel=document.createElement('div');
          panel.style.cssText='position:fixed;bottom:12px;left:12px;z-index:10000;padding:16px;background:white;color:black;font:16px sans-serif';
          const label=document.createElement('p');label.textContent='Experimental still refinement: client ordering only; cursor included.';
          const refine=document.createElement('button');refine.textContent='Refine still';
          const back=document.createElement('button');back.textContent='Show video';
          const status=document.createElement('p');status.id='refinement-status';status.textContent='Video';
          window.probeResults=[];
          refine.onclick=async()=>{status.textContent='Requesting PNG';const result=await probe.refine();
            probeResults.push(result);status.textContent=JSON.stringify(result);};
          back.onclick=()=>{probe.invalidate('show-video');status.textContent='Video';};
          panel.append(label,refine,back,status);document.body.appendChild(panel);
        }""", {"token": token or query.get("token", ""), "path": args.api_path})
        if args.capture_once:
            page.wait_for_function("probe.status().idleMs > 1000", timeout=30000)
            page.get_by_role("button", name="Refine still", exact=True).click()
            page.wait_for_function("probeResults.length > 0")
        else:
            deadline = time.monotonic() + args.seconds
            while time.monotonic() < deadline and not page.is_closed():
                page.wait_for_timeout(250)
        results = page.evaluate("probeResults") if not page.is_closed() else []
        if not page.is_closed():
            if args.capture_once and results and results[-1].get("accepted"):
                results[-1]["rgbExact"] = page.evaluate("""async () => {
                  const bitmap=await createImageBitmap(probeSource,{colorSpaceConversion:'none'});
                  try {
                    const c=document.createElement('canvas');c.width=bitmap.width;c.height=bitmap.height;
                    const ctx=c.getContext('2d');ctx.drawImage(bitmap,0,0);
                    const expected=ctx.getImageData(0,0,c.width,c.height).data;
                    const actual=probe.canvas.getContext('2d').getImageData(0,0,c.width,c.height).data;
                    return probe.status().shown && expected.every((value,index)=>value===actual[index]);
                  } finally {bitmap.close();}
                }""")
            page.screenshot(path=str(output / "live-refined.png"))
        return results
    finally:
        context.close()


def main() -> None:
    """Run deterministic checks or a bounded, explicitly selected live probe."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url")
    parser.add_argument("--token-file")
    parser.add_argument("--api-path", default="./api/screenshot?display=primary")
    parser.add_argument("--mode", choices=("websockets", "webrtc"), default="websockets")
    parser.add_argument("--headed", action="store_true")
    parser.add_argument("--capture-once", action="store_true")
    parser.add_argument("--seconds", type=int, default=120)
    parser.add_argument("--browser-endpoint-file", help="Optional private Playwright run-server endpoint")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as pw:
        options = {"headless": not args.headed, "args": ["--no-sandbox", "--disable-dev-shm-usage",
                   "--autoplay-policy=no-user-gesture-required", "--use-gl=swiftshader"]}
        if args.browser_endpoint_file:
            endpoint = Path(args.browser_endpoint_file).read_text().strip()
            browser = pw.chromium.connect(endpoint, headers={"x-playwright-browser": "chromium",
                "x-playwright-launch-options": json.dumps(options)})
        else:
            browser = pw.chromium.launch(**options)
        try:
            results = live_probe(browser, args, args.output) if args.url else self_test(browser, args.output)
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
            if not results or any(not item.get("accepted" if args.url else "passed")
                                  or item.get("rgbExact") is False for item in results):
                raise SystemExit(1)
        finally:
            browser.close()


if __name__ == "__main__":
    main()
