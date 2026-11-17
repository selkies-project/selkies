/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 1. Check all encoders, CRF/CBR, paintover, video streaming mode, 4:4:4
// colors, and CPU encoding working properly.
//
// Driven from the dashboard's own controls (the dashboard's video section,
// which the images serve): every encoder its menu offers under CBR and then
// CRF, Turbo (the video streaming mode) with paint-over on and off, 4:4:4 where
// its switch is offered, and CPU encoding. Each is verified on the decoded
// picture -- the session's own browser shows a known pattern, and its four
// quadrants must come back in the page -- and by the stream description the
// server publishes (encoder, hardware, codec, chroma), which the stats read.
import type { Page } from '@playwright/test';

import { fields, numberOr, stringOr } from '../../../web/selkies-web-core/lib/narrow.ts';
import { openVideoSettings, pickRateControl, switchOn } from '../../lib/dashboard.ts';
import { sleep } from '../../lib/env.ts';
import { callJs, chromaKept, patternMatches, type Cell } from '../lib.ts';

export const ITEM = 1;
export const TITLE = 'encoders, CRF/CBR, paint-over, Turbo, 4:4:4, CPU';
const CODEC: Record<string, string> = { h264enc: 'h264', h265enc: 'h265', vp8enc: 'vp8', vp9enc: 'vp9', av1enc: 'av1', jpeg: 'jpeg' };

// Playwright's WebKit, a GStreamer port, hands WebRTC VP8 to libwebrtc's decoder
// where GStreamer's best is vp8dec, and stamps those frames with their render
// time: zero under the stream's zero playout delay, and on no clock the codecs
// GStreamer decodes share. Its player drops each one as late and holds the frame
// before, so only the stream is checked there. Safari stamps them with the
// capture time.
const PICTURE_UNCHECKED: Record<string, string> = {
    'webkit/webrtc/vp8enc': "WebKit's GStreamer player drops the VP8 frames libwebrtc decodes as late",
};

// The longest a cleanup runs ("half a minute at most"), with a margin, and
// the low rate it is judged at (the sliders' floor, where a still screen needs it most).
const SETTLE = 40;
const LOW_RATE = '(0.1 Mbps)';
// Frames that reached the page: the WebSockets client counts the chunks it
// received; a <video> counts the frames it presented (requestVideoFrameCallback,
// which every engine has, unlike a playback-quality count of a live stream).
const FRAMES_JS = `() => {
  const v = document.querySelector('video');
  if (v && v.videoWidth > 0 && v.style.display !== 'none') {
    if (v.__itFrames === undefined && v.requestVideoFrameCallback) {
      v.__itFrames = 0;
      const cb = (now, meta) => { v.__itFrames = meta.presentedFrames; v.requestVideoFrameCallback(cb); };
      v.requestVideoFrameCallback(cb);
    }
    return v.__itFrames || 0;
  }
  return window.videoChunksReceived || 0;
}`;

type Info = Record<string, unknown>;

async function info(page: Page): Promise<Info> {
    return fields(await page.evaluate('window.stream_info || {}'));
}

/** The stream description once every field in `want` reads as asked, else the last one. */
async function described(page: Page, want: Info, timeoutS = 25): Promise<Info> {
    let got: Info = {};
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        got = await info(page);
        if (Object.entries(want).every(([k, v]) => got[k] === v)) return got;
        await sleep(500);
    }
    return got;
}

async function frames(page: Page): Promise<number> {
    return numberOr(await callJs(page, FRAMES_JS)) ?? 0;
}

async function framesWhile(page: Page, secs: number): Promise<number> {
    await frames(page);
    await sleep(100);
    const before = await frames(page);
    await sleep(secs * 1000);
    return (await frames(page)) - before;
}

const SLIDER_JS = `([id, idx]) => {
  const el = document.getElementById(id);
  if (!el) return null;
  if (idx !== null) {
    Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value').set.call(el, String(idx));
    el.dispatchEvent(new Event('input', {bubbles: true}));
    el.dispatchEvent(new Event('change', {bubbles: true}));
  }
  const label = document.querySelector(\`label[for="\${id}"]\`);
  return [Number(el.value), Number(el.max), label ? label.textContent.trim() : ''];
}`;

/** A slider's `[index, max, label]`. */
type Stop = [number, number, string];

async function readSlider(page: Page, id: string, idx: number | null): Promise<Stop | null> {
    const got = await callJs(page, SLIDER_JS, [id, idx]);
    return Array.isArray(got) ? [numberOr(got[0]) ?? 0, numberOr(got[1]) ?? 0, stringOr(got[2]) ?? ''] : null;
}

