#!/usr/bin/env python3
"""IME client paths: replayed composition traces must type the same text the
IME committed locally, on every platform branch.

Two event anatomies are replayed as DOM events against the real input.js in
Chromium (platform-spoofed via CDP where a branch needs it):

- Real IBus (Linux Chrome): the commit arrives as textInput AFTER an
  empty-data compositionend. A handler that only listens for textInput on
  Linux erases every committed syllable a Chrome client sends from
  Windows/macOS — the "keeps replacing one symbol" report.
- CDP-style (insertText API): textInput and compositionend BOTH carry the
  commit. The end must see the stamp and clear the preedit instead of typing
  the text a second time.
"""
import functools
import http.server
import os
import socketserver
import threading
import time
from typing import Any, Optional

from playwright.sync_api import sync_playwright

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ROOT = os.path.join(REPO, "addons", "selkies-web-core")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ime-client] {label}  {detail}", flush=True)


PAGE = """<!doctype html><html><head><meta charset="utf-8"></head><body>
<input id="overlayInput" type="search">
<input id="keyboard-input-assist" style="display:none">
<script type="module">
window.__sent = [];
window.__errs = [];
try {
  const mod = await import('/lib/input.js');
  const el = document.getElementById('overlayInput');
  const send = (m) => window.__sent.push(m);
  window.__input = new mod.Input(el, send, false, null, false);
  window.__input.attach();
  el.focus();
  window.__ready = true;
} catch (e) { window.__errs.push(e.message + ' ' + (e.stack || '').slice(0, 300)); }
window.__dispatch = (kind, data) => {
  const el = document.getElementById('overlayInput');
  let ev;
  if (kind.startsWith('composition')) {
    ev = new CompositionEvent(kind, { data });
  } else if (kind === 'textInput') {
    ev = document.createEvent('TextEvent');
    ev.initTextEvent('textInput', true, true, window, data, 0, '');
  } else {
    return;
  }
  el.dispatchEvent(ev);
};
window.__replay = (groups) => {
  for (const g of groups) {
    window.__dispatch('compositionstart', '');
    for (const [kind, data] of g) window.__dispatch(kind, data);
  }
};
// Same replay, but the wire text is captured after step index `probeIdx` of
// each group (indexing matches the trace arrays): asserts the preedit rolled
// forward mid-syllable instead of only checking end-state text.
window.__replayWithProbe = (groups, probeIdx) => {
  window.__mid = [];
  for (const g of groups) {
    window.__dispatch('compositionstart', '');
    for (let i = 0; i < g.length; i++) {
      window.__dispatch(g[i][0], g[i][1]);
      if (i === probeIdx) window.__mid.push(window.__decodeWire());
    }
  }
};
window.__decodeWire = () => {
  let txt = '';
  for (const m of window.__sent) {
    if (m.startsWith('kd,')) {
      const ks = parseInt(m.slice(3), 10);
      if (ks === 65288) txt = txt.slice(0, -1);
      else if (ks >= 0x01000000) txt += String.fromCodePoint(ks - 0x01000000);
    }
  }
  return txt;
};
</script></body></html>"""

# Trace 1: real IBus Chrome anatomy (captured on Linux Chrome 2026-08 with a
# live ibus-hangul engine): per syllable the preedit rolls forward, cancels to
# an empty update, compositionend arrives data-less and the commit text rides
# the textInput right after.
IBUS_SYLLABLE = [
    ["compositionupdate", "PRE1"],
    ["compositionupdate", "FULL"],
    ["compositionupdate", ""],
    ["textInput", ""],
    ["compositionend", ""],
    ["textInput", "FULL"],
]
# Trace 2: CDP-driven anatomy: the commit text appears in textInput first and
# in the compositionend data right after.
CDP_SYLLABLE = [
    ["compositionupdate", "PRE1"],
    ["compositionupdate", "FULL"],
    ["textInput", "FULL"],
    ["compositionend", "FULL"],
]


def groups(template: list, syllables: list) -> list:
    return [[[kind, data.replace("PRE1", p).replace("FULL", f)]
             for kind, data in template] for p, f in syllables]


IBUS_GROUPS = groups(IBUS_SYLLABLE,
                     [("ㅎ", "한"), ("ㄱ", "글"), ("ㅇ", "안"), ("ㄴ", "녕")])
CDP_GROUPS = groups(CDP_SYLLABLE, [("ㅎ", "한"), ("ㄱ", "글")])
IBUS_EXPECT = "한글안녕"
IBUS_PROBES = ["한", "한글", "한글안", "한글안녕"]
IBUS_PROBE_IDX = 1
CDP_EXPECT = "한글"


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args) -> None:
        pass

    def do_GET(self) -> None:
        if self.path == "/tests-ime-client.html":
            body = PAGE.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()


