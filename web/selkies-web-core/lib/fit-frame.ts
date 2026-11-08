/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A decoded picture cut to the size its stream was configured for.
 *
 * A hardware encoder may code a larger picture than the stream's (VA-API on
 * AMD before VCN 5 pads AV1: 1920x1080 to 1920x1082, 1366x768 to 1408x768)
 * and name the stream's size in the AV1 render size, which Chromium's
 * decoders do not apply, so the canvas would take the padded size and show
 * the desktop squeezed or stretched at its edges.
 * @module
 */

/**
 * Makes the cut. A frame whose visible picture is larger than the stream's
 * size is cut to that size at the same origin and displayed at it, whatever
 * display size the decoder gave it; any other frame passes through. A
 * `visibleRect` crop copies no pixels, but WebKit draws a frame whole
 * whatever its visible rectangle or the source rectangle of the draw, so
 * where a first crop does not draw cut (probed once, on the first frame that
 * needs one) the frame is drawn at its full size onto a canvas of the
 * stream's size instead, which leaves the padding outside. A frame the
 * engine cannot cut passes through.
 *
 * Self-contained by design: the video worker embeds it by `toString()`.
 * @param drawsCrop Whether drawing honors a `visibleRect` crop;
 *   probed when not given.
 * @returns Takes a frame and the stream's width and height; closes the frame
 *   when another replaces it.
 */
export function createFrameFit(drawsCrop?: boolean): (frame: VideoFrame, width: number, height: number) => VideoFrame {
  let crops: boolean | undefined = drawsCrop;
  let pad: OffscreenCanvas | null = null;
  const probe = (): boolean => {
    try {
      const source = new OffscreenCanvas(2, 4);
      const sx = source.getContext('2d') as unknown as CanvasRenderingContext2D;
      sx.fillStyle = '#000';
      sx.fillRect(0, 0, 2, 2);
      sx.fillStyle = '#fff';
      sx.fillRect(0, 2, 2, 2);
      const whole = new VideoFrame(source, { timestamp: 0 });
      const top = new VideoFrame(whole, { visibleRect: { x: 0, y: 0, width: 2, height: 2 } });
      whole.close();
      const out = new OffscreenCanvas(2, 2).getContext('2d', { willReadFrequently: true }) as unknown as CanvasRenderingContext2D;
      out.drawImage(top, 0, 0);
      top.close();
      return out.getImageData(0, 1, 1, 1).data[0] < 128;
    } catch (e) {
      return true;
    }
  };
  return (frame: VideoFrame, width: number, height: number): VideoFrame => {
    const v = frame.visibleRect;
    if (!v || v.width < width || v.height < height || (v.width === width && v.height === height)) return frame;
    try {
      if (crops === undefined) crops = probe();
      let fit: VideoFrame;
      if (crops) {
        fit = new VideoFrame(frame, {
          visibleRect: { x: v.x, y: v.y, width, height }, displayWidth: width, displayHeight: height,
        });
      } else {
        if (!pad || pad.width !== width || pad.height !== height) pad = new OffscreenCanvas(width, height);
        const padContext = pad.getContext('2d') as unknown as CanvasRenderingContext2D;
        padContext.drawImage(frame, 0, 0, frame.displayWidth, frame.displayHeight);
        fit = new VideoFrame(pad, { timestamp: frame.timestamp, ...(frame.duration ? { duration: frame.duration } : {}) });
      }
      frame.close();
      return fit;
    } catch (e) {
      return frame;
    }
  };
}
