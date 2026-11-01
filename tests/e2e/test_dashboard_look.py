#!/usr/bin/env python3
"""How the dashboard reads: contrast and spacing, in both layouts and both themes.

contrast: every view a user opens (the sidebar's sections, the toolbar's settings
tabs with each card open, its menu and every submenu, the soft keys on a touch
client), in the light and the dark theme, is held to WCAG AA. Text reads 4.5:1
against the surface composited under it (3:1 when large); an icon, a switch's
edge and its knob, a field's edge (a text input, select or text area), and a
slider's empty track and its filled part read 3:1 (1.4.11), the slider parts
sampled from the rendered pixels. A slider's thumb is the fill's color ringed in
the surface's, so the ring, not its fill, is what separates it from the track.
Disabled controls are exempt.

spacing: the rows of a panel keep one rhythm. A label sits one field gap above
its control and a row one stack gap below the last, the sidebar's sections end
as far under their last row as each other, and the toolbar's settings card
leaves as much room under its last card as above its tabs. The gaps are read
between the rendered rows (a label, a control, a line of text), so a margin
added under one row shows up as the row that sits lower than the rest.

    python3 tests/e2e/test_dashboard_look.py [contrast|spacing|all]
"""
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
# Each layout's rhythm (--field-gap, --stack-gap), as rendered between rows, and how far a gap may stray from it.
RHYTHM = {"default": (6, 14), "wish": (6, 16)}
SLACK = 2
# Sections whose rows are a list or a figure with its own rhythm.
FREE_SECTIONS = ("stats-content", "sharing-content")

AUDIT_JS = r"""
(view) => {
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
  return out;
}
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


def audit(page: Any, view: str, found: List[dict]) -> None:
    time.sleep(0.5)
    found.extend(page.evaluate(AUDIT_JS, view))
    found.extend(sliders(page, view))


def sidebar_views(page: Any, tag: str, found: List[dict], touch: bool) -> None:
    TD.sidebar_open(page)
    audit(page, f"{tag} sidebar", found)
    headers = page.locator(".sidebar-section-header")
    for name in [headers.nth(i).inner_text().strip() for i in range(headers.count())]:
        TD.sidebar_section(page, name, True)
        content = page.locator(".sidebar-section-header[aria-expanded=true] + .sidebar-section-content").first
        if content.count():
            content.scroll_into_view_if_needed()
        audit(page, f"{tag} {name}", found)
        TD.sidebar_section(page, name, False)
    if touch:
        toggle = page.locator(".sidebar .key-palette-toggle")
        if toggle.count():
            toggle.first.click()
            page.locator(".sidebar-mobile-key-actions").first.scroll_into_view_if_needed()
            audit(page, f"{tag} soft keys", found)
            toggle.first.click()


def toolbar_views(page: Any, tag: str, found: List[dict], touch: bool) -> None:
    TD.wish_show_toolbar(page)
    audit(page, f"{tag} toolbar", found)
    for tab in ("Video", "Resolution", "Audio"):
        if not TD.open_wish_settings_tab(page, tab):
            continue
        triggers = page.locator('[role="tabpanel"] [data-slot="accordion-trigger"]')
        for i in range(triggers.count()):
            if triggers.nth(i).get_attribute("aria-expanded") != "true":
                triggers.nth(i).click()
                time.sleep(0.4)
            audit(page, f"{tag} {tab} card {i + 1}", found)
    TD.wish_close_menus(page)
    page.keyboard.press("Escape")
    menu = page.locator('button[aria-label="Menu"]').first
    if menu.count():
        menu.click()
        time.sleep(0.6)
        audit(page, f"{tag} menu", found)
        count = page.locator('[role="menu"] [aria-haspopup="menu"]').count()
        page.keyboard.press("Escape")
        for i in range(count):
            # One submenu at a time, from a freshly opened menu: an open one covers the triggers under it.
            menu.click()
            time.sleep(0.5)
            sub = page.locator('[role="menu"] [aria-haspopup="menu"]').nth(i)
            name = sub.inner_text().strip()
            sub.focus()
            page.keyboard.press("ArrowRight")
            time.sleep(0.7)
            audit(page, f"{tag} menu {name}", found)
            page.keyboard.press("Escape")
            page.keyboard.press("Escape")
            time.sleep(0.3)
    if touch:
        keys = page.locator(".soft-keys-button").first
        if keys.count():
            keys.click()
            time.sleep(0.5)
            toggle = page.locator(".key-palette-toggle").first
            if toggle.count():
                toggle.click()
                time.sleep(0.4)
            audit(page, f"{tag} soft keys", found)


def page_for(browser: Any, theme: str, touch: bool) -> Any:
    ctx = browser.new_context(**(TOUCH if touch else DESK))
    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets'; try { localStorage.setItem('theme', '%s'); } catch (e) {}"
                        % theme)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    TD.wait_chunk(page, 30)
    if touch:
        page.touchscreen.tap(640, 400)
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
                for touch in (False, True):
                    page = page_for(browser, theme, touch)
                    tag = f"{theme} {'touch' if touch else 'desk'}"
                    (sidebar_views if layout == "default" else toolbar_views)(page, tag, found, touch)
                    page.context.close()
                below = [f for f in found if f["ratio"] < f["need"] and not f.get("disabled")]
                kinds = sorted({f["kind"] for f in found})
                res.check(f"{layout} {theme}: {len(found)} texts, icons and controls read at AA ({', '.join(kinds)})",
                          not below and len(found) > 100,
                          [(f["view"], f["kind"], f["text"], f["ratio"]) for f in below[:6]])
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
    const r = range.getBoundingClientRect();
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
            page = page_for(browser, "light", False)
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
    blocks = []
    for layout in H.LAYOUTS:
        if which in ("all", "contrast"):
            blocks.append(contrast_block(layout))
        if which in ("all", "spacing"):
            blocks.append(spacing_block(layout))
    ok = all([b.summary() for b in blocks])
    print(f"=== DASHBOARD LOOK: {'PASS' if ok else 'FAIL'} ===")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
