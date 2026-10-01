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
 */

/**
 * Frame rates in frames per second, ascending: the rates displays and videos
 * run at, with the rates between them where a step of a few frames is felt.
 */
export const FRAMERATE_STOPS = [8, 10, 12, 15, 18, 20, 24, 25, 30, 36, 40, 45, 48, 50, 60, 72, 75, 80, 90, 100, 120, 144, 165, 180, 200, 240];

/**
 * CBR bitrates in kbps, ascending: sub-Mbps steps for constrained links,
 * every Mbps up to 100 Mbps, then coarse steps to 1 Gbps.
 */
export const BITRATE_STOPS = [
    100, 250, 500, 750,
    ...Array.from({ length: 100 }, (_, i) => (i + 1) * 1000),
    150000, 200000, 300000, 400000, 500000, 750000, 1000000,
];

/** Every CRF from 50 to 5, descending, so the slider's right end is the higher quality. */
export const CRF_STOPS = Array.from({ length: 46 }, (_, i) => 50 - i);

/**
 * The stops inside the span a server allows, in the list's order; the span's
 * lower bound alone when none is.
 * @param {number[]} stops
 * @param {number} min
 * @param {number} max
 * @returns {number[]}
 */
export function stopsWithin(stops, min, max) {
    const inside = stops.filter((v) => v >= min && v <= max);
    return inside.length ? inside : [min];
}

/**
 * The index of the stop nearest a value, so a value between stops (a server
 * default, a clamp, a choice from before the list changed) still has a place
 * on the track; a tie goes to the earlier stop.
 * @param {number[]} stops
 * @param {number} value
 * @returns {number}
 */
export function stopIndex(stops, value) {
    let nearest = 0;
    for (let i = 1; i < stops.length; i++) {
        if (Math.abs(stops[i] - value) < Math.abs(stops[nearest] - value)) nearest = i;
    }
    return nearest;
}

/**
 * The frame-rate stops with the display's own among them: the rate a stream
 * matching the display runs at, placed where it sorts and after a listed stop
 * of the same rate, so a fixed rate and following the display stay two
 * choices; `display` is its index, -1 where the display is unmeasured.
 * @param {number[]} stops
 * @param {?number} displayRate
 * @returns {{stops: number[], display: number}}
 */
export function withDisplayStop(stops, displayRate) {
    if (!displayRate) return { stops, display: -1 };
    const after = stops.findIndex((v) => v > displayRate);
    const display = after < 0 ? stops.length : after;
    return { stops: [...stops.slice(0, display), displayRate, ...stops.slice(display)], display };
}

/**
 * The slider position of a frame rate among `withDisplayStop`'s stops: the
 * display's own where the rate follows the display, else the listed stop
 * nearest the rate.
 * @param {{stops: number[], display: number}} options
 * @param {number} framerate
 * @param {boolean} follows
 * @returns {number}
 */
export function framerateStopIndex({ stops, display }, framerate, follows) {
    if (display < 0) return stopIndex(stops, framerate);
    if (follows) return display;
    const listed = stopIndex(stops.filter((_, i) => i !== display), framerate);
    return listed >= display ? listed + 1 : listed;
}
