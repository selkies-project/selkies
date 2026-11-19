/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The moving scene's geometry (`crates/selkies-suites/src/scene.rs`, drawn by
 * the `motion-scene` and `wl-motion-scene` programs) and the page scripts
 * that check a decoded picture against it: the frame index read off the
 * picture, that frame rebuilt from the geometry, and the sampled pixels that
 * disagree with it counted. Only the interiors of the flat regions and of the
 * checks are compared: quantization softens edges, while a picture decoded
 * against the wrong reference is wrong well inside them.
 * @module
 */

import type { Page } from '@playwright/test';

import { within } from './stream.ts';

/** Where each part of the scene sits. */
export const GEOMETRY = {
    codeBits: 16, codeSize: 48, codeGap: 8, codeX: 16, codeY: 16, bandY: 96, bandH: 64,
    check: 16, bandStep: 4, barW: 160, barStep: 12, noiseH: 240,
    background: [0x1e, 0x28, 0x78], bar: [0xdc, 0x3c, 0x28],
} as const;

/** A decoded picture this far from the frame it claims to be shows a broken reference; a clean one reads zero. */
export const CORRUPT_FRACTION = 0.02;
/** At most this far, a picture is clean. */
export const CLEAN_FRACTION = 0.005;

/** A picture checked against the frame it spells. */
export interface Checked {
    index: number;
    ambiguous: number;
    mismatch: number;
    w: number;
    h: number;
}

/** The check of a WebRTC <video>'s picture, a function of the geometry, as page source. */
export const SAMPLE_JS = `(geom) => {
  const v = document.querySelector('video');
  if (!v || v.videoWidth === 0) return null;
  const W = v.videoWidth, H = v.videoHeight;
  let c = window.__probeCanvas;
  if (!c || c.width !== W || c.height !== H) {
    c = window.__probeCanvas = document.createElement('canvas');
    c.width = W; c.height = H;
  }
  const ctx = c.getContext('2d', {willReadFrequently: true});
  ctx.drawImage(v, 0, 0, W, H);
  const px = ctx.getImageData(0, 0, W, H).data;
  const at = (x, y) => { const i = (y * W + x) * 4; return [px[i], px[i + 1], px[i + 2]]; };
  let index = 0, ambiguous = 0;
  for (let b = 0; b < geom.codeBits; b++) {
    const cx = geom.codeX + b * (geom.codeSize + geom.codeGap) + geom.codeSize / 2;
    const cy = geom.codeY + geom.codeSize / 2;
    let sum = 0, n = 0;
    for (let dy = -8; dy < 8; dy += 2) for (let dx = -8; dx < 8; dx += 2) {
      const p = at(cx + dx, cy + dy); sum += p[0] + p[1] + p[2]; n += 3;
    }
    const lum = sum / n;
    if (lum > 160) index |= (1 << b); else if (lum > 96) ambiguous++;
  }
  const period = 2 * geom.check;
  const bandOff = (index * geom.bandStep) % period;
  const barX = (index * geom.barStep) % (W - geom.barW);
  const bandEnd = geom.bandY + geom.bandH;
  const edge = 6;
  const far = (p, e, tol) => Math.abs(p[0] - e[0]) > tol || Math.abs(p[1] - e[1]) > tol || Math.abs(p[2] - e[2]) > tol;
  let bad = 0, n = 0;
  for (let y = geom.codeY + geom.codeSize + 8; y < H - geom.noiseH; y += 2) {
    for (let x = 0; x < W; x += 2) {
      const p = at(x, y);
      if (y >= geom.bandY && y < bandEnd) {
        const cx = (x + bandOff) % geom.check, cy = (y - geom.bandY) % geom.check;
        if (cx < 4 || cx >= geom.check - 4 || cy < 4 || cy >= geom.check - 4) continue;
        const white = ((Math.floor((x + bandOff) / geom.check)
                        + Math.floor((y - geom.bandY) / geom.check)) % 2) === 0;
        const lum = (p[0] + p[1] + p[2]) / 3;
        if (white ? lum < 128 : lum >= 128) bad++;
        n++;
        continue;
      }
      if (y < bandEnd + edge) continue;
      if (x >= barX - edge && x < barX + geom.barW + edge) {
        if (x < barX + edge || x >= barX + geom.barW - edge) continue;
        if (far(p, geom.bar, 56)) bad++;
      } else if (far(p, geom.background, 40)) {
        bad++;
      }
      n++;
    }
  }
  return {index, ambiguous, mismatch: bad / n, w: W, h: H};
}`;

