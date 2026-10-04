#!/usr/bin/env python3
"""What the image tier's items share: the client browser, the cell an item runs
in, the tester site inside the session, and the decoded-picture sample.

A cell is one image x transport x backend x engine. Its client browser is the
engine a user would run, headed on the slot's own X display (`E2E_DISPLAY`),
so focus, the X clipboard, and pointer lock are real there: Chrome's focus
emulation is turned off on every page, and the Firefox profile drops the
clipboard testing pref the e2e tier sets. WebKit runs headless (see
`launch_client`), which is one more way it cannot stand in for Safari.
"""
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from typing import Any, Optional

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, os.path.join(os.path.dirname(HERE), "e2e"))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import test_dashboards as TD  # noqa: E402,F401

# Inside the session: the tester site's root and port.
SITE_ROOT = "/tmp/selkies-imagetest"
SITE_PORT = 8765
SITE_URL = f"http://127.0.0.1:{SITE_PORT}"

# The core's messages to the dashboard that the items read back, and the
# sizes of what the page writes to the local clipboard.
TAP_JS = """
  window.__itWrites = [];
  if (navigator.clipboard) {
    const wt = navigator.clipboard.writeText && navigator.clipboard.writeText.bind(navigator.clipboard);
    const w = navigator.clipboard.write && navigator.clipboard.write.bind(navigator.clipboard);
    if (wt) navigator.clipboard.writeText = (t) => { window.__itWrites.push({type: 'text/plain', size: t.length}); return wt(t); };
    if (w) navigator.clipboard.write = async (items) => {
      for (const it of items) for (const ty of it.types) {
        try { const b = await it.getType(ty); window.__itWrites.push({type: ty, size: b.size}); } catch (e) {}
      }
      return w(items);
    };
  }
  window.__it = {settings: null, stats: null, roles: [], clip: [], status: null, msgs: {}};
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    window.__it.msgs[d.type] = (window.__it.msgs[d.type] || 0) + 1;
    if (d.type === 'serverSettings') window.__it.settings = d.payload;
    else if (d.type === 'stats') window.__it.stats = d;
    else if (d.type === 'clientRoleUpdate') window.__it.roles.push(d.role);
    else if (d.type === 'clipboardContentUpdate') window.__it.clip.push(d);
    else if (d.type === 'pipelineStatusUpdate') window.__it.status = d;
    else if (d.type === 'gamingModeUpdate') window.__it.gaming = !!d.active;
  });
"""

# The decoded picture, read at its own resolution from whichever sink shows it.
SAMPLE_JS = """
(points) => {
  const v = document.querySelector('video');
  let src = null, w = 0, h = 0, kind = '';
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') {
    src = v; w = v.videoWidth; h = v.videoHeight; kind = 'video';
  } else {
    for (const c of document.querySelectorAll('canvas')) {
      if (c.width >= 320 && c.style.display !== 'none') { src = c; w = c.width; h = c.height; kind = 'canvas'; break; }
    }
  }
  if (!src) return null;
  const oc = document.createElement('canvas'); oc.width = w; oc.height = h;
  const g = oc.getContext('2d', {willReadFrequently: true}); g.drawImage(src, 0, 0, w, h);
  const px = ([fx, fy]) => {
    const x = Math.min(w - 1, Math.round(fx * w)), y = Math.min(h - 1, Math.round(fy * h));
    const d = g.getImageData(x, y, 1, 1).data; return [d[0], d[1], d[2]];
  };
  const row = g.getImageData(Math.round(0.4 * w), Math.round(0.5 * h), 16, 1).data;
  const band = [];
  for (let i = 0; i < 16; i++) band.push([row[i * 4], row[i * 4 + 1], row[i * 4 + 2]]);
  return {kind, w, h, points: points.map(px), band};
}
"""
# How far the decoded picture strays from texture.html's blocks: the mean
# absolute error over every block centre, per channel, in levels.
TEXTURE_JS = """
(seed) => {
  const v = document.querySelector('video');
  let src = null, w = 0, h = 0;
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') { src = v; w = v.videoWidth; h = v.videoHeight; }
  else for (const c of document.querySelectorAll('canvas'))
    if (c.width >= 320 && c.style.display !== 'none') { src = c; w = c.width; h = c.height; break; }
  if (!src) return null;
  const oc = document.createElement('canvas'); oc.width = w; oc.height = h;
  const g = oc.getContext('2d', {willReadFrequently: true}); g.drawImage(src, 0, 0, w, h);
  const d = g.getImageData(0, 0, w, h).data;
  let err = 0, n = 0;
  for (let by = 0; by * 16 + 8 < h; by++)
    for (let bx = 0; bx * 16 + 8 < w; bx++) {
      let k = (Math.imul(bx, 73856093) ^ Math.imul(by, 19349663) ^ Math.imul(seed, 83492791)) >>> 0;
      k = Math.imul(k ^ (k >>> 15), 2246822507) >>> 0;
      const i = ((by * 16 + 8) * w + bx * 16 + 8) * 4;
      err += Math.abs(d[i] - (k & 255)) + Math.abs(d[i + 1] - ((k >>> 8) & 255)) + Math.abs(d[i + 2] - ((k >>> 16) & 255));
      n += 3;
    }
  return Math.round(err / n * 10) / 10;
}
"""
# pattern.html's quadrants, as fractions of the picture, and their colors.
QUADRANTS = [((0.25, 0.2), (40, 120, 220)), ((0.75, 0.2), (255, 0, 0)),
             ((0.25, 0.8), (20, 160, 60)), ((0.75, 0.8), (230, 230, 230))]


