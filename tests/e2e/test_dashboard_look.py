#!/usr/bin/env python3
"""How the dashboard reads: contrast and spacing, in both layouts and both themes.

contrast: every view a user opens is held to WCAG AA, in the light and the dark
theme, on a desktop, a touch tablet and a phone: the sidebar shut and open with
each section, its dialogs (files, apps), the stats strip, the touch gamepad and
the soft keys; the toolbar, each tooltip, the settings card's tabs with each card
open, the menu with every submenu and the dialogs it opens, the floating panels,
the stats, the soft keys and the folded bar; and in both, every toast and the
poor-connection mark. A walk that fails to open one of those fails, so the
coverage cannot shrink unnoticed. Each view is read with every scrolling
container swept to its end, at rest, with every control hovered (a pointer's
client) and with every control focused, the states forced through the DevTools
protocol. Text reads 4.5:1 against the surface composited under it (3:1 when
large); an icon, a switch's edge and its knob, a field's edge (a text input,
select or text area), a slider's empty track and its filled part, and a focus
indicator (what focus adds: an outline, a ring, a border) read 3:1 (1.4.11),
the slider parts sampled from the rendered pixels. A slider's thumb is the
fill's color ringed in the surface's, so the ring, not its fill, is what
separates it from the track. Disabled controls are exempt.

spacing: the rows of a panel keep one rhythm. A label sits one field gap above
its control and a row one stack gap below the last, the sidebar's sections end
as far under their last row as each other, and the toolbar's settings card
leaves as much room under its last card as above its tabs. The gaps are read
between the rendered rows (a label, a control, a line of text), so a margin
added under one row shows up as the row that sits lower than the rest.

    python3 tests/e2e/test_dashboard_look.py [contrast|contrast-default|contrast-wish|spacing|all] [default|wish ...]

The contrast walk of one layout takes about nine minutes on an idle 32-core host: some thirty views, each read three
times (at rest, hovered, focused) with its scrolling swept, in two themes on three clients.
"""
import contextlib
import json
import os
import sys
import time
from typing import Any, List

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import test_dashboards as TD  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

THEMES = ("light", "dark")
DESK = {"viewport": {"width": 1440, "height": 900}, "device_scale_factor": 1}
TOUCH = {"viewport": {"width": 1280, "height": 800}, "device_scale_factor": 1, "has_touch": True}
PHONE = {"viewport": {"width": 390, "height": 844}, "device_scale_factor": 1, "has_touch": True, "is_mobile": True}
# Each layout's rhythm (--field-gap, --stack-gap), as rendered between rows, and how far a gap may stray from it.
RHYTHM = {"default": (6, 14), "wish": (6, 16)}
SLACK = 2
# Sections whose rows are a list or a figure with its own rhythm.
FREE_SECTIONS = ("stats-content", "sharing-content")

# The controls a pointer or the keyboard can land on, which the hovered and focused passes force into those states.
INTERACTIVE = ('button, a[href], input, select, textarea, summary, [role="button"], [role="tab"], [role="switch"], '
               '[role^="menuitem"], [role="option"], [role="slider"], [tabindex]:not([tabindex="-1"])')

# Each control's look at rest, kept on the page, which the focused pass tells its focus indicator from.
REST_JS = r"""
(sel) => {
  window.__lookRest = new Map();
  for (const el of document.querySelectorAll(sel)) {
    const cs = getComputedStyle(el);
    window.__lookRest.set(el, { outline: cs.outlineStyle + cs.outlineWidth + cs.outlineColor, shadow: cs.boxShadow,
                                border: cs.borderTopColor, focused: el.matches(':focus-visible') });
  }
}
"""

