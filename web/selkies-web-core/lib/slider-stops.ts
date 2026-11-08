/**
 * The stops the settings sliders offer for the stream's frame rate, bitrate,
 * and CRF.
 *
 * Each of those sliders carries an index into one of these lists. The frame
 * rate's is sparse, so a stop owns a wide band of the track and a thumb moved
 * by a finger or a mouse settles on a rate worth choosing: a slider over
 * every frame rate from 8 to 240 changed by a frame or two on the smallest
 * move. The CRF's and the bitrate's are single steps, since one CRF or one
 * Mbps is a difference worth choosing.
 * @module
 */

/**
 * Frame rates in frames per second, ascending: the rates displays and videos
 * run at, with the rates between them where a step of a few frames is felt.
 */
export const FRAMERATE_STOPS: number[] = [8, 10, 12, 15, 18, 20, 24, 25, 30, 36, 40, 45, 48, 50, 60, 72, 75, 80, 90, 100, 120, 144, 165, 180, 200, 240];

/**
 * CBR bitrates in kbps, ascending: sub-Mbps steps for constrained links,
 * every Mbps up to 100 Mbps, then coarse steps to 1 Gbps.
 */
export const BITRATE_STOPS: number[] = [
  100, 250, 500, 750,
  ...Array.from({ length: 100 }, (_, i) => (i + 1) * 1000),
  150000, 200000, 300000, 400000, 500000, 750000, 1000000,
];

/** Every CRF from 50 to 5, descending, so the slider's right end is the higher quality. */
export const CRF_STOPS: number[] = Array.from({ length: 46 }, (_, i) => 50 - i);

/**
 * The stops inside the span a server allows, in the list's order; the span's
 * lower bound alone when none is.
 * @param stops
 * @param min
 * @param max
 */
export function stopsWithin(stops: number[], min: number, max: number): number[] {
  const inside = stops.filter((v: number) => v >= min && v <= max);
  return inside.length ? inside : [min];
}

/**
 * The index of the stop nearest a value, so a value between stops (a server
 * default, a clamp, a choice from before the list changed) still has a place
 * on the track; a tie goes to the earlier stop.
 * @param stops
 * @param value
 */
export function stopIndex(stops: number[], value: number): number {
  let nearest = 0;
  for (let i = 1; i < stops.length; i++) {
    if (Math.abs(stops[i] - value) < Math.abs(stops[nearest] - value)) nearest = i;
  }
  return nearest;
}

/** The stops with the display's rate inserted, and that rate's index. */
export interface DisplayStops {
  stops: number[];
  display: number;
}

/**
 * The frame-rate stops with the display's own among them: the rate a stream
 * matching the display runs at, placed where it sorts and after a listed stop
 * of the same rate, so a fixed rate and following the display stay two
 * choices; `display` is its index, -1 where the display is unmeasured.
 * @param stops
 * @param displayRate
 */
export function withDisplayStop(stops: number[], displayRate: number | null): DisplayStops {
  if (!displayRate) return { stops, display: -1 };
  const after = stops.findIndex((v: number) => v > displayRate);
  const display = after < 0 ? stops.length : after;
  return { stops: [...stops.slice(0, display), displayRate, ...stops.slice(display)], display };
}

/**
 * The slider position of a frame rate among `withDisplayStop`'s stops: the
 * display's own where the rate follows the display, else the listed stop
 * nearest the rate.
 * @param options
 * @param framerate
 * @param follows
 */
export function framerateStopIndex(options: DisplayStops, framerate: number, follows: boolean): number {
  const { stops, display } = options;
  if (display < 0) return stopIndex(stops, framerate);
  if (follows) return display;
  const listed = stopIndex(stops.filter((_: number, i: number) => i !== display), framerate);
  return listed >= display ? listed + 1 : listed;
}