# How loud what the page plays is, 0-100: the WebSockets client's own meter, or
# for a WebRTC <video> an analyser over its incoming audio track.
AUDIO_LEVEL_JS = """() => {
  const v = document.querySelector('video');
  const tracks = v && v.srcObject && v.srcObject.getAudioTracks ? v.srcObject.getAudioTracks() : [];
  if (!tracks.length) return window.currentAudioLevel || 0;
  if (!window.__itAn) {
    const ac = new AudioContext(), an = ac.createAnalyser();
    ac.createMediaStreamSource(new MediaStream(tracks)).connect(an);
    ac.resume(); window.__itAn = an;
  }
  const buf = new Float32Array(2048); window.__itAn.getFloatTimeDomainData(buf);
  let sum = 0; for (const x of buf) sum += x * x;
  return Math.min(100, Math.round(Math.sqrt(sum / buf.length) * 141));
}"""


def audio_level(page: Any, secs: float = 8) -> int:
    """The loudest the page's audio got over `secs` seconds."""
    deadline, best = time.time() + secs, 0
    while time.time() < deadline:
        try:
            best = max(best, int(page.evaluate(AUDIO_LEVEL_JS) or 0))
        except Exception:
            pass
        time.sleep(0.4)
    return best


def near(rgb: Any, want: tuple, tolerance: int = 28) -> bool:
    return isinstance(rgb, list) and all(abs(a - b) <= tolerance for a, b in zip(rgb, want))


def pattern_matches(sample: Optional[dict]) -> bool:
    """Whether a decoded picture shows pattern.html's four quadrants."""
    return bool(sample) and all(near(p, want) for p, (_, want) in zip(sample["points"], QUADRANTS))


def chroma_kept(sample: Optional[dict]) -> Optional[bool]:
    """Whether the one-pixel red and blue columns survived (4:4:4), or None off the band."""
    if not sample or not sample.get("band"):
        return None
    reds = sum(1 for p in sample["band"] if p[0] > 150 and p[2] < 100)
    blues = sum(1 for p in sample["band"] if p[2] > 150 and p[0] < 100)
    return reds >= 4 and blues >= 4


def client_env() -> dict:
    """The environment a client browser starts in: the slot's X display, never the shell's."""
    return dict(os.environ, DISPLAY=H.require_display())


