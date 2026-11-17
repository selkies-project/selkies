/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// How the dashboard reads: contrast and spacing, in every layout and both themes.
//
// contrast: every view a user opens is held to WCAG AA, in the light and the
// dark theme, on a desktop, a touch tablet and a phone: the sidebar shut and
// open with each section, its dialogs (files, apps), the stats strip, the
// touch gamepad and the soft keys; the toolbar, each tooltip, the settings
// card's tabs with each card open, the menu with every submenu and the
// dialogs it opens, the floating panels, the stats, the soft keys and the
// folded bar; the edge button at rest and folded, and its fan page by page
// with each tooltip, then what the toolbar's walk opens; and in every layout,
// every toast and the poor-connection mark. A walk that fails to open one of
// those fails, so the coverage cannot shrink unnoticed. Each view is read with
// every scrolling container swept to its end, at rest, with every control
// hovered (a pointer's client) and with every control focused, the states
// forced through the DevTools protocol. Text reads 4.5:1 against the surface
// composited under it (3:1 when large); an icon, a switch's edge and its
// knob, a field's edge (a text input, select or text area), a slider's empty
// track and its filled part, and a focus indicator (what focus adds: an
// outline, a ring, a border) read 3:1 (1.4.11), the slider parts sampled from
// the rendered pixels. A slider's thumb is the fill's color ringed in the
// surface's, so the ring, not its fill, is what separates it from the track.
// Disabled controls are exempt.
//
// spacing: the rows of a panel keep one rhythm. A label sits one field gap
// above its control and a row one stack gap below the last, the sidebar's
// sections end as far under their last row as each other, and a bar layout's
// settings card leaves as much room under its last card as above its tabs.
// The gaps are read between the rendered rows (a label, a control, a line of
// text), so a margin added under one row shows up as the row that sits lower
// than the rest.
//
// The contrast walk of one layout takes about nine minutes on an idle 32-core
// host: some thirty views, each read three times (at rest, hovered, focused)
// with its scrolling swept, in two themes on three clients.
import { appendFileSync } from 'node:fs';

