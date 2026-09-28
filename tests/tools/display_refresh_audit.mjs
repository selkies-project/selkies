/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The display refresh a page measures from its animation frames, and the frame
// rate it asks the server for from it. Animation frames are synthesized the
// way engines hand them out: on a display's refreshes, stamped at a timer's
// resolution (0.1 ms in Chromium, 20 us in Firefox, 1 ms where an engine
// coarsens further), with a software vsync's jitter, frames a busy page
// missed, and the pauses and 1 Hz throttling of a page in the background.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const {
    FRAMERATE_DISPLAY, estimateRefresh, snapRefresh, matchDisplay, followsDisplay, requestedFramerate,
} = await import('../../addons/selkies-web-core/lib/display-refresh.js');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [display-refresh] ${label}  ${detail}`);
}

/** A seeded generator, so a failure reproduces. */
function lcg(seed) {
    let s = seed >>> 0;
    return () => {
        s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
        return s / 4294967296;
    };
}

/**
 * Animation-frame timestamps of `seconds` on a display at `hz`.
 * @param {number} hz
 * @param {number} seconds
 * @param {{quantum?: number, jitter?: number, missed?: number, pauses?: number[][], throttled?: number[][], seed?: number}} o
 *     `quantum` the timer's resolution in ms, `jitter` the vsync's standard
 *     deviation in ms, `missed` the share of refreshes the page was too busy
 *     for, `pauses` [start, end] seconds without frames, `throttled` [start,
 *     end] seconds at one frame a second.
 */
function frames(hz, seconds, o = {}) {
    const rand = lcg(o.seed ?? 7);
    const gauss = () => Math.sqrt(-2 * Math.log(1 - rand())) * Math.cos(2 * Math.PI * rand());
    const quantum = o.quantum ?? 0.1;
    const period = 1000 / hz;
    const out = [];
    let lastThrottled = -Infinity;
    for (let n = 0; n * period < seconds * 1000; n++) {
        const at = 1234.5 + n * period + (o.jitter ? o.jitter * gauss() : 0);
        const s = n * period / 1000;
        if ((o.pauses ?? []).some(([a, b]) => s >= a && s < b)) continue;
        if ((o.throttled ?? []).some(([a, b]) => s >= a && s < b)) {
            if (at - lastThrottled < 1000) continue;
            lastThrottled = at;
        } else if (o.missed && rand() < o.missed) {
            continue;
        }
        out.push(Math.floor(at / quantum) * quantum);
    }
    return out;
}

const NTSC = (n) => n * 1000 / 1001;
const measured = (hz, o) => {
    const e = estimateRefresh(frames(hz, 2, o));
    return e === null ? null : snapRefresh(e);
};

{
    for (const hz of [24, 30, 50, 60, 75, 90, 120, 144, 165, 240, 360]) {
        const got = measured(hz);
        check(`a ${hz} Hz display reads as ${hz}`, got === hz, got);
    }
    for (const n of [24, 30, 48, 60, 120, 144, 240]) {
        const got = measured(NTSC(n));
        check(`a ${n}000/1001 Hz display reads as that fraction`, got === NTSC(n), got);
    }
    const raw = estimateRefresh(frames(NTSC(60), 2));
    check('the fit resolves the refresh past the timer resolution', Math.abs(raw - NTSC(60)) < 0.002, raw);
}

{
    for (const [label, o] of [
        ['a 1 ms timer', { quantum: 1 }],
        ['a 20 us timer and a 0.5 ms software vsync jitter', { quantum: 0.02, jitter: 0.5 }],
        ['a page too busy for a third of its frames', { missed: 1 / 3 }],
        ['a page too busy for three frames in four', { missed: 0.75 }],
        ['a 1 ms timer, jitter, and missed frames at once', { quantum: 1, jitter: 0.3, missed: 0.3, seed: 3 }],
    ]) {
        for (const hz of [60, NTSC(60), 144, NTSC(120)]) {
            const got = measured(hz, o);
            check(`${label}: ${hz.toFixed(3)} Hz`, got === hz, got);
        }
    }
}

{
    const rough = frames(60, 6, { quantum: 0.02, jitter: 1.5 });
    check('a 1.5 ms jitter leaves two seconds short of the precision a rate stands at',
        estimateRefresh(rough.filter((t) => t - rough[0] < 2000)) === null);
    check('and six seconds read a 60 Hz display as 60', snapRefresh(estimateRefresh(rough)) === 60,
        estimateRefresh(rough));
    const hidden = frames(NTSC(60), 6, { pauses: [[1, 4]] });
    check('frames either side of a hidden stretch still measure',
        snapRefresh(estimateRefresh(hidden)) === NTSC(60), estimateRefresh(hidden));
    const throttled = frames(144, 6, { throttled: [[0.8, 4.5]] });
    check('frames either side of a throttled stretch still measure',
        snapRefresh(estimateRefresh(throttled)) === 144, estimateRefresh(throttled));
    check('a page throttled throughout measures nothing',
        estimateRefresh(frames(60, 30, { throttled: [[0, 30]] })) === null);
    check('too few frames measure nothing', estimateRefresh(frames(60, 0.3)) === null);
    const rand = lcg(11);
    let t = 0;
    const erratic = Array.from({ length: 200 }, () => (t += 4 + 30 * rand()));
    check('frames on no cadence measure nothing', estimateRefresh(erratic) === null, estimateRefresh(erratic));
}

{
    check('a refresh within 0.1% of an NTSC rate is that rate', snapRefresh(59.951) === NTSC(60), snapRefresh(59.951));
    check('a refresh within 0.1% of a whole rate is that rate',
        snapRefresh(60.02) === 60 && snapRefresh(143.98) === 144 && snapRefresh(74.97) === 75,
        `${snapRefresh(60.02)} ${snapRefresh(143.98)} ${snapRefresh(74.97)}`);
    check('any other refresh is kept to a hundredth',
        snapRefresh(144.2) === 144.2 && snapRefresh(48.51234) === 48.51, `${snapRefresh(144.2)} ${snapRefresh(48.51234)}`);
}

{
    check('a display inside the span streams at its refresh', matchDisplay(144, 8, 240) === 144);
    check('a display past the span streams at the largest division inside it',
        matchDisplay(360, 8, 240) === 180 && matchDisplay(144, 8, 60) === 48 && matchDisplay(NTSC(120), 8, 60) === NTSC(60),
        `${matchDisplay(360, 8, 240)} ${matchDisplay(144, 8, 60)} ${matchDisplay(NTSC(120), 8, 60)}`);
    check('a display outside the span, no division inside it, takes the nearer end',
        matchDisplay(30, 60, 240) === 60 && matchDisplay(240, 100, 110) === 110);
}

{
    const span = { min: 8, max: 240, default: 60, overridden: false };
    check('a stored rate is asked for', requestedFramerate('90', NTSC(60), span) === 90);
    check('a stored fraction is asked for as it is',
        requestedFramerate(String(NTSC(60)), 144, span) === NTSC(60));
    check('the display, when chosen, is asked for',
        requestedFramerate(FRAMERATE_DISPLAY, NTSC(60), span) === NTSC(60) && requestedFramerate(FRAMERATE_DISPLAY, 144, span) === 144);
    check('with nothing chosen, a display no faster than the default is asked for',
        requestedFramerate(null, NTSC(60), span) === NTSC(60) && requestedFramerate(null, 60.02, span) === 60.02
            && requestedFramerate(null, 50, span) === 50);
    check('with nothing chosen, a faster display leaves the default',
        requestedFramerate(null, 144, span) === null && requestedFramerate(null, 120, span) === null);
    check('nothing is asked before the display is measured',
        requestedFramerate(FRAMERATE_DISPLAY, null, span) === null && requestedFramerate(null, null, span) === null);
    check('nothing is asked before the server says its span', requestedFramerate(null, 60, null) === null);
    check("an operator's rate stands where the user chose none",
        requestedFramerate(null, NTSC(60), { ...span, overridden: true }) === null);
    check("the display, when chosen, overrides the operator's rate",
        requestedFramerate(FRAMERATE_DISPLAY, 144, { ...span, overridden: true }) === 144);
    check('a locked rate asks for nothing', requestedFramerate(FRAMERATE_DISPLAY, 144, { min: 60, max: 60, default: 60, overridden: true }) === null);
    check('the page follows its display where it is chosen, or nothing is, the server allows it, and it is no faster',
        followsDisplay(FRAMERATE_DISPLAY, span, 144) && followsDisplay(FRAMERATE_DISPLAY, null, null)
            && followsDisplay(null, span, NTSC(60)) && !followsDisplay(null, span, 144) && !followsDisplay('60', span, 60)
            && !followsDisplay(null, null, 60) && !followsDisplay(null, { ...span, overridden: true }, 60)
            && !followsDisplay(FRAMERATE_DISPLAY, { min: 60, max: 60, default: 60, overridden: true }, 60));
}

console.log(`\n[display-refresh] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
