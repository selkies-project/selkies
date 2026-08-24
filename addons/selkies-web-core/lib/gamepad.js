/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Gamepad polling for the streaming cores: reads `navigator.getGamepads()` on
 * a fixed interval and reports button and axis changes in the standard layout.
 *
 * Pads the browser could not map to the standard layout are remapped through
 * the per-platform profile database that gendb.js generates: raw button and
 * axis indices differ across platforms for the same pad, so the lookup is
 * scoped to the platform this browser runs on, and anything unmatched
 * (ChromeOS, the BSDs) uses the evdev layout Linux browsers report. Such pads
 * also carry the D-pad on axes 4 and 5, which is translated to the standard
 * buttons 12 to 15.
 * @module
 */

/** SDL control names to standard-layout indices, the target of every remap. */
const STANDARD_LAYOUT = {
    buttons: {
        'a': 0, 'b': 1, 'x': 2, 'y': 3,
        'leftshoulder': 4, 'rightshoulder': 5,
        'lefttrigger': 6, 'righttrigger': 7,
        'back': 8, 'start': 9,
        'leftstick': 10, 'rightstick': 11,
        'dpup': 12, 'dpdown': 13, 'dpleft': 14, 'dpright': 15,
        'guide': 16
    },
    axes: {
        'leftx': 0, 'lefty': 1, 'rightx': 2, 'righty': 3
    }
};

/*eslint no-unused-vars: ["error", { "vars": "local" }]*/
/** Poll interval in milliseconds. */
export const GP_TIMEOUT = 16;
const MAX_GAMEPADS = 4;

/** The remap database platform this browser's pads are looked up under. */
const JSDB_PLATFORM = (() => {
    const ua = (typeof navigator !== 'undefined' && navigator.userAgent) || '';
    if (/iPhone|iPad|iPod/i.test(ua)) return 'ios';
    if (/Android/i.test(ua)) return 'android';
    if (/Windows/i.test(ua)) return 'windows';
    if (/Macintosh|Mac OS X/i.test(ua)) return 'mac';
    return 'linux';
})();

/**
 * Polls every connected pad and reports changes through callbacks.
 *
 * Polling starts in the constructor and runs until `destroy`; `enable` and
 * `disable` pause it without losing per-pad state.
 */
export class GamepadManager {
    /**
     * @param {Gamepad|null} gamepad Pad the manager was created for, kept for the caller.
     * @param {(index: number, button: number, value: number, pressed: boolean) => void} onButton
     *     Called with the pad slot and standard-layout button index on every change.
     * @param {(index: number, axis: number, value: number) => void} onAxis
     *     Called with the pad slot and standard-layout axis index on every change.
     * @param {(() => void)=} onHeld Called about ten times a second while any
     *     control is away from rest, so the server can neutralize a held pad
     *     whose client died without a transport close.
     */
    constructor(gamepad, onButton, onAxis, onHeld) {
        this.gamepad = gamepad;
        this.onButton = onButton;
        this.onAxis = onAxis;
        this.onHeld = onHeld || null;
        this._lastHeldBeat = 0;
        this.state = {};
        this._active = true;
        this.interval = setInterval(() => {
            this._poll();
        }, GP_TIMEOUT);
    }

    /** Resumes polling. */
    enable() {
        if (!this._active) {
            this._active = true;
            console.log("GamepadManager polling activated.");
        }
    }

    /** Pauses polling; the per-pad state is kept. */
    disable() {
        if (this._active) {
            this._active = false;
            console.log("GamepadManager polling deactivated.");
        }
    }

    /**
     * Loads a pad's remap profile and stores it on the pad's state as a map
     * from raw to standard-layout indices; a missing or unreadable profile
     * leaves the pad on the browser's mapping.
     * @param {string} gamepadId The pad's `vendor-product` id, four hex digits each.
     * @param {object} state The pad's entry in `this.state`.
     */
    async _loadRemapProfile(gamepadId, state) {
        state.loadingProfile = true;
        const url = `jsdb/${JSDB_PLATFORM}/${gamepadId}.json`;

        try {
            console.log(`Attempting to load mapping for ${gamepadId} from ${url}`);
            const response = await fetch(url);

            if (!response.ok) {
                if (response.status === 404) {
                    console.log(`No custom mapping file found for ${gamepadId}. Using browser default.`);
                } else {
                    console.warn(`Failed to load mapping for ${gamepadId} (HTTP Status: ${response.status})`);
                }
                state.remapProfile = null;
                return;
            }

            const dbEntryMapping = await response.json();
            console.log(`Successfully loaded and applying custom mapping for: ${gamepadId}`);

            const reverseMap = { buttons: {}, axes: {} };
            for (const sdlName in dbEntryMapping) {
                const raw = dbEntryMapping[sdlName];
                if (raw.type === 'button') {
                    const standardIndex = STANDARD_LAYOUT.buttons[sdlName];
                    if (standardIndex !== undefined) {
                        reverseMap.buttons[raw.index] = standardIndex;
                    }
                } else if (raw.type === 'axis') {
                    const standardIndex = STANDARD_LAYOUT.axes[sdlName];
                    if (standardIndex !== undefined) {
                        reverseMap.axes[raw.index] = standardIndex;
                    }
                }
            }
            state.remapProfile = reverseMap;

        } catch (error) {
            console.error(`Error fetching or parsing mapping file for ${gamepadId}:`, error);
            state.remapProfile = null;
        }
    }

