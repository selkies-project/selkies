/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a decoder should do with each frame that arrives, so a decoder that
 * falls behind costs the stream as little as possible.
 *
 * A decoder given more than it can decode has to let something go, and what it
 * lets go breaks the frames that predict from it. Where the encoder says what
 * each frame predicts from, the cheapest repair is to drop the frame, tell the
 * server which one went, and hold back only what predicts from it: the encoder
 * predicts past it and decoding resumes on its next frame, no key frame on the
 * wire. Where the encoder says nothing -- a stripe of a striped stream, a codec
 * whose session does not track its references -- the only repair is a key
 * frame, so the gate asks for one instead. A run of drops that the encoder
 * never predicts past is the same case and ends in that request too.
 *
 * @module
 */

/** Frames of decode backlog that count as falling behind. */
export const OVERLOAD_QUEUE = 6;
/** How long that backlog must stand before a frame is let go, in ms. */
export const OVERLOAD_HOLD_MS = 250;
/** How long frames may go undecodable before a key frame is asked for, in ms. */
export const LOST_RECOVERY_MS = 1000;
/** How many dropped frame ids are remembered. A frame predicts from one of the
 * last few its encoder produced, so an older id can never be named again. */
export const LOST_MEMORY = 64;

/**
 * @typedef {'decode'|'lost'|'no_key'|'overload'} Decision What to do with a
 *     frame: decode it, let it go and report it lost, or ask for a key frame
 *     because none has been decoded yet or because dropping more would not help.
 */

export class DecodeGate {
  /**
   * @param {object} [options]
   * @param {() => number} [options.now] Monotonic clock in ms.
   */
  constructor({ now = () => performance.now() } = {}) {
    this._now = now;
    this._lost = [];
    this._lostSince = 0;
    this._overloadSince = 0;
    this._haveKey = false;
    this._needKey = false;
  }

  /** The decoder was (re)built, so it needs a key frame before anything else. */
  configured() {
    this._haveKey = false;
    this._needKey = true;
    this._reset();
  }

  /** Nothing decoded so far is usable, so only a key frame is. */
  invalidated() {
    this._needKey = true;
  }

  _reset() {
    this._lost.length = 0;
    this._lostSince = 0;
    this._overloadSince = 0;
  }

  /**
   * @param {boolean} key Whether the frame decodes on its own.
   * @param {number} frameId The frame's own id.
   * @param {number|undefined} reference The id of the frame it predicts from;
   *     its own id, or undefined, where the encoder does not say.
   * @param {number} queueSize Frames the decoder has yet to decode.
   * @returns {Decision}
   */
  decide(key, frameId, reference, queueSize) {
    if (key) {
      this._haveKey = true;
      this._needKey = false;
      this._reset();
      return 'decode';
    }
    if (!this._haveKey || this._needKey) return 'no_key';
    const tracked = reference !== undefined && reference !== frameId;
    const now = this._now();
    if (tracked && this._lost.includes(reference)) {
      this._remember(frameId);
      if (now - this._lostSince > LOST_RECOVERY_MS) {
        this._needKey = true;
        return 'overload';
      }
      return 'lost';
    }
    if (queueSize > OVERLOAD_QUEUE) {
      if (!this._overloadSince) this._overloadSince = now;
      else if (now - this._overloadSince > OVERLOAD_HOLD_MS) {
        if (!tracked) {
          this._needKey = true;
          return 'overload';
        }
        this._remember(frameId);
        if (!this._lostSince) this._lostSince = now;
        return 'lost';
      }
      return 'decode';
    }
    this._overloadSince = 0;
    this._lostSince = 0;
    return 'decode';
  }

  _remember(frameId) {
    this._lost.push(frameId);
    if (this._lost.length > LOST_MEMORY) this._lost.shift();
  }
}
