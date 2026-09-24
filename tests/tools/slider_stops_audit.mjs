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

const { FRAMERATE_STOPS, BITRATE_STOPS, CRF_STOPS, stopsWithin, stopIndex } = await import(
    '../../addons/selkies-web-core/lib/slider-stops.js');

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
        inside.join(',') === '30,48,50,60,90,100,120', inside.join(','));
    check('a span between two stops offers its floor alone',
        stopsWithin(FRAMERATE_STOPS, 61, 89).join(',') === '61', stopsWithin(FRAMERATE_STOPS, 61, 89).join(','));
    check('a descending list keeps its order inside a span',
        stopsWithin(CRF_STOPS, 10, 30).join(',') === '30,25,20,15,10', stopsWithin(CRF_STOPS, 10, 30).join(','));
}

{
    check('a stop maps to its own index', stopIndex(FRAMERATE_STOPS, 60) === 8, stopIndex(FRAMERATE_STOPS, 60));
    check('a value between stops maps to the nearer one', stopIndex(FRAMERATE_STOPS, 56) === 8 && stopIndex(FRAMERATE_STOPS, 52) === 7,
        `${stopIndex(FRAMERATE_STOPS, 56)} ${stopIndex(FRAMERATE_STOPS, 52)}`);
    check('a tie goes to the earlier stop', stopIndex(BITRATE_STOPS, 7000) === BITRATE_STOPS.indexOf(6000), stopIndex(BITRATE_STOPS, 7000));
    check('a value past either end maps to that end',
        stopIndex(FRAMERATE_STOPS, 1) === 0 && stopIndex(FRAMERATE_STOPS, 1000) === FRAMERATE_STOPS.length - 1);
    check('a CRF between stops maps to the nearer one on the descending list',
        stopIndex(CRF_STOPS, 23) === CRF_STOPS.indexOf(25) && stopIndex(CRF_STOPS, 7) === CRF_STOPS.indexOf(5),
        `${stopIndex(CRF_STOPS, 23)} ${stopIndex(CRF_STOPS, 7)}`);
}

console.log(`\n[slider-stops] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