def tone_wav() -> str:
    """A steady 440 Hz tone for Chrome's fake microphone, whose own beeps are too sparse to measure."""
    path = os.path.join(H.WORKDIR, "tone-440.wav")
    if not os.path.exists(path):
        import math
        import struct
        import wave
        with wave.open(path, "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(48000)
            w.writeframes(b"".join(struct.pack("<h", int(12000 * math.sin(2 * math.pi * 440 * i / 48000)))
                                   for i in range(48000 * 2)))
    return path


def launch_client(pw: Any, engine: str) -> tuple:
    """A headed client browser for `engine` on the slot's display: `(closer, context)`.

    It accepts the session's self-signed certificate, plays media without a
    gesture, and offers fake camera and microphone devices, as the checklist's
    testers do with their own browsers' flags.
    """
    env = client_env()
    if engine == "chromium":
        args = ["--ignore-certificate-errors", "--use-fake-device-for-media-stream",
                f"--use-file-for-fake-audio-capture={tone_wav()}",
                "--use-fake-ui-for-media-stream", "--autoplay-policy=no-user-gesture-required",
                "--no-first-run", "--no-default-browser-check", "--password-store=basic",
                "--window-position=0,0"]
        kwargs = {"headless": False, "args": args, "env": env}
        if C.CHROME_PATH:
            kwargs["executable_path"] = C.CHROME_PATH
        browser = pw.chromium.launch(**kwargs)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1,
                                  ignore_https_errors=True)
        # What a user's "Allow" on Chrome's clipboard prompt grants once.
        ctx.grant_permissions(["clipboard-read", "clipboard-write"])
        return browser, ctx
    if engine == "firefox":
        prefs = {"dom.events.testing.asyncClipboard": False,
                 "media.navigator.streams.fake": True, "media.navigator.permission.disabled": True}
        ctx = C.firefox_persistent_context(pw, viewport={"width": 1280, "height": 720}, prefs=prefs,
                                           headless=False, env=env, ignore_https_errors=True)
        return ctx, ctx
    # Headed WebKitGTK's web process dies on an Xvfb display (no DRI3), so
    # WebKit runs headless as in the e2e tier: no X focus or clipboard of its own.
    browser = pw.webkit.launch(headless=True, env=env)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1,
                              ignore_https_errors=True)
    try:
        ctx.grant_permissions(["camera", "microphone"])
    except Exception:
        pass
    return browser, ctx


