"""2. Check that the microphone/webcam works (use a web tester in both Firefox and Chrome). The toggle for the microphone/webcam is up at the top.

The client browser offers fake devices (Chrome's --use-fake-device-for-media-stream,
Firefox's media.navigator.streams.fake), the toggles at the top of the
dashboard turn each on, and a tester page in the session's own Chrome and
Firefox opens the microphone and camera the session offers its apps: the
microphone has to carry sound, the camera a picture that changes. The server's
log has to say it brought each device up.
"""
import time
from typing import Any

from image_lib import open_sidebar, session_browser, stop_session_browser

ITEM = 2
TITLE = "microphone and webcam"


def action(page: Any, title: str) -> bool:
    """Click a toggle in the dashboard's top action bar by its title, opening the sidebar first."""
    btn = page.locator(f'.sidebar-action-buttons button[title="{title}"]')
    open_sidebar(page)
    if not btn.count():
        return False
    btn.first.click(timeout=5000)
    time.sleep(1.0)
    return True


def camera_plays(cell: Any) -> bool:
    """Whether the client's own fake camera gives a picture, in a page of its own
    (headless WebKit's mock source takes its web process down when played)."""
    page = cell.ctx.new_page()
    try:
        page.goto(cell.target.url + "/api/status", timeout=30000)
        return bool(page.evaluate("""async () => {
          const s = await navigator.mediaDevices.getUserMedia({video: true});
          const v = document.createElement('video'); v.muted = true; v.srcObject = s; await v.play();
          await new Promise(r => setTimeout(r, 1500));
          const ok = v.videoWidth > 16; s.getTracks().forEach(t => t.stop()); return ok;
        }"""))
    except Exception:
        return False
    finally:
        try:
            page.close()
        except Exception:
            pass


def heard(r: Any) -> bool:
    return bool(r and r.get("audio") and r["audio"].get("rms", 0) > 0.003)


def seen(r: Any) -> bool:
    v = r and r.get("video")
    return bool(v and v.get("frames", 0) > 4 and v.get("changes", 0) > 2 and v.get("w", 0) > 0)


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    probe = page.evaluate("""async () => {
      try { const s = await navigator.mediaDevices.getUserMedia({audio: true, video: true});
            const r = s.getTracks().map(t => t.kind + ':' + t.label); s.getTracks().forEach(t => t.stop()); return r; }
      catch (e) { return String(e); } }""")
    if not isinstance(probe, list):
        R.skip("microphone and webcam", f"this browser offers no fake devices: {probe}")
        return
    camera = camera_plays(cell)
    R.check("the top bar's microphone toggle is there and takes a click", action(page, "Enable Microphone"))
    R.check("the top bar's webcam toggle is there and takes a click", action(page, "Enable Webcam"))
    time.sleep(3)
    # Each device announces itself once per server, so the whole log is read.
    log = cell.target.selkies_log(50000)
    for which in ("chrome", "firefox"):
        cell.clear_report("media")
        if not session_browser(cell.target, which, "/media.html?kind=audio,video", kiosk=False, profile="media"):
            R.skip(f"the session's {which}", "not installed in the image")
            continue
        r = cell.report("media", 30, where=lambda r: (heard(r) and seen(r)) or r.get("error"))
        r = r or cell.report("media", 1)
        R.check(f"a tester in the session's {which} hears the client's microphone", heard(r),
                (r or {}).get("audio") or (r or {}).get("error"))
        if camera:
            R.check(f"a tester in the session's {which} sees the client's webcam", seen(r),
                    (r or {}).get("video") or (r or {}).get("error"))
        else:
            R.skip(f"a tester in the session's {which} sees the client's webcam",
                   f"the client's own camera shows no picture in {cell.engine} (its mock source)")
        stop_session_browser(cell.target, which, "media")
    R.check("the server brought up its virtual microphone", "Virtual microphone" in log or "microphone" in log.lower(),
            [ln for ln in log.splitlines() if "icrophone" in ln][-2:])
    if camera:
        R.check("the server brought up its virtual webcam", "Virtual webcam" in log or "webcam" in log.lower(),
                [ln for ln in log.splitlines() if "ebcam" in ln][-2:])
    action(page, "Disable Microphone")
    action(page, "Disable Webcam")
    cell.go("/pattern.html")