import type { Browser, BrowserContextOptions, Locator, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks, finishAll } from '../lib/checks.ts';
import {
    barCloseMenus, barClosePanel, barShow, barTool, closeDialogs, EDGE_FAN, EDGE_MORE, edgeCloseFan, edgeFanPage, edgeOpenFan, isEdge, openBarSettingsTab, sidebarClose, sidebarOpen, sidebarSection, waitChunk,
} from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { decodePng, rgbAt } from '../lib/png.ts';
import { within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const THEMES = ['light', 'dark'] as const;
const DESK: BrowserContextOptions = { viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 };
const TOUCH: BrowserContextOptions = { viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1, hasTouch: true };
const PHONE: BrowserContextOptions = { viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, hasTouch: true, isMobile: true };
// Each layout's rhythm (--field-gap, --stack-gap), as rendered between rows, and how far a gap may stray from it.
const RHYTHM: Record<string, [number, number]> = { default: [6, 14], wish: [6, 16], edge: [6, 16] };
const SLACK = 2;
// Sections whose rows are a list or a figure with its own rhythm.
const FREE_SECTIONS = ['stats-content', 'sharing-content'];

// The controls a pointer or the keyboard can land on, which the hovered and focused passes force into those states.
const INTERACTIVE = 'button, a[href], input, select, textarea, summary, [role="button"], [role="tab"], [role="switch"], '
    + '[role^="menuitem"], [role="option"], [role="slider"], [tabindex]:not([tabindex="-1"])';

// REST_JS: each control's look at rest, kept on the page, which the focused
// pass tells its focus indicator from. AUDIT_JS: every text, placeholder,
// switch, field, icon and focus ring in view with its contrast and what it
// needs. SCROLLERS_JS: every container that scrolls its content, tagged for
// the sweep. SLIDERS_JS: the sliders, for their parts to be pixel-sampled.
// ROWS_JS: the rows of a container, its visible controls and lines of text
// grouped where they overlap. CARD_JS: the settings card's room above its tabs
// and under its last card. HOLD_POOR / LET_GO_POOR: the poor-connection mark
// held up whatever the core records meanwhile, and let go again.
const REST_JS = "(sel) => {\n  window.__lookRest = new Map();\n  for (const el of document.querySelectorAll(sel)) {\n    const cs = getComputedStyle(el);\n    window.__lookRest.set(el, { outline: cs.outlineStyle + cs.outlineWidth + cs.outlineColor, shadow: cs.boxShadow,\n                                border: cs.borderTopColor, focused: el.matches(':focus-visible') });\n  }\n}";
const AUDIT_JS = "({ view, rings, sel }) => {\n  const probe = document.createElement('canvas'); probe.width = probe.height = 1;\n  const pc = probe.getContext('2d', { willReadFrequently: true });\n  const rgba = (css) => {\n    if (!css || css === 'transparent') return [0, 0, 0, 0];\n    pc.clearRect(0, 0, 1, 1); pc.fillStyle = '#000'; pc.fillStyle = css; pc.fillRect(0, 0, 1, 1);\n    // getImageData hands back unpremultiplied components.\n    const [r, g, b, a] = pc.getImageData(0, 0, 1, 1).data;\n    if (a === 0) return [0, 0, 0, 0];\n    return [r, g, b, a / 255];\n  };\n  const over = (top, bottom) => {\n    const a = top[3] + bottom[3] * (1 - top[3]);\n    if (a === 0) return [0, 0, 0, 0];\n    return [0, 1, 2].map((i) => (top[i] * top[3] + bottom[i] * bottom[3] * (1 - top[3])) / a).concat([a]);\n  };\n  const lum = ([r, g, b]) => {\n    const c = [r, g, b].map((v) => { v /= 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; });\n    return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];\n  };\n  const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };\n  const visible = (el) => {\n    const r = el.getBoundingClientRect();\n    if (r.width < 1 || r.height < 1 || r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;\n    for (let e = el; e; e = e.parentElement) {\n      const cs = getComputedStyle(e);\n      if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return false;\n    }\n    return true;\n  };\n  const opacityOf = (el) => { let o = 1; for (let e = el; e; e = e.parentElement) o *= +getComputedStyle(e).opacity; return o; };\n  // The page's own ground under the chrome is the stream; black stands in for it.\n  const background = (el) => {\n    const layers = [];\n    for (let e = el; e; e = e.parentElement) {\n      const c = rgba(getComputedStyle(e).backgroundColor);\n      if (c[3] > 0) layers.push(c);\n      if (c[3] >= 0.999) break;\n    }\n    let bg = [0, 0, 0, 1];\n    for (let i = layers.length - 1; i >= 0; i--) bg = over(layers[i], bg);\n    return bg;\n  };\n  const path = (el) => {\n    const parts = [];\n    for (let e = el; e && parts.length < 4 && e !== document.body; e = e.parentElement) {\n      let p = e.tagName.toLowerCase();\n      if (e.id) p += '#' + e.id;\n      else if (e.dataset.slot) p += `[data-slot=${e.dataset.slot}]`;\n      else if (typeof e.className === 'string' && e.className.trim()) p += '.' + e.className.trim().split(/\\s+/).slice(0, 2).join('.');\n      parts.unshift(p);\n    }\n    return parts.join(' > ');\n  };\n  const disabled = (el) => !!el.closest('[disabled], [aria-disabled=\"true\"], [data-disabled]');\n  const out = [];\n  const seen = new Set();\n  for (const el of document.querySelectorAll('body *')) {\n    if (el.closest('video, canvas, #root > video')) continue;\n    const own = [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent).join('').trim();\n    const isInput = (el.tagName === 'INPUT' && !['range', 'checkbox', 'radio', 'hidden'].includes(el.type)) || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT';\n    if (!own && !isInput) continue;\n    if (!visible(el)) continue;\n    const cs = getComputedStyle(el);\n    const fg = rgba(cs.color); fg[3] *= opacityOf(el);\n    const bg = background(el);\n    const shown = over(fg, bg);\n    const size = parseFloat(cs.fontSize), bold = +cs.fontWeight >= 700;\n    const large = size >= 24 || (bold && size >= 18.66);\n    const r = ratio(shown, bg);\n    const text = (own || el.value || el.placeholder || '').slice(0, 40);\n    const key = path(el) + '|' + text;\n    if (seen.has(key)) continue;\n    seen.add(key);\n    out.push({ view, kind: 'text', text, where: path(el), ratio: +r.toFixed(2), need: large ? 3 : 4.5, size, disabled: disabled(el),\n      fg: shown.slice(0, 3).map(Math.round), bg: bg.slice(0, 3).map(Math.round) });\n    if (isInput && el.placeholder && !el.value) {\n      const ph = rgba(getComputedStyle(el, '::placeholder').color);\n      const phShown = over(ph, bg);\n      out.push({ view, kind: 'placeholder', text: el.placeholder.slice(0, 40), where: path(el), ratio: +ratio(phShown, bg).toFixed(2),\n        need: 4.5, size, disabled: disabled(el), fg: phShown.slice(0, 3).map(Math.round), bg: bg.slice(0, 3).map(Math.round) });\n    }\n  }\n  // A switch: its boundary (the border where it has one, else the track) against the surface, and its\n  // knob, which carries the state, against the track it sits in.\n  for (const sw of document.querySelectorAll('[data-slot=\"switch\"]')) {\n    if (!visible(sw)) continue;\n    const cs = getComputedStyle(sw);\n    const surface = background(sw.parentElement);\n    const track = over(rgba(cs.backgroundColor), surface);\n    const border = parseFloat(cs.borderTopWidth) >= 1 ? over(rgba(cs.borderTopColor), surface) : null;\n    const thumb = sw.querySelector('[data-slot=\"switch-thumb\"]');\n    const knob = thumb ? over(rgba(getComputedStyle(thumb).backgroundColor), track) : null;\n    const label = sw.id || sw.getAttribute('aria-label') || '';\n    const checked = sw.hasAttribute('data-checked');\n    const boundary = Math.max(ratio(track, surface), border ? ratio(border, surface) : 0);\n    const base = { view, text: label, where: path(sw) + (checked ? ' (on)' : ' (off)'), need: 3, disabled: disabled(sw) };\n    out.push({ ...base, kind: 'switch boundary', ratio: +boundary.toFixed(2), fg: (border || track).slice(0, 3).map(Math.round), bg: surface.slice(0, 3).map(Math.round) });\n    if (knob) {\n      out.push({ ...base, kind: 'switch knob/track', ratio: +ratio(knob, track).toFixed(2), fg: knob.slice(0, 3).map(Math.round), bg: track.slice(0, 3).map(Math.round) });\n    }\n  }\n  // A field (text input, select, text area) is found by its edge, or its fill where that differs from the\n  // surface: either has to read 3:1 against what is behind it (1.4.11).\n  for (const f of document.querySelectorAll('input:not([type=range]):not([type=checkbox]):not([type=radio]):not([type=hidden]):not(#overlayInput), select, textarea')) {\n    if (!visible(f) || f.closest('[aria-hidden=\"true\"]')) continue;\n    const cs = getComputedStyle(f);\n    const surface = background(f.parentElement);\n    const fill = over(rgba(cs.backgroundColor), surface);\n    const edge = parseFloat(cs.borderTopWidth) >= 1 ? over(rgba(cs.borderTopColor), surface) : null;\n    const bound = Math.max(ratio(fill, surface), edge ? ratio(edge, surface) : 0);\n    out.push({ view, kind: 'field edge', text: (f.id || f.name || f.tagName).slice(0, 40), where: path(f), ratio: +bound.toFixed(2),\n      need: 3, disabled: disabled(f), fg: (edge || fill).slice(0, 3).map(Math.round), bg: surface.slice(0, 3).map(Math.round) });\n  }\n  for (const svg of document.querySelectorAll('body svg.lucide')) {\n    if (!visible(svg) || svg.closest('[aria-hidden=\"true\"] [aria-hidden=\"true\"]')) continue;\n    const cs = getComputedStyle(svg);\n    const fg = rgba(cs.stroke !== 'none' ? cs.stroke : cs.color); fg[3] *= opacityOf(svg);\n    const bg = background(svg);\n    const label = (svg.closest('[aria-label]') || {}).getAttribute?.('aria-label') || [...svg.classList].find((c) => c.startsWith('lucide-')) || 'icon';\n    const key = path(svg) + '|' + label;\n    if (seen.has(key)) continue;\n    seen.add(key);\n    out.push({ view, kind: 'icon', text: label.slice(0, 40), where: path(svg), ratio: +ratio(over(fg, bg), bg).toFixed(2), need: 3,\n      disabled: disabled(svg), fg: over(fg, bg).slice(0, 3).map(Math.round), bg: bg.slice(0, 3).map(Math.round) });\n  }\n  // A focus indicator (2.4.7, read 3:1 under 1.4.11): whatever the focused state adds over the control's look at\n  // rest, an outline, a ring (the widest coloured box shadow) or a changed border, against the surface around it;\n  // the strongest of them is the indicator.\n  if (rings && window.__lookRest) {\n    const shadowColors = (value) => {\n      if (!value || value === 'none') return [];\n      return value.split(/,(?![^(]*\\))/).map((part) => {\n        const m = part.match(/(rgba?\\([^)]*\\)|oklch\\([^)]*\\)|oklab\\([^)]*\\)|lab\\([^)]*\\)|color\\([^)]*\\)|#[0-9a-f]{3,8})/i);\n        if (!m) return null;\n        const nums = part.replace(m[1], '').trim().split(/\\s+/).map(parseFloat).filter((x) => !isNaN(x));\n        const spread = nums.length >= 4 ? nums[3] : 0, blur = nums[2] || 0;\n        return (spread > 0 || blur > 0) ? { c: rgba(m[1]), spread } : null;\n      }).filter((x) => x && x.c[3] > 0).sort((a, b) => b.spread - a.spread);\n    };\n    for (const el of document.querySelectorAll(sel)) {\n      const rest = window.__lookRest.get(el);\n      if (!rest || !visible(el) || disabled(el) || el.closest('[aria-hidden=\"true\"]')) continue;\n      // The edge button at rest trades its glow for its focus ring in script, on a real focus (`useReveal`),\n      // which a forced :focus never reaches; test_dashboards' edge block holds that ring.\n      if (el.matches('.edge-button.edge-halo')) continue;\n      const cs = getComputedStyle(el);\n      const surface = background(el.parentElement || el);\n      const marks = [];\n      if (cs.outlineStyle !== 'none' && parseFloat(cs.outlineWidth) >= 1 && cs.outlineStyle + cs.outlineWidth + cs.outlineColor !== rest.outline) {\n        marks.push(rgba(cs.outlineColor));\n      }\n      if (cs.boxShadow !== rest.shadow) {\n        const ring = shadowColors(cs.boxShadow)[0];\n        if (ring) marks.push(ring.c);\n      }\n      if (parseFloat(cs.borderTopWidth) >= 1 && cs.borderTopColor !== rest.border) marks.push(rgba(cs.borderTopColor));\n      if (!marks.length) {\n        // A control the keyboard can reach that shows nothing when it has the focus has no indicator at all. A menu\n        // item is the exception: its menu highlights the item the focus is on (data-highlighted), which a forced\n        // :focus does not reach, and the open submenu's trigger is highlighted already. So is a control that has the\n        // focus at rest (a dialog's first, focused as it opens): its indicator is on in both looks.\n        if (el.tabIndex >= 0 && !rest.focused && !(el.getAttribute('role') || '').startsWith('menuitem')) {\n          const text = (el.getAttribute('aria-label') || el.textContent || el.id || el.tagName).trim().slice(0, 40);\n          out.push({ view, kind: 'focus ring', text, where: path(el), ratio: 0, need: 3, disabled: false,\n            fg: null, bg: surface.slice(0, 3).map(Math.round), note: 'no focus indicator' });\n        }\n        continue;\n      }\n      const shown = marks.map((c) => over(c, surface));\n      const best = shown.reduce((a, b) => (ratio(a, surface) >= ratio(b, surface) ? a : b));\n      const text = (el.getAttribute('aria-label') || el.textContent || el.id || el.tagName).trim().slice(0, 40);\n      out.push({ view, kind: 'focus ring', text, where: path(el), ratio: +ratio(best, surface).toFixed(2), need: 3,\n        disabled: false, fg: best.slice(0, 3).map(Math.round), bg: surface.slice(0, 3).map(Math.round) });\n    }\n  }\n  return out;\n}";
const SCROLLERS_JS = "() => [...document.querySelectorAll('body *')].filter((el) => {\n  const cs = getComputedStyle(el);\n  if (!/(auto|scroll)/.test(cs.overflowY) || el.scrollHeight <= el.clientHeight + 2) return false;\n  const r = el.getBoundingClientRect();\n  return r.width > 1 && r.height > 1 && cs.visibility !== 'hidden';\n}).map((el, i) => { el.dataset.lookScroll = String(i);\n  return { i, top: el.scrollTop, page: el.clientHeight, max: el.scrollHeight - el.clientHeight }; })";
const SLIDERS_JS = "() => [...document.querySelectorAll('input[type=\"range\"]')].filter((el) => {\n  const r = el.getBoundingClientRect();\n  if (r.width < 4 || r.height < 4 || r.top < 0 || r.bottom > innerHeight) return false;\n  for (let e = el; e; e = e.parentElement) {\n    const cs = getComputedStyle(e);\n    if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return false;\n  }\n  return !el.disabled;\n}).map((el) => { const r = el.getBoundingClientRect();\n  return { id: el.id, x: r.x, y: r.y, w: r.width, h: r.height, value: (el.value - el.min) / ((el.max - el.min) || 1) }; })";
const ROWS_JS = "(root) => {\n  const vis = (el) => {\n    const cs = getComputedStyle(el);\n    if (cs.display === 'none' || cs.visibility === 'hidden' || +cs.opacity === 0) return false;\n    if (cs.clip !== 'auto' || (cs.clipPath && cs.clipPath !== 'none')) return false;\n    const r = el.getBoundingClientRect();\n    return r.width > 1 && r.height > 1;\n  };\n  const ATOM = 'button,input,select,textarea,a,label,kbd,[role=\"switch\"],[data-slot=\"kbd-group\"],h3,h4,p,canvas,img,.range-slider';\n  const atoms = [], seen = new Set();\n  const inside = (el) => { for (let p = el.parentElement; p && p !== root; p = p.parentElement) if (seen.has(p)) return true; return false; };\n  for (const el of root.querySelectorAll(ATOM)) {\n    if (!vis(el) || inside(el)) continue;\n    seen.add(el);\n    const r = el.getBoundingClientRect();\n    atoms.push([r.top, r.bottom, (el.innerText || el.id || el.tagName).trim().slice(0, 24)]);\n  }\n  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);\n  for (let n = walker.nextNode(); n; n = walker.nextNode()) {\n    const el = n.parentElement;\n    if (!n.textContent.trim() || !el || !vis(el) || seen.has(el) || inside(el)) continue;\n    const range = document.createRange(); range.selectNodeContents(n);\n    let r = range.getBoundingClientRect();\n    // A block of text alone is measured by its line box, as the other atoms are by their boxes: the glyphs'\n    // own box is the font's ascent and descent, which differ between fonts (DejaVu's sits 3 px inside its line).\n    const cs = getComputedStyle(el);\n    if (!el.childElementCount && cs.display !== 'inline') {\n      const b = el.getBoundingClientRect();\n      r = { top: b.top + parseFloat(cs.borderTopWidth) + parseFloat(cs.paddingTop),\n            bottom: b.bottom - parseFloat(cs.borderBottomWidth) - parseFloat(cs.paddingBottom),\n            height: b.height };\n    }\n    if (r.height > 0) atoms.push([r.top, r.bottom, n.textContent.trim().slice(0, 24)]);\n  }\n  atoms.sort((a, b) => a[0] - b[0]);\n  const rows = [];\n  for (const [top, bottom, text] of atoms) {\n    const row = rows[rows.length - 1];\n    if (row && top < row[1] - 2) { row[1] = Math.max(row[1], bottom); row[2].push(text); }\n    else rows.push([top, bottom, [text]]);\n  }\n  const box = root.getBoundingClientRect();\n  return { top: box.top, bottom: box.bottom, rows };\n}";
const CARD_JS = "() => {\n  const card = document.querySelector('[data-slot=\"card\"]');\n  const list = card && card.querySelector('[role=\"tablist\"]');\n  const panel = card && card.querySelector('[role=\"tabpanel\"]:not([hidden])');\n  const items = panel ? [...panel.querySelectorAll('[data-slot=\"accordion-item\"]')] : [];\n  const scroller = panel && panel.querySelector('[data-slot=\"card-content\"]');\n  if (!list || !items.length || (scroller && scroller.scrollHeight > scroller.clientHeight + 1)) return null;\n  const c = card.getBoundingClientRect();\n  return { above: Math.round(list.getBoundingClientRect().top - c.top),\n           under: Math.round(c.bottom - items[items.length - 1].getBoundingClientRect().bottom) };\n}";
const HOLD_POOR = "() => { const c = window.stream_client = window.stream_client || {}; c.__lookConn = c.connection;\n  Object.defineProperty(c, 'connection', { configurable: true, get: () => 'poor', set: (v) => { c.__lookConn = v; } });\n  window.dispatchEvent(new Event('selkies-stream-stats')); }";
const LET_GO_POOR = "() => { const c = window.stream_client; if (!c) return; const v = c.__lookConn; delete c.connection;\n  c.connection = v; window.dispatchEvent(new Event('selkies-stream-stats')); }";

// The notices every layout shows over the stream, one at a time: an upload's
// toasts, the transport advice with its action, a warning, and (on a touch
// client) a document to print.
const TOASTS: readonly (readonly [string, string, boolean])[] = [
    ['upload', "{type: 'fileUpload', payload: {status: 'progress', fileName: 'look-progress.png', progress: 40}}", false],
    ['uploaded', "{type: 'fileUpload', payload: {status: 'end', fileName: 'look-done.png'}}", false],
    ['upload failed', "{type: 'fileUpload', payload: {status: 'error', fileName: 'look-failed.png', message: 'disk full'}}", false],
    ['warning', "{type: 'fileUpload', payload: {status: 'warning', fileName: 'look-warn', message: 'not allowed'}}", false],
    ['transport advice', "{type: 'transportAdvice', offer: 'websockets'}", false],
    ['print', "{type: 'printDocument', url: 'about:blank', name: 'look.pdf'}", true],
];

// A desktop, a touch tablet and a phone: the phone's layout is the narrow one (max-sm), with its own controls.
const CLIENTS: readonly (readonly [string, BrowserContextOptions, boolean])[] = [['desk', DESK, false], ['tablet', TOUCH, true], ['phone', PHONE, true]];
// The views a walk has to reach in each client, so a view that stops opening fails here instead of going unread.
const MUST: Record<string, readonly string[]> = {
    default: ['sidebar shut', 'sidebar', 'Video Settings', 'Shortcuts', 'files dialog', 'stats strip', 'toast upload', 'toast uploaded', 'toast upload failed', 'toast warning', 'toast transport advice', 'poor connection'],
    wish: ['toolbar', 'Video card 1', 'Resolution card 1', 'Audio card 1', 'menu', 'menu Shortcuts', 'menu Clipboard', 'Files dialog', 'gamepads panel', 'stats', 'toolbar folded', 'toast upload', 'toast uploaded',
        'toast upload failed', 'toast warning', 'toast transport advice', 'poor connection'],
    edge: ['edge button', 'fan', 'Video card 1', 'Resolution card 1', 'Audio card 1', 'menu', 'menu Shortcuts', 'menu Clipboard', 'Files dialog', 'gamepads panel', 'stats', 'edge button folded', 'toast upload',
        'toast uploaded', 'toast upload failed', 'toast warning', 'toast transport advice', 'poor connection'],
};
// A touch client's own views; its tools are more than a fan holds, so the edge fan has a second page there.
const MUST_TOUCH: Record<string, readonly string[]> = { default: ['soft keys', 'touch gamepad', 'toast print'], wish: ['soft keys', 'touch gamepad', 'toast print'], edge: ['fan page 2', 'soft keys', 'touch gamepad', 'toast print'] };

type Row = Record<string, unknown>;
const r = (v: unknown): string => JSON.stringify(v);
const call = (page: Page, fn: string, arg?: unknown): Promise<unknown> => within(page.evaluate(`(${fn})(${arg === undefined ? '' : r(arg)})`));
const rowsOf = (v: unknown): Row[] => (Array.isArray(v) ? v.filter((x): x is Row => x !== null && typeof x === 'object').map((x) => ({ ...x })) : []);

function lum(c: readonly number[]): number {
    const ch = (v: number): number => {
        const x = v / 255;
        return x <= 0.04045 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2]);
}