class Cell:
    """One image x transport x backend x engine, as an item sees it."""

    def __init__(self, target: Any, label: str, transport: str, backend: str, engine: str,
                 ctx: Any, facts: dict, outdir: str) -> None:
        self.target, self.label, self.transport, self.backend = target, label, transport, backend
        self.engine, self.ctx, self.facts, self.outdir = engine, ctx, facts, outdir
        self.id = f"{label}/{transport}/{backend}/{engine}"
        self.res: Any = None

    # The client side.
    def open(self, url_hash: str = "", wait: bool = True, timeout: float = 45, init: tuple = (),
             ctx: Any = None) -> Any:
        """A new page of the session in the cell's browser (or `ctx`), with the
        `init` scripts installed before the client starts; its stream up when `wait`."""
        # A browser whose renderer died can leave a new page waiting forever.
        with H.answers_within(60, "the browser"):
            page = (ctx or self.ctx).new_page()
        page.add_init_script(TAP_JS)
        for script in init:
            page.add_init_script(script)
        if os.environ.get("E2E_IMAGE_CONSOLE"):
            log = os.path.join(self.outdir, f"{self.id.replace('/', '-')}-console.log")
            page.on("console", lambda m: open(log, "a").write(f"{time.strftime('%H:%M:%S')} {m.type}: {m.text}\n"))
        page.goto(self.target.url + "/" + url_hash, wait_until="load", timeout=60000)
        if self.engine == "chromium":
            cdp = (ctx or self.ctx).new_cdp_session(page)
            cdp.send("Emulation.setFocusEmulationEnabled", {"enabled": False})
        if wait:
            self.video(page, timeout)
        return page

    def video(self, page: Any, timeout: float = 45) -> Optional[dict]:
        """The stream's picture size once a frame arrived, else None."""
        if self.transport == "webrtc":
            return C.wait_wr_video(page, timeout)
        return C.wait_ws_video(page, timeout)

    def sample(self, page: Any, points: Optional[list] = None) -> Optional[dict]:
        try:
            return page.evaluate(SAMPLE_JS, points or [p for p, _ in QUADRANTS])
        except Exception:
            return None

    def wait_pattern(self, page: Any, timeout: float = 20) -> Optional[dict]:
        """The decoded picture once it shows the session's pattern page, else the last sample."""
        deadline, s = time.time() + timeout, None
        while time.time() < deadline:
            s = self.sample(page)
            if pattern_matches(s):
                return s
            time.sleep(0.5)
        return s

    def texture_error(self, page: Any, seed: int = 1) -> Optional[float]:
        try:
            return page.evaluate(TEXTURE_JS, seed)
        except Exception:
            return None

    def settings(self, page: Any) -> dict:
        return page.evaluate("window.__it && window.__it.settings || {}") or {}

    def shot(self, page: Any, name: str) -> str:
        path = os.path.join(self.outdir, f"{self.id.replace('/', '-')}-{name}.png")
        try:
            page.screenshot(path=path)
        except Exception:
            pass
        return path

    def xclient(self, *args: str, data: Optional[bytes] = None, timeout: float = 15) -> subprocess.CompletedProcess:
        """A command on the client's X display (xdotool, xclip): what a local app does there."""
        try:
            return subprocess.run(list(args), input=data, capture_output=True, timeout=timeout, env=client_env())
        except subprocess.TimeoutExpired as e:
            return subprocess.CompletedProcess(args, 124, e.stdout or b"", e.stderr or b"")

    # The session side.
    def go(self, path: str, ch: str = "main") -> None:
        """Move the session browser following channel `ch` to `path` on the tester site."""
        self.target.sh(f"echo {shlex.quote(channel_url(path, ch))} > {SITE_ROOT}/next-{ch}")

    def report(self, name: str, timeout: float = 15, where: Any = None) -> Optional[dict]:
        """The tester page's latest report `name`; with `where`, the first one it accepts."""
        deadline, last = time.time() + timeout, None
        while time.time() < deadline:
            raw = self.target.out(f"cat {SITE_ROOT}/out/{name}.json 2>/dev/null")
            if raw:
                try:
                    last = json.loads(raw)
                except ValueError:
                    last = None
                if last is not None and (where is None or where(last)):
                    return last
            time.sleep(0.5)
        return last if where is None else None

    def clear_report(self, name: str) -> None:
        self.target.sh(f"rm -f {SITE_ROOT}/out/{name}.json")


def open_sidebar(page: Any) -> bool:
    """Open the classic dashboard's sidebar, as its edge handle does; whether it is open."""
    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        try:
            page.locator(".toggle-handle").first.click(timeout=3000)
        except Exception:
            page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)
    return page.evaluate("!!document.querySelector('.sidebar.is-open')")


def reveal(page: Any, sel: str) -> bool:
    """Bring a classic dashboard control into the DOM: open the sidebar, then try
    each collapsed section until the control renders. Whether it is there."""
    if page.locator(sel).count():
        return True
    open_sidebar(page)
    heads = page.locator(".sidebar-section-header")
    for i in range(heads.count()):
        if page.locator(sel).count():
            break
        try:
            heads.nth(i).scroll_into_view_if_needed(timeout=2000)
            heads.nth(i).click(timeout=3000)
            time.sleep(0.5)
            if not page.locator(sel).count():
                heads.nth(i).click(timeout=3000)
                time.sleep(0.3)
        except Exception:
            pass
    return page.locator(sel).count() > 0


CLIP_ARGS = ("-f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 "
             "-t 12 -c:v libvpx -b:v 800k -c:a libopus -b:a 96k -shortest")


