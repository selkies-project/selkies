/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Which decoded frames are cut to the stream's size, and how. A stand-in
 * VideoFrame takes the WebCodecs crop: the visible rectangle given, the
 * display size given or else scaled from the source's. A stand-in
 * OffscreenCanvas records what is drawn on it, and draws a frame either
 * cut to its visible rectangle or whole (as WebKit does), for the probe.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { createFrameFit } from '../../web/selkies-web-core/lib/fit-frame.ts';

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [fit-frame] ${label}  ${String(detail)}`);
}

/** Whether the stand-in engine draws a frame cut to its visible rectangle. */
let engineCrops = true;

/** A rectangle of a picture, as WebCodecs names it. */
interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** What a stand-in frame is made from: the fields of a decoded picture, or the options of a copy. */
interface FrameInit {
  timestamp?: number;
  duration?: number;
  codedWidth?: number;
  codedHeight?: number;
  displayWidth?: number;
  displayHeight?: number;
  visibleRect?: Rect;
}

/** The members of an `OffscreenCanvas` the fit reads. */
type CanvasLike = Pick<OffscreenCanvas, 'width' | 'height'>;

/** The members of a `VideoFrame` the fit reads, beside its visible rectangle. */
type FrameLike = Pick<VideoFrame, 'timestamp' | 'duration' | 'codedWidth' | 'codedHeight' | 'displayWidth'
  | 'displayHeight' | 'close'>;

/** A canvas recording what is drawn on it, a color per row. */
class FakeCanvas implements CanvasLike {
  width: number;
  height: number;
  /** Every `drawImage` call: the image, and where it was drawn. */
  draws: Array<{ image: FakeFrame; at: number[] }> = [];
  /** The fill color of each row. */
  rows: string[] = [];

  constructor(width: number, height: number) {
    this.width = width;
    this.height = height;
  }

  getContext() {
    const context = {
      fillStyle: '',
      fillRect: (x: number, y: number, w: number, h: number): void => {
        for (let r = y; r < y + h; r++) this.rows[r] = context.fillStyle;
      },
      drawImage: (image: FakeFrame, ...at: number[]): void => {
        this.draws.push({ image, at });
        if (!image.rows) return;
        const shown = engineCrops ? image.visibleRect.height : image.codedHeight;
        for (let r = 0; r < this.height; r++) {
          this.rows[r] = image.rows[image.visibleRect.y + Math.floor(r * shown / this.height)];
        }
      },
      getImageData: (x: number, y: number) => ({ data: [this.rows[y] === '#fff' ? 255 : 0] }),
    };
    return context;
  }
}

/** Whether the stand-in engine refuses to crop a frame. */
let cropRefused = false;

/** A frame that takes the WebCodecs crop and remembers the rows and the canvas it was made from. */
class FakeFrame implements FrameLike {
  timestamp!: number;
  duration!: number | null;
  codedWidth!: number;
  codedHeight!: number;
  displayWidth: number;
  displayHeight: number;
  visibleRect: Rect;
  /** The fill color of each row, where the frame was made from a canvas. */
  rows?: string[];
  /** The canvas the frame was made from. */
  canvas?: FakeCanvas;
  closed = false;

  constructor(src: FakeFrame | FakeCanvas | FrameInit, init: FrameInit = {}) {
    if (src instanceof FakeFrame) {
      if (cropRefused) throw new TypeError('crop refused');
      const v = init.visibleRect || src.visibleRect;
      Object.assign(this, { timestamp: src.timestamp, duration: src.duration, codedWidth: src.codedWidth,
        codedHeight: src.codedHeight, rows: src.rows });
      this.visibleRect = { ...v };
      this.displayWidth = init.displayWidth ?? Math.round(v.width * src.displayWidth / src.visibleRect.width);
      this.displayHeight = init.displayHeight ?? Math.round(v.height * src.displayHeight / src.visibleRect.height);
    } else if (src instanceof FakeCanvas) {
      Object.assign(this, { timestamp: init.timestamp, duration: init.duration, codedWidth: src.width,
        codedHeight: src.height, rows: [...src.rows], canvas: src });
      this.displayWidth = src.width;
      this.displayHeight = src.height;
      this.visibleRect = { x: 0, y: 0, width: src.width, height: src.height };
    } else {
      Object.assign(this, src);
      this.displayWidth = src.displayWidth!;
      this.displayHeight = src.displayHeight!;
      this.visibleRect = { ...src.visibleRect! };
    }
  }

  close(): void { this.closed = true; }
}
Object.assign(globalThis, { VideoFrame: FakeFrame, OffscreenCanvas: FakeCanvas });

/** The fit under audit, over stand-in frames. */
type Fit = (frame: FakeFrame, width: number, height: number) => FakeFrame;

/**
 * The module's fit as the stand-in frames take it.
 * @param drawsCrop Whether the engine draws a crop cut, or undefined to probe.
 * @returns The fit.
 */
function frameFit(drawsCrop?: boolean): Fit {
  const fit = createFrameFit(drawsCrop);
  return (frame, width, height) => fit(frame as FrameLike as VideoFrame, width, height) as FrameLike as FakeFrame;
}

const decoded = (coded: number[], visible: number[], display?: number[]): FakeFrame => new FakeFrame({
  timestamp: 42, duration: 16666, codedWidth: coded[0], codedHeight: coded[1],
  visibleRect: { x: visible[0], y: visible[1], width: visible[2], height: visible[3] },
  displayWidth: (display || visible.slice(2))[0], displayHeight: (display || visible.slice(2))[1],
});
const shape = (f: FakeFrame): string => `${f.visibleRect.x},${f.visibleRect.y} ${f.visibleRect.width}x${f.visibleRect.height} `
  + `shown ${f.displayWidth}x${f.displayHeight}`;

{
  const fit = frameFit(true);
  const padded: Array<[coded: number[], w: number, h: number]> = [[[1920, 1082], 1920, 1080], [[1408, 768], 1366, 768]];
  for (const [coded, w, h] of padded) {
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

  cropRefused = true;
  f = decoded([1920, 1082], [0, 0, 1920, 1082]);
  g = fit(f, 1920, 1080);
  cropRefused = false;
  check('a frame the engine will not crop passes through open', g === f && !f.closed, shape(g));
}

{
  const fit = frameFit(false);
  const f = decoded([1920, 1082], [0, 0, 1920, 1082]);
  const g = fit(f, 1920, 1080);
  const draw = g.canvas?.draws[0];
  check('where a crop draws whole, the frame is drawn at its full size on a canvas of the stream size',
    g.canvas?.width === 1920 && g.canvas.height === 1080 && draw?.image === f
      && draw.at.join() === '0,0,1920,1082', draw?.at.join());
  check('which keeps its timing and closes the padded frame',
    g.timestamp === 42 && g.duration === 16666 && f.closed && shape(g) === '0,0 1920x1080 shown 1920x1080', shape(g));
  const h = fit(decoded([1920, 1082], [0, 0, 1920, 1082]), 1920, 1080);
  check('and the canvas is kept for the next frame', h.canvas === g.canvas);
}

const engines: Array<[crops: boolean, name: string]> = [
  [true, 'an engine that draws a crop cut'], [false, 'an engine that draws a frame whole (WebKit)']];
for (const [crops, name] of engines) {
  engineCrops = crops;
  const fit = frameFit();
  const f = decoded([1920, 1082], [0, 0, 1920, 1082]);
  const g = fit(f, 1920, 1080);
  check(`the probe tells ${name}`, crops ? !g.canvas : !!g.canvas, g.canvas ? 'canvas' : 'crop');
}

process.exit(failed ? 1 : 0);
