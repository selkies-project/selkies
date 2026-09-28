/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The stops the frame rate, bitrate, and CRF sliders step through, and how a
// value lands on them. The lists and the two helpers are shared by both
// dashboards, so they are checked here once rather than through a browser.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const { FRAMERATE_STOPS, BITRATE_STOPS, CRF_STOPS, stopsWithin, stopIndex, withDisplayStop, framerateStopIndex } =
    await import('../../addons/selkies-web-core/lib/slider-stops.js');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [slider-stops] ${label}  ${detail}`);
}

const ascending = (list) => list.every((v, i) => i === 0 || v > list[i - 1]);
const descending = (list) => list.every((v, i) => i === 0 || v < list[i - 1]);

{
    check('frame rate stops ascend from the server floor to its ceiling',
        ascending(FRAMERATE_STOPS) && FRAMERATE_STOPS[0] === 8 && FRAMERATE_STOPS.at(-1) === 240,
        FRAMERATE_STOPS.join(','));
    check('bitrate stops ascend from 100 kbps to 1 Gbps',
        ascending(BITRATE_STOPS) && BITRATE_STOPS[0] === 100 && BITRATE_STOPS.at(-1) === 1000000,
        BITRATE_STOPS.length);
    // A finger settles on a band of the track; on a phone-width track of some
    // 300 pixels, forty stops is where a band shrinks below a fingertip.
    check('each list is short enough for a fingertip to land between stops',
        FRAMERATE_STOPS.length <= 40 && BITRATE_STOPS.length <= 40 && CRF_STOPS.length <= 40,
        `${FRAMERATE_STOPS.length} ${BITRATE_STOPS.length} ${CRF_STOPS.length}`);
    // Below 1 Mbps the steps are the few rates a constrained link is sized in.
    check('from 1 Mbps up, no step is over half the stop it follows',
        BITRATE_STOPS.every((v, i) => i === 0 || v < 1000 || v <= BITRATE_STOPS[i - 1] * 1.5 + 1e-9),
        BITRATE_STOPS.join(','));
    check('CRF stops descend so the right end is the higher quality',
        descending(CRF_STOPS) && CRF_STOPS[0] === 50 && CRF_STOPS.at(-1) === 5, CRF_STOPS.join(','));
    check('the encoder defaults are stops', FRAMERATE_STOPS.includes(60) && BITRATE_STOPS.includes(8000) && CRF_STOPS.includes(25));
}

{
    const inside = stopsWithin(FRAMERATE_STOPS, 30, 120);
    check('a server span keeps the stops inside it, in order',
        inside.join(',') === '30,36,40,45,48,50,60,72,75,80,90,100,120', inside.join(','));
    check('a span between two stops offers its floor alone',
        stopsWithin(FRAMERATE_STOPS, 61, 71).join(',') === '61', stopsWithin(FRAMERATE_STOPS, 61, 71).join(','));
    check('a descending list keeps its order inside a span',
        stopsWithin(CRF_STOPS, 10, 30).join(',') === '30,25,20,15,10', stopsWithin(CRF_STOPS, 10, 30).join(','));
}

{
    check('a stop maps to its own index', stopIndex(FRAMERATE_STOPS, 60) === FRAMERATE_STOPS.indexOf(60), stopIndex(FRAMERATE_STOPS, 60));
    check('a value between stops maps to the nearer one',
        stopIndex(FRAMERATE_STOPS, 56) === FRAMERATE_STOPS.indexOf(60) && stopIndex(FRAMERATE_STOPS, 52) === FRAMERATE_STOPS.indexOf(50),
        `${stopIndex(FRAMERATE_STOPS, 56)} ${stopIndex(FRAMERATE_STOPS, 52)}`);
    check('a tie goes to the earlier stop', stopIndex(BITRATE_STOPS, 7000) === BITRATE_STOPS.indexOf(6000), stopIndex(BITRATE_STOPS, 7000));
    check('a value past either end maps to that end',
        stopIndex(FRAMERATE_STOPS, 1) === 0 && stopIndex(FRAMERATE_STOPS, 1000) === FRAMERATE_STOPS.length - 1);
    check('a CRF between stops maps to the nearer one on the descending list',
        stopIndex(CRF_STOPS, 23) === CRF_STOPS.indexOf(25) && stopIndex(CRF_STOPS, 7) === CRF_STOPS.indexOf(5),
        `${stopIndex(CRF_STOPS, 23)} ${stopIndex(CRF_STOPS, 7)}`);
}

{
    const listed = stopsWithin(FRAMERATE_STOPS, 30, 120);
    const ntsc = 60000 / 1001;
    const withNtsc = withDisplayStop(listed, ntsc);
    check("the display's own stop sits where its rate sorts",
        withNtsc.stops.join(',') === `30,36,40,45,48,50,${ntsc},60,72,75,80,90,100,120` && withNtsc.stops[withNtsc.display] === ntsc,
        withNtsc.stops.join(','));
    const with60 = withDisplayStop(listed, 60);
    check('a display at a listed rate is a stop of its own after that one',
        with60.stops.length === listed.length + 1 && with60.display === listed.indexOf(60) + 1 && with60.stops[with60.display - 1] === 60,
        `${with60.display} ${with60.stops.join(',')}`);
    const none = withDisplayStop(listed, null);
    check('an unmeasured display adds no stop', none.stops === listed && none.display === -1);
    check("a rate following the display sits at the display's stop", framerateStopIndex(with60, 60, true) === with60.display);
    check('a fixed rate sits at its listed stop, never the display\'s',
        framerateStopIndex(with60, 60, false) === listed.indexOf(60)
            && framerateStopIndex(withNtsc, ntsc, false) === withNtsc.stops.indexOf(60)
            && framerateStopIndex(withNtsc, 120, false) === withNtsc.stops.length - 1,
        `${framerateStopIndex(with60, 60, false)} ${framerateStopIndex(withNtsc, ntsc, false)}`);
    check('without a display stop the position is the nearest listed stop',
        framerateStopIndex(none, 56, true) === listed.indexOf(60));
}

console.log(`\n[slider-stops] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
