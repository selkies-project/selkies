/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The density a display page streams at and the scale it publishes for its
// neighbours. Both are pure functions of the layout the server broadcast and
// the box the page draws, so they are checked here rather than through two
// browsers on two densities.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

// streamDensity reads the display's own density off the window.
globalThis.window = { devicePixelRatio: 2 };

const { streamDensity, publishedScale } = await import(
    '../../addons/selkies-web-core/lib/stream-density.js');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [stream-density] ${label}  ${detail}`);
}

const AT_PRIMARY = { layouts: { primary: { w: 1512, h: 806, scale: 1 } } };

{
    const own = streamDensity({ displayId: 'primary', useCssScaling: false, ...AT_PRIMARY });
    check('the primary streams at its own density', own === 2, own);
}

{
    const scaled = streamDensity({ displayId: 'primary', useCssScaling: true, ...AT_PRIMARY });
    check('CSS scaling makes that density 1', scaled === 1, scaled);
}

{
    const second = streamDensity({ displayId: 'display2', useCssScaling: false, ...AT_PRIMARY });
    check("a secondary streams at the primary's published scale", second === 1, second);
}

{
    const alone = streamDensity({ displayId: 'display2', useCssScaling: false, layouts: null });
    check('and at its own until the layout carries one', alone === 2, alone);
}

{
    const viewer = streamDensity({ displayId: 'display2', useCssScaling: false, shared: true,
                                   ...AT_PRIMARY });
    check('a shared viewer keeps its own density', viewer === 2, viewer);
}

{
    const scale = publishedScale({ stream: [3024, 1612], css: [1512, 806],
                                   realized: [3024, 1612], density: 1 });
    check('a box showing the realized stream publishes its ratio', scale === 2, scale);
}

{
    // The page asked for 1512x806 and the server realized it; the box is still
    // holding the 3024x1612 stream from before the resize.
    const scale = publishedScale({ stream: [3024, 1612], css: [1512, 806],
                                   realized: [1512, 806], density: 1 });
    check('a box still holding the previous stream publishes the density', scale === 1, scale);
}

{
    const scale = publishedScale({ stream: [1280, 720], css: [1512, 806],
                                   realized: null, density: 1 });
    check('so does a page with no layout yet', scale === 1, scale);
}

{
    const scale = publishedScale({ stream: null, css: [1512, 806],
                                   realized: [1512, 806], density: 2 });
    check('and one with nothing decoded yet', scale === 2, scale);
}

{
    const scale = publishedScale({ stream: [1280, 720], css: [1280, 900],
                                   realized: [1280, 720], density: 1 });
    check('a fitted box scales by its tighter dimension', scale === 1, scale);
}

{
    // A manual resolution the user scaled down: the ratio is not the density.
    const scale = publishedScale({ stream: [1280, 720], css: [2560, 1440],
                                   realized: [1280, 720], density: 2 });
    check('a stream scaled below its box publishes that ratio', scale === 0.5, scale);
}

console.log(`\n[stream-density] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
