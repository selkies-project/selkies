/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The frame rate a decoder that cannot keep up asks the server for. A decoder
// behind for ENGAGE_S seconds in a row asks for CAP_SHARE of the frames it
// turned out, and less again while it stays behind; one keeping up asks for
// LIFT_STEP more after LIFT_S seconds, takes a rise back when it falls behind
// within REGRET_S of it, and waits twice as long before the next.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import {
  CAP_CEILING, CAP_FLOOR, CAP_SHARE, DecodePace, ENGAGE_S, LIFT_S, LIFT_STEP, REGRET_S,
} from '../../addons/selkies-web-core/lib/decode-pace.js';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [decode-pace] ${label}  ${detail}`);
}

/**
 * Runs `seconds` seconds of `fps` frames a second through `pace`, each decided at
 * `backlog` frames, `drops` of them let go; the paces its seconds returned.
 */
function run(pace, seconds, { fps = 60, backlog = 0, drops = 0 } = {}) {
  const changes = [];
  for (let s = 0; s < seconds; s++) {
    for (let f = 0; f < fps; f++) {
      pace.decided(backlog, f < drops);
      pace.decoded();
    }
    const change = pace.second();
    if (change !== undefined) changes.push(change);
  }
  return changes;
}

{
  const pace = new DecodePace();
  const changes = run(pace, 120, { backlog: 1 });
  check('a decoder keeping up asks for nothing', changes.length === 0 && pace.cap === null, JSON.stringify(changes));
}

{
  const pace = new DecodePace();
  const early = run(pace, ENGAGE_S - 1, { fps: 45, backlog: 6 });
  const engaged = run(pace, 1, { fps: 45, backlog: 6 });
  const want = Math.floor(CAP_SHARE * 45);
  check(`one behind for ${ENGAGE_S} s asks for ${CAP_SHARE} of the frames it turned out`,
    early.length === 0 && engaged.length === 1 && engaged[0] === want, `${JSON.stringify(early)} ${engaged} want ${want}`);
}

{
  const pace = new DecodePace();
  const changes = run(pace, ENGAGE_S, { fps: 50, backlog: 0, drops: 1 });
  check('frames the gate let go count as behind', changes.length === 1 && changes[0] === Math.floor(CAP_SHARE * 50),
    JSON.stringify(changes));
}

{
  const pace = new DecodePace();
  run(pace, 1, { backlog: 8 });
  run(pace, 1, { backlog: 0 });
  const changes = run(pace, 1, { backlog: 8 }).concat(run(pace, 1, { backlog: 0 }));
  check("a backlog that clears between seconds, such as a key frame's, asks for nothing",
    changes.length === 0 && pace.cap === null, JSON.stringify(changes));
}

{
  const pace = new DecodePace();
  run(pace, ENGAGE_S, { fps: 45, backlog: 6 });
  const first = pace.cap;
  const again = run(pace, ENGAGE_S, { fps: 30, backlog: 6 });
  check('one still behind at its pace asks for less again', again.length === 1 && again[0] === Math.floor(CAP_SHARE * 30)
    && again[0] < first, `${first} -> ${JSON.stringify(again)}`);
  const same = run(pace, ENGAGE_S, { fps: 40, backlog: 6 });
  check('and never for more while behind', same.length === 0 && pace.cap === again[0], JSON.stringify(same));
}

{
  const pace = new DecodePace();
  run(pace, ENGAGE_S, { fps: 5, backlog: 9 });
  check(`the pace never falls under ${CAP_FLOOR} fps`, pace.cap === CAP_FLOOR, pace.cap);
}

{
  const pace = new DecodePace();
  run(pace, ENGAGE_S, { fps: 45, backlog: 6 });
  const capped = pace.cap;
  const waiting = run(pace, LIFT_S - 1, { fps: capped, backlog: 0 });
  const rise = run(pace, 1, { fps: capped, backlog: 0 });
  const raised = Math.ceil(capped * LIFT_STEP);
  check(`${LIFT_S} s keeping up raises the pace by ${LIFT_STEP}`, waiting.length === 0 && rise[0] === raised,
    `${capped} -> ${JSON.stringify(rise)} want ${raised}`);
  const regret = run(pace, ENGAGE_S, { fps: raised, backlog: 6 });
  check(`falling behind within ${REGRET_S} s of a rise takes it back`, regret.length === 1 && regret[0] === capped,
    JSON.stringify(regret));
  const patient = run(pace, 2 * LIFT_S - 1, { fps: capped, backlog: 0 });
  const next = run(pace, 1, { fps: capped, backlog: 0 });
  check('and the next rise waits twice as long', patient.length === 0 && next.length === 1 && next[0] === raised,
    `${JSON.stringify(patient)} ${JSON.stringify(next)}`);
  run(pace, REGRET_S + 1, { fps: raised, backlog: 0 });
  const late = run(pace, ENGAGE_S, { fps: 40, backlog: 6 });
  check(`falling behind later than ${REGRET_S} s after a rise asks for a pace of its own`,
    late.length === 1 && late[0] === Math.floor(CAP_SHARE * 40), JSON.stringify(late));
}

{
  const pace = new DecodePace();
  run(pace, ENGAGE_S, { fps: 200, backlog: 6 });
  let cleared = false;
  for (let i = 0; i < 20 && !cleared; i++) {
    cleared = run(pace, 4 * LIFT_S, { fps: 60, backlog: 0 }).includes(null);
  }
  check(`a pace rising to ${CAP_CEILING} fps asks for nothing`, cleared && pace.cap === null, pace.cap);
}

{
  const pace = new DecodePace();
  run(pace, ENGAGE_S, { fps: 45, backlog: 6 });
  const capped = pace.cap;
  const still = run(pace, 3 * LIFT_S, { fps: 0 });
  check('seconds without frames, a still screen, count neither way', still.length === 0 && pace.cap === capped,
    JSON.stringify(still));
  run(pace, ENGAGE_S - 1, { fps: 30, backlog: 6 });
  run(pace, 1, { fps: 0 });
  const resumed = run(pace, 1, { fps: 30, backlog: 6 });
  check('nor break a run of seconds behind', resumed.length === 1 && resumed[0] === Math.floor(CAP_SHARE * 30),
    JSON.stringify(resumed));
}

{
  const pace = new DecodePace();
  run(pace, ENGAGE_S, { fps: 45, backlog: 6 });
  pace.reset();
  const changes = run(pace, ENGAGE_S - 1, { fps: 45, backlog: 6 });
  check('a reset asks for nothing and starts the count again', pace.cap === null && changes.length === 0,
    JSON.stringify(changes));
}

process.exitCode = failed ? 1 : 0;
