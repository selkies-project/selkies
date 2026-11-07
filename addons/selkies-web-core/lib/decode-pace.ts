/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The frame rate a decoder that cannot keep up asks the server for.
 *
 * A decoder slower than the stream keeps a backlog, which the decode gate
 * (lib/decode-gate.ts) answers by letting frames go: each backlog frame is
 * latency on the screen, and each drop a repair on the wire. Asking the server
 * for fewer frames removes both. Once the decoder has stood behind for
 * ENGAGE_S seconds in a row, the pace is CAP_SHARE of the frames it turned out
 * over them, never under CAP_FLOOR, and falls again the same way while it
 * stays behind. After a wait of LIFT_S seconds keeping up the pace rises by
 * LIFT_STEP, and falling behind within REGRET_S of a rise takes the rise back
 * and doubles the wait before the next. A pace rising to CAP_CEILING asks for
 * nothing. A second without frames, a still screen's, counts neither way.
 * @module
 */

/** Mean frames of decode backlog over a second that count as behind. */
export const BEHIND_QUEUE = 4;
/** Seconds behind in a row before the pace falls. */
export const ENGAGE_S = 3;
/** Share of the frames the decoder turned out that the pace asks for. */
export const CAP_SHARE = 0.93;
/** The lowest pace, in frames per second. */
export const CAP_FLOOR = 10;
/** Seconds keeping up before the pace rises, doubled at each regret. */
export const LIFT_S = 20;
/** How far each rise goes. */
export const LIFT_STEP = 1.1;
/** How soon after a rise falling behind takes it back, in seconds. */
export const REGRET_S = 10;
/** A pace rising this high asks for nothing: no display the page follows runs faster. */
export const CAP_CEILING = 240;

export class DecodePace {
  /** The frames per second asked for, null for none. */
  public cap: number | null;
  private _frames: number;
  private _decisions: number;
  private _backlog: number;
  private _drops: number;
  private _behind: number;
  private _behindFrames: number;
  private _keeping: number;
  private _wait: number;
  private _second: number;
  private _raisedAt: number;
  private _beforeRise: number | null;

  constructor() {
    this.cap = null;
    this._frames = 0;
    this._decisions = 0;
    this._backlog = 0;
    this._drops = 0;
    this._behind = 0;
    this._behindFrames = 0;
    this._keeping = 0;
    this._wait = LIFT_S;
    this._second = 0;
    this._raisedAt = -Infinity;
    this._beforeRise = null;
  }

  /** A new decoder or stream, or a rate the user chose: the pace asks for nothing. */
  public reset(): void {
    this.cap = null;
    this._frames = 0;
    this._decisions = 0;
    this._backlog = 0;
    this._drops = 0;
    this._behind = 0;
    this._behindFrames = 0;
    this._keeping = 0;
    this._wait = LIFT_S;
    this._second = 0;
    this._raisedAt = -Infinity;
    this._beforeRise = null;
  }

  /**
   * A frame the gate decided on.
   * @param backlog Frames the decoder had yet to decode then.
   * @param dropped Whether the gate let it, or the backlog, go.
   */
  public decided(backlog: number, dropped: boolean): void {
    this._decisions++;
    this._backlog += backlog;
    if (dropped) this._drops++;
  }

  /** A frame the decoder turned out. */
  public decoded(): void {
    this._frames++;
  }

  /**
   * Closes a second of the stream.
   * @returns The new pace where it changed (null: ask for nothing), undefined where it stayed.
   */
  public second(): number | null | undefined {
    const now = ++this._second;
    const frames = this._frames;
    const decisions = this._decisions;
    const behind = this._drops > 0 || (decisions > 0 && this._backlog / decisions > BEHIND_QUEUE);
    this._frames = 0;
    this._decisions = 0;
    this._backlog = 0;
    this._drops = 0;
    if (!decisions) return undefined;
    const was = this.cap;
    if (behind) {
      this._keeping = 0;
      this._behindFrames += frames;
      if (++this._behind >= ENGAGE_S) {
        const pace = Math.max(CAP_FLOOR, Math.floor(CAP_SHARE * this._behindFrames / this._behind));
        this._behind = 0;
        this._behindFrames = 0;
        if (this._beforeRise !== null && now - this._raisedAt <= REGRET_S) {
          this.cap = this._beforeRise;
          this._wait *= 2;
        } else if (this.cap === null || pace < this.cap) {
          this.cap = pace;
        }
        this._beforeRise = null;
      }
    } else {
      this._behind = 0;
      this._behindFrames = 0;
      if (this.cap !== null && ++this._keeping >= this._wait) {
        this._keeping = 0;
        const raised = Math.ceil(this.cap * LIFT_STEP);
        this._beforeRise = this.cap;
        this._raisedAt = now;
        this.cap = raised >= CAP_CEILING ? null : raised;
      }
    }
    return this.cap !== was ? this.cap : undefined;
  }
}