AUDIT_JS = r"""
({ view, rings, sel }) => {
  const probe = document.createElement('canvas'); probe.width = probe.height = 1;
  const pc = probe.getContext('2d', { willReadFrequently: true });
  const rgba = (css) => {
    if (!css || css === 'transparent') return [0, 0, 0, 0];
    pc.clearRect(0, 0, 1, 1); pc.fillStyle = '#000'; pc.fillStyle = css; pc.fillRect(0, 0, 1, 1);
    // getImageData hands back unpremultiplied components.
    const [r, g, b, a] = pc.getImageData(0, 0, 1, 1).data;
    if (a === 0) return [0, 0, 0, 0];
    return [r, g, b, a / 255];
  };
  const over = (top, bottom) => {
    const a = top[3] + bottom[3] * (1 - top[3]);
    if (a === 0) return [0, 0, 0, 0];
    return [0, 1, 2].map((i) => (top[i] * top[3] + bottom[i] * bottom[3] * (1 - top[3])) / a).concat([a]);
  };
  const lum = ([r, g, b]) => {
    const c = [r, g, b].map((v) => { v /= 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; });
    return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
  };
  const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1 || r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
    for (let e = el; e; e = e.parentElement) {
      const cs = getComputedStyle(e);
      if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return false;
    }
    return true;
  };
  const opacityOf = (el) => { let o = 1; for (let e = el; e; e = e.parentElement) o *= +getComputedStyle(e).opacity; return o; };
  // The page's own ground under the chrome is the stream; black stands in for it.
  const background = (el) => {
    const layers = [];
    for (let e = el; e; e = e.parentElement) {
      const c = rgba(getComputedStyle(e).backgroundColor);
      if (c[3] > 0) layers.push(c);
      if (c[3] >= 0.999) break;
    }
    let bg = [0, 0, 0, 1];
    for (let i = layers.length - 1; i >= 0; i--) bg = over(layers[i], bg);
    return bg;
  };
  const path = (el) => {
    const parts = [];
    for (let e = el; e && parts.length < 4 && e !== document.body; e = e.parentElement) {
      let p = e.tagName.toLowerCase();
      if (e.id) p += '#' + e.id;
      else if (e.dataset.slot) p += `[data-slot=${e.dataset.slot}]`;
      else if (typeof e.className === 'string' && e.className.trim()) p += '.' + e.className.trim().split(/\s+/).slice(0, 2).join('.');
      parts.unshift(p);
    }
    return parts.join(' > ');
  };
  const disabled = (el) => !!el.closest('[disabled], [aria-disabled="true"], [data-disabled]');
  const out = [];
  const seen = new Set();
  for (const el of document.querySelectorAll('body *')) {
    if (el.closest('video, canvas, #root > video')) continue;
    const own = [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent).join('').trim();
    const isInput = (el.tagName === 'INPUT' && !['range', 'checkbox', 'radio', 'hidden'].includes(el.type)) || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT';
    if (!own && !isInput) continue;
    if (!visible(el)) continue;
    const cs = getComputedStyle(el);
    const fg = rgba(cs.color); fg[3] *= opacityOf(el);
    const bg = background(el);
    const shown = over(fg, bg);
    const size = parseFloat(cs.fontSize), bold = +cs.fontWeight >= 700;
    const large = size >= 24 || (bold && size >= 18.66);
    const r = ratio(shown, bg);
    const text = (own || el.value || el.placeholder || '').slice(0, 40);
    const key = path(el) + '|' + text;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ view, kind: 'text', text, where: path(el), ratio: +r.toFixed(2), need: large ? 3 : 4.5, size, disabled: disabled(el),
      fg: shown.slice(0, 3).map(Math.round), bg: bg.slice(0, 3).map(Math.round) });
    if (isInput && el.placeholder && !el.value) {
      const ph = rgba(getComputedStyle(el, '::placeholder').color);
      const phShown = over(ph, bg);
      out.push({ view, kind: 'placeholder', text: el.placeholder.slice(0, 40), where: path(el), ratio: +ratio(phShown, bg).toFixed(2),
        need: 4.5, size, disabled: disabled(el), fg: phShown.slice(0, 3).map(Math.round), bg: bg.slice(0, 3).map(Math.round) });
    }
  }
  // A switch: its boundary (the border where it has one, else the track) against the surface, and its
  // knob, which carries the state, against the track it sits in.
  for (const sw of document.querySelectorAll('[data-slot="switch"]')) {
    if (!visible(sw)) continue;
    const cs = getComputedStyle(sw);
    const surface = background(sw.parentElement);
    const track = over(rgba(cs.backgroundColor), surface);
    const border = parseFloat(cs.borderTopWidth) >= 1 ? over(rgba(cs.borderTopColor), surface) : null;
    const thumb = sw.querySelector('[data-slot="switch-thumb"]');
    const knob = thumb ? over(rgba(getComputedStyle(thumb).backgroundColor), track) : null;
    const label = sw.id || sw.getAttribute('aria-label') || '';
    const checked = sw.hasAttribute('data-checked');
    const boundary = Math.max(ratio(track, surface), border ? ratio(border, surface) : 0);
    const base = { view, text: label, where: path(sw) + (checked ? ' (on)' : ' (off)'), need: 3, disabled: disabled(sw) };
    out.push({ ...base, kind: 'switch boundary', ratio: +boundary.toFixed(2), fg: (border || track).slice(0, 3).map(Math.round), bg: surface.slice(0, 3).map(Math.round) });
    if (knob) {
      out.push({ ...base, kind: 'switch knob/track', ratio: +ratio(knob, track).toFixed(2), fg: knob.slice(0, 3).map(Math.round), bg: track.slice(0, 3).map(Math.round) });
    }
  }
  // A field (text input, select, text area) is found by its edge, or its fill where that differs from the
  // surface: either has to read 3:1 against what is behind it (1.4.11).
  for (const f of document.querySelectorAll('input:not([type=range]):not([type=checkbox]):not([type=radio]):not([type=hidden]):not(#overlayInput), select, textarea')) {
    if (!visible(f) || f.closest('[aria-hidden="true"]')) continue;
    const cs = getComputedStyle(f);
    const surface = background(f.parentElement);
    const fill = over(rgba(cs.backgroundColor), surface);
    const edge = parseFloat(cs.borderTopWidth) >= 1 ? over(rgba(cs.borderTopColor), surface) : null;
    const bound = Math.max(ratio(fill, surface), edge ? ratio(edge, surface) : 0);
    out.push({ view, kind: 'field edge', text: (f.id || f.name || f.tagName).slice(0, 40), where: path(f), ratio: +bound.toFixed(2),
      need: 3, disabled: disabled(f), fg: (edge || fill).slice(0, 3).map(Math.round), bg: surface.slice(0, 3).map(Math.round) });
  }
  for (const svg of document.querySelectorAll('body svg.lucide')) {
    if (!visible(svg) || svg.closest('[aria-hidden="true"] [aria-hidden="true"]')) continue;
    const cs = getComputedStyle(svg);
    const fg = rgba(cs.stroke !== 'none' ? cs.stroke : cs.color); fg[3] *= opacityOf(svg);
    const bg = background(svg);
    const label = (svg.closest('[aria-label]') || {}).getAttribute?.('aria-label') || [...svg.classList].find((c) => c.startsWith('lucide-')) || 'icon';
    const key = path(svg) + '|' + label;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ view, kind: 'icon', text: label.slice(0, 40), where: path(svg), ratio: +ratio(over(fg, bg), bg).toFixed(2), need: 3,
      disabled: disabled(svg), fg: over(fg, bg).slice(0, 3).map(Math.round), bg: bg.slice(0, 3).map(Math.round) });
  }
  // A focus indicator (2.4.7, read 3:1 under 1.4.11): whatever the focused state adds over the control's look at
  // rest, an outline, a ring (the widest coloured box shadow) or a changed border, against the surface around it;
  // the strongest of them is the indicator.
  if (rings && window.__lookRest) {
    const shadowColors = (value) => {
      if (!value || value === 'none') return [];
      return value.split(/,(?![^(]*\))/).map((part) => {
        const m = part.match(/(rgba?\([^)]*\)|oklch\([^)]*\)|oklab\([^)]*\)|lab\([^)]*\)|color\([^)]*\)|#[0-9a-f]{3,8})/i);
        if (!m) return null;
        const nums = part.replace(m[1], '').trim().split(/\s+/).map(parseFloat).filter((x) => !isNaN(x));
        const spread = nums.length >= 4 ? nums[3] : 0, blur = nums[2] || 0;
        return (spread > 0 || blur > 0) ? { c: rgba(m[1]), spread } : null;
      }).filter((x) => x && x.c[3] > 0).sort((a, b) => b.spread - a.spread);
    };
    for (const el of document.querySelectorAll(sel)) {
      const rest = window.__lookRest.get(el);
      if (!rest || !visible(el) || disabled(el) || el.closest('[aria-hidden="true"]')) continue;
      const cs = getComputedStyle(el);
      const surface = background(el.parentElement || el);
      const marks = [];
      if (cs.outlineStyle !== 'none' && parseFloat(cs.outlineWidth) >= 1 && cs.outlineStyle + cs.outlineWidth + cs.outlineColor !== rest.outline) {
        marks.push(rgba(cs.outlineColor));
      }
      if (cs.boxShadow !== rest.shadow) {
        const ring = shadowColors(cs.boxShadow)[0];
        if (ring) marks.push(ring.c);
      }
      if (parseFloat(cs.borderTopWidth) >= 1 && cs.borderTopColor !== rest.border) marks.push(rgba(cs.borderTopColor));
      if (!marks.length) {
        // A control the keyboard can reach that shows nothing when it has the focus has no indicator at all. A menu
        // item is the exception: its menu highlights the item the focus is on (data-highlighted), which a forced
        // :focus does not reach, and the open submenu's trigger is highlighted already. So is a control that has the
        // focus at rest (a dialog's first, focused as it opens): its indicator is on in both looks.
        if (el.tabIndex >= 0 && !rest.focused && !(el.getAttribute('role') || '').startsWith('menuitem')) {
          const text = (el.getAttribute('aria-label') || el.textContent || el.id || el.tagName).trim().slice(0, 40);
          out.push({ view, kind: 'focus ring', text, where: path(el), ratio: 0, need: 3, disabled: false,
            fg: null, bg: surface.slice(0, 3).map(Math.round), note: 'no focus indicator' });
        }
        continue;
      }
      const shown = marks.map((c) => over(c, surface));
      const best = shown.reduce((a, b) => (ratio(a, surface) >= ratio(b, surface) ? a : b));
      const text = (el.getAttribute('aria-label') || el.textContent || el.id || el.tagName).trim().slice(0, 40);
      out.push({ view, kind: 'focus ring', text, where: path(el), ratio: +ratio(best, surface).toFixed(2), need: 3,
        disabled: false, fg: best.slice(0, 3).map(Math.round), bg: surface.slice(0, 3).map(Math.round) });
    }
  }
  return out;
}
"""

