/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// When a tablet's attached keyboard hides the on-screen keyboard button. The
// keys an on-screen keyboard lacks say a keyboard is attached; letters, which
// an engine may report from an on-screen keyboard with a physical code, and
// an on-screen keyboard's own keys (keyCode 229, no code) do not. A touch
// after the idle period without such a key takes the verdict back, so a
// keyboard detached mid-session leaves the button reachable again.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const { createHardwareKeyboardWatch, isKeyboardOnlyKeystroke, HARDWARE_KEYBOARD_IDLE_MS } =
    await import('../../addons/selkies-web-core/lib/hardware-keyboard.js');

let failed = 0;
function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [hw-keyboard] ${label}  ${detail}`);
}

const key = (code, extra = {}) => ({ type: 'keydown', code, key: code, keyCode: 65, isTrusted: true, ...extra });

// An on-screen keyboard's keys, as Android's and iOS's report them.
const onScreen = [
    key('', { key: 'Unidentified', keyCode: 229 }),
    key('KeyA', { keyCode: 229, isComposing: true }),
    key('', { key: 'a' }),
    key('Unidentified', { key: 'Enter', keyCode: 13 }),
    key('KeyA', { key: 'a' }),
    key('Enter', { key: 'Enter', keyCode: 13 }),
    key('Backspace', { key: 'Backspace', keyCode: 8 }),
    key('Space', { key: ' ', keyCode: 32 }),
    key('ShiftLeft', { key: 'Shift', keyCode: 16 }),
];
check('no on-screen key, letters and Enter included, counts as a keyboard',
      onScreen.every((e) => !isKeyboardOnlyKeystroke(e)),
      JSON.stringify(onScreen.filter((e) => isKeyboardOnlyKeystroke(e)).map((e) => e.code)));

const physical = ['Escape', 'Tab', 'ArrowLeft', 'F5', 'Home', 'PageDown', 'Delete', 'CapsLock', 'ControlLeft', 'MetaRight']
    .map((c) => key(c, { key: c }));
check('every key an on-screen keyboard lacks counts',
      physical.every((e) => isKeyboardOnlyKeystroke(e)),
      JSON.stringify(physical.filter((e) => !isKeyboardOnlyKeystroke(e)).map((e) => e.code)));
check('so does a letter chorded with Command or Control',
      isKeyboardOnlyKeystroke(key('KeyC', { key: 'c', metaKey: true }))
      && isKeyboardOnlyKeystroke(key('KeyZ', { key: 'z', ctrlKey: true })));
check('but not a key the page dispatched itself',
      !isKeyboardOnlyKeystroke(key('Escape', { isTrusted: false })));

{
    let t = 0;
    const watch = createHardwareKeyboardWatch({ now: () => t });
    const seen = [];
    watch.subscribe((v) => seen.push(v));
    for (const e of onScreen) watch.onKeyDown(e);
    check('typing on the on-screen keyboard leaves the button', watch.attached() === false && seen.length === 0);
    watch.onKeyDown(key('ArrowDown', { key: 'ArrowDown' }));
    check('an arrow key hides it, said once', watch.attached() === true && seen.join() === 'true');
    t += HARDWARE_KEYBOARD_IDLE_MS - 1;
    watch.onTouch();
    check('a touch while the keyboard was in use keeps it hidden', watch.attached() === true);
    t += 1;
    watch.onTouch();
    check('a touch once the keyboard went idle brings it back', watch.attached() === false && seen.join() === 'true,false');
    watch.onKeyDown(key('Tab', { key: 'Tab' }));
    t += HARDWARE_KEYBOARD_IDLE_MS / 2;
    watch.onKeyDown(key('Escape', { key: 'Escape' }));
    t += HARDWARE_KEYBOARD_IDLE_MS / 2 + 1;
    watch.onTouch();
    check('each such key keeps the verdict fresh', watch.attached() === true);
    watch.reset();
    check('asking for the button back resets it', watch.attached() === false);
}

console.log(`[hw-keyboard] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
