/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// When a striped codec's frame counts as whole. Presenting on socket quiet is
// what saves the capture period the frame-id boundary costs, so the quiet a
// frame must show has to sit above the gaps that open INSIDE a frame (the
// encoder's thread pool and the event loop both leave them) and below the gap
// to the next frame. Present too early and half a frame reaches the screen;
// too late and the composite is a frame behind for nothing.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { createStripeClock } from '../../addons/selkies-web-core/lib/stripe-clock.js';

// The factory's floor and decay are locals there (the video worker embeds the
// factory by toString, so it exports nothing else); pinned here so the audit
// fails when they drift, and bound to the API by the first check below.
const STRIPE_SETTLE_FLOOR_MS = 2;
const STRIPE_WAVE_DECAY = 0.9;

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [stripe-clock] ${label}  ${detail}`);
}

/** A clock over a hand-driven millisecond counter. */
function fake() {
    const state = { t: 1000 };
    const clock = createStripeClock(() => state.t);
    return { clock, at: (t) => { state.t = t; }, advance: (dt) => { state.t += dt; } };
}

check('settleWaitMs starts at the pinned floor',
    createStripeClock(() => 0).settleWaitMs() === STRIPE_SETTLE_FLOOR_MS,
    createStripeClock(() => 0).settleWaitMs());

/** Feeds one frame's stripes at the given arrival offsets. */
function frame(f, id, offsets) {
    for (const dt of offsets) {
        f.advance(dt);
        f.clock.note(id);
    }
}

{
    const f = fake();
    check('idle before any stripe', f.clock.settled(), 'nothing arrived, nothing held');
}

{
    const f = fake();
    f.clock.note(1);
    check('a stripe just landed is not settled', !f.clock.settled());
    f.advance(STRIPE_SETTLE_FLOOR_MS - 0.5);
    check('short of the floor is not settled', !f.clock.settled());
    f.advance(1);
    check('past the floor is settled', f.clock.settled());
}

{
    // One frame's stripes arriving in two waves 5 ms apart: quiet shorter than
    // the wave gap would present the first wave alone.
    const f = fake();
    frame(f, 1, [0, 0.2, 0.2, 5, 0.2]);
    frame(f, 2, [15]);
    f.advance(STRIPE_SETTLE_FLOOR_MS + 1);
    check('floor alone does not settle a waved stream', !f.clock.settled(),
        `waveGap=${f.clock.waveGapMs().toFixed(1)}ms`);
    f.advance(5);
    check('floor plus the wave gap settles', f.clock.settled());
    check('the wave gap is the widest in-frame gap', Math.abs(f.clock.waveGapMs() - 5) < 1e-9,
        `${f.clock.waveGapMs()}`);
}

{
    // The gap to the NEXT frame must not be mistaken for an in-frame wave, or a
    // stream at any frame rate would end up demanding a frame period of quiet.
    const f = fake();
    frame(f, 1, [0, 0.2]);
    frame(f, 2, [16.7, 0.2]);
    frame(f, 3, [16.7, 0.2]);
    check('the inter-frame gap is not a wave gap', f.clock.waveGapMs() < 1,
        `${f.clock.waveGapMs().toFixed(2)}ms`);
    f.advance(STRIPE_SETTLE_FLOOR_MS + f.clock.waveGapMs());
    check('a smooth stream settles at the floor', f.clock.settled(),
        `waveGap=${f.clock.waveGapMs().toFixed(2)}ms`);
}

{
    // A one-off wave must not hold the stream back for long.
    const f = fake();
    frame(f, 1, [0, 8]);
    const peak = f.clock.waveGapMs();
    check('a gap inside the frame in flight counts at once', Math.abs(peak - 8) < 1e-9,
        `${peak}ms`);
    const smooth = 10;
    for (let id = 2; id < 2 + smooth; id++) frame(f, id, [16.7, 0.2]);
    const after = f.clock.waveGapMs();
    check('the wave gap decays per frame', after < peak * 0.5,
        `${peak.toFixed(1)}ms -> ${after.toFixed(1)}ms over ${smooth} frames`);
    // The first of those frames folds the peak in; the rest decay it.
    const want = peak * Math.pow(STRIPE_WAVE_DECAY, smooth - 1);
    check('decay follows the declared rate', Math.abs(after - want) < 1e-6,
        `${after.toFixed(4)} vs ${want.toFixed(4)}`);
}

{
    // A stream that keeps waving keeps the allowance.
    const f = fake();
    for (let id = 1; id < 8; id++) frame(f, id, [16.7, 0.2, 4, 0.2]);
    check('a persistently waved stream keeps its allowance', f.clock.waveGapMs() >= 4,
        `${f.clock.waveGapMs().toFixed(1)}ms`);
    f.advance(STRIPE_SETTLE_FLOOR_MS + 4.1);
    check('and settles once the wave gap has passed', f.clock.settled());
}

{
    // Quiet is measured from the newest stripe, not the frame's first.
    const f = fake();
    frame(f, 1, [0, 0.2, 0.2]);
    f.advance(STRIPE_SETTLE_FLOOR_MS - 0.1);
    f.clock.note(1);
    check('a late stripe restarts the quiet', !f.clock.settled());
}

{
    // A backlogged socket never goes quiet, so the caller keeps its frame-id
    // boundary behaviour instead of presenting torn frames.
    const f = fake();
    for (let id = 1; id < 40; id++) frame(f, id, [1, 1, 1, 1]);
    check('a saturated socket never settles', !f.clock.settled(),
        `waveGap=${f.clock.waveGapMs().toFixed(1)}ms`);
}

console.log(`\n[stripe-clock] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