# Every container that scrolls its content, tagged for the sweep: where it stands and how far it goes.
SCROLLERS_JS = r"""
() => [...document.querySelectorAll('body *')].filter((el) => {
  const cs = getComputedStyle(el);
  if (!/(auto|scroll)/.test(cs.overflowY) || el.scrollHeight <= el.clientHeight + 2) return false;
  const r = el.getBoundingClientRect();
  return r.width > 1 && r.height > 1 && cs.visibility !== 'hidden';
}).map((el, i) => { el.dataset.lookScroll = String(i);
  return { i, top: el.scrollTop, page: el.clientHeight, max: el.scrollHeight - el.clientHeight }; })
"""

# The slider parts, pixel-sampled: the empty track and the filled part against the surface.
SLIDERS_JS = r"""
() => [...document.querySelectorAll('input[type="range"]')].filter((el) => {
  const r = el.getBoundingClientRect();
  if (r.width < 4 || r.height < 4 || r.top < 0 || r.bottom > innerHeight) return false;
  for (let e = el; e; e = e.parentElement) {
    const cs = getComputedStyle(e);
    if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return false;
  }
  return !el.disabled;
}).map((el) => { const r = el.getBoundingClientRect();
  return { id: el.id, x: r.x, y: r.y, w: r.width, h: r.height, value: (el.value - el.min) / ((el.max - el.min) || 1) }; })
"""


