/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Whether a touch device has a keyboard attached, told from its pointer and the
 * keys it types.
 *
 * The dashboards' on-screen keyboard button does nothing on a tablet with a
 * keyboard attached, since the system keeps its on-screen keyboard down then,
 * so the button goes once one is in use. No web API says a keyboard is there,
 * but its keys do: an on-screen keyboard has no Escape, Tab, arrows, function
 * or editing keys, and no Control, Alt or Command to chord with, where every
 * keyboard has them. Letters are no evidence: an engine may report an
 * on-screen key with the code of the key it stands for. So the first of those
 * keys, or a chord, says a keyboard is attached; the verdict lasts while such
 * keys keep coming, and a touch after `HARDWARE_KEYBOARD_IDLE_MS` without one
 * takes it back, so a keyboard detached mid-session leaves the on-screen one
 * reachable again.
 *
 * A convertible says it at once where the engine follows its posture. Windows
 * leaves its tablet posture when a Surface's keyboard is attached, and Edge
 * turns the primary pointer from coarse to fine with the touchpad that comes
 * with it, and back when the keyboard is detached (Edge 154 on a Surface Pro
 * 8, within 0.1 s both ways). So on a touch screen a fine primary pointer, at
 * load or on a change, says a keyboard is attached until the pointer turns
 * coarse again, keys or no keys.
 * @module
 */

/** How long after its last key an attached keyboard stops being assumed, on the next touch. */
export const HARDWARE_KEYBOARD_IDLE_MS = 2 * 60 * 1000;

/** Codes only a physical keyboard reports. */
const KEYBOARD_ONLY =
    /^(Escape|Tab|Arrow(Up|Down|Left|Right)|F\d{1,2}|Home|End|Page(Up|Down)|Delete|Insert|CapsLock|Control(Left|Right)|Alt(Left|Right)|Meta(Left|Right)|OS(Left|Right))$/;

/**
 * Whether a keydown came from a physical keyboard: one of the keys an
 * on-screen keyboard lacks, or any key chorded with Control, Alt or Command.
 * @param {KeyboardEvent} event
 * @returns {boolean}
 */
export function isKeyboardOnlyKeystroke(event) {
    // A key the page dispatched itself (a soft button's) is no keyboard's.
    if (!event || event.isTrusted === false || event.isComposing || event.keyCode === 229) return false;
    const code = event.code || '';
    if (!code || code === 'Unidentified') return false;
    return KEYBOARD_ONLY.test(code) || !!(event.ctrlKey || event.altKey || event.metaKey);
}

/**
 * A store of the verdict, fed keydowns, touches and the primary pointer.
 * @param {{now?: () => number, idleMs?: number}} [options]
 */
export function createHardwareKeyboardWatch({ now = () => Date.now(), idleMs = HARDWARE_KEYBOARD_IDLE_MS } = {}) {
    let attached = false;
    let lastKeyAt = -Infinity;
    // Whether the verdict rests on a fine primary pointer, which only its turning coarse ends.
    let byPointer = false;
    const subscribers = new Set();
    const set = (value) => {
        if (value === attached) return;
        attached = value;
        for (const fn of subscribers) fn(attached);
    };
    return {
        /** @returns {boolean} Whether a keyboard is taken to be attached. */
        attached: () => attached,
        /** @param {KeyboardEvent} event */
        onKeyDown(event) {
            if (!isKeyboardOnlyKeystroke(event)) return;
            lastKeyAt = now();
            set(true);
        },
        onTouch() {
            if (attached && !byPointer && now() - lastKeyAt >= idleMs) set(false);
        },
        /**
         * The primary pointer of a touch screen turned fine, a keyboard attached
         * with its touchpad, or coarse, the keyboard gone with the keys it typed.
         * @param {boolean} fine
         */
        onPointer(fine) {
            byPointer = fine;
            lastKeyAt = -Infinity;
            set(fine);
        },
        /**
         * Forgets the verdict until the next keyboard-only key or pointer change,
         * as when the user asks for the button back.
         */
        reset() {
            lastKeyAt = -Infinity;
            byPointer = false;
            set(false);
        },
        /**
         * @param {(attached: boolean) => void} fn
         * @returns {() => void} Unsubscribes.
         */
        subscribe(fn) {
            subscribers.add(fn);
            return () => subscribers.delete(fn);
        },
    };
}

let pageWatch = null;

/**
 * The page's watch, listening from its first use: keydowns and touches on the
 * window in the capture phase, before the stream's own handlers take them, and
 * on a touch screen the primary pointer.
 * @returns {ReturnType<typeof createHardwareKeyboardWatch>}
 */
export function hardwareKeyboard() {
    if (pageWatch) return pageWatch;
    pageWatch = createHardwareKeyboardWatch();
    if (typeof window !== 'undefined' && typeof window.addEventListener === 'function') {
        window.addEventListener('keydown', (e) => pageWatch.onKeyDown(e), { capture: true, passive: true });
        window.addEventListener('touchstart', () => pageWatch.onTouch(), { capture: true, passive: true });
        const fine = typeof window.matchMedia === 'function' ? window.matchMedia('(pointer: fine)') : null;
        if (fine && typeof navigator !== 'undefined' && navigator.maxTouchPoints > 0) {
            if (fine.matches) pageWatch.onPointer(true);
            fine.addEventListener('change', (e) => pageWatch.onPointer(e.matches));
        }
    }
    return pageWatch;
}
