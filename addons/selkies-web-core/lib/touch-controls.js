/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What both dashboards offer touch clients beside their soft modifier keys.
 *
 * The special-key palette: the keys an on-screen keyboard lacks (function
 * keys, arrows, the editing block), a few chords, and the user's own chords.
 * A chord is text such as `Ctrl+Shift+T`; the dashboards play it as the
 * synthetic `KeyboardEvent`s their soft keys already dispatch, which the
 * input handler forwards like real ones (`chordEvents`).
 *
 * The trackpad speeds the dashboards offer, as factors on the input handler's
 * accelerated trackpad travel (`Input.setTrackpadSpeed`).
 * @module
 */

/** The trackpad speeds offered, 1 being the handler's own curve. */
export const TRACKPAD_SPEEDS = [0.5, 0.75, 1, 1.5, 2, 3];

/** The storage key, under the dashboards' prefix, of the trackpad speed (the cores keep it). */
export const TRACKPAD_SPEED_KEY = 'trackpad_speed';

/** The palette's one-shot keys, as [label, `key`, `code`]. */
export const PALETTE_KEYS = [
    ...Array.from({ length: 12 }, (_, i) => [`F${i + 1}`, `F${i + 1}`, `F${i + 1}`]),
    ['↑', 'ArrowUp', 'ArrowUp'], ['↓', 'ArrowDown', 'ArrowDown'],
    ['←', 'ArrowLeft', 'ArrowLeft'], ['→', 'ArrowRight', 'ArrowRight'],
    ['Home', 'Home', 'Home'], ['End', 'End', 'End'],
    ['PgUp', 'PageUp', 'PageUp'], ['PgDn', 'PageDown', 'PageDown'],
    ['Ins', 'Insert', 'Insert'], ['Del', 'Delete', 'Delete'],
];

/** The chords every palette offers before the user's own. */
export const PALETTE_CHORDS = ['Alt+Tab', 'Alt+F4', 'Ctrl+Alt+Del', 'Ctrl+Shift+Esc'];

/** The storage key, under the dashboards' prefix, of the user's own chords. */
export const USER_CHORDS_KEY = 'user_chords';

/** Most user chords kept. */
export const USER_CHORDS_MAX = 24;

/** Modifier names a chord may use, as [name, `key`, `code`], `name` being how it is written back. */
const MODIFIERS = {
    ctrl: ['Ctrl', 'Control', 'ControlLeft'],
    control: ['Ctrl', 'Control', 'ControlLeft'],
    alt: ['Alt', 'Alt', 'AltLeft'],
    option: ['Alt', 'Alt', 'AltLeft'],
    shift: ['Shift', 'Shift', 'ShiftLeft'],
    win: ['Win', 'Meta', 'MetaLeft'],
    meta: ['Win', 'Meta', 'MetaLeft'],
    super: ['Win', 'Meta', 'MetaLeft'],
    cmd: ['Win', 'Meta', 'MetaLeft'],
};

/** Named keys a chord may end in, as [name, `key`, `code`]. */
const NAMED_KEYS = {
    tab: ['Tab', 'Tab', 'Tab'],
    esc: ['Esc', 'Escape', 'Escape'],
    escape: ['Esc', 'Escape', 'Escape'],
    enter: ['Enter', 'Enter', 'Enter'],
    return: ['Enter', 'Enter', 'Enter'],
    space: ['Space', ' ', 'Space'],
    backspace: ['Backspace', 'Backspace', 'Backspace'],
    del: ['Del', 'Delete', 'Delete'],
    delete: ['Del', 'Delete', 'Delete'],
    ins: ['Ins', 'Insert', 'Insert'],
    insert: ['Ins', 'Insert', 'Insert'],
    home: ['Home', 'Home', 'Home'],
    end: ['End', 'End', 'End'],
    pgup: ['PgUp', 'PageUp', 'PageUp'],
    pageup: ['PgUp', 'PageUp', 'PageUp'],
    pgdn: ['PgDn', 'PageDown', 'PageDown'],
    pagedown: ['PgDn', 'PageDown', 'PageDown'],
    up: ['Up', 'ArrowUp', 'ArrowUp'],
    down: ['Down', 'ArrowDown', 'ArrowDown'],
    left: ['Left', 'ArrowLeft', 'ArrowLeft'],
    right: ['Right', 'ArrowRight', 'ArrowRight'],
    printscreen: ['PrtSc', 'PrintScreen', 'PrintScreen'],
    prtsc: ['PrtSc', 'PrintScreen', 'PrintScreen'],
};