def lum(c: tuple) -> float:
    def ch(v: float) -> float:
        v /= 255
        return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4
    return 0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])


def ratio(a: tuple, b: tuple) -> float:
    x, y = sorted((lum(a), lum(b)), reverse=True)
    return (x + 0.05) / (y + 0.05)


def sliders(page: Any, view: str) -> List[dict]:
    from PIL import Image
    import io
    boxes = page.evaluate(SLIDERS_JS)
    if not boxes:
        return []
    shot = Image.open(io.BytesIO(page.screenshot())).convert("RGB")
    scale = shot.width / page.viewport_size["width"]

    def px(x: float, y: float) -> tuple:
        return shot.getpixel((min(shot.width - 1, max(0, int(x * scale))), min(shot.height - 1, max(0, int(y * scale)))))
    out = []
    for b in boxes:
        cy = b["y"] + b["h"] / 2
        surface = px(b["x"] + b["w"] + 3, cy)
        if b["value"] < 0.8:
            empty = px(b["x"] + b["w"] - 3, cy)
            out.append({"view": view, "kind": "slider track", "text": b["id"], "ratio": round(ratio(empty, surface), 2), "need": 3})
        if b["value"] > 0.2:
            filled = px(b["x"] + 3, cy)
            out.append({"view": view, "kind": "slider fill", "text": b["id"], "ratio": round(ratio(filled, surface), 2), "need": 3})
    return out


@contextlib.contextmanager
def forced(page: Any, states: List[str]):
    """Every control (INTERACTIVE) held in the CSS `states` through the DevTools protocol while the block runs, as a
    pointer over it or the keyboard on it would put it; transitions are let settle first."""
    cdp = page.context.new_cdp_session(page)
    ids: List[int] = []
    try:
        cdp.send("DOM.enable")
        cdp.send("CSS.enable")
        root = cdp.send("DOM.getDocument", {"depth": 0})["root"]["nodeId"]
        ids = cdp.send("DOM.querySelectorAll", {"nodeId": root, "selector": INTERACTIVE})["nodeIds"]
        for nid in ids:
            cdp.send("CSS.forcePseudoState", {"nodeId": nid, "forcedPseudoClasses": states})
        time.sleep(0.4)
        yield
    finally:
        for nid in ids:
            try:
                cdp.send("CSS.forcePseudoState", {"nodeId": nid, "forcedPseudoClasses": []})
            except Exception:
                pass
        try:
            cdp.detach()
        except Exception:
            pass
        time.sleep(0.3)


def sweep(page: Any, view: str, state: str, found: List[dict], rings: bool = False) -> None:
    """Audit what shows, then every scrolling container a page at a time to its end, and put each back."""
    def one(tag: str) -> None:
        rows = page.evaluate(AUDIT_JS, {"view": tag, "rings": rings, "sel": INTERACTIVE})
        found.extend(dict(r, state=state) for r in rows)
        if state == "rest":
            found.extend(dict(r, state=state) for r in sliders(page, tag))
    one(view)
    for sc in page.evaluate(SCROLLERS_JS):
        step, pos = max(40, int(sc["page"] * 0.8)), 0
        while pos < sc["max"]:
            pos = min(sc["max"], pos + step)
            page.evaluate("([i, y]) => { const el = document.querySelector(`[data-look-scroll='${i}']`); if (el) el.scrollTop = y; }",
                          [sc["i"], pos])
            time.sleep(0.25)
            one(f"{view} (scrolled {pos}px)")
        page.evaluate("([i, y]) => { const el = document.querySelector(`[data-look-scroll='${i}']`); if (el) el.scrollTop = y; }",
                      [sc["i"], sc["top"]])