def install_site(target: Any) -> bool:
    """Copy the tester site into the session and serve it on loopback there,
    with a clip (a moving picture and a tone) made by whichever side has ffmpeg."""
    site = os.path.join(HERE, "site")
    for name in os.listdir(site):
        with open(os.path.join(site, name), "rb") as f:
            target.put(f"{SITE_ROOT}/site/{name}", f.read())
    clip = os.path.join(H.WORKDIR, "av.webm")
    if not os.path.exists(clip) and shutil.which("ffmpeg"):
        subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *CLIP_ARGS.split(), clip],
                       capture_output=True, timeout=120)
    if os.path.exists(clip):
        with open(clip, "rb") as f:
            target.put(f"{SITE_ROOT}/site/av.webm", f.read())
    else:
        target.sh(f"ffmpeg -hide_banner -loglevel error -y {CLIP_ARGS} {SITE_ROOT}/site/av.webm", 120)
    # A shell of its own for the kill: the pattern would match the launch below.
    target.sh(f"pkill -f '[s]ite/site.py {SITE_ROOT}'; sleep 0.5")
    target.sh(f"mkdir -p {SITE_ROOT}/out; cd {SITE_ROOT} && "
              f"(setsid nohup python3 site/site.py {SITE_ROOT} {SITE_PORT} > site.log 2>&1 &)")
    deadline = time.time() + 15
    while time.time() < deadline:
        if target.sh(f"curl -sf -o /dev/null {SITE_URL}/it.js").returncode == 0:
            return True
        time.sleep(0.5)
    return False


# The session's own browsers, started on the desktop like a user's.
SESSION_CHROME = "google-chrome chromium chromium-browser"
SESSION_FLAGS = ("--no-first-run --no-default-browser-check --password-store=basic --disable-features=Translate "
                 "--autoplay-policy=no-user-gesture-required --use-fake-ui-for-media-stream "
                 "--disable-session-crashed-bubble --test-type")


def channel_url(path: str, ch: str) -> str:
    """`path` on the tester site for the browser following `ch`, made unique so it reloads."""
    return f"{path}{'&' if '?' in path else '?'}ch={ch}&n={time.time_ns()}"


def session_browser(target: Any, which: str = "chrome", start: str = "/pattern.html", kiosk: bool = True,
                    profile: str = "main") -> bool:
    """Start the session's Chrome (or Firefox) on the tester site, following the
    channel named after its profile; whether the browser is installed."""
    url = f"{SITE_URL}{channel_url(start, profile)}"
    target.sh(f"rm -f {SITE_ROOT}/next-{profile}")
    if which == "chrome":
        exe = target.out(f"for b in {SESSION_CHROME}; do command -v $b && break; done")
        if not exe:
            return False
        # A kiosk fills the screen; otherwise an app window, which has no tabs or toolbar.
        mode = f"--kiosk {shlex.quote(url)}" if kiosk else f"--app={shlex.quote(url)}"
        cmd = f"{exe} {SESSION_FLAGS} --user-data-dir={SITE_ROOT}/chrome-{profile} {mode}"
    else:
        exe = target.out("command -v firefox")
        if not exe:
            return False
        prof = f"{SITE_ROOT}/firefox-{profile}"
        prefs = ('user_pref("media.autoplay.default", 0);\n'
                 'user_pref("media.navigator.permission.disabled", true);\n'
                 'user_pref("browser.shell.checkDefaultBrowser", false);\n'
                 'user_pref("datareporting.policy.dataSubmissionEnabled", false);\n'
                 'user_pref("browser.aboutwelcome.enabled", false);\n')
        target.put(f"{prof}/user.js", prefs.encode())
        cmd = f"{exe} --no-remote --profile {prof} {'--kiosk ' if kiosk else ''}{shlex.quote(url)}"
    target.sh(f"(setsid nohup {cmd} > {SITE_ROOT}/{which}-{profile}.log 2>&1 &)")
    return True


def stop_session_browser(target: Any, which: str = "chrome", profile: str = "main") -> None:
    """End a session browser `session_browser` started, by its profile directory."""
    flag = f"user-data-dir={SITE_ROOT}/chrome-{profile}" if which == "chrome" else f"{SITE_ROOT}/firefox-{profile}"
    # The bracket keeps the pattern from matching this shell's own command line;
    # the wait lets the profile's lock go before the next start reuses it.
    pattern = shlex.quote('[' + flag[0] + ']' + flag[1:])
    target.sh(f"pkill -f -- {pattern}; for i in $(seq 40); do pgrep -f -- {pattern} > /dev/null || break; "
              f"sleep 0.25; done; pkill -9 -f -- {pattern}; true", 30)
    time.sleep(0.5)
