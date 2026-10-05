/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Whether a touch device has a keyboard attached, told from the keys it types.
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
 * A store of the verdict, fed keydowns and touches.
 * @param {{now?: () => number, idleMs?: number}} [options]
 */
export function createHardwareKeyboardWatch({ now = () => Date.now(), idleMs = HARDWARE_KEYBOARD_IDLE_MS } = {}) {
    let attached = false;
    let lastKeyAt = -Infinity;
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
            if (attached && now() - lastKeyAt >= idleMs) set(false);
        },
        /** Forgets the verdict until the next keyboard-only key, as when the user asks for the button back. */
        reset() {
            lastKeyAt = -Infinity;
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
 * window in the capture phase, before the stream's own handlers take them.
 * @returns {ReturnType<typeof createHardwareKeyboardWatch>}
 */
export function hardwareKeyboard() {
    if (pageWatch) return pageWatch;
    pageWatch = createHardwareKeyboardWatch();
    if (typeof window !== 'undefined' && typeof window.addEventListener === 'function') {
        window.addEventListener('keydown', (e) => pageWatch.onKeyDown(e), { capture: true, passive: true });
        window.addEventListener('touchstart', () => pageWatch.onTouch(), { capture: true, passive: true });
    }
    return pageWatch;
}
