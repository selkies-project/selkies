/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The timestamps a decoder's chunks carry. A frame is matched to its chunk by
// the timestamp WebCodecs hands back in whole microseconds, so each stamp must
// be whole microseconds already, and no two chunks may share one: WebKit's
// worker clock reads whole milliseconds, and a burst of chunks within one
// would otherwise share it.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { createChunkStamp } from '../../addons/selkies-web-core/lib/chunk-stamp.js';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [chunk-stamp] ${label}  ${detail}`);
}

{
  // 1.005 ms is 1004.9999999999999 microseconds, of which a chunk keeps 1004.
  const stamp = createChunkStamp(() => 1.005);
  const got = stamp();
  check('a clock off whole microseconds gives the whole microseconds a chunk keeps', got === 1004, got);
}

{
  // A clock at 1 ms resolution, as WebKit's worker reads, under a burst of three chunks.
  let t = 2917;
  const stamp = createChunkStamp(() => t);
  const burst = [stamp(), stamp(), stamp()];
  t = 2962;
  const next = stamp();
  check('a burst within one clock reading gets distinct, rising stamps',
    burst[0] === 2917000 && burst[1] === 2917001 && burst[2] === 2917002, JSON.stringify(burst));
  check('and the clock moving on is followed again', next === 2962000, next);
}

{
  let t = 1000;
  const stamp = createChunkStamp(() => t);
  const first = stamp();
  t = 999;
  const back = stamp();
  check('a clock stepping back still rises', back === first + 1, `${first} -> ${back}`);
}

process.exit(failed ? 1 : 0);