function ratio(a: readonly number[], b: readonly number[]): number {
    const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p);
    return (x + 0.05) / (y + 0.05);
}

/** The slider parts, pixel-sampled: the empty track and the filled part against the surface. */
async function sliders(page: Page, view: string): Promise<Row[]> {
    const boxes = rowsOf(await call(page, SLIDERS_JS));
    if (!boxes.length) return [];
    const shot = decodePng(await page.screenshot());
    const scale = shot.width / (page.viewportSize()?.width ?? shot.width);
    const px = (x: number, y: number): [number, number, number] => rgbAt(shot, Math.min(shot.width - 1, Math.max(0, Math.trunc(x * scale))), Math.min(shot.height - 1, Math.max(0, Math.trunc(y * scale))));
    const out: Row[] = [];
    for (const b of boxes) {
        const [x, y, w, h, value] = [Number(b.x), Number(b.y), Number(b.w), Number(b.h), Number(b.value)];
        const cy = y + h / 2;
        const surface = px(x + w + 3, cy);
        if (value < 0.8) out.push({ view, kind: 'slider track', text: b.id, ratio: Number(ratio(px(x + w - 3, cy), surface).toFixed(2)), need: 3 });
        if (value > 0.2) out.push({ view, kind: 'slider fill', text: b.id, ratio: Number(ratio(px(x + 3, cy), surface).toFixed(2)), need: 3 });
    }
    return out;
}

