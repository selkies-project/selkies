/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Which decoded frames are cut to the stream's size, and how. A stand-in
// VideoFrame takes the WebCodecs crop: the visible rectangle given, the
// display size given or else scaled from the source's. A stand-in
// OffscreenCanvas records what is drawn on it, and draws a frame either
// cut to its visible rectangle or whole (as WebKit does), for the probe.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { createFrameFit } from '../../web/selkies-web-core/lib/fit-frame.ts';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [fit-frame] ${label}  ${detail}`);
}

/** Whether the stand-in engine draws a frame cut to its visible rectangle. */
let engineCrops = true;

class FakeCanvas {
  constructor(width, height) {
    this.width = width;
    this.height = height;
    this.draws = [];
    this.rows = [];
  }

  getContext() {
    const canvas = this;
    return {
      fillStyle: '',
      fillRect(x, y, w, h) { for (let r = y; r < y + h; r++) canvas.rows[r] = this.fillStyle; },
      drawImage(image, ...at) {
        canvas.draws.push([image, ...at]);
        if (!image.rows) return;
        const shown = engineCrops ? image.visibleRect.height : image.codedHeight;
        for (let r = 0; r < canvas.height; r++) {
          canvas.rows[r] = image.rows[image.visibleRect.y + Math.floor(r * shown / canvas.height)];
        }
      },
      getImageData(x, y) { return { data: [canvas.rows[y] === '#fff' ? 255 : 0] }; },
    };
  }
}

class FakeFrame {
  constructor(src, init = {}) {
    if (src instanceof FakeFrame) {
      if (globalThis.cropRefused) throw new TypeError('crop refused');
      const v = init.visibleRect || src.visibleRect;
      Object.assign(this, { timestamp: src.timestamp, duration: src.duration, codedWidth: src.codedWidth,
        codedHeight: src.codedHeight, rows: src.rows });
      this.visibleRect = { ...v };
      this.displayWidth = init.displayWidth ?? Math.round(v.width * src.displayWidth / src.visibleRect.width);
      this.displayHeight = init.displayHeight ?? Math.round(v.height * src.displayHeight / src.visibleRect.height);
    } else if (src instanceof FakeCanvas) {
      Object.assign(this, { timestamp: init.timestamp, duration: init.duration, codedWidth: src.width,
        codedHeight: src.height, displayWidth: src.width, displayHeight: src.height, rows: [...src.rows], canvas: src });
      this.visibleRect = { x: 0, y: 0, width: src.width, height: src.height };
    } else {
      Object.assign(this, src);
      this.visibleRect = { ...src.visibleRect };
    }
    this.closed = false;
  }

  close() { this.closed = true; }
}
globalThis.VideoFrame = FakeFrame;
globalThis.OffscreenCanvas = FakeCanvas;

const decoded = (coded, visible, display) => new FakeFrame({
  timestamp: 42, duration: 16666, codedWidth: coded[0], codedHeight: coded[1],
  visibleRect: { x: visible[0], y: visible[1], width: visible[2], height: visible[3] },
  displayWidth: (display || visible.slice(2))[0], displayHeight: (display || visible.slice(2))[1],
});
const shape = (f) => `${f.visibleRect.x},${f.visibleRect.y} ${f.visibleRect.width}x${f.visibleRect.height} `
  + `shown ${f.displayWidth}x${f.displayHeight}`;

{
  const fit = createFrameFit(true);
  for (const [coded, w, h] of [[[1920, 1082], 1920, 1080], [[1408, 768], 1366, 768]]) {
    const f = decoded(coded, [0, 0, ...coded]);
    const g = fit(f, w, h);
    check(`a ${coded.join('x')} picture of a ${w}x${h} stream is cut to the stream`,
      g !== f && shape(g) === `0,0 ${w}x${h} shown ${w}x${h}` && g.timestamp === 42, shape(g));
    check('and the padded frame is closed', f.closed && !g.closed);
  }

  let f = decoded([1920, 1088], [0, 0, 1920, 1080]);
  let g = fit(f, 1920, 1080);
  check('a frame whose decoder already cropped to the stream passes through', g === f && !f.closed, shape(g));

  f = decoded([1280, 720], [0, 0, 1280, 720]);
  g = fit(f, 1920, 1080);
  check('a smaller picture (a resize in flight) passes through', g === f && !f.closed, shape(g));

  f = decoded([1920, 1082], [0, 0, 1920, 1082], [1920, 1080]);
  g = fit(f, 1920, 1080);
  check('a decoder showing the render size still gets its rows cut, shown at the stream size',
    shape(g) === '0,0 1920x1080 shown 1920x1080', shape(g));

  f = decoded([1936, 1096], [8, 8, 1920, 1082]);
  g = fit(f, 1920, 1080);
  check('the crop keeps the visible origin', shape(g) === '8,8 1920x1080 shown 1920x1080', shape(g));

  globalThis.cropRefused = true;
  f = decoded([1920, 1082], [0, 0, 1920, 1082]);
  g = fit(f, 1920, 1080);
  globalThis.cropRefused = false;
  check('a frame the engine will not crop passes through open', g === f && !f.closed, shape(g));
}

{
  const fit = createFrameFit(false);
  const f = decoded([1920, 1082], [0, 0, 1920, 1082]);
  const g = fit(f, 1920, 1080);
  const draw = g.canvas && g.canvas.draws[0];
  check('where a crop draws whole, the frame is drawn at its full size on a canvas of the stream size',
    g.canvas && g.canvas.width === 1920 && g.canvas.height === 1080 && draw[0] === f
      && draw.slice(1).join() === '0,0,1920,1082', draw && draw.slice(1).join());
  check('which keeps its timing and closes the padded frame',
    g.timestamp === 42 && g.duration === 16666 && f.closed && shape(g) === '0,0 1920x1080 shown 1920x1080', shape(g));
  const h = fit(decoded([1920, 1082], [0, 0, 1920, 1082]), 1920, 1080);
  check('and the canvas is kept for the next frame', h.canvas === g.canvas);
}

for (const [crops, name] of [[true, 'an engine that draws a crop cut'], [false, 'an engine that draws a frame whole (WebKit)']]) {
  engineCrops = crops;
  const fit = createFrameFit();
  const f = decoded([1920, 1082], [0, 0, 1920, 1082]);
  const g = fit(f, 1920, 1080);
  check(`the probe tells ${name}`, crops ? !g.canvas : !!g.canvas, g.canvas ? 'canvas' : 'crop');
}

process.exit(failed ? 1 : 0);
