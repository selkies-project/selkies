/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the client puts on the wire for a modifier chord, under every engine that
// describes the same physical keyboard differently. One physical action is
// rendered as each engine reports it and driven through the real handler: the wire
// has to come out identical, and equal to what the action means.
//
// The engines genuinely disagree about the Alt-position key. macOS Option is a
// level-3 shift, and Gecko reports it as AltGraph, Blink only as altKey, WebKit as
// a Meta key; a PC AltGr reports as AltGraph, or as Ctrl+Alt on an older engine.
// Reading those flags to tell a level shift from a shortcut therefore gives a
// different answer per browser, which is the defect this matrix exists to catch.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

globalThis.window = globalThis;
globalThis.document = { body: null, activeElement: null, fullscreenElement: null };

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [keyboard-chords] ${label}  ${detail}`);
}

const MOD_CODES = {
    ShiftLeft: 'Shift', ShiftRight: 'Shift', ControlLeft: 'Control',
    ControlRight: 'Control', AltLeft: 'Alt', AltRight: 'Alt',
    MetaLeft: 'Meta', MetaRight: 'Meta',
};
const LOCATION = { ShiftLeft: 1, ShiftRight: 2, ControlLeft: 1, ControlRight: 2,
                   AltLeft: 1, AltRight: 2, MetaLeft: 1, MetaRight: 2 };
const held = (down, side) => down.has(side + 'Left') || down.has(side + 'Right');

/**
 * How one engine on one platform describes the keyboard. `key` names the
 * modifier codes it reports differently, and `flags` turns the set of codes
 * physically down into the modifier flags it would report for them.
 */
const ENGINES = {
    'blink-mac': {
        platform: 'MacIntel', ua: 'Mozilla/5.0 (Macintosh) Chrome/151', chrome: true,
        // Option reaches Blink as optionKey, which it maps to kAltKey alone.
        flags: () => ({ altGraph: false }),
    },
    'gecko-mac': {
        platform: 'MacIntel', ua: 'Mozilla/5.0 (Macintosh) Gecko Firefox/140', chrome: false,
        flags: (down) => ({ altGraph: held(down, 'Alt') }),
    },
    'webkit-mac': {
        platform: 'MacIntel', ua: 'Mozilla/5.0 (Macintosh) Version/18 Safari/605', chrome: false,
        key: { AltLeft: 'Meta', AltRight: 'Meta' },
        flags: () => ({ altGraph: false }),
    },
    'blink-pc': {
        platform: 'Linux x86_64', ua: 'Mozilla/5.0 (X11; Linux) Chrome/151', chrome: true,
        key: { AltRight: 'AltGraph' },
        flags: (down) => ({ altGraph: down.has('AltRight'), alt: down.has('AltLeft') }),
    },
    'gecko-pc': {
        platform: 'Linux x86_64', ua: 'Mozilla/5.0 (X11; Linux) Gecko Firefox/140', chrome: false,
        key: { AltRight: 'AltGraph' },
        flags: (down) => ({ altGraph: down.has('AltRight'), alt: down.has('AltLeft') }),
    },
    'legacy-pc': {
        // Engines before the AltGraph flag reported AltGr as its Ctrl+Alt pair.
        platform: 'Win32', ua: 'Mozilla/5.0 (Windows NT) Chrome/60', chrome: true,
        key: { AltRight: 'AltGraph' },
        flags: (down) => ({ altGraph: down.has('AltRight'),
                            ctrl: down.has('ControlLeft') || down.has('AltRight'),
                            alt: down.has('AltLeft') || down.has('AltRight') }),
    },
    'soft-keyboard': {
        // The dashboards' on-screen modifiers: a constructed KeyboardEvent
        // naming the modifier it wants held, with no platform behind it.
        platform: 'MacIntel', ua: 'Mozilla/5.0 (Macintosh) Chrome/151', chrome: true,
        trusted: false, flags: (down) => ({ altGraph: false }),
    },
};

function setPlatform(engine) {
    Object.defineProperty(globalThis, 'navigator', {
        value: { platform: engine.platform, userAgent: engine.ua,
                 userAgentData: engine.chrome ? { brands: [{ brand: 'Google Chrome' }] } : undefined },
        configurable: true, writable: true });
    globalThis.chrome = engine.chrome ? { runtime: {} } : undefined;
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
    input._isSynth = false;
    input._EVENT_MARKER = '_AUDIT_MARK';
    input._startKeyHeartbeat = () => {};
    input._stopKeyHeartbeat = () => {};
    return input;
}

/**
 * Run one action under one engine and return the wire it produced.
 *
 * An action is physical: `['down'|'up', code, character?, forced?]` steps, with
 * the character the layout produces for that key under the modifiers then down.
 * The engine supplies everything else the browser would say about it; `forced`
 * overrides a flag, for a modifier whose own keydown never reached the page, or
 * carries `replay` for a step clipboard-sync re-dispatched and `physical` for a
 * real keypress among page-built ones. `opts.synth` raises synthetic mode, as
 * the dashboards do while a soft modifier is held.
 */
function wire(engineName, steps, opts = {}) {
    const engine = ENGINES[engineName];
    setPlatform(engine);
    const input = makeInput();
    input._isSynth = !!opts.synth;
    const down = new Set();
    for (const [action, code, char, forced] of steps) {
        if (action === 'down') down.add(code); else down.delete(code);
        const { replay, physical, ...override } = forced || {};
        const flags = {
            shift: held(down, 'Shift'), ctrl: held(down, 'Control'),
            alt: held(down, 'Alt'), meta: held(down, 'Meta'),
            ...engine.flags(down), ...override,
        };
        const key = char !== undefined ? char
            : ((engine.key && engine.key[code]) || MOD_CODES[code] || code);
        const state = { Alt: flags.alt, Control: flags.ctrl, Meta: flags.meta,
                        Shift: flags.shift, AltGraph: flags.altGraph };
        const event = {
            key, code, location: LOCATION[code] || 0, keyCode: 0, isComposing: false,
            timeStamp: 0,
            isTrusted: physical === true || (!replay && engine.trusted !== false),
            altKey: flags.alt, ctrlKey: flags.ctrl, metaKey: flags.meta, shiftKey: flags.shift,
            target: { classList: { contains: () => false }, parentElement: null },
            getModifierState: (name) => !!state[name],
            preventDefault() {}, stopPropagation() {},
        };
        if (replay) event.__selkiesClipReplay = true;
        if (action === 'down') input._handleKeyDown(event);
        else input._handleKeyUp(event);
    }
    clearTimeout(input._momentaryChordModsTimer);
    return input.sent.join(' ');
}

const XK = { Alt_L: 65513, Mode_switch: 65406, ISO_Level3_Shift: 65027, Control_L: 65507,
             Control_R: 65508, Super_L: 65515, Meta_L: 65511, Omega: 0x7d9, lstroke: 435,
             Tab: 65289, Left: 65361 };

/**
 * One physical action, the engines that can perform it, and the wire it means.
 * Every engine listed has to produce that same wire, whatever it calls Option.
 */
const ACTIONS = [
    // -- macOS: Option is the layout's level-3 shift, and the client sends it
    // -- as one, so a chord that produced a character carries the character.
    { name: 'Option+Z types the omega it produced',
      engines: ['blink-mac', 'gecko-mac', 'webkit-mac'],
      steps: [['down', 'AltLeft'], ['down', 'KeyZ', 'Ω'], ['up', 'KeyZ', 'Ω'], ['up', 'AltLeft']],
      wire: `kd,LEVEL3 kd,${XK.Omega} ku,${XK.Omega} ku,LEVEL3` },
    { name: 'right Option+Z types it too, past the momentary shift',
      engines: ['blink-mac', 'gecko-mac'],
      steps: [['down', 'AltRight'], ['down', 'KeyZ', 'Ω'], ['up', 'KeyZ', 'Ω'], ['up', 'AltRight']],
      wire: `kd,${XK.ISO_Level3_Shift} ku,${XK.ISO_Level3_Shift} kd,${XK.Omega} ku,${XK.Omega}` },
    { name: 'Option+L types the @ a German layout puts there',
      engines: ['blink-mac', 'gecko-mac', 'webkit-mac'],
      steps: [['down', 'AltLeft'], ['down', 'KeyL', '@'], ['up', 'KeyL', '@'], ['up', 'AltLeft']],
      wire: 'kd,LEVEL3 kd,64 ku,64 ku,LEVEL3' },
    { name: 'Shift+Option+Z types its own character, with no Shift on the wire',
      engines: ['blink-mac', 'gecko-mac'],
      steps: [['down', 'ShiftLeft'], ['down', 'AltLeft'], ['down', 'KeyZ', '¸']],
      wire: 'kd,65505 kd,LEVEL3 kd,184' },
    // -- macOS: Option over a key that produced no character is a shortcut, and
    // -- Command or Control makes one of any chord.
    { name: 'Option+Tab stays Alt+Tab',
      engines: ['blink-mac', 'gecko-mac', 'webkit-mac'],
      steps: [['down', 'AltLeft'], ['down', 'Tab']],
      wire: `kd,LEVEL3 kd,${XK.Alt_L} kd,${XK.Tab} ku,${XK.Alt_L}` },
    { name: 'Option+ArrowLeft stays Alt+Left',
      engines: ['blink-mac', 'gecko-mac'],
      steps: [['down', 'AltLeft'], ['down', 'ArrowLeft']],
      wire: `kd,LEVEL3 kd,${XK.Alt_L} kd,${XK.Left} ku,${XK.Alt_L}` },
    { name: 'Cmd+Option+Z is a shortcut on the physical key',
      engines: ['blink-mac', 'gecko-mac'],
      steps: [['down', 'MetaLeft'], ['down', 'AltLeft'], ['down', 'KeyZ', 'Ω']],
      wire: `kd,${XK.Alt_L} kd,LEVEL3 kd,${XK.Super_L} kd,122 ku,${XK.Super_L}` },
    { name: 'Ctrl+Option+X is a shortcut on the physical key',
      engines: ['blink-mac', 'gecko-mac'],
      steps: [['down', 'ControlLeft'], ['down', 'AltLeft'], ['down', 'KeyX', '≈']],
      wire: `kd,${XK.Control_L} kd,LEVEL3 kd,${XK.Alt_L} kd,120 ku,${XK.Alt_L}` },
    { name: 'Cmd+C reaches the server as its Ctrl chord',
      engines: ['blink-mac', 'gecko-mac'],
      steps: [['down', 'MetaLeft'], ['down', 'KeyC', 'c']],
      wire: `kd,${XK.Alt_L} ku,${XK.Alt_L} kd,${XK.Control_L} kd,99` },
    // -- PC: AltGr is the level-3 shift and Alt is the action modifier, whether
    // -- or not the engine has an AltGraph flag to say so.
    { name: 'AltGr+L types the Polish l-stroke',
      engines: ['blink-pc', 'gecko-pc', 'legacy-pc'],
      steps: [['down', 'AltRight'], ['down', 'KeyL', 'ł'], ['up', 'KeyL', 'ł'], ['up', 'AltRight']],
      wire: `kd,${XK.ISO_Level3_Shift} kd,${XK.lstroke} ku,${XK.lstroke} ku,${XK.ISO_Level3_Shift}` },
    { name: 'Alt over a Russian layout still names the positional shortcut',
      engines: ['blink-pc', 'gecko-pc'],
      steps: [['down', 'AltLeft'], ['down', 'KeyZ', 'я']],
      wire: `kd,${XK.Alt_L} kd,122` },
    { name: 'Ctrl over a Russian layout still names the positional shortcut',
      engines: ['blink-pc', 'gecko-pc', 'legacy-pc'],
      steps: [['down', 'ControlLeft'], ['down', 'KeyZ', 'я']],
      wire: `kd,${XK.Control_L} kd,122` },
    { name: 'Alt+F stays Alt+F',
      engines: ['blink-pc', 'gecko-pc', 'legacy-pc'],
      steps: [['down', 'AltLeft'], ['down', 'KeyF', 'f']],
      wire: `kd,${XK.Alt_L} kd,102` },
    { name: 'a right Option reported as a Meta key still types its character',
      engines: ['webkit-mac'],
      steps: [['down', 'AltRight'], ['down', 'KeyZ', 'Ω'], ['up', 'KeyZ', 'Ω'], ['up', 'AltRight']],
      wire: `kd,65512 kd,${XK.Omega} ku,${XK.Omega} ku,65512` },
    { name: 'a right Alt with no AltGr on it names the shortcut',
      engines: ['blink-pc', 'gecko-pc'],
      steps: [['down', 'AltRight', 'Alt', { altGraph: false, alt: true }],
              ['down', 'KeyF', 'f', { altGraph: false, alt: true }]],
      wire: 'kd,65514 kd,102' },
    { name: 'the right Shift and Meta hold their own side',
      engines: ['blink-pc', 'gecko-pc'],
      steps: [['down', 'ShiftRight'], ['down', 'MetaRight'], ['down', 'KeyA', 'A']],
      wire: 'kd,65506 kd,65516 kd,65' },
    { name: 'the right Control names the shortcut its own side does',
      engines: ['blink-pc', 'gecko-pc'],
      steps: [['down', 'ControlRight'], ['down', 'KeyZ', 'я']],
      wire: `kd,${XK.Control_R} kd,122` },
    // -- A page-built event names the modifier it wants held, with no platform
    // -- remap over it: the on-screen Alt is Alt even on a macOS client.
    { name: 'the on-screen Alt holds Alt, not the platform level-3 shift',
      engines: ['soft-keyboard'],
      steps: [['down', 'AltLeft'], ['down', 'KeyA', 'a']],
      wire: `kd,${XK.Alt_L} kd,97` },
    { name: 'the on-screen Meta holds Super',
      engines: ['soft-keyboard'],
      steps: [['down', 'MetaLeft'], ['down', 'KeyA', 'a']],
      wire: `kd,${XK.Super_L} kd,97` },
];

// The left Option is Mode_switch and a PC AltGr is ISO_Level3_Shift; both are
// the level-3 shift, and which one an action gets is the platform's business.
const LEVEL3 = { 'blink-mac': XK.Mode_switch, 'gecko-mac': XK.Mode_switch,
                 'webkit-mac': XK.Meta_L };

for (const action of ACTIONS) {
    const seen = new Map();
    for (const engine of action.engines) {
        seen.set(engine, wire(engine, action.steps));
    }
    const want = action.engines.map((e) =>
        [e, action.wire.replaceAll('LEVEL3', String(LEVEL3[e] ?? XK.ISO_Level3_Shift))]);
    const wrong = want.filter(([e, w]) => seen.get(e) !== w);
    check(action.name, wrong.length === 0,
          wrong.map(([e, w]) => `${e}: ${seen.get(e)} != ${w}`).join(' | '));
}

// The same physical action reaches the server the same way whatever the browser
// calls the Alt-position key, which is the invariant the flags cannot hold up.
for (const action of ACTIONS) {
    const platforms = new Map();
    for (const engine of action.engines) {
        const family = engine.split('-')[1];
        if (!platforms.has(family)) platforms.set(family, []);
        platforms.get(family).push([engine, wire(engine, action.steps)]);
    }
    for (const [family, runs] of platforms) {
        if (runs.length < 2) continue;
        // The macOS engines differ only in which keysym they name Option with.
        const normal = runs.map(([e, w]) =>
            [e, w.replaceAll(String(LEVEL3[e] ?? XK.ISO_Level3_Shift), 'LEVEL3')]);
        const agreed = normal.every(([, w]) => w === normal[0][1]);
        check(`${family} engines agree on '${action.name}'`, agreed,
              agreed ? '' : normal.map(([e, w]) => `${e}: ${w}`).join(' | '));
    }
}

// A modifier whose keydown never reached the page (an IME or an OS grab took
// it) leaves nothing resolved for the Alt-position key, so only the engine's
// own AltGraph can still say the chord picked its character.
const swallowedAltGr = wire('blink-pc', [['down', 'KeyL', 'ł', { altGraph: true }]]);
check('an AltGr whose keydown was swallowed still types its character',
      swallowedAltGr === `kd,${XK.lstroke}`, swallowedAltGr);

const swallowedAlt = wire('blink-mac', [['down', 'KeyZ', 'Ω', { alt: true }]]);
check('an Alt whose keydown was swallowed still names the shortcut',
      swallowedAlt === `kd,${XK.Alt_L} kd,122 ku,${XK.Alt_L}`, swallowedAlt);

// A Control the page never saw go down is held by nothing, so only its flag
// says the chord is a shortcut rather than the character Option composed.
const swallowedCtrl = wire('blink-mac',
    [['down', 'AltLeft'], ['down', 'KeyX', '≈', { ctrl: true }]]);
check('a Control whose keydown was swallowed still names the shortcut',
      swallowedCtrl === `kd,${XK.Mode_switch} kd,${XK.Control_L} kd,${XK.Alt_L} kd,120 `
                     + `ku,${XK.Alt_L} ku,${XK.Control_L}`, swallowedCtrl);

// clipboard-sync holds a paste chord while a transfer is in flight and
// re-dispatches it after, so the replay is the physical key, deferred.
const paste = [['down', 'MetaLeft'], ['down', 'KeyV', 'v'], ['up', 'KeyV', 'v']];
const replayed = [['down', 'MetaLeft'], ['down', 'KeyV', 'v', { replay: true }],
                  ['up', 'KeyV', 'v', { replay: true }]];
const replayedPaste = wire('blink-mac', replayed);
const physicalPaste = wire('blink-mac', paste);
check('a held paste replays as the physical key it was', replayedPaste === physicalPaste,
      replayedPaste === physicalPaste ? physicalPaste : `${replayedPaste} != ${physicalPaste}`);

// A soft modifier button raises synthetic mode, and a physical key pressed
// under it reports no modifier of its own: healing on that would release the
// modifier the user is holding on screen, breaking the chord.
const softHeld = wire('soft-keyboard',
    [['down', 'ControlLeft'], ['down', 'KeyC', 'c', { physical: true, ctrl: false }]],
    { synth: true });
check('a soft modifier survives a physical key pressed under it',
      softHeld === `kd,${XK.Control_L} kd,99`, softHeld);

process.exit(failed === 0 ? 0 : 1);