/**
 * Every control (INTERACTIVE) held in the CSS `states` through the DevTools
 * protocol while `body` runs, as a pointer over it or the keyboard on it would
 * put it; transitions are let settle first.
 */
async function forced(page: Page, states: string[], body: () => Promise<void>): Promise<void> {
    const cdp = await page.context().newCDPSession(page);
    let ids: number[] = [];
    try {
        await cdp.send('DOM.enable');
        await cdp.send('CSS.enable');
        const root = (await cdp.send('DOM.getDocument', { depth: 0 })).root.nodeId;
        ids = (await cdp.send('DOM.querySelectorAll', { nodeId: root, selector: INTERACTIVE })).nodeIds;
        for (const nodeId of ids) await cdp.send('CSS.forcePseudoState', { nodeId, forcedPseudoClasses: states });
        await sleep(400);
        await body();
    } finally {
        for (const nodeId of ids) await cdp.send('CSS.forcePseudoState', { nodeId, forcedPseudoClasses: [] }).catch(() => undefined);
        await cdp.detach().catch(() => undefined);
        await sleep(300);
    }
}

/** Scroll container `i` (as the audit marked it) to `y`, its values passed to the page as data, not code. */
const scrollTo = (page: Page, i: unknown, y: unknown): Promise<unknown> => within(page.evaluate(([index, top]) => {
    for (const el of document.querySelectorAll<HTMLElement>('[data-look-scroll]')) {
        if (el.dataset.lookScroll === String(index)) el.scrollTop = Number(top);
    }
}, [i, y] as const));

