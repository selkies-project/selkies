/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The init script the gamepad suites give a page before its own scripts: one
 * synthetic standard gamepad that `navigator.getGamepads` returns in slot 0,
 * driven through `window.__padPress` and `window.__padAxis`, with every
 * `setInterval` the page starts counted in `window.__timers` (its period
 * under `created_<id>`, its runs under `<id>`, its clears under
 * `cleared_<id>`). A classic script once its types are stripped
 * (`tests/helpers.py` `pad_init_js`).
 * @module
 */

import type { StandIn } from './stand_in.ts';

/** One button of the synthetic pad. */
interface PadButton {
    pressed: boolean;
    touched: boolean;
    value: number;
}

/** The synthetic pad, as much of a `Gamepad` as the page reads. */
interface Pad {
    index: number;
    id: string;
    mapping: GamepadMappingType;
    connected: boolean;
    timestamp: number;
    buttons: PadButton[];
    axes: number[];
}

declare global {
    interface Window {
        __pad: Pad;
        /** How often the page has polled the pads. */
        __gpCalls: number;
        __padPress: (index: number, value: number) => void;
        __padAxis: (index: number, value: number) => void;
        __timers: Record<string, number | undefined>;
    }
}

window.__pad = {
    index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
    mapping: "standard", connected: true, timestamp: 1,
    buttons: Array.from({ length: 17 }, () => ({ pressed: false, touched: false, value: 0 })),
    axes: [0, 0, 0, 0],
};
window.__gpCalls = 0;
navigator.getGamepads = (): (Gamepad | null)[] => {
    window.__gpCalls++;
    return [window.__pad as StandIn<Gamepad> as Gamepad, null, null, null];
};
window.__padPress = (i: number, v: number): void => {
    window.__pad.buttons[i] = { pressed: v > 0, touched: v > 0, value: v };
    window.__pad.timestamp = performance.now();
};
window.__padAxis = (i: number, v: number): void => {
    window.__pad.axes[i] = v;
    window.__pad.timestamp = performance.now();
};

window.__timers = {};
(() => {
    const SI = window.setInterval.bind(window), CI = window.clearInterval.bind(window);
    window.setInterval = function (handler: TimerHandler, ms?: number, ...rest: unknown[]): number {
        // The pages pass functions; a string handler fails here as it did before the count.
        const fn = handler as (...args: unknown[]) => unknown;
        const id = SI(function (this: unknown, ...args: unknown[]): unknown {
            window.__timers[id] = (window.__timers[id] || 0) + 1;
            return fn.apply(this, args);
        }, ms, ...rest);
        window.__timers['created_' + id] = ms;
        return id;
    };
    window.clearInterval = function (id?: number): void {
        window.__timers['cleared_' + id] = (window.__timers['cleared_' + id] || 0) + 1;
        CI(id);
    };
})();