/** The same check of whichever sink shows the picture: the <video> a full-frame stream renders through, else the canvas. */
export const PICTURE_JS = `(geom) => {
  let v = document.querySelector('video'), W = 0, H = 0;
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') {
    W = v.videoWidth; H = v.videoHeight;
  } else {
    v = [...document.querySelectorAll('canvas')].find((c) => c.width >= 640 && c.style.display !== 'none');
    if (!v) return null;
    W = v.width; H = v.height;
  }
  let c = window.__probeCanvas;
  if (!c || c.width !== W || c.height !== H) {
    c = window.__probeCanvas = document.createElement('canvas');
    c.width = W; c.height = H;
  }
  const ctx = c.getContext('2d', {willReadFrequently: true});
  ctx.drawImage(v, 0, 0, W, H);
  const px = ctx.getImageData(0, 0, W, H).data;
  const at = (x, y) => { const i = (y * W + x) * 4; return [px[i], px[i + 1], px[i + 2]]; };
  let index = 0, ambiguous = 0;
  for (let b = 0; b < geom.codeBits; b++) {
    const cx = geom.codeX + b * (geom.codeSize + geom.codeGap) + geom.codeSize / 2;
    const cy = geom.codeY + geom.codeSize / 2;
    let sum = 0, n = 0;
    for (let dy = -8; dy < 8; dy += 2) for (let dx = -8; dx < 8; dx += 2) {
      const p = at(cx + dx, cy + dy); sum += p[0] + p[1] + p[2]; n += 3;
    }
    const lum = sum / n;
    if (lum > 160) index |= (1 << b); else if (lum > 96) ambiguous++;
  }
  const period = 2 * geom.check;
  const bandOff = (index * geom.bandStep) % period;
  const barX = (index * geom.barStep) % (W - geom.barW);
  const bandEnd = geom.bandY + geom.bandH;
  const edge = 6;
  const far = (p, e, tol) => Math.abs(p[0] - e[0]) > tol || Math.abs(p[1] - e[1]) > tol || Math.abs(p[2] - e[2]) > tol;
  let bad = 0, n = 0;
  for (let y = geom.codeY + geom.codeSize + 8; y < H - geom.noiseH; y += 2) {
    for (let x = 0; x < W; x += 2) {
      const p = at(x, y);
      if (y >= geom.bandY && y < bandEnd) {
        const cx = (x + bandOff) % geom.check, cy = (y - geom.bandY) % geom.check;
        if (cx < 4 || cx >= geom.check - 4 || cy < 4 || cy >= geom.check - 4) continue;
        const white = ((Math.floor((x + bandOff) / geom.check)
                        + Math.floor((y - geom.bandY) / geom.check)) % 2) === 0;
        const lum = (p[0] + p[1] + p[2]) / 3;
        if (white ? lum < 128 : lum >= 128) bad++;
        n++;
        continue;
      }
      if (y < bandEnd + edge) continue;
      if (x >= barX - edge && x < barX + geom.barW + edge) {
        if (x < barX + edge || x >= barX + geom.barW - edge) continue;
        if (far(p, geom.bar, 56)) bad++;
      } else if (far(p, geom.background, 40)) {
        bad++;
      }
      n++;
    }
  }
  return {index, ambiguous, mismatch: bad / n, w: W, h: H};
}`;

/**
 * Check the picture `page` shows.
 * @param page The client.
 * @param script SAMPLE_JS or PICTURE_JS.
 * @returns The check, or null with no picture yet.
 */
export async function checkPicture(page: Page, script: string = PICTURE_JS): Promise<Checked | null> {
    const got: unknown = await within(page.evaluate(`(${script})(${JSON.stringify(GEOMETRY)})`));
    if (typeof got !== 'object' || got === null) return null;
    const n = (k: string): number => Number(Reflect.get(got, k));
    return { index: n('index'), ambiguous: n('ambiguous'), mismatch: n('mismatch'), w: n('w'), h: n('h') };
}

/**
 * An init script that reads the frame index off whichever sink shows the
 * picture every 100 ms, from the code strip alone, into `window.__shown` as
 * [performance.now(), Date.now(), index or -1 where unreadable]. With the
 * scene's `t0` and `interval` (motion-scene prints them), the gap between the
 * wall clock and frame index's drawing time is the age of the picture on
 * screen ({@link pictureAge}).
 */
export const SHOWN_JS = `(() => {
  const g = ${JSON.stringify(GEOMETRY)};
  const rows = [];
  window.__shown = rows;
  const W = g.codeX + g.codeBits * (g.codeSize + g.codeGap), H = g.codeY + g.codeSize;
  let c = null;
  setInterval(() => {
    let src = document.querySelector('video');
    if (!(src && src.videoWidth > 0 && src.readyState >= 2 && src.style.display !== 'none')) {
      src = [...document.querySelectorAll('canvas')].find((k) => k.width >= 640 && k.style.display !== 'none') || null;
    }
    let index = -1;
    if (src) {
      if (!c) { c = document.createElement('canvas'); c.width = W; c.height = H; }
      const ctx = c.getContext('2d', {willReadFrequently: true});
      ctx.drawImage(src, 0, 0, W, H, 0, 0, W, H);
      const px = ctx.getImageData(0, 0, W, H).data;
      index = 0;
      for (let b = 0; b < g.codeBits && index >= 0; b++) {
        const cx = g.codeX + b * (g.codeSize + g.codeGap) + g.codeSize / 2, cy = g.codeY + g.codeSize / 2;
        let sum = 0, n = 0;
        for (let dy = -8; dy < 8; dy += 2) for (let dx = -8; dx < 8; dx += 2) {
          const i = ((cy + dy) * W + cx + dx) * 4; sum += px[i] + px[i + 1] + px[i + 2]; n += 3;
        }
        const lum = sum / n;
        if (lum > 160) index |= (1 << b); else if (lum > 96) index = -1;
      }
    }
    rows.push([performance.now(), Date.now(), index]);
  }, 100);
})();`;

/**
 * How old the picture shown at wall-clock `now` was, in milliseconds: the
 * time since the scene drew frame `index`, the newest drawing of that index
 * at or before `now` (the index wraps every 2^codeBits frames).
 * @param now The page's `Date.now()` when it read the picture.
 * @param index The frame index read off it.
 * @param t0 The scene's first frame, in wall-clock milliseconds.
 * @param interval The scene's frame interval in milliseconds.
 * @returns The age, or null before the scene's first frame.
 */
export function pictureAge(now: number, index: number, t0: number, interval: number): number | null {
    const elapsed = now - t0;
    if (elapsed < 0 || index < 0) return null;
    const wrap = 2 ** GEOMETRY.codeBits;
    const due = Math.floor(elapsed / interval);
    const behind = (((due - index) % wrap) + wrap) % wrap;
    return behind * interval + (elapsed - due * interval);
}
