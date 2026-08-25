/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// When the webcam uplink gives up on a codec. A frame offered to a busy encoder
// is dropped rather than queued, so an encoder that cannot keep up with the
// camera never grows a queue -- it drops a growing share of frames while a core
// runs flat out. That share, not a synthetic probe, is what decides.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import {
    createEncodePace,
    PACE_BEHIND_RATIO,
    PACE_MIN_SAMPLES,
} from '../../addons/selkies-web-core/lib/encode-pace.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [encode-pace] ${label}  ${detail}`);
}

/** Offers `n` frames, `behind` of them to a busy encoder. */
function offer(pace, n, behind) {
    for (let i = 0; i < n; i++) pace.note(i < behind);
}

{
    const pace = createEncodePace();
    offer(pace, PACE_MIN_SAMPLES - 1, PACE_MIN_SAMPLES - 1);
    check('a short burst of drops is not a verdict', !pace.tooSlow(),
        `${PACE_MIN_SAMPLES - 1} frames, all behind`);
}

{
    const pace = createEncodePace();
    offer(pace, PACE_MIN_SAMPLES, PACE_MIN_SAMPLES);
    check('an encoder taking nothing is too slow', pace.tooSlow());
}

{
    // Exactly at the ratio is still keeping up; past it is not.
    const pace = createEncodePace();
    offer(pace, 120, 20);
    check('dropping exactly a sixth is not yet too slow', !pace.tooSlow(), '20 of 120');
    offer(pace, 120, 21);
    check('dropping more than a sixth is', pace.tooSlow(),
        `ratio ${(21 / 120).toFixed(3)} > ${PACE_BEHIND_RATIO.toFixed(3)}`);
}

{
    // 30 fps asked for, 19 delivered: what Firefox does with a real camera.
    const pace = createEncodePace();
    offer(pace, 120, 44);
    check('a codec delivering 19 of 30 is too slow', pace.tooSlow(), 'ratio 0.367');
}

{
    // 30 fps asked for, 25 delivered: still choppy, still a busy core.
    const pace = createEncodePace();
    offer(pace, 120, 20);
    offer(pace, 60, 12);
    check('a codec delivering 25 of 30 is too slow', pace.tooSlow(), 'ratio 0.178');
}

{
    const pace = createEncodePace();
    offer(pace, 240, 8);
    check('an encoder keeping up is never demoted', !pace.tooSlow(), `ratio ${(8 / 240).toFixed(3)}`);
}

{
    // The window restarts on a verdict, so the next codec is judged on its own
    // frames rather than inheriting the backlog of the one it replaced.
    const pace = createEncodePace();
    offer(pace, PACE_MIN_SAMPLES, PACE_MIN_SAMPLES);
    pace.tooSlow();
    check('a verdict starts a fresh window', pace.behindRatio() === 0);
    offer(pace, PACE_MIN_SAMPLES, 0);
    check('and the next codec is judged alone', !pace.tooSlow());
}

{
    const pace = createEncodePace();
    offer(pace, 10, 10);
    pace.reset();
    check('reset clears the window', pace.behindRatio() === 0);
}

console.log(`\n[encode-pace] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