def audit(page: Any, view: str, found: List[dict], views: List[str], touch: bool) -> None:
    """One view at rest, hovered (where a pointer hovers) and focused, scrolled through each time."""
    time.sleep(0.5)
    views.append(view)
    sweep(page, view, "rest", found)
    if not touch:
        with forced(page, ["hover"]):
            sweep(page, view, "hovered", found)
    page.evaluate(REST_JS, INTERACTIVE)
    with forced(page, ["focus", "focus-visible"]):
        sweep(page, view, "focused", found, rings=True)


def wait_for(page: Any, selector: str, timeout: float = 4.0) -> bool:
    try:
        page.locator(selector).first.wait_for(state="visible", timeout=timeout * 1000)
        return True
    except Exception:
        return False


# The notices every layout shows over the stream, one at a time: an upload's toasts, the transport advice with its
# action, a warning, and (on a touch client) a document to print.
TOASTS = (
    ("upload", "{type: 'fileUpload', payload: {status: 'progress', fileName: 'look-progress.png', progress: 40}}", False),
    ("uploaded", "{type: 'fileUpload', payload: {status: 'end', fileName: 'look-done.png'}}", False),
    ("upload failed", "{type: 'fileUpload', payload: {status: 'error', fileName: 'look-failed.png', message: 'disk full'}}", False),
    ("warning", "{type: 'fileUpload', payload: {status: 'warning', fileName: 'look-warn', message: 'not allowed'}}", False),
    ("transport advice", "{type: 'transportAdvice', offer: 'websockets'}", False),
    ("print", "{type: 'printDocument', url: 'about:blank', name: 'look.pdf'}", True),
)
# The poor-connection mark held up whatever the core records meanwhile, and let go again.
HOLD_POOR = """() => { const c = window.stream_client = window.stream_client || {}; c.__lookConn = c.connection;
  Object.defineProperty(c, 'connection', { configurable: true, get: () => 'poor', set: (v) => { c.__lookConn = v; } });
  window.dispatchEvent(new Event('selkies-stream-stats')); }"""
LET_GO_POOR = """() => { const c = window.stream_client; if (!c) return; const v = c.__lookConn; delete c.connection;
  c.connection = v; window.dispatchEvent(new Event('selkies-stream-stats')); }"""


def notice_views(page: Any, tag: str, found: List[dict], views: List[str], touch: bool) -> None:
    for name, message, touch_only in TOASTS:
        if touch_only and not touch:
            continue
        page.evaluate(f"window.postMessage({message}, location.origin)")
        if wait_for(page, "[data-sonner-toast]"):
            audit(page, f"{tag} toast {name}", found, views, touch)
        page.evaluate("document.querySelectorAll('[data-sonner-toast] [data-close-button]').forEach((b) => b.click())")
        for _ in range(20):
            if not page.locator("[data-sonner-toast]").count():
                break
            time.sleep(0.2)
    page.evaluate(HOLD_POOR)
    if wait_for(page, ".connection-indicator"):
        audit(page, f"{tag} poor connection", found, views, touch)
    page.evaluate(LET_GO_POOR)


def dialog_view(page: Any, opener: Any, tag: str, name: str, found: List[dict], views: List[str], touch: bool) -> None:
    """Open a dialog by `opener`, audit it, and close it."""
    if not opener.count():
        return
    opener.first.click()
    if wait_for(page, '[role="dialog"]'):
        audit(page, f"{tag} {name} dialog", found, views, touch)
    page.keyboard.press("Escape")
    time.sleep(0.5)


def sidebar_views(page: Any, tag: str, found: List[dict], views: List[str], touch: bool) -> None:
    TD.sidebar_close(page)
    audit(page, f"{tag} sidebar shut", found, views, touch)
    TD.sidebar_open(page)
    audit(page, f"{tag} sidebar", found, views, touch)
    headers = page.locator(".sidebar-section-header")
    # Their own text: a header renders it upper-cased.
    for name in [(headers.nth(i).text_content() or "").strip() for i in range(headers.count())]:
        TD.sidebar_section(page, name, True)
        content = page.locator(".sidebar-section-header[aria-expanded=true] + .sidebar-section-content").first
        if content.count():
            content.scroll_into_view_if_needed()
        audit(page, f"{tag} {name}", found, views, touch)
        dialog_view(page, page.locator('#files-content button:has-text("Download Files")'), tag, "files", found, views, touch)
        dialog_view(page, page.locator('#apps-content button'), tag, "apps", found, views, touch)
        toggle = page.locator("#statsStripToggle")
        if toggle.count() and toggle.first.is_visible():
            toggle.click()
            TD.sidebar_close(page)
            if wait_for(page, ".stream-strip"):
                audit(page, f"{tag} stats strip", found, views, touch)
            TD.sidebar_section(page, name, True)
            page.locator("#statsStripToggle").click()
        pad = page.locator("#touchGamepadToggle")
        if touch and pad.count():
            pad.click()
            TD.sidebar_close(page)
            time.sleep(1.0)
            audit(page, f"{tag} touch gamepad", found, views, touch)
            TD.sidebar_section(page, name, True)
            page.locator("#touchGamepadToggle").click()
        TD.sidebar_section(page, name, False)
    if touch:
        toggle = page.locator(".sidebar .key-palette-toggle")
        if toggle.count():
            toggle.first.click()
            page.locator(".sidebar-mobile-key-actions").first.scroll_into_view_if_needed()
            audit(page, f"{tag} soft keys", found, views, touch)
            toggle.first.click()
    TD.sidebar_close(page)