/** Audit what shows, then every scrolling container a page at a time to its end, and put each back. */
async function sweep(page: Page, view: string, state: string, found: Row[], rings = false): Promise<void> {
    const one = async (tag: string): Promise<void> => {
        found.push(...rowsOf(await call(page, AUDIT_JS, { view: tag, rings, sel: INTERACTIVE })).map((row) => ({ ...row, state })));
        if (state === 'rest') found.push(...(await sliders(page, tag)).map((row) => ({ ...row, state })));
    };
    await one(view);
    for (const sc of rowsOf(await call(page, SCROLLERS_JS))) {
        const step = Math.max(40, Math.trunc(Number(sc.page) * 0.8));
        let pos = 0;
        while (pos < Number(sc.max)) {
            pos = Math.min(Number(sc.max), pos + step);
            await scrollTo(page, sc.i, pos);
            await sleep(250);
            await one(`${view} (scrolled ${pos}px)`);
        }
        await scrollTo(page, sc.i, sc.top);
    }
}

interface Walk {
    found: Row[];
    views: string[];
    touch: boolean;
}

/** One view at rest, hovered (where a pointer hovers) and focused, scrolled through each time. */
async function audit(page: Page, view: string, w: Walk): Promise<void> {
    await sleep(500);
    w.views.push(view);
    await sweep(page, view, 'rest', w.found);
    if (!w.touch) await forced(page, ['hover'], () => sweep(page, view, 'hovered', w.found));
    await call(page, REST_JS, INTERACTIVE);
    await forced(page, ['focus', 'focus-visible'], () => sweep(page, view, 'focused', w.found, true));
}

