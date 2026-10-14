/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The one count of this dashboard's views showing the stream's numbers.
 * @module
 */

/**
 * How many of this dashboard's views are showing the numbers: the section and
 * the strip over the stream. The core is told the stats shut only when the last
 * one goes.
 */
let holders = 0;

/**
 * Counts one view of the numbers in or out, telling the core when the count
 * crosses zero.
 * @param {1|-1} delta
 */
export function holdStats(delta) {
  const before = holders;
  holders += delta;
  if ((before === 0) !== (holders === 0)) {
    window.postMessage({ type: "statsOpen", open: holders > 0 }, window.location.origin);
  }
}