    /**
     * One polling tick: reports changed buttons and axes for every connected
     * pad and forgets pads that disconnected.
     */
    _poll() {
        if (!this._active) {
            return;
        }
        const gamepads = navigator.getGamepads();
        for (let i = 0; i < MAX_GAMEPADS; i++) {
            const currentGp = gamepads[i];
            if (currentGp) {
                let gpState = this.state[i];

                if (!gpState) {
                    gpState = this.state[i] = {
                        axes: new Array(currentGp.axes.length).fill(0),
                        buttons: new Array(currentGp.buttons.length).fill(0),
                        dpadAxisState: { 12: false, 13: false, 14: false, 15: false },
                        remapProfile: null,
                        loadingProfile: false,
                    };

                    if (currentGp.mapping !== 'standard') {
                        const match = currentGp.id.match(/Vendor: ([0-9a-f]{4}) Product: ([0-9a-f]{4})/i);
                        if (match && !gpState.loadingProfile) {
                            const vendor = match[1].toLowerCase();
                            const product = match[2].toLowerCase();
                            const gamepadId = `${vendor}-${product}`;
                            this._loadRemapProfile(gamepadId, gpState);
                        }
                    }
                }

                if (gpState.buttons.length !== currentGp.buttons.length) {
                    gpState.buttons = new Array(currentGp.buttons.length).fill(0);
                }
                if (gpState.axes.length !== currentGp.axes.length) {
                    gpState.axes = new Array(currentGp.axes.length).fill(0);
                }

                for (let x = 0; x < currentGp.buttons.length; x++) {
                    if (currentGp.buttons[x] === undefined) continue;
                    const value = currentGp.buttons[x].value;
                    const pressed = currentGp.buttons[x].pressed;
                    let buttonIndex = x;

                    // Firefox reports X/Y swapped only for pads it could not map
                    // to the standard layout; a pad that declares standard mapping
                    // (including the synthetic touch gamepad) is already in
                    // standard order and must not be re-swapped.
                    if (currentGp.mapping !== "standard" && navigator.userAgent.includes("Firefox")) {
                        if (x === 2) buttonIndex = 3;
                        else if (x === 3) buttonIndex = 2;
                    }

                    if (gpState.buttons[x] !== value) {
                        if (gpState.remapProfile) {
                            const standardIndex = gpState.remapProfile.buttons[buttonIndex];
                            if (standardIndex !== undefined) {
                                buttonIndex = standardIndex;
                            } else {
                                continue;
                            }
                        }
                        this.onButton(i, buttonIndex, value, pressed);
                        gpState.buttons[x] = value;
                    }
                }

                for (let x = 0; x < currentGp.axes.length; x++) {
                    if (currentGp.axes[x] === undefined) continue;

                    let val = currentGp.axes[x];
                    if (Math.abs(val) < 0.05) val = 0;

                    if (gpState.axes[x] !== val) {
                        const isUniversalDpadAxis = (currentGp.mapping !== 'standard' && (x === 4 || x === 5));

                        if (!isUniversalDpadAxis) {
                            let axisIndex = x;
                            if (gpState.remapProfile && gpState.remapProfile.axes[x] !== undefined) {
                                axisIndex = gpState.remapProfile.axes[x];
                            }
                            this.onAxis(i, axisIndex, val);
                        }
                        
                        gpState.axes[x] = val;
                    }
                }

                if (currentGp.mapping !== 'standard' && currentGp.axes.length >= 6) {
                    const axisThreshold = 0.5;
                    // Axes 4/5 carry the D-pad on these pads; the flags map to the
                    // standard buttons 12 (up), 13 (down), 14 (left) and 15 (right).
                    const dpad = {
                        up: currentGp.axes[5] < -axisThreshold,
                        down: currentGp.axes[5] > axisThreshold,
                        left: currentGp.axes[4] < -axisThreshold,
                        right: currentGp.axes[4] > axisThreshold,
                    };

                    if (dpad.up !== gpState.dpadAxisState[12]) {
                        this.onButton(i, 12, dpad.up ? 1 : 0, dpad.up);
                        gpState.dpadAxisState[12] = dpad.up;
                    }
                    if (dpad.down !== gpState.dpadAxisState[13]) {
                        this.onButton(i, 13, dpad.down ? 1 : 0, dpad.down);
                        gpState.dpadAxisState[13] = dpad.down;
                    }
                    if (dpad.left !== gpState.dpadAxisState[14]) {
                        this.onButton(i, 14, dpad.left ? 1 : 0, dpad.left);
                        gpState.dpadAxisState[14] = dpad.left;
                    }
                    if (dpad.right !== gpState.dpadAxisState[15]) {
                        this.onButton(i, 15, dpad.right ? 1 : 0, dpad.right);
                        gpState.dpadAxisState[15] = dpad.right;
                    }
                }

            } else if (this.state[i]) {
                delete this.state[i];
            }
        }

        if (this.onHeld && this._anyHeld()) {
            const now = Date.now();
            if (now - this._lastHeldBeat >= 100) {
                this._lastHeldBeat = now;
                this.onHeld();
            }
        }
    }

    /**
     * True while any tracked pad has a button pressed or an axis away from
     * rest. Axes that idle off-zero (some trigger conventions) read as held;
     * that only sustains the heartbeat, which is harmless.
     */
    _anyHeld() {
        for (const i in this.state) {
            const s = this.state[i];
            if (s.buttons.some((v) => v !== 0) || s.axes.some((v) => v !== 0)) {
                return true;
            }
        }
        return false;
    }

    /** Stops polling and forgets every pad. */
    destroy() {
        clearInterval(this.interval);
        this.state = {};
        console.log("GamepadManager destroyed.");
    }
}
