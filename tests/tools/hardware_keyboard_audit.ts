/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * When a tablet's attached keyboard hides the on-screen keyboard button. The
 * keys an on-screen keyboard lacks say a keyboard is attached; letters, which
 * an engine may report from an on-screen keyboard with a physical code, and
 * an on-screen keyboard's own keys (keyCode 229, no code) do not. A touch
 * after the idle period without such a key takes the verdict back, so a
 * keyboard detached mid-session leaves the button reachable again. On a touch
 * screen the primary pointer decides at once: fine with a convertible's
 * keyboard and touchpad attached, coarse once they are detached. Where the
 * engine misses an attach, the query read again on a resize or a mouse event
 * catches a change that came without its event, and a convertible's touchpad
 * in use says its keyboard is back.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import {
    createHardwareKeyboardWatch, isKeyboardOnlyKeystroke, HARDWARE_KEYBOARD_IDLE_MS,
} from '../../web/selkies-web-core/lib/hardware-keyboard.ts';
import type { HardwareKeyboardWatch } from '../../web/selkies-web-core/lib/hardware-keyboard.ts';
import type { StandIn } from './stand_in.ts';

let failed = 0;
/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [hw-keyboard] ${label}  ${String(detail)}`);
}

const key = (code: string, extra: StandIn<KeyboardEvent> = {}): KeyboardEvent =>
    ({ type: 'keydown', code, key: code, keyCode: 65, isTrusted: true, ...extra }) as StandIn<KeyboardEvent> as KeyboardEvent;

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
    const seen: boolean[] = [];
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

{
    let t = 0;
    const watch = createHardwareKeyboardWatch({ now: () => t });
    const seen: boolean[] = [];
    watch.subscribe((v) => seen.push(v));
    watch.onPointer(true);
    check('a primary pointer turning fine hides it at once, as a keyboard attached with its touchpad',
          watch.attached() === true && seen.join() === 'true');
    t += HARDWARE_KEYBOARD_IDLE_MS * 10;
    watch.onTouch();
    check('no touch brings it back while the pointer stays fine', watch.attached() === true);
    watch.onKeyDown(key('Escape', { key: 'Escape' }));
    watch.onPointer(false);
    check('the pointer turning coarse brings it back at once, keys typed before or not',
          watch.attached() === false && seen.join() === 'true,false');
    watch.onPointer(true);
    watch.reset();
    t += HARDWARE_KEYBOARD_IDLE_MS * 10;
    watch.onTouch();
    check('asking for the button back outweighs a fine pointer', watch.attached() === false);
}

{
    let t = 0;
    const watch = createHardwareKeyboardWatch({ now: () => t });
    watch.onMouse();
    check('a mouse on a screen whose pointer was never fine says nothing of a keyboard', watch.attached() === false);
    watch.onPointer(true);
    watch.onPointer(false);
    watch.onMouse();
    check("a convertible's touchpad in use hides the button, its pointer left coarse", watch.attached() === true);
    t += HARDWARE_KEYBOARD_IDLE_MS - 1;
    watch.onTouch();
    check('a touch while the touchpad was in use keeps it hidden', watch.attached() === true);
    t += 1;
    watch.onTouch();
    check('a touch once the touchpad went idle brings it back', watch.attached() === false);
    watch.onMouse();
    watch.onPointer(false);
    check('as does the pointer turning coarse', watch.attached() === false);
    watch.onMouse();
    watch.reset();
    watch.onMouse();
    check('asking for the button back outweighs the touchpad until the pointer turns fine again',
          watch.attached() === false);
}

/** The page's watch, imported afresh on a window whose primary pointer starts `fine`. */
async function pageWatchOn(fine: boolean, maxTouchPoints: number, tag: string): Promise<{
    watch: HardwareKeyboardWatch;
    turn: (to: boolean) => void;
    turnSilently: (to: boolean) => void;
    fire: (type: string, event?: StandIn<PointerEvent>) => void;
}> {
    const listeners = new Set<(event: StandIn<MediaQueryListEvent>) => void>();
    const windowListeners = new Map<string, ((event: StandIn<PointerEvent>) => void)[]>();
    const pointer = {
        matches: fine,
        addEventListener: (type: string, fn: (event: StandIn<MediaQueryListEvent>) => void) => type === 'change' && listeners.add(fn),
    } satisfies StandIn<MediaQueryList>;
    Object.assign(globalThis, {
        window: {
            addEventListener: (type: string, fn: (event: StandIn<PointerEvent>) => void) => {
                windowListeners.set(type, [...(windowListeners.get(type) ?? []), fn]);
            },
            matchMedia: (query: string) => (query === '(pointer: fine)' ? pointer : { matches: false, addEventListener() {} }),
        } satisfies StandIn<Window>,
    });
    Object.defineProperty(globalThis, 'navigator', {
        value: { maxTouchPoints } satisfies StandIn<Navigator>, configurable: true, writable: true,
    });
    const mod = await import(`../../web/selkies-web-core/lib/hardware-keyboard.ts?${tag}`) as
        typeof import('../../web/selkies-web-core/lib/hardware-keyboard.ts');
    const turn = (to: boolean): void => {
        pointer.matches = to;
        for (const fn of listeners) fn({ matches: to });
    };
    const turnSilently = (to: boolean): void => { pointer.matches = to; };
    const fire = (type: string, event: StandIn<PointerEvent> = {}): void => {
        for (const fn of windowListeners.get(type) ?? []) fn(event);
    };
    return { watch: mod.hardwareKeyboard(), turn, turnSilently, fire };
}
{
    const { watch, turn } = await pageWatchOn(true, 10, 'surface');
    check('a touch screen opened with a fine pointer counts as one with a keyboard', watch.attached() === true);
    turn(false);
    check('the page follows its pointer turning coarse', watch.attached() === false);
    turn(true);
    check('and fine again', watch.attached() === true);
}
{
    const { watch, turn, turnSilently, fire } = await pageWatchOn(true, 10, 'missed');
    turn(false);
    turnSilently(true);
    fire('resize');
    check('a pointer turned fine without its event is caught on the next resize', watch.attached() === true);
    turn(false);
    turnSilently(true);
    fire('pointermove', { pointerType: 'touch', isTrusted: true });
    check('but not on a touch', watch.attached() === false);
    fire('pointermove', { pointerType: 'mouse', isTrusted: true });
    check('or on the next mouse or touchpad event', watch.attached() === true);
    turn(false);
    fire('pointermove', { pointerType: 'mouse', isTrusted: false });
    check('a pointer event the page made says nothing', watch.attached() === false);
    fire('pointerdown', { pointerType: 'mouse', isTrusted: true });
    check("with the pointer left coarse, the convertible's touchpad says its keyboard is back",
          watch.attached() === true);
}
{
    const { watch, turn } = await pageWatchOn(true, 0, 'desktop');
    turn(false);
    turn(true);
    check('a screen without touch is told only by its keys', watch.attached() === false);
}

console.log(`[hw-keyboard] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
