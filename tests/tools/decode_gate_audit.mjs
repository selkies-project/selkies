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

process.exit(failed ? 1 : 0);