async function waitFor(page: Page, selector: string, timeoutS = 4): Promise<boolean> {
    try {
        await page.locator(selector).first().waitFor({ state: 'visible', timeout: timeoutS * 1000 });
        return true;
    } catch {
        return false;
    }
}

async function noticeViews(page: Page, tag: string, w: Walk): Promise<void> {
    for (const [name, message, touchOnly] of TOASTS) {
        if (touchOnly && !w.touch) continue;
        await within(page.evaluate(`window.postMessage(${message}, location.origin)`));
        if (await waitFor(page, '[data-sonner-toast]')) await audit(page, `${tag} toast ${name}`, w);
        await within(page.evaluate("document.querySelectorAll('[data-sonner-toast] [data-close-button]').forEach((b) => b.click())"));
        for (let i = 0; i < 20 && (await page.locator('[data-sonner-toast]').count()); i++) await sleep(200);
    }
    await call(page, HOLD_POOR);
    if (await waitFor(page, '.connection-indicator')) await audit(page, `${tag} poor connection`, w);
    await call(page, LET_GO_POOR);
}

/** Open a dialog by `opener`, audit it, and close it. */
async function dialogView(page: Page, opener: Locator, tag: string, name: string, w: Walk): Promise<void> {
    if (!(await opener.count())) return;
    await opener.first().click();
    if (await waitFor(page, '[role="dialog"]')) await audit(page, `${tag} ${name} dialog`, w);
    await page.keyboard.press('Escape');
    await sleep(500);
}

async function sidebarViews(page: Page, tag: string, w: Walk): Promise<void> {
    await sidebarClose(page);
    await audit(page, `${tag} sidebar shut`, w);
    await sidebarOpen(page);
    await audit(page, `${tag} sidebar`, w);
    const headers = page.locator('.sidebar-section-header');
    // Their own text: a header renders it upper-cased.
    const names: string[] = [];
    for (let i = 0; i < (await headers.count()); i++) names.push(((await headers.nth(i).textContent()) ?? '').trim());
    for (const name of names) {
        await sidebarSection(page, name, true);
        const content = page.locator('.sidebar-section-header[aria-expanded=true] + .sidebar-section-content').first();
        if (await content.count()) await content.scrollIntoViewIfNeeded();
        await audit(page, `${tag} ${name}`, w);
        await dialogView(page, page.locator('#files-content button:has-text("Download Files")'), tag, 'files', w);
        await dialogView(page, page.locator('#apps-content button'), tag, 'apps', w);
        const toggle = page.locator('#statsStripToggle');
        if ((await toggle.count()) && (await toggle.first().isVisible())) {
            await toggle.click();
            await sidebarClose(page);
            if (await waitFor(page, '.stream-strip')) await audit(page, `${tag} stats strip`, w);
            await sidebarSection(page, name, true);
            await page.locator('#statsStripToggle').click();
        }
        const pad = page.locator('#touchGamepadToggle');
        if (w.touch && (await pad.count())) {
            await pad.click();
            await sidebarClose(page);
            await sleep(1000);
            await audit(page, `${tag} touch gamepad`, w);
            await sidebarSection(page, name, true);
            await page.locator('#touchGamepadToggle').click();
        }
        await sidebarSection(page, name, false);
    }
    if (w.touch) {
        const toggle = page.locator('.sidebar .key-palette-toggle');
        if (await toggle.count()) {
            await toggle.first().click();
            await page.locator('.sidebar-mobile-key-actions').first().scrollIntoViewIfNeeded();
            await audit(page, `${tag} soft keys`, w);
            await toggle.first().click();
        }
    }
    await sidebarClose(page);
}

/** Each tooltip of the controls `triggers` matches, hovered in turn. */
async function tooltipViews(page: Page, triggers: string, tag: string, w: Walk): Promise<void> {
    const tips = page.locator(triggers);
    for (let i = 0; i < (await tips.count()); i++) {
        const label = (await tips.nth(i).getAttribute('aria-label')) ?? String(i);
        await tips.nth(i).hover();
        if (await waitFor(page, '[data-slot="tooltip-content"]', 3)) await audit(page, `${tag} tooltip ${label}`, w);
        await page.mouse.move(700, 700);
        await sleep(500);
    }
}

/**
 * A bar layout's views: the toolbar, or the edge button at rest and its fan,
 * page by page, with each tooltip; then what both open from their buttons, the
 * settings card's cards, the menu and its submenus and dialogs, the floating
 * panels, the stats, the soft keys, and the bar folded away.
 */
