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

const { streamDensity, publishedScale, autoScalingDpi, resolutionScalingDpi,
        DPI_STOPS } = await import(
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
    // The automatic pick is the display's own scaling as a DPI, so the stream
    // comes out at the CSS size and the browser stretches it back.
    const matched = streamDensity({ displayId: 'primary', useCssScaling: true,
                                    localScale: 2, ...AT_PRIMARY });
    check('CSS scaling at the pick the display scales by asks for the CSS size',
        matched === 1, matched);
}

{
    const unscaled = streamDensity({ displayId: 'primary', useCssScaling: true,
                                     localScale: 1, ...AT_PRIMARY });
    check('a 100% pick under CSS scaling asks for native pixels, nothing to stretch',
        unscaled === 2, unscaled);
}

{
    const stretched = streamDensity({ displayId: 'primary', useCssScaling: true,
                                      localScale: 4, ...AT_PRIMARY });
    check('a pick past the display\'s own scaling asks for less than the CSS size',
        stretched === 0.5, stretched);
}

{
    const hidpi = streamDensity({ displayId: 'primary', useCssScaling: false,
                                  localScale: 2, ...AT_PRIMARY });
    check('the pick leaves the density alone with HiDPI on, where the desktop takes it',
        hidpi === 2, hidpi);
}

{
    const broken = streamDensity({ displayId: 'primary', useCssScaling: true,
                                   localScale: 0, ...AT_PRIMARY });
    check('an unusable pick stretches by nothing', broken === 2, broken);
}

{
    // The operator's framebuffer is fixed, so the pick reaches the desktop as
    // its DPI; dividing here too would publish a density the page's own box
    // does not draw at, and the neighbour would stream at that.
    const fixed = streamDensity({ displayId: 'primary', useCssScaling: true,
                                  localScale: 1.5, manual: true, ...AT_PRIMARY });
    check('a manual resolution streams at the display\'s density, pick or no pick',
        fixed === 2, fixed);
}

{
    const neighbour = streamDensity({ displayId: 'display2', useCssScaling: true,
                                      localScale: 1.5, manual: false,
                                      layouts: { primary: { w: 3024, h: 1612, scale: 2 } } });
    check("and its neighbour streams at the density it published", neighbour === 2, neighbour);
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

{
    // The default pick is the display's own scaling as a desktop DPI, so both
    // cores derive it from the same place.
    const at = (dpr) => { window.devicePixelRatio = dpr; return autoScalingDpi(); };
    const derived = [1, 1.25, 1.5, 2].map(at);
    check('the default pick is the display scaling as a DPI',
        derived.join() === '96,120,144,192', derived.join());
    check('a density between the stops takes the nearest', at(1.75) === 168, at(1.75));
    check('and one past the last stop is clamped there',
        at(3.5) === DPI_STOPS[DPI_STOPS.length - 1], at(3.5));
    window.devicePixelRatio = 2;
}

{
    // A resolution the operator set is a framebuffer of its own: the pick
    // follows it rather than the screen showing it, off the shorter side, so
    // an ultrawide is not read as a dense screen and a turned one is the same
    // screen. The local density is left out of it entirely.
    window.devicePixelRatio = 1;
    const at = (w, h) => resolutionScalingDpi(w, h);
    const standard = [[1920, 1080], [2560, 1440], [3840, 2160]].map(([w, h]) => at(w, h));
    check('a manual resolution picks the DPI its own size asks for',
        standard.join() === '96,120,192', standard.join());
    check('a resolution below the unity rows stays at the first stop',
        at(1280, 720) === DPI_STOPS[0], at(1280, 720));
    check('an ultrawide is read as wide, not dense', at(3840, 1080) === 96, at(3840, 1080));
    check('and a portrait screen as the same screen turned',
        at(1080, 1920) === 96, at(1080, 1920));
    check('past the last stop it is clamped there',
        at(7680, 4320) === DPI_STOPS[DPI_STOPS.length - 1], at(7680, 4320));
    window.devicePixelRatio = 2;
    check('a size not yet known falls back to the display scaling',
        at(0, 0) === autoScalingDpi(), at(0, 0));
    check('and a known one ignores it', at(1920, 1080) === 96, at(1920, 1080));
}

console.log(`\n[stream-density] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
