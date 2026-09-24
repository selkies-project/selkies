/**
 * The stops the settings sliders offer for the stream's frame rate, bitrate,
 * and CRF.
 *
 * Each of those sliders carries an index into one of these lists, so a stop
 * owns a wide band of the track instead of a pixel or two, and a thumb moved
 * by a finger or a mouse settles on a value worth choosing: a slider over
 * every frame rate from 8 to 240 changed by a frame or two on the smallest
 * move. The lists are dense where a step is worth choosing and sparse where
 * it is not.
 */

/**
 * Frame rates in frames per second, ascending: the rates displays and videos
 * run at, with the rates between them where a step of a few frames is felt.
 */
export const FRAMERATE_STOPS = [8, 10, 12, 15, 18, 20, 24, 25, 30, 36, 40, 45, 48, 50, 60, 72, 75, 80, 90, 100, 120, 144, 165, 180, 200, 240];

/**
 * CBR bitrates in kbps, ascending: sub-Mbps steps for constrained links,
 * steps of a fifth to a third up to 100 Mbps, then coarse steps to 1 Gbps.
 */
export const BITRATE_STOPS = [
    100, 250, 500, 750,
    1000, 1500, 2000, 2500, 3000, 4000, 5000, 6000, 8000, 10000, 12000, 15000,
    20000, 25000, 30000, 40000, 50000, 60000, 80000, 100000,
    150000, 200000, 300000, 400000, 500000, 750000, 1000000,
];

/** CRF values in fives, descending, so the slider's right end is the higher quality. */
export const CRF_STOPS = [50, 45, 40, 35, 30, 25, 20, 15, 10, 5];

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