/** A dashboard slider's `[index, max, label]`, after moving it to `idx` when given. */
async function slider(page: Page, id: string, idx: number | null = null): Promise<Stop | null> {
    let got = await readSlider(page, id, idx);
    if (idx !== null) {
        await sleep(2000);
        got = await readSlider(page, id, null);
    }
    return got;
}

/** Move a slider to the first stop whose label reads `label`. */
async function sliderTo(page: Page, id: string, label: string): Promise<Stop | null> {
    const first = await readSlider(page, id, null);
    if (!first) return null;
    for (let idx = 0; idx <= first[1]; idx += 1) {
        const got = await readSlider(page, id, idx);
        if (got?.[2].includes(label)) break;
    }
    await sleep(2000);
    return readSlider(page, id, null);
}

/** Set a dashboard toggle; its state after, or null where it is not offered. */
async function toggle(page: Page, sel: string, on: boolean): Promise<boolean | null> {
    const el = page.locator(sel);
    if (!await el.count()) return null;
    if (await switchOn(el.first()) !== on) {
        await el.first().click({ timeout: 5000 });
        await sleep(1500);
    }
    return switchOn(el.first());
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    await cell.go('/pattern.html');
    const page = await cell.open();
    const first = await cell.waitPattern(page, 30);
    R.check("the stream shows the session's picture", patternMatches(first), first);
    await openVideoSettings(page, 'default');
    if (!await page.locator('#encoderSelect').count()) {
        R.check('the dashboard offers the encoder menu', false, 'no #encoderSelect');
        return;
    }
    const menu = await page.locator('#encoderSelect option').evaluateAll(
        (els) => els.map((e): [string, boolean, string] => (e instanceof HTMLOptionElement ? [e.value, e.disabled, (e.textContent ?? '').trim()] : ['', true, ''])));
    const offered = menu.filter(([, off]) => !off).map(([v]) => v);
    const encoder = fields((await cell.settings(page)).encoder);
    const allowed = Array.isArray(encoder.allowed) ? encoder.allowed.map(String) : [];
    R.check('the encoder menu offers what the server allows', offered.length > 0 && offered.every((v) => allowed.includes(v)),
        `menu ${JSON.stringify(offered)}, server ${JSON.stringify(allowed)}`);
    for (const [value, off, text] of menu) {
        if (off) R.skip(value, `the menu greys it out here: ${text}`);
    }
    // A run that stopped midway leaves its pick on the server; start from the defaults.
    await page.selectOption('#encoderSelect', 'h264enc', { timeout: 5000 });
    await pickRateControl(page, 'default', 'cbr');
    for (const enc of offered) {
        await page.selectOption('#encoderSelect', enc, { timeout: 5000 });
        // JPEG has a quality, not a rate control.
        for (const rc of enc !== 'jpeg' ? ['cbr', 'crf'] : ['quality']) {
            const picked = rc === 'quality' || await pickRateControl(page, 'default', rc);
            const codec = CODEC[enc] ?? enc;
            const d = await described(page, { codec });
            const name = `${enc} ${rc !== 'quality' ? rc.toUpperCase() : ''}`;
            const used = `(${String(d.encoder)}${d.hardware ? ', hardware' : ''})`;
            const reason = stringOr(d.encoder_reason) || 'no reason';
            const why = PICTURE_UNCHECKED[`${cell.engine}/${cell.transport}/${enc}`];
            if (why) {
                R.check(`${name}: streams ${codec} ${used}`, picked && d.codec === codec, `codec ${String(d.codec)} (${reason})`);
                R.skip(`${name}: decodes the picture`, why);
                continue;
            }
            const s = await cell.waitPattern(page, 25);
            R.check(`${name}: decodes the picture ${used}`, picked && patternMatches(s) && d.codec === codec,
                `sample ${JSON.stringify(s?.points ?? null)} codec ${String(d.codec)} (${reason})`);
        }
    }
    await page.selectOption('#encoderSelect', 'h264enc', { timeout: 5000 });
    await pickRateControl(page, 'default', 'cbr');
    await described(page, { codec: 'h264' });

    // Turbo off on a detailed still picture: under CBR at a low rate the screen gets
    // frames after it stops only while the cleanup refines it to the paint-over
    // quality, whatever the paint-over switch says, so the dashboards hide it there;
    // under CRF it does so only with paint-over on, ending no worse than without.
    // The CRF arms run at the session's own rate, which still paces WebRTC's sender
    // where the dashboard hides it. With Turbo on every frame goes out.
    const rate = await slider(page, 'videoBitrateSlider');
    await toggle(page, '#videoStreamingModeToggle', false);
    let low = await sliderTo(page, 'videoBitrateSlider', LOW_RATE);
    const lowLabel = low?.[2] ?? null;
    // Each arm starts from a settled rate and a texture of its own, not from a
    // picture another arm refined.
    await cell.go('/texture.html?motion=1&seed=3');
    await sleep(3000);
    const errors = new Map<string, number | null>();
    for (const [mode, paint, seed] of [['cbr', true, 1], ['crf', false, 2], ['crf', true, 4]] as const) {
        if (mode === 'crf' && low) {
            await slider(page, 'videoBitrateSlider', rate ? rate[0] : null);
            low = null;
        }
        await pickRateControl(page, 'default', mode);
        const t = await switchOn(page.locator('#videoStreamingModeToggle').first());
        const p = await toggle(page, '#usePaintOverQualityToggle', paint);
        await cell.go(`/texture.html?motion=1&seed=${seed}`);
        await sleep(3000);
        await cell.go(`/texture.html?seed=${seed}`);
        const stopped = Date.now();
        let after = 0;
        let quiet = 0;
        while ((Date.now() - stopped) / 1000 < SETTLE && quiet < 2) {
            const n = await framesWhile(page, 1.0);
            after += n;
            quiet = n <= 1 ? quiet + 1 : 0;
        }
        const settled = quiet >= 2 ? Math.round(10 * ((Date.now() - stopped) / 1000 - quiet)) / 10 : null;
        const error = await cell.textureError(page, seed);
        errors.set(`${mode}/${paint}`, error);
        await cell.shot(page, `${mode}-paint-over-${paint ? 'on' : 'off'}`);
        const how = `${after} frames after the stop, quiet after ${String(settled)} s; error ${String(error)} levels`;
        if (mode === 'cbr') {
            R.check(`Turbo off, CBR at ${String(lowLabel)}: a still screen is cleaned up, then sends nothing`,
                !t && p !== false && after >= 1 && settled !== null, how);
        } else if (paint) {
            R.check('Turbo off, CRF, paint-over on: a still screen is cleaned up, then sends nothing',
                !t && p === true && after >= 1 && settled !== null, how);
        } else {
            R.check('Turbo off, CRF, paint-over off: a still screen sends nothing',
                !t && p === false && settled !== null && settled <= 3, how);
        }
    }
    const on = errors.get('crf/true') ?? null;
    const off = errors.get('crf/false') ?? null;
    R.check('paint-over leaves the still screen no worse than without it',
        on !== null && off !== null && on <= off + 1, `error ${String(on)} levels with it, ${String(off)} without`);
    await pickRateControl(page, 'default', 'cbr');
    await slider(page, 'videoBitrateSlider', rate ? rate[0] : null);
    const turbo = await toggle(page, '#videoStreamingModeToggle', true);
    const still = await framesWhile(page, 3.0);
    R.check('Turbo on: every frame keeps going out on a still screen', turbo === true && still >= 10, `${still} frames in 3 s`);
    await cell.go('/pattern.html');

    // 4:4:4 keeps the one-pixel chroma columns apart that 4:2:0 averages.
    const full = await toggle(page, '#videoFullColorToggle', true);
    if (full === null) {
        R.skip('4:4:4', 'the dashboard offers no full-color switch here (server or browser cannot)');
    } else {
        let d = await described(page, { fullcolor: true });
        let s = await cell.waitPattern(page, 20);
        R.check('4:4:4 on: the stream is full color and keeps one-pixel chroma',
            full && d.fullcolor === true && chromaKept(s) === true, { info: d.fullcolor, band: s ? s.band.slice(0, 4) : null });
        await toggle(page, '#videoFullColorToggle', false);
        d = await described(page, { fullcolor: false });
        s = await cell.waitPattern(page, 20);
        R.check('4:4:4 off: back to 4:2:0', d.fullcolor === false && chromaKept(s) === false,
            { info: d.fullcolor, band: s ? s.band.slice(0, 4) : null });
    }

    // CPU encoding: the software encoder takes over wherever a GPU encoded, and
    // the GPU takes the stream back once it is off again.
    const before = await info(page);
    const cpu = await toggle(page, '#useCpuToggle', true);
    if (cpu === null) {
        R.skip('CPU encoding', `no CPU switch where nothing encodes on a GPU (${String((await info(page)).encoder)} already)`);
    } else {
        let d = await described(page, { hardware: false });
        let s = await cell.waitPattern(page, 25);
        R.check(`CPU encoding on: a software encoder (${String(d.encoder)}) streams the picture`,
            cpu && d.hardware === false && patternMatches(s), d.encoder);
        await toggle(page, '#useCpuToggle', false);
        const back = Boolean(before.hardware);
        d = await described(page, { hardware: back });
        s = await cell.waitPattern(page, 25);
        R.check(`CPU encoding off: ${String(before.encoder)} streams the picture again`,
            patternMatches(s) && Boolean(d.hardware) === back,
            `${String(before.encoder)} before, ${String(d.encoder)} after`);
    }
}