def toolbar_views(page: Any, tag: str, found: List[dict], views: List[str], touch: bool) -> None:
    TD.wish_show_toolbar(page)
    audit(page, f"{tag} toolbar", found, views, touch)
    if not touch:
        tips = page.locator('.toolbar [data-slot="tooltip-trigger"]')
        for i in range(tips.count()):
            label = tips.nth(i).get_attribute("aria-label") or str(i)
            tips.nth(i).hover()
            if wait_for(page, '[data-slot="tooltip-content"]', 3):
                audit(page, f"{tag} tooltip {label}", found, views, touch)
            page.mouse.move(700, 700)
            time.sleep(0.5)
    if TD.open_wish_settings_tab(page, "Video"):
        for tab in [t.strip() for t in page.locator('[role="tab"]').all_inner_texts()]:
            if not TD.open_wish_settings_tab(page, tab):
                continue
            triggers = page.locator('[role="tabpanel"] [data-slot="accordion-trigger"]')
            for i in range(triggers.count()):
                if triggers.nth(i).get_attribute("aria-expanded") != "true":
                    triggers.nth(i).click()
                    time.sleep(0.4)
                audit(page, f"{tag} {tab} card {i + 1}", found, views, touch)
    TD.wish_close_menus(page)
    page.keyboard.press("Escape")
    menu = page.locator('button[aria-label="Menu"]').first
    if menu.count():
        menu.click()
        time.sleep(0.6)
        audit(page, f"{tag} menu", found, views, touch)
        subs = page.locator('[role="menu"] [aria-haspopup="menu"]').count()
        dialogs = [t.strip() for t in page.locator('[role="menu"] [role="menuitem"]:not([aria-haspopup])').all_inner_texts()]
        page.keyboard.press("Escape")
        for i in range(subs):
            # One submenu at a time, from a freshly opened menu: an open one covers the triggers under it.
            menu.click()
            time.sleep(0.5)
            sub = page.locator('[role="menu"] [aria-haspopup="menu"]').nth(i)
            name = sub.inner_text().strip()
            sub.focus()
            page.keyboard.press("ArrowRight")
            time.sleep(0.7)
            audit(page, f"{tag} menu {name}", found, views, touch)
            TD.wish_close_menus(page)
        for name in dialogs:
            if name == "Add a second screen":
                continue
            menu.click()
            time.sleep(0.5)
            page.locator(f'[role="menu"] [role="menuitem"]:has-text("{name}")').first.click()
            if wait_for(page, '[role="dialog"]'):
                audit(page, f"{tag} {name} dialog", found, views, touch)
            page.keyboard.press("Escape")
            time.sleep(0.5)
    # The floating panels and the stats, each opened by its own button and closed by it again.
    for sel, name in (('.toolbar button[aria-label="Gamepads"]', "gamepads panel"), (".toolbar .stats-button", "stats"),
                      (".toolbar .touch-gamepad-button", "touch gamepad")):
        button = page.locator(sel)
        if not button.count():
            continue
        button.first.click()
        time.sleep(1.0)
        audit(page, f"{tag} {name}", found, views, touch)
        TD.wish_show_toolbar(page)
        page.locator(sel).first.click()
        time.sleep(0.5)
    if touch:
        keys = page.locator(".soft-keys-button").first
        if keys.count():
            keys.click()
            time.sleep(0.5)
            toggle = page.locator(".key-palette-toggle").first
            if toggle.count():
                toggle.click()
                time.sleep(0.4)
            audit(page, f"{tag} soft keys", found, views, touch)
            page.locator(".soft-keys-button").first.click()
            time.sleep(0.4)
    hide = page.locator('.toolbar button[aria-label="Hide toolbar"]')
    if hide.count():
        hide.first.click()
        time.sleep(0.8)
        audit(page, f"{tag} toolbar folded", found, views, touch)
        TD.wish_show_toolbar(page)


# A desktop, a touch tablet and a phone: the phone's layout is the narrow one (max-sm), with its own controls.
CLIENTS = (("desk", DESK, False), ("tablet", TOUCH, True), ("phone", PHONE, True))
# The views a walk has to reach in each client, so a view that stops opening fails here instead of going unread.
MUST = {
    "default": ("sidebar shut", "sidebar", "Video Settings", "Shortcuts", "files dialog", "stats strip", "toast upload",
                "toast uploaded", "toast upload failed", "toast warning", "toast transport advice", "poor connection"),
    "wish": ("toolbar", "Video card 1", "Resolution card 1", "Audio card 1", "menu", "menu Shortcuts", "menu Clipboard",
             "Files dialog", "gamepads panel", "stats", "toolbar folded", "toast upload", "toast uploaded",
             "toast upload failed", "toast warning", "toast transport advice", "poor connection"),
}
MUST_TOUCH = {"default": ("soft keys", "touch gamepad", "toast print"), "wish": ("soft keys", "touch gamepad", "toast print")}


