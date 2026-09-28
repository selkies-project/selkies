/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the client's decoder does with each frame when it cannot keep up. A
// decoder that lets a frame go breaks everything predicting from it, so the
// gate has to know which repair is available: where the encoder says what each
// frame predicts from, the frame is reported lost and the encoder predicts past
// it; where it says nothing, only a key frame repairs the stream. A run of
// drops the encoder never predicts past ends in that request too.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import {
  DecodeGate, LOST_MEMORY, LOST_RECOVERY_MS, OVERLOAD_HOLD_MS, OVERLOAD_QUEUE,
} from '../../addons/selkies-web-core/lib/decode-gate.js';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [decode-gate] ${label}  ${detail}`);
}

/** A gate on a clock the test moves, with `tick` to advance it. */
function makeGate() {
  const clock = { now: 1000 };
  const gate = new DecodeGate({ now: () => clock.now });
  gate.configured();
  return { gate, tick: (ms) => { clock.now += ms; } };
}

const BACKED_UP = OVERLOAD_QUEUE + 1;
const KEEPING_UP = 0;
// A tracked frame names the frame before it; an untracked one names itself.
const tracked = (gate, id, queue) => gate.decide(false, id, id - 1, queue);
const untracked = (gate, id, queue) => gate.decide(false, id, id, queue);

/** A gate holding a key frame, with the backlog already standing past the hold. */
function stalled({ track = true } = {}) {
  const { gate, tick } = makeGate();
  gate.decide(true, 0, 0, KEEPING_UP);
  (track ? tracked : untracked)(gate, 1, BACKED_UP);
  tick(OVERLOAD_HOLD_MS + 1);
  return { gate, tick };
}

{
  const { gate } = makeGate();
  check('a fresh decoder asks for a key frame', tracked(gate, 1, KEEPING_UP) === 'no_key');
  check('and takes the key frame it asked for', gate.decide(true, 2, 2, KEEPING_UP) === 'decode');
  check('then decodes what predicts from it', tracked(gate, 3, KEEPING_UP) === 'decode');
}

{
  const { gate, tick } = makeGate();
  gate.decide(true, 0, 0, KEEPING_UP);
  check('a backlog within the hold is decoded through',
        tracked(gate, 1, BACKED_UP) === 'decode' && tracked(gate, 2, BACKED_UP) === 'decode');
  tick(OVERLOAD_HOLD_MS + 1);
  check('a backlog standing past the hold loses the frame, not a key frame',
        tracked(gate, 3, BACKED_UP) === 'lost');
  check('and what predicts from the lost frame is lost too, without a key frame',
        gate.decide(false, 4, 3, KEEPING_UP) === 'lost');
  check('while the frame the encoder predicted past it with decodes',
        gate.decide(false, 5, 2, KEEPING_UP) === 'decode');
  check('a backlog that clears is forgotten, so the next stall waits the hold again',
        tracked(gate, 6, BACKED_UP) === 'decode');
}

{
  const { gate } = stalled({ track: false });
  check('a stream whose encoder names nothing asks for a key frame instead',
        untracked(gate, 2, BACKED_UP) === 'overload');
  check('and holds everything until it arrives',
        untracked(gate, 3, KEEPING_UP) === 'no_key' && tracked(gate, 4, KEEPING_UP) === 'no_key');
  check('the key frame reopens it', gate.decide(true, 5, 5, KEEPING_UP) === 'decode'
        && tracked(gate, 6, KEEPING_UP) === 'decode');
}

{
  const { gate, tick } = stalled();
  check('the stalled frame is the one reported lost', tracked(gate, 2, BACKED_UP) === 'lost');
  let chained = 0, last = 'lost';
  for (let id = 3; id < 40 && last === 'lost'; id++) {
    tick(LOST_RECOVERY_MS / 8);
    last = gate.decide(false, id, id - 1, KEEPING_UP);
    if (last === 'lost') chained++;
  }
  check('frames predicting from a lost one are dropped while the encoder catches up',
        chained > 0, chained);
  check('but a stream still predicting from it past the recovery window asks for a key frame',
        last === 'overload', last);
}

{
  const { gate, tick } = stalled();
  const lost = [];
  for (let id = 2; id < LOST_MEMORY + 60; id++) {
    tick(1);
    if (tracked(gate, id, BACKED_UP) === 'lost') lost.push(id);
  }
  check('the memory of lost ids is bounded', lost.length > LOST_MEMORY, lost.length);
  check('an id past that memory is forgotten, since nothing can predict from it',
        gate.decide(false, 5000, lost[0], KEEPING_UP) === 'decode', lost[0]);
}

{
  const { gate } = stalled();
  tracked(gate, 2, BACKED_UP);
  gate.decide(true, 3, 3, KEEPING_UP);
  check('a key frame clears what was lost',
        gate.decide(false, 4, 2, KEEPING_UP) === 'decode');
}

{
  // Frame ids are 16-bit, and an infinite GOP sends no key frame to clear what was lost.
  const { gate, tick } = stalled();
  tracked(gate, 2, BACKED_UP);
  gate.decide(false, 3, 2, KEEPING_UP);
  check('a frame the encoder predicted past a loss with decodes',
        gate.decide(false, 4, 1, KEEPING_UP) === 'decode');
  let id = 5, last = 'decode';
  for (; id <= 0x10000 + 4 && last === 'decode'; id++) {
    tick(16);
    last = gate.decide(false, id & 0xFFFF, (id - 1) & 0xFFFF, KEEPING_UP);
  }
  check('65536 frames later the frames naming the recurring ids of the lost ones decode too',
        last === 'decode' && id === 0x10000 + 5, `${last} at ${id - 1}`);
}

{
  const { gate } = stalled();
  check('a key frame behind a backlog past the bound is decoded in place of that backlog',
        gate.decide(true, 2, 2, BACKED_UP) === 'flush');
  check('and what predicts from it decodes', tracked(gate, 3, KEEPING_UP) === 'decode');
  check('a key frame the decoder keeps up with is decoded behind what is queued',
        gate.decide(true, 4, 4, OVERLOAD_QUEUE) === 'decode');
}

{
  // A decoder at 25 frames a second under a 60 fps stream whose encoder predicts past
  // every loss and sends a key frame twice a second (loss repair on a wrapping frame_num).
  const FRAME_MS = 1000 / 60, DECODE_MS = 40, KEY_EVERY = 30;
  const run = (flushes) => {
    const { gate, tick } = makeGate();
    let queue = 0, done = 0, lastGood = 0, peak = 0, late = 0;
    for (let id = 0; id < 60 * 20; id++) {
      tick(FRAME_MS);
      done += FRAME_MS;
      while (queue > 0 && done >= DECODE_MS) { queue--; done -= DECODE_MS; }
      if (queue === 0) done = 0;
      const key = id % KEY_EVERY === 0;
      let d = gate.decide(key, id, key ? id : lastGood, queue);
      if (d === 'flush' && !flushes) d = 'decode';
      if (d === 'flush') { queue = 0; d = 'decode'; }
      if (d === 'decode') { queue++; lastGood = id; }
      peak = Math.max(peak, queue);
      if (id >= 60 * 19) late = Math.max(late, queue);
    }
    return { peak, late };
  };
  const bound = OVERLOAD_QUEUE + Math.ceil(OVERLOAD_HOLD_MS / FRAME_MS) + 2;
  const without = run(false), withFlush = run(true);
  check('a decoder slower than the stream fell further behind at every key frame without the flush',
        without.late > 2 * bound, JSON.stringify(without));
  check('and stays within the overload bound and its hold with it',
        withFlush.peak <= bound && withFlush.late <= bound, `${JSON.stringify(withFlush)} <= ${bound}`);
}

process.exit(failed ? 1 : 0);