async function barViews(page: Page, tag: string, w: Walk): Promise<void> {
    const edge = await isEdge(page);
    if (edge) {
        await barCloseMenus(page);
        await edgeCloseFan(page);
        await page.mouse.move(700, 700);
        await audit(page, `${tag} edge button`, w);
        await edgeOpenFan(page);
        for (let number = 1; number <= 8; number++) {
            await audit(page, `${tag} fan${number > 1 ? ` page ${number}` : ''}`, w);
            if (!w.touch) await tooltipViews(page, `${EDGE_FAN} [data-slot="tooltip-trigger"]`, tag, w);
            const more = page.locator(EDGE_MORE);
            if (!(await more.count())) break;
            // Pressed through the page, so each page is read where something covers the dots button.
            await more.first().dispatchEvent('click');
            await sleep(700);
            if ((await edgeFanPage(page))[0] === 1) break;
        }
    } else {
        await barShow(page);
        await audit(page, `${tag} toolbar`, w);
        if (!w.touch) await tooltipViews(page, '.toolbar [data-slot="tooltip-trigger"]', tag, w);
    }
    if (await openBarSettingsTab(page, 'Video')) {
        for (const tab of (await page.locator('[role="tab"]').allInnerTexts()).map((t) => t.trim())) {
            if (!(await openBarSettingsTab(page, tab))) continue;
            const triggers = page.locator('[role="tabpanel"] [data-slot="accordion-trigger"]');
            for (let i = 0; i < (await triggers.count()); i++) {
                if ((await triggers.nth(i).getAttribute('aria-expanded')) !== 'true') {
                    await triggers.nth(i).click();
                    await sleep(400);
                }
                await audit(page, `${tag} ${tab} card ${i + 1}`, w);
            }
        }
    }
    await barCloseMenus(page);
    await page.keyboard.press('Escape');
    let menu = await barTool(page, 'button[aria-label="Menu"]');
    if (menu !== null) {
        await menu.click();
        await sleep(600);
        await audit(page, `${tag} menu`, w);
        const subs = await page.locator('[role="menu"] [aria-haspopup="menu"]').count();
        const dialogs = (await page.locator('[role="menu"] [role="menuitem"]:not([aria-haspopup])').allInnerTexts()).map((t) => t.trim());
        await page.keyboard.press('Escape');
        for (let i = 0; i < subs; i++) {
            // One submenu at a time, from a freshly opened menu: an open one covers the triggers under it.
            menu = await barTool(page, 'button[aria-label="Menu"]');
            await menu?.click();
            await sleep(500);
            const sub = page.locator('[role="menu"] [aria-haspopup="menu"]').nth(i);
            const name = (await sub.innerText()).trim();
            await sub.focus();
            await page.keyboard.press('ArrowRight');
            await sleep(700);
            await audit(page, `${tag} menu ${name}`, w);
            await barCloseMenus(page);
        }
        for (const name of dialogs) {
            if (name === 'Add a second screen') continue;
            menu = await barTool(page, 'button[aria-label="Menu"]');
            await menu?.click();
            await sleep(500);
            await page.locator(`[role="menu"] [role="menuitem"]:has-text("${name}")`).first().click();
            if (await waitFor(page, '[role="dialog"]')) await audit(page, `${tag} ${name} dialog`, w);
            await closeDialogs(page);
        }
    }
    // The floating panels and the stats, each opened by its own button and closed by it again.
    for (const [sel, name] of [['button[aria-label="Gamepads"]', 'gamepads panel'], ['.stats-button', 'stats'], ['.touch-gamepad-button', 'touch gamepad']] as const) {
        const button = await barTool(page, sel);
        if (button === null) continue;
        await button.click();
        await sleep(1000);
        await audit(page, `${tag} ${name}`, w);
        await barClosePanel(page, sel);
    }
    if (w.touch) {
        const keys = await barTool(page, '.soft-keys-button');
        if (keys !== null) {
            await keys.click();
            await sleep(500);
            const toggle = page.locator('.key-palette-toggle').first();
            if (await toggle.count()) {
                await toggle.click();
                await sleep(400);
            }
            await audit(page, `${tag} soft keys`, w);
            await (await barTool(page, '.soft-keys-button'))?.click();
            await sleep(400);
        }
    }
    // The toolbar folds by its own button, the edge button by Ctrl+Shift+M alone.
    const hide = page.locator('.toolbar button[aria-label="Hide toolbar"]');
    if (edge || (await hide.count())) {
        await barCloseMenus(page);
        if (edge) {
            await edgeCloseFan(page);
            await within(page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)"));
        } else {
            await hide.first().click();
        }
        await sleep(800);
        await audit(page, `${tag} ${edge ? 'edge button' : 'toolbar'} folded`, w);
        await barShow(page);
        if (edge) await edgeCloseFan(page);
    }
}