def page_for(browser: Any, theme: str, client: dict) -> Any:
    ctx = browser.new_context(**client)
    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets'; try { localStorage.setItem('theme', '%s'); } catch (e) {}"
                        % theme)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    TD.wait_chunk(page, 30)
    if client.get("has_touch"):
        page.touchscreen.tap(client["viewport"]["width"] // 2, client["viewport"]["height"] // 2)
    time.sleep(1.2)
    return page


def contrast_block(layout: str) -> "H.Results":
    res = H.Results(f"look-contrast-{layout}")
    H.server_start(mode="websockets", wayland=False, web_root=H.DASHBOARD_DIST, layout=layout)
    try:
        with sync_playwright() as p:
            browser = C.launch_browser(p, "chromium")
            for theme in THEMES:
                found: List[dict] = []
                for name, client, touch in CLIENTS:
                    page = page_for(browser, theme, client)
                    tag = f"{theme} {name}"
                    views: List[str] = []
                    (sidebar_views if layout == "default" else toolbar_views)(page, tag, found, views, touch)
                    notice_views(page, tag, found, views, touch)
                    page.context.close()
                    want = MUST[layout] + (MUST_TOUCH[layout] if touch else ())
                    missing = [v for v in want if f"{tag} {v}" not in views]
                    res.check(f"{layout} {theme} {name}: every view opens for the audit ({len(views)} views)", not missing, missing)
                for state in ("rest", "hovered", "focused"):
                    rows = [f for f in found if f["state"] == state]
                    below, seen = [], set()
                    for f in rows:
                        key = (f["kind"], f["text"], f.get("where"), f["view"].split(" (scrolled")[0])
                        if f["ratio"] < f["need"] and not f.get("disabled") and key not in seen:
                            seen.add(key)
                            below.append(f)
                    if os.environ.get("LOOK_DUMP"):
                        with open(os.environ["LOOK_DUMP"], "a") as dump:
                            for f in below:
                                dump.write(json.dumps(dict(f, layout=layout, theme=theme)) + "\n")
                    kinds = sorted({f["kind"] for f in rows})
                    res.check(f"{layout} {theme}, {state}: {len(rows)} texts, icons and controls read at AA ({', '.join(kinds)})",
                              not below and len(rows) > 100,
                              [(f["view"], f["kind"], f["text"], f["ratio"], f.get("where", "")[-50:]) for f in below[:8]])
            browser.close()
    finally:
        H.server_stop()
    return res


# The rows of a container: its visible controls and lines of text, grouped where they overlap vertically.
ROWS_JS = r"""
(root) => {
  const vis = (el) => {
    const cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return false;
    if (cs.clip !== 'auto' || (cs.clipPath && cs.clipPath !== 'none')) return false;
    const r = el.getBoundingClientRect();
    return r.width > 1 && r.height > 1;
  };
  const ATOM = 'button,input,select,textarea,a,label,kbd,[role="switch"],[data-slot="kbd-group"],h3,h4,p,canvas,img,.range-slider';
  const atoms = [], seen = new Set();
  const inside = (el) => { for (let p = el.parentElement; p && p !== root; p = p.parentElement) if (seen.has(p)) return true; return false; };
  for (const el of root.querySelectorAll(ATOM)) {
    if (!vis(el) || inside(el)) continue;
    seen.add(el);
    const r = el.getBoundingClientRect();
    atoms.push([r.top, r.bottom, (el.innerText || el.id || el.tagName).trim().slice(0, 24)]);
  }
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const el = n.parentElement;
    if (!n.textContent.trim() || !el || !vis(el) || seen.has(el) || inside(el)) continue;
    const range = document.createRange(); range.selectNodeContents(n);
    let r = range.getBoundingClientRect();
    // A block of text alone is measured by its line box, as the other atoms are by their boxes: the glyphs'
    // own box is the font's ascent and descent, which differ between fonts (DejaVu's sits 3 px inside its line).
    const cs = getComputedStyle(el);
    if (!el.childElementCount && cs.display !== 'inline') {
      const b = el.getBoundingClientRect();
      r = { top: b.top + parseFloat(cs.borderTopWidth) + parseFloat(cs.paddingTop),
            bottom: b.bottom - parseFloat(cs.borderBottomWidth) - parseFloat(cs.paddingBottom),
            height: b.height };
    }
    if (r.height > 0) atoms.push([r.top, r.bottom, n.textContent.trim().slice(0, 24)]);
  }
  atoms.sort((a, b) => a[0] - b[0]);
  const rows = [];
  for (const [top, bottom, text] of atoms) {
    const row = rows[rows.length - 1];
    if (row && top < row[1] - 2) { row[1] = Math.max(row[1], bottom); row[2].push(text); }
    else rows.push([top, bottom, [text]]);
  }
  const box = root.getBoundingClientRect();
  return { top: box.top, bottom: box.bottom, rows };
}
"""

# The settings card's room above its tabs and under its last card, where its content does not scroll.
CARD_JS = r"""
() => {
  const card = document.querySelector('[data-slot="card"]');
  const list = card && card.querySelector('[role="tablist"]');
  const panel = card && card.querySelector('[role="tabpanel"]:not([hidden])');
  const items = panel ? [...panel.querySelectorAll('[data-slot="accordion-item"]')] : [];
  const scroller = panel && panel.querySelector('[data-slot="card-content"]');
  if (!list || !items.length || (scroller && scroller.scrollHeight > scroller.clientHeight + 1)) return null;
  const c = card.getBoundingClientRect();
  return { above: Math.round(list.getBoundingClientRect().top - c.top),
           under: Math.round(c.bottom - items[items.length - 1].getBoundingClientRect().bottom) };
}
"""


def gaps_off(layout: str, rows: list) -> list:
    """The gaps between consecutive rows that are neither the field gap nor the stack gap."""
    field, stack = RHYTHM[layout]
    off = []
    for a, b in zip(rows, rows[1:]):
        gap = round(b[0] - a[1])
        if min(abs(gap - field), abs(gap - stack)) > SLACK:
            off.append((gap, a[2][:2], b[2][:2]))
    return off


def spacing_block(layout: str) -> "H.Results":
    res = H.Results(f"look-spacing-{layout}")
    H.server_start(mode="websockets", wayland=False, web_root=H.DASHBOARD_DIST, layout=layout)
    try:
        with sync_playwright() as p:
            browser = C.launch_browser(p, "chromium")
            page = page_for(browser, "light", DESK)
            if layout == "default":
                TD.sidebar_open(page)
                headers = page.locator(".sidebar-section-header")
                ends = {}
                for name in [headers.nth(i).inner_text().strip() for i in range(headers.count())]:
                    TD.sidebar_section(page, name, True)
                    cid = page.evaluate("""(n) => { const h = [...document.querySelectorAll('.sidebar-section-header')]
                        .find((x) => x.innerText.trim() === n); const c = h && h.parentElement.querySelector('.sidebar-section-content');
                        return c ? c.id : null; }""", name)
                    if cid and cid not in FREE_SECTIONS:
                        data = page.evaluate(f"() => ({ROWS_JS})(document.getElementById('{cid}'))")
                        if data and data["rows"]:
                            off = gaps_off(layout, data["rows"])
                            res.check(f"default: {name} keeps the rhythm between its rows", not off, off)
                            ends[name] = round(data["bottom"] - data["rows"][-1][1])
                    TD.sidebar_section(page, name, False)
                spread = max(ends.values()) - min(ends.values()) if ends else 0
                res.check("default: every section ends as far under its last row", ends and spread <= SLACK, ends)
            else:
                TD.wish_show_toolbar(page)
                for tab in ("Video", "Resolution", "Audio"):
                    if not TD.open_wish_settings_tab(page, tab):
                        res.check(f"wish: the {tab} tab opens", False)
                        continue
                    triggers = page.locator('[role="tabpanel"] [data-slot="accordion-trigger"]')
                    for i in range(triggers.count()):
                        if triggers.nth(i).get_attribute("aria-expanded") != "true":
                            triggers.nth(i).click()
                            time.sleep(0.4)
                        body = page.evaluate("""() => ({ROWS})([...document.querySelectorAll(
                            '[role="tabpanel"] [data-slot="accordion-content"]')].find((e) => e.getBoundingClientRect().height > 0)
                            || document.body)""".replace("{ROWS}", ROWS_JS))
                        if body and body["rows"]:
                            off = gaps_off(layout, body["rows"])
                            res.check(f"wish: the {tab} tab's card {i + 1} keeps the rhythm between its rows", not off, off)
                            res.check(f"wish: the {tab} tab's card {i + 1} pads its first and last rows alike",
                                      abs((body["rows"][0][0] - body["top"]) - (body["bottom"] - body["rows"][-1][1])) <= 2 * SLACK + 2,
                                      (round(body["rows"][0][0] - body["top"]), round(body["bottom"] - body["rows"][-1][1])))
                        room = page.evaluate(CARD_JS)
                        if room:
                            res.check(f"wish: the card leaves as much room under its last card as above its tabs ({tab}, {i + 1})",
                                      abs(room["above"] - room["under"]) <= SLACK, room)
            page.context.close()
            browser.close()
    finally:
        H.server_stop()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    # contrast-wish: one layout's walk, the selectors suites.py runs each on its own.
    which, _, only = which.partition("-")
    layouts = [only] if only else (sys.argv[2:] or H.LAYOUTS)
    blocks = []
    for layout in layouts:
        if which in ("all", "contrast"):
            blocks.append(contrast_block(layout))
        if which in ("all", "spacing"):
            blocks.append(spacing_block(layout))
    ok = all([b.summary() for b in blocks])
    print(f"=== DASHBOARD LOOK: {'PASS' if ok else 'FAIL'} ===")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