/**
 * Reads a chord written as modifiers and one key joined by `+`, in any case
 * (`ctrl+shift+t`, `Alt+F4`, `Win+D`).
 * @param {string} text
 * @returns {{mods: Array<Array<string>>, key: Array<string>}|null} Its
 *     modifiers and its key, each as [name, `key`, `code`], or null when it
 *     is not a chord: no key, a key that is not one, a modifier written
 *     twice, or a modifier alone.
 */
export function parseChord(text) {
    const parts = String(text || '').split('+').map((p) => p.trim());
    if (parts.length === 0 || parts.some((p) => p === '')) return null;
    const mods = [];
    for (const part of parts.slice(0, -1)) {
        const mod = MODIFIERS[part.toLowerCase()];
        if (!mod || mods.some((m) => m[1] === mod[1])) return null;
        mods.push(mod);
    }
    const last = parts[parts.length - 1];
    const lower = last.toLowerCase();
    const shifted = mods.some((m) => m[1] === 'Shift');
    let key = NAMED_KEYS[lower] || null;
    if (!key && /^f([1-9]|1[0-9]|2[0-4])$/.test(lower)) {
        key = [last.toUpperCase(), last.toUpperCase(), last.toUpperCase()];
    } else if (!key && /^[a-z]$/.test(lower)) {
        key = [lower.toUpperCase(), shifted ? lower.toUpperCase() : lower, `Key${lower.toUpperCase()}`];
    } else if (!key && /^[0-9]$/.test(lower)) {
        key = [lower, lower, `Digit${lower}`];
    }
    if (!key) return null;
    return { mods, key };
}

/**
 * A chord written back the one way the palette shows and stores it.
 * @param {{mods: Array<Array<string>>, key: Array<string>}} chord From `parseChord`.
 * @returns {string}
 */
export function formatChord(chord) {
    return [...chord.mods.map((m) => m[0]), chord.key[0]].join('+');
}

/**
 * The key events a chord is played as: its modifiers pressed in order, its
 * key pressed and released, and its modifiers released in reverse. A
 * modifier the user already holds on a soft key is left to it, neither
 * pressed nor released, and every event carries the modifier state it
 * leaves.
 * @param {{mods: Array<Array<string>>, key: Array<string>}} chord From `parseChord`.
 * @param {{Control?: boolean, Alt?: boolean, Meta?: boolean, Shift?: boolean}} held
 *     The soft keys held now.
 * @returns {Array<{type: string, key: string, code: string, state: object}>}
 */
export function chordEvents(chord, held = {}) {
    const state = { Control: !!held.Control, Alt: !!held.Alt, Meta: !!held.Meta, Shift: !!held.Shift };
    const pressed = chord.mods.filter((m) => !state[m[1]]);
    const out = [];
    for (const [, key, code] of pressed) {
        state[key] = true;
        out.push({ type: 'keydown', key, code, state: { ...state } });
    }
    const [, key, code] = chord.key;
    out.push({ type: 'keydown', key, code, state: { ...state } });
    out.push({ type: 'keyup', key, code, state: { ...state } });
    for (const [, mkey, mcode] of [...pressed].reverse()) {
        state[mkey] = false;
        out.push({ type: 'keyup', key: mkey, code: mcode, state: { ...state } });
    }
    return out;
}

/**
 * The user's own chords from `storage`, each still one (`parseChord`),
 * written back as the palette writes them, without repeats.
 * @param {Storage} storage
 * @param {string} storageKey The prefixed `USER_CHORDS_KEY`.
 * @returns {string[]}
 */
export function readUserChords(storage, storageKey) {
    let list;
    try {
        list = JSON.parse(storage.getItem(storageKey) || '[]');
    } catch (e) {
        return [];
    }
    if (!Array.isArray(list)) return [];
    const out = [];
    for (const text of list) {
        const chord = typeof text === 'string' ? parseChord(text) : null;
        const name = chord && formatChord(chord);
        if (name && !out.includes(name)) out.push(name);
    }
    return out.slice(0, USER_CHORDS_MAX);
}

/**
 * Keeps the user's own chords in `storage`; a storage that refuses them
 * (a private window, a full quota) leaves them for this page's life.
 * @param {Storage} storage
 * @param {string} storageKey The prefixed `USER_CHORDS_KEY`.
 * @param {string[]} chords
 */
export function writeUserChords(storage, storageKey, chords) {
    try {
        storage.setItem(storageKey, JSON.stringify(chords.slice(0, USER_CHORDS_MAX)));
    } catch (e) {
        /* kept in memory only */
    }
}
