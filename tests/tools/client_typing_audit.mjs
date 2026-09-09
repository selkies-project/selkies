/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the client puts on the wire when text is typed, pasted or re-typed. Both
// text-entry paths must emit the character's OWN keysym: a capital sent as Shift plus
// the lowercase keysym arrives lowercase wherever the X keymap names Shift without
// binding it as a modifier, and that failure is invisible until someone types one.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

const XK_Shift_L = 0xffe1;

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [client-typing] ${label}  ${detail}`);
}

/** An Input with only what the text paths touch, capturing what it would send. */
function makeInput() {
    const input = Object.create(Input.prototype);
    input.sent = [];
    input.send = (msg) => input.sent.push(msg);
    input._momentaryChordMods = new Set();
    input._keyDownList = {};
    input._assistTyped = '';
    input.isComposing = false;
    input._clearCompositionHostSoon = () => {};
    return input;
}

/** The keysyms pressed, in order, from a captured wire sequence. */
function pressed(sent) {
    return sent.filter((m) => m.startsWith('kd,')).map((m) => Number(m.slice(3)));
}

function typeMobile(text, input = makeInput()) {
    const target = { value: text };
    input.keyboardInputAssist = target;
    input._handleMobileInput({ target });
    return { sent: input.sent, target };
}

function typeText(text) {
    const input = makeInput();
    input._handleTextInput({ data: text });
    return { sent: input.sent };
}

// A capital arrives as its own keysym, with no Shift anywhere in the sequence.
const upper = typeMobile('Ab');
check('capital sends its own keysym',
      pressed(upper.sent).join(',') === '65,98', pressed(upper.sent).join(','));
check('capital sends no Shift',
      !upper.sent.some((m) => m.endsWith(',' + XK_Shift_L)), upper.sent.join(' '));
check('every press is released',
      upper.sent.join(' ') === 'kd,65 ku,65 kd,98 ku,98', upper.sent.join(' '));

// The soft-input and text-echo paths are one implementation; they cannot drift again.
const sample = 'Ab!Z';
check('mobile and text paths agree',
      typeMobile(sample).sent.join(' ') === typeText(sample).sent.join(' '),
      typeMobile(sample).sent.join(' '));

// An astral character is one keysym, not two lone surrogates (which are not keysyms).
const astral = typeMobile('\u{1F600}');
check('astral character sends one keysym',
      pressed(astral.sent).length === 1 && pressed(astral.sent)[0] === (0x01000000 | 0x1f600),
      pressed(astral.sent).join(','));

// Latin-1 and the Unicode range above it both resolve.
check('non-ASCII latin sends its keysym', pressed(typeMobile('é').sent).join(',') === '233');
check('CJK sends a unicode keysym',
      pressed(typeMobile('漢').sent).join(',') === String(0x01000000 | 0x6f22));

// A chord key the keydown path already sent (iOS names the key on the keydown)
// must not be typed again by its text echo.
const chorded = makeInput();
chorded._keyDownList = { ControlLeft: 0xffe3 };
chorded._chordKeySent = true;
const echo = typeMobile('c', chorded);
check('chord echo types nothing', echo.sent.length === 0, echo.sent.join(' '));
check('chord echo still clears the field', echo.target.value === '');

// A tap the keydown path could not name (Android reports keyCode 229 with no
// key) reaches the wire only as the assist text, so under a held modifier it
// goes out once as the plain key the modifier turns into the shortcut.
const unnamed = makeInput();
unnamed._keyDownList = { ControlLeft: 0xffe3 };
const tap = typeMobile('c', unnamed);
check('unnamed tap under a held chord types the key once',
      tap.sent.join(' ') === 'kd,99 ku,99', tap.sent.join(' '));
check('unnamed tap clears the field', tap.target.value === '');

// The assist field is drained, or the next input event retypes everything before it.
check('field cleared after typing', upper.target.value === '');

process.exit(failed === 0 ? 0 : 1);
