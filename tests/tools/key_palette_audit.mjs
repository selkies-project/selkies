/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The dashboards' special-key palette: what a chord the user writes is read
// as, and what reaches the wire when it is played as the synthetic key events
// the soft keys dispatch, through the real input handler. A soft modifier the
// user already holds has to stay held across a chord that names it too.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';
import {
    PALETTE_CHORDS, PALETTE_KEYS, chordEvents, formatChord, parseChord, readUserChords, writeUserChords,
} from '../../addons/selkies-web-core/lib/touch-controls.js';

globalThis.window = globalThis;
globalThis.document = { body: null, activeElement: null, fullscreenElement: null };
Object.defineProperty(globalThis, 'navigator', {
    value: { platform: 'Linux x86_64', userAgent: 'Mozilla/5.0 (X11; Linux) Chrome/154',
             userAgentData: { brands: [{ brand: 'Google Chrome' }] } },
    configurable: true, writable: true });
globalThis.chrome = { runtime: {} };

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [key-palette] ${label}  ${detail}`);
}

/** An Input with only what the key handlers touch, capturing what it would send. */
function makeInput() {
    const input = Object.create(Input.prototype);
    input.sent = [];
    input.send = (msg) => input.sent.push(msg);
    input._keyDownList = {};
    input._momentaryChordMods = new Set();
    input._momentaryChordModsTimer = null;
    input._altKeysymByCode = new Map();
    input._altGrArmed = false;
    input._altGrTimeout = null;
    input._macCmdSwapped = false;
    input.isComposing = false;
    input.gamingMode = false;
    input._isSynth = true;
    input._EVENT_MARKER = '_AUDIT_MARK';
    input._startKeyHeartbeat = () => {};
    input._stopKeyHeartbeat = () => {};
    return input;
}

/** Plays key events the way a dashboard dispatches them: constructed, not trusted. */
function play(input, events) {
    for (const { type, key, code, state } of events) {
        const event = {
            key, code, location: 0, keyCode: 0, isComposing: false, timeStamp: 0, isTrusted: false,
            altKey: !!state.Alt, ctrlKey: !!state.Control, metaKey: !!state.Meta, shiftKey: !!state.Shift,
            target: { classList: { contains: () => false }, parentElement: null },
            getModifierState: (name) => !!state[name],
            preventDefault() {}, stopPropagation() {},
        };
        if (type === 'keydown') input._handleKeyDown(event);
        else input._handleKeyUp(event);
    }
    clearTimeout(input._momentaryChordModsTimer);
    return input.sent;
}

const XK = { Control_L: 65507, Alt_L: 65513, Shift_L: 65505, Super_L: 65515, Delete: 65535,
             Escape: 65307, Tab: 65289, F4: 65473, t: 116, T: 84 };

// --- reading what the user writes -------------------------------------------
{
    const cases = [
        ['ctrl+shift+t', 'Ctrl+Shift+T'], ['Alt + F4', 'Alt+F4'], ['control+alt+delete', 'Ctrl+Alt+Del'],
        ['win+d', 'Win+D'], ['Super+1', 'Win+1'], ['esc', 'Esc'], ['Ctrl+PgDn', 'Ctrl+PgDn'],
    ];
    const got = cases.map(([text]) => { const c = parseChord(text); return c && formatChord(c); });
    check('a chord is read in any case and spacing, and written back one way',
          got.every((g, i) => g === cases[i][1]), JSON.stringify(got));
    const refused = ['', 'Ctrl+', 'Ctrl', 'Ctrl+Ctrl+A', 'Hyper+A', 'Ctrl+Foo', 'A+B', 'Ctrl++A'];
    const accepted = refused.filter((text) => parseChord(text) !== null);
    check('what is not a chord is refused', accepted.length === 0, JSON.stringify(accepted));
    check('every chord the palette offers reads', PALETTE_CHORDS.every((c) => parseChord(c) !== null),
          PALETTE_CHORDS.join(' '));
    check('the palette has the function keys, the arrows, and the editing block',
          PALETTE_KEYS.length === 22 && PALETTE_KEYS[0][2] === 'F1' && PALETTE_KEYS[11][2] === 'F12',
          PALETTE_KEYS.map((k) => k[0]).join(' '));
}

// --- what reaches the wire ---------------------------------------------------
{
    const wire = play(makeInput(), chordEvents(parseChord('Ctrl+Alt+Del')));
    const want = [`kd,${XK.Control_L}`, `kd,${XK.Alt_L}`, `kd,${XK.Delete}`, `ku,${XK.Delete}`,
                  `ku,${XK.Alt_L}`, `ku,${XK.Control_L}`];
    check('Ctrl+Alt+Del goes out as its keys pressed in order and released in reverse',
          JSON.stringify(wire) === JSON.stringify(want), wire.join(' '));

    const shifted = play(makeInput(), chordEvents(parseChord('ctrl+shift+t')));
    check('a letter under Shift goes out as the capital a keyboard sends',
          shifted.includes(`kd,${XK.T}`) && shifted[0] === `kd,${XK.Control_L}` && shifted.includes(`kd,${XK.Shift_L}`),
          shifted.join(' '));

    // CTL held on its soft key first, then the palette's Alt+F4... with Ctrl too.
    const input = makeInput();
    play(input, [{ type: 'keydown', key: 'Control', code: 'ControlLeft', state: { Control: true } }]);
    input.sent.length = 0;
    const kept = play(input, chordEvents(parseChord('Ctrl+Alt+F4'), { Control: true }));
    check('a soft modifier already held is neither pressed nor released by a chord naming it',
          !kept.some((m) => m.endsWith(`,${XK.Control_L}`)) && kept.includes(`kd,${XK.F4}`)
          && Object.keys(input._keyDownList).includes('ControlLeft'), kept.join(' '));
}

// --- the user's own chords ---------------------------------------------------
{
    const store = new Map();
    const storage = { getItem: (k) => store.get(k) ?? null, setItem: (k, v) => store.set(k, v) };
    writeUserChords(storage, 'x_user_chords', ['ctrl+shift+t', 'Ctrl+Shift+T', 'nonsense', 'Win+E']);
    const back = readUserChords(storage, 'x_user_chords');
    check('the user\'s chords come back written one way, without repeats or anything unreadable',
          JSON.stringify(back) === JSON.stringify(['Ctrl+Shift+T', 'Win+E']), JSON.stringify(back));
    store.set('x_user_chords', '{broken');
    check('a damaged store reads as no chords', readUserChords(storage, 'x_user_chords').length === 0);
}

process.exit(failed === 0 ? 0 : 1);