async function pageFor(browser: Browser, url: string, theme: string, client: BrowserContextOptions): Promise<Page> {
    const ctx = await browser.newContext(client);
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = 'websockets'; try { localStorage.setItem('theme', '${theme}'); } catch (e) {}` });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    await waitChunk(page, 30);
    const size = client.viewport;
    if (client.hasTouch && size) await page.touchscreen.tap(Math.floor(size.width / 2), Math.floor(size.height / 2));
    await sleep(1200);
    return page;
}

async function contrastBlock(host: Host, res: Checks, layout: string): Promise<void> {
    const { url } = await host.start({ label: `contrast-${layout}`, mode: 'websockets', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: layout } });
    try {
        const browser = await launchBrowser('chromium');
        try {
            for (const theme of THEMES) {
                const found: Row[] = [];
                for (const [name, client, touch] of CLIENTS) {
                    const page = await pageFor(browser, url, theme, client);
                    const tag = `${theme} ${name}`;
                    const w: Walk = { found, views: [], touch };
                    await (layout === 'default' ? sidebarViews : barViews)(page, tag, w);
                    await noticeViews(page, tag, w);
                    await page.context().close();
                    const want = [...MUST[layout], ...(touch ? MUST_TOUCH[layout] : [])];
                    const missing = want.filter((v) => !w.views.includes(`${tag} ${v}`));
                    res.check(`${layout} ${theme} ${name}: every view opens for the audit (${w.views.length} views)`, missing.length === 0, missing);
                }
                for (const state of ['rest', 'hovered', 'focused']) {
                    const rows = found.filter((f) => f.state === state);
                    const below: Row[] = [];
                    const seen = new Set<string>();
                    for (const f of rows) {
                        const key = r([f.kind, f.text, f.where, String(f.view).split(' (scrolled')[0]]);
                        if (Number(f.ratio) < Number(f.need) && !f.disabled && !seen.has(key)) {
                            seen.add(key);
                            below.push(f);
                        }
                    }
                    const dump = process.env.LOOK_DUMP ?? '';
                    if (dump && /^\/[^\0]*$/.test(dump) && !dump.split('/').includes('..')) {
                        for (const f of below) appendFileSync(dump, `${r({ ...f, layout, theme })}\n`);
                    }
                    const kinds = [...new Set(rows.map((f) => String(f.kind)))].sort();
                    res.check(`${layout} ${theme}, ${state}: ${rows.length} texts, icons and controls read at AA (${kinds.join(', ')})`, below.length === 0 && rows.length > 100,
                        below.slice(0, 8).map((f) => [f.view, f.kind, f.text, f.ratio, (typeof f.where === 'string' ? f.where : '').slice(-50)]));
                }
            }
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

type RowBox = { top: number; bottom: number; rows: [number, number, string[]][] };

function rowBox(v: unknown): RowBox | null {
    if (v === null || typeof v !== 'object') return null;
    const rows: unknown = Reflect.get(v, 'rows');
    return {
        top: Number(Reflect.get(v, 'top')), bottom: Number(Reflect.get(v, 'bottom')),
        rows: Array.isArray(rows) ? rows.filter(Array.isArray).map((x): [number, number, string[]] => [Number(x[0]), Number(x[1]), Array.isArray(x[2]) ? x[2].map(String) : []]) : [],
    };
}

/** The gaps between consecutive rows that are neither the field gap nor the stack gap. */
function gapsOff(layout: string, rows: RowBox['rows']): unknown[] {
    const [field, stack] = RHYTHM[layout];
    const off: unknown[] = [];
    for (let i = 0; i + 1 < rows.length; i++) {
        const gap = Math.round(rows[i + 1][0] - rows[i][1]);
        if (Math.min(Math.abs(gap - field), Math.abs(gap - stack)) > SLACK) off.push([gap, rows[i][2].slice(0, 2), rows[i + 1][2].slice(0, 2)]);
    }
    return off;
}

async function spacingBlock(host: Host, res: Checks, layout: string): Promise<void> {
    const { url } = await host.start({ label: `spacing-${layout}`, mode: 'websockets', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: layout } });
    try {
        const browser = await launchBrowser('chromium');
        try {
            const page = await pageFor(browser, url, 'light', DESK);
            if (layout === 'default') {
                await sidebarOpen(page);
                const headers = page.locator('.sidebar-section-header');
                const names: string[] = [];
                for (let i = 0; i < (await headers.count()); i++) names.push((await headers.nth(i).innerText()).trim());
                const ends: Record<string, number> = {};
                for (const name of names) {
                    await sidebarSection(page, name, true);
                    const cid = await within(page.evaluate(`((n) => { const h = [...document.querySelectorAll('.sidebar-section-header')]
                        .find((x) => x.innerText.trim() === n); const c = h && h.parentElement.querySelector('.sidebar-section-content');
                        return c ? c.id : null; })(${r(name)})`));
                    if (typeof cid === 'string' && cid && !FREE_SECTIONS.includes(cid)) {
                        const data = rowBox(await within(page.evaluate(`(${ROWS_JS})(document.getElementById(${r(cid)}))`)));
                        if (data && data.rows.length) {
                            const off = gapsOff(layout, data.rows);
                            res.check(`default: ${name} keeps the rhythm between its rows`, off.length === 0, off);
                            ends[name] = Math.round(data.bottom - data.rows[data.rows.length - 1][1]);
                        }
                    }
                    await sidebarSection(page, name, false);
                }
                const values = Object.values(ends);
                const spread = values.length ? Math.max(...values) - Math.min(...values) : 0;
                res.check('default: every section ends as far under its last row', values.length > 0 && spread <= SLACK, ends);
            } else {
                await barShow(page);
                for (const tab of ['Video', 'Resolution', 'Audio']) {
                    if (!(await openBarSettingsTab(page, tab))) {
                        res.check(`${layout}: the ${tab} tab opens`, false);
                        continue;
                    }
                    const triggers = page.locator('[role="tabpanel"] [data-slot="accordion-trigger"]');
                    for (let i = 0; i < (await triggers.count()); i++) {
                        if ((await triggers.nth(i).getAttribute('aria-expanded')) !== 'true') {
                            await triggers.nth(i).click();
                            await sleep(400);
                        }
                        const body = rowBox(await within(page.evaluate(`(${ROWS_JS})([...document.querySelectorAll('[role="tabpanel"] [data-slot="accordion-content"]')].find((e) => e.getBoundingClientRect().height > 0) || document.body)`)));
                        if (body && body.rows.length) {
                            const off = gapsOff(layout, body.rows);
                            res.check(`${layout}: the ${tab} tab's card ${i + 1} keeps the rhythm between its rows`, off.length === 0, off);
                            const first = body.rows[0][0] - body.top;
                            const last = body.bottom - body.rows[body.rows.length - 1][1];
                            res.check(`${layout}: the ${tab} tab's card ${i + 1} pads its first and last rows alike`, Math.abs(first - last) <= 2 * SLACK + 2, [Math.round(first), Math.round(last)]);
                        }
                        const room = await call(page, CARD_JS);
                        if (room !== null && typeof room === 'object') {
                            const above = Number(Reflect.get(room, 'above'));
                            const under = Number(Reflect.get(room, 'under'));
                            res.check(`${layout}: the card leaves as much room under its last card as above its tabs (${tab}, ${i + 1})`, Math.abs(above - under) <= SLACK, room);
                        }
                    }
                }
            }
            await page.context().close();
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

for (const which of ['contrast-default', 'contrast-wish', 'contrast-edge', 'spacing']) {
    test(which, async () => {
        const [kind, only] = which.split('-');
        const layouts = only ? [only] : [...LAYOUTS];
        const host = new Host(suiteOf(import.meta.filename));
        const blocks: Checks[] = [];
        try {
            for (const layout of layouts) {
                const res = new Checks(`look-${kind}-${layout}`);
                blocks.push(res);
                if (kind === 'contrast') await contrastBlock(host, res, layout);
                else await spacingBlock(host, res, layout);
            }
        } finally {
            await host.close();
        }
        finishAll(blocks);
    });
}
