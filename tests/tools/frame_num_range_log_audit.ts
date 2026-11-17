/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The frame_num range a suite reads off the server log.
 *
 * pixelflux logs the frame_num range an H.264 key frame carries each time it
 * changes (`frame_num_range_line` in crates/pixelflux/src/encoders/reference.rs),
 * and the reference-invalidation suite reads the ranges a load ran on from
 * those lines (`frameNumRanges` in tests/lib/serverlog.ts): the one in force when
 * the load began, then each a later key frame changed it to. A VA-API session
 * changes it within one stream, from the 65536 values of the set pixelflux
 * writes to the 128 of the one radeonsi writes.
 * @module
 */
import { frameNumRanges } from '../lib/serverlog.ts';

// The lines as pixelflux prints them, beside the lines around them in a log.
const LOG = '[X11] Stream settings active -> Res: 1280x720 | FPS: 60.0 | Encoder: VAAPI | Mode: H264\n'
    + '[pixelflux] H.264 frame_num range: 65536 values.\n'
    + "INFO:webrtc:Display 'primary' streaming\n";
const LATER = '[pixelflux] H.264 frame_num range: 128 values.\n';

let failed = 0;
let passed = 0;

function check(name: string, ok: boolean, detail: unknown): void {
    if (ok) passed++;
    else failed++;
    process.stdout.write(`${ok ? 'PASS' : 'FAIL'}  [frame-num-range-log] ${name}  ${JSON.stringify(detail)}\n`);
}

const same = (a: number[], b: number[]): boolean => a.length === b.length && a.every((v, i) => v === b[i]);

const text = LOG + LATER;
check('the range in force at the mark comes first, then each change after it',
    same(frameNumRanges(text, LOG.length), [65536, 128]), frameNumRanges(text, LOG.length));
check('a mark past every line still names the range in force',
    same(frameNumRanges(text, text.length), [128]), frameNumRanges(text, text.length));
check('a mark before every line names each range the log carries',
    same(frameNumRanges(text, 0), [65536, 128]), frameNumRanges(text, 0));
const first = LOG.split('\n')[0];
check('a log naming no range names none', same(frameNumRanges(first, 0), []), frameNumRanges(first, 0));
process.stdout.write(`[frame-num-range-log] ${passed}/${passed + failed} passed\n`);
process.exitCode = failed ? 1 : 0;
