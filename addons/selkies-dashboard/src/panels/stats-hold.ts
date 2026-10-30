/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The one count of the views showing the stream's numbers: a section, the
 * strip, or the floating panel. The core is told the stats shut only when
 * the last one goes.
 * @module
 */

let holders = 0;

/** Counts one view in or out, telling the core when the count crosses zero. */
export function holdStats(delta: 1 | -1): void {
	const before = holders;
	holders += delta;
	if ((before === 0) !== (holders === 0)) {
		window.postMessage({ type: "statsOpen", open: holders > 0 }, window.location.origin);
	}
}
