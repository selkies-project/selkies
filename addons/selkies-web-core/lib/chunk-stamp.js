/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The timestamps a decoder's chunks carry.
 *
 * WebCodecs keeps a chunk's timestamp in whole microseconds and hands it back
 * on the frame decoded from it, which is how a frame is matched to its chunk
 * (the decode time and arrival the stats report). The page clock in
 * microseconds is fractional wherever `performance.now()` is not a whole
 * number of them, and WebKit's worker clock reads whole milliseconds, so a
 * burst of chunks arriving within one would share a timestamp, which WebKit
 * answers with frames timed otherwise (repeated values, stepping by a frame's
 * duration in nanoseconds). Each stamp is the clock's whole microseconds,
 * past the last one given.
 * @module
 */

/**
 * Self-contained by design: the video worker embeds this factory by
 * `toString()`, so a reference to anything in module scope would arrive
 * there minified and unbound.
 * @param {function(): number} [clock] Millisecond clock; defaults to
 *   `performance.now`.
 * @returns {function(): number} Gives the next chunk's timestamp.
 */
export function createChunkStamp(clock) {
  const now = clock || (() => performance.now());
  let last = -Infinity;
  return () => (last = Math.max(Math.trunc(now() * 1000), last + 1));
}
