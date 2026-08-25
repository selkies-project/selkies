/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Whether the chosen webcam encoder is keeping up with the camera.
 *
 * A frame offered while the encoder still has one outstanding is dropped
 * rather than queued, so an encoder that cannot sustain the capture rate never
 * shows a growing queue: it shows a growing share of frames thrown away, a
 * core at full tilt, and a receiver watching a stream that lags further behind
 * every second. That share is the signal here. Deciding it from the live
 * camera is the whole point: a codec's cost depends on what the sensor is
 * showing, and a synthetic probe frame ranks codecs by how cheap its own
 * content was to encode rather than by how the camera's will be.
 * @module
 */

/** Frames the share is measured over before it can be believed. */
export const PACE_MIN_SAMPLES = 60;
/**
 * Share of offered frames the encoder may drop before it counts as too slow.
 * Every drop here is a frame the rate budget had already decided to send, so a
 * sustained sixth is a codec carrying five sixths of the camera at best -- the
 * point where a viewer sees the stutter and a core is busy producing it.
 */
export const PACE_BEHIND_RATIO = 1 / 6;

/**
 * @returns {{note: function(boolean): void, tooSlow: function(): boolean,
 *   behindRatio: function(): number, reset: function(): void}}
 */
export function createEncodePace() {
  let offered = 0;
  let behind = 0;
  return {
    /**
     * Records one frame offered to the encoder.
     * @param {boolean} wasBehind Whether it had to be dropped because the
     *   encoder was still busy with the previous one.
     */
    note(wasBehind) {
      offered++;
      if (wasBehind) behind++;
    },
    /**
     * Whether the encoder has fallen behind for a whole window. Answering true
     * starts a fresh window, so a caller that changes codec measures the new
     * one from scratch rather than from the old one's backlog.
     * @returns {boolean}
     */
    tooSlow() {
      if (offered < PACE_MIN_SAMPLES) return false;
      const slow = behind / offered > PACE_BEHIND_RATIO;
      offered = 0;
      behind = 0;
      return slow;
    },
    /** @returns {number} Share of frames dropped so far in this window. */
    behindRatio() {
      return offered ? behind / offered : 0;
    },
    /** Starts a fresh window. */
    reset() {
      offered = 0;
      behind = 0;
    },
  };
}
