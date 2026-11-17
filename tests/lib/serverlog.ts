/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a suite reads off a server log from a mark on: the value a line held
 * there, then each later line's.
 * @module
 */

/**
 * What `pattern` captured in the log `text` that held at offset `mark`: the
 * last match before it, then every match after it.
 * @param pattern A global pattern with one group.
 * @param text The log.
 * @param mark An offset into it.
 * @returns The captures.
 */
export function logSince(pattern: RegExp, text: string, mark: number): string[] {
    const found = [...text.matchAll(pattern)].map((m): [number, string] => [m.index, m[1]]);
    return [...found.filter(([at]) => at < mark).slice(-1), ...found.filter(([at]) => at >= mark)].map(([, v]) => v);
}

// How many values an H.264 stream's frame_num takes before it wraps, as
// pixelflux logs it with each key frame that changes it.
const FRAME_NUM_RANGE = /H\.264 frame_num range: (\d+) values/g;

/**
 * The frame_num ranges the H.264 streams in the log `text` carried from offset
 * `mark` on: the one in force there, then each a key frame changed it to.
 * @param text The log.
 * @param mark An offset into it.
 * @returns The ranges.
 */
export function frameNumRanges(text: string, mark: number): number[] {
    return logSince(FRAME_NUM_RANGE, text, mark).map(Number);
}