class Server(socketserver.TCPServer):
    allow_reuse_address = True


httpd = Server(("127.0.0.1", 0), functools.partial(Quiet, directory=ROOT))
port = httpd.server_address[1]
threading.Thread(target=httpd.serve_forever, daemon=True).start()

WIN_UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
          "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
MAC_UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
          "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")


def run_branch(pw: Any, tag: str, platform: Optional[str], ua: Optional[str],
               replay_groupss: list, engine: str = "chromium") -> bool:
    browser = getattr(pw, engine).launch()
    try:
        context = browser.new_context(user_agent=ua) if ua else browser.new_context()
        page = context.new_page()
        if platform and engine == "chromium":
            cdp = page.context.new_cdp_session(page)
            cdp.send("Emulation.setUserAgentOverride",
                     {"userAgent": ua, "platform": platform})
        page.goto(f"http://127.0.0.1:{port}/tests-ime-client.html")
        page.wait_for_function(
            "window.__ready === true || window.__errs.length > 0", timeout=15000)
        errs = page.evaluate("window.__errs")
        check(f"{tag}: input.js constructs", not errs, errs)
        if errs:
            return False
        ok = True

        def replay_groups(gseq, mid=None):
            if mid is not None:
                page.evaluate("([g, idx]) => window.__replayWithProbe(g, idx)",
                              [gseq, IBUS_PROBE_IDX])
            else:
                page.evaluate("(g) => window.__replay(g)", gseq)
            time.sleep(0.4)
            if mid is not None:
                got_mid = page.evaluate("window.__mid")
                one_mid = (got_mid == mid)
                check(f"{tag}: preedit text rolls forward mid-syllable",
                      one_mid, f"mid={got_mid!r} expect={mid!r}")
                return one_mid
            return True

        for entry in replay_groupss:
            name, gseq, expect = entry[0], entry[1], entry[2]
            mid = entry[3] if len(entry) > 3 else None
            page.evaluate("window.__sent = []")
            mid_ok = replay_groups(gseq, mid)
            got = page.evaluate("window.__decodeWire()")
            one = (got == expect)
            ok = ok and one and mid_ok
            check(f"{tag}: {name} mirrors local text remotely", one,
                  f"remote={got!r} expect={expect!r}")
        return ok

    finally:
        browser.close()


try:
    with sync_playwright() as pw:
        run_branch(pw, "linux-ibus", None, None,
                   [("ibus anatomy", IBUS_GROUPS, IBUS_EXPECT, IBUS_PROBES)])
        run_branch(pw, "win-ibus", "Windows", WIN_UA,
                   [("ibus anatomy", IBUS_GROUPS, IBUS_EXPECT, IBUS_PROBES)])
        run_branch(pw, "mac-ibus", "MacIntel", MAC_UA,
                   [("ibus anatomy", IBUS_GROUPS, IBUS_EXPECT, IBUS_PROBES)])
        run_branch(pw, "linux-cdp", None, None,
                   [("cdp anatomy", CDP_GROUPS, CDP_EXPECT),
                    ("two cdp syllables stay single", CDP_GROUPS, CDP_EXPECT)])
        run_branch(pw, "win-cdp", "Windows", WIN_UA,
                   [("cdp anatomy", CDP_GROUPS, CDP_EXPECT)])
        # WebKit is the Safari stand-in. Its Editor commits a composition by
        # dispatching textInput before compositionend (Editor.cpp
        # setComposition -> insertTextForConfirmedComposition ->
        # EventHandler::handleTextInputEvent), which is the cdp anatomy; the
        # ibus anatomy replays the opposite order so the handler stays
        # correct in WebKit's DOM even if a platform IME delivers it
        # reversed. Real macOS/iOS Safari stays uncovered here.
        run_branch(pw, "webkit-textinput-first", None, None,
                   [("cdp anatomy", CDP_GROUPS, CDP_EXPECT)], engine="webkit")
        run_branch(pw, "webkit-textinput-after", None, None,
                   [("ibus anatomy", IBUS_GROUPS, IBUS_EXPECT, IBUS_PROBES)],
                   engine="webkit")
        # Gecko is the engine the report contrasts against, and the one whose
        # older builds dispatch no textInput at all: the handler decides which
        # event carries the commit by asking the engine, so both anatomies have
        # to land the same text here as everywhere else.
        run_branch(pw, "firefox-ibus", None, None,
                   [("ibus anatomy", IBUS_GROUPS, IBUS_EXPECT, IBUS_PROBES)],
                   engine="firefox")
        run_branch(pw, "firefox-cdp", None, None,
                   [("cdp anatomy", CDP_GROUPS, CDP_EXPECT)], engine="firefox")
finally:
    httpd.shutdown()

print(f"[ime-client] {passed}/{passed + failed} passed")
raise SystemExit(1 if failed else 0)
