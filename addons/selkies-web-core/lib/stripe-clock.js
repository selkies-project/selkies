/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * When a striped codec's frame can be taken as whole.
 *
 * The striped paths composite partial-height stripes, so a frame is only
 * known complete once the next frame id arrives — a capture period later.
 * Presenting on socket quiet instead puts the frame on screen in the tick its
 * stripes landed. A frame's stripes arrive in waves (the encoder's thread
 * pool, then the event loop), so quiet shorter than the widest gap seen
 * inside recent frames proves nothing; the clock tracks that gap and asks for
 * it back, decaying it per frame so a one-off wave does not hold the stream
 * back for long. Under a backlog the socket never goes quiet and the caller
 * falls back to presenting at frame-id boundaries.
 * @module
 */

/**
 * Self-contained by design: the video worker embeds this factory by
 * `toString()`, so a reference to anything in module scope would arrive
 * there minified and unbound.
 * @param {function(): number} [clock] Millisecond clock; defaults to
 *   `performance.now`.
 * @returns {{note: function(number): void, settled: function(): boolean,
 *   waveGapMs: function(): number, settleWaitMs: function(): number}}
 */
export function createStripeClock(clock) {
  // Quiet a frame must show on top of the tracked wave gap, in milliseconds,
  // and the per-frame decay of that remembered gap.
  const SETTLE_FLOOR_MS = 2;
  const WAVE_DECAY = 0.9;
  const now = clock || (() => performance.now());
  let lastArrival = -Infinity;
  let lastFrameId = -1;
  let waveGap = 0;
  let framePeak = 0;
  /**
   * The gap a frame is currently allowed to leave between its stripes: the
   * widest of recent frames, or of the one in flight.
   * @returns {number}
   */
  const waveGapMs = () => (waveGap > framePeak ? waveGap : framePeak);
  return {
    /**
     * Folds one stripe's arrival into the clock.
     * @param {number} frameId The stripe's frame id.
     */
    note(frameId) {
      const t = now();
      if (frameId === lastFrameId) {
        const gap = t - lastArrival;
        if (gap > framePeak) framePeak = gap;
      } else {
        waveGap = Math.max(framePeak, waveGap * WAVE_DECAY);
        framePeak = 0;
        lastFrameId = frameId;
      }
      lastArrival = t;
    },
    /** @returns {boolean} Whether the composite holds a whole frame. */
    settled: () => (now() - lastArrival) >= (SETTLE_FLOOR_MS + waveGapMs()),
    waveGapMs,
    /** @returns {number} Quiet to wait for before checking settled again. */
    settleWaitMs: () => SETTLE_FLOOR_MS + waveGapMs(),
  };
}
