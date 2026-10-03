/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Gamepad polling for the streaming cores: reads `navigator.getGamepads()` on
 * a fixed interval and reports button and axis changes in the standard layout.
 * The interval is the rate Chromium samples pads at, so a change waits on the
 * page no longer than it already waited in the browser. The on-screen touch
 * gamepad announces its own changes (`touchgamepadinput`), and is read the
 * moment it does rather than on the next tick.
 *
 * A client drives one server slot, so of its local pads one drives it at a
 * time (`GamepadManager._choose`): a lone pad at once, and among several the
 * one last taken up, so a second device's resting or noisy controls (a flight
 * stick's pots, a throttle parked at an end) never write over the pad in use.
 *
 * Rumble goes the other way (`rumble`): the dual-rumble effect of the
 * Gamepad API's `vibrationActuator` (Chromium, WebKit), else Gecko's
 * `hapticActuators` pulse.
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
/** Poll interval in milliseconds: Chromium samples pads every 4 ms. */
export const GP_TIMEOUT = 4;
const MAX_GAMEPADS = 4;

/** Distance from center, in axis units, a stick rests within (`GamepadManager._deadzone`). */
const STICK_DEADZONE = 0.05;

/** The standard-layout axis pairs that are one stick each. */
const STICK_AXES = [[0, 1], [2, 3]];

/** How far a button or a centered stick goes before it takes a pad up (`GamepadManager._takenUp`). */
const TAKE_UP = 0.5;

/** The bit of standard axis 0 in a `_takenUp` mask; the buttons take the bits below it. */
const TAKE_UP_AXIS_BIT = 24;

/** A pad's state before anything it holds has been reported. */
const DPAD_REST = () => ({ 12: false, 13: false, 14: false, 15: false });

/** The longest effect the Gamepad API plays at once, in milliseconds. */
const RUMBLE_MAX_MS = 5000;

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
     * @param {((gamepad: Gamepad|null, switched: boolean) => void)=} onActive
     *     Called with the pad that now drives the slot, or null when none
     *     does; `switched` when it replaces another, whose held controls the
     *     slot must drop, rather than announcing the same pad again.
     */
    constructor(gamepad, onButton, onAxis, onHeld, onActive) {
        this.gamepad = gamepad;
        this.onButton = onButton;
        this.onAxis = onAxis;
        this.onHeld = onHeld || null;
        this.onActive = onActive || null;
        /** The browser index of the pad that drives the slot, or null. */
        this.active = null;
        /** Per pad, the controls that held it taken up at the last tick (`_takenUp`). */
        this._takenUpAt = {};
        this._lastHeldBeat = 0;
        this.state = {};
        this._active = true;
        this.interval = setInterval(() => {
            this._poll();
        }, GP_TIMEOUT);
        this._onTouchInput = () => this._poll();
        window.addEventListener('touchgamepadinput', this._onTouchInput);
        /** Pads play the rumble relayed to them (the dashboards' toggle). */
        this.rumbleEnabled = true;
        this._rumbling = false;
    }

    /** Resumes polling. */
    enable() {
        if (!this._active) {
            this._active = true;
            console.log("GamepadManager polling activated.");
        }
    }

    /** Pauses polling, and stops a rumble playing; the per-pad state is kept. */
    disable() {
        if (this._active) {
            this._active = false;
            this.stopRumble();
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
        this._choose(gamepads);
        for (let i = 0; i < MAX_GAMEPADS; i++) {
            const currentGp = gamepads[i];
            if (currentGp) {
                let gpState = this.state[i];

                if (!gpState) {
                    gpState = this.state[i] = {
                        axes: new Array(currentGp.axes.length).fill(0),
                        buttons: new Array(currentGp.buttons.length).fill(0),
                        dpadAxisState: DPAD_REST(),
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
                if (i !== this.active) continue;

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

                    // Firefox swaps X/Y only on pads it could not map to the standard
                    // layout; a standard-mapped pad (the touch gamepad too) must not be re-swapped.
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

                const axes = this._deadzone(currentGp, gpState);
                for (let x = 0; x < currentGp.axes.length; x++) {
                    if (currentGp.axes[x] === undefined) continue;

                    const val = axes[x];

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
     * Picks the pad that drives the slot. A lone pad drives it; among several,
     * the one whose control was taken up last (`_takenUp`) takes over, and
     * until one is, none does. A pad that went away gives the slot up.
     * @param {(Gamepad|null)[]} gamepads
     */
    _choose(gamepads) {
        let count = 0;
        let lone = null;
        let takenUp = null;
        for (let i = 0; i < MAX_GAMEPADS; i++) {
            const gp = gamepads[i];
            if (!gp) {
                delete this._takenUpAt[i];
                continue;
            }
            count++;
            lone = i;
            const now = this._takenUp(gp);
            if ((now & ~(this._takenUpAt[i] || 0)) && takenUp === null && i !== this.active) takenUp = i;
            this._takenUpAt[i] = now;
        }
        let next = (this.active !== null && gamepads[this.active]) ? this.active : null;
        if (takenUp !== null) next = takenUp;
        else if (next === null && count === 1) next = lone;
        if (next !== this.active) this._setActive(next, next === null ? null : gamepads[next]);
    }

    /**
     * The controls a pad is being used with, as a mask: each button pressed
     * past `TAKE_UP`, and, on a standard-mapped pad, whose stick axes rest
     * centered, each stick axis pushed past it. An unmapped pad's axes are
     * left out, since they may rest anywhere. A control that joins the mask
     * takes the pad up, whatever else it holds.
     * @param {Gamepad} gp
     * @returns {number}
     */
    _takenUp(gp) {
        let mask = 0;
        for (let x = 0; x < gp.buttons.length && x < TAKE_UP_AXIS_BIT; x++) {
            const b = gp.buttons[x];
            if (b && b.value >= TAKE_UP) mask |= 1 << x;
        }
        if (gp.mapping === 'standard') {
            for (let x = 0; x < 4 && x < gp.axes.length; x++) {
                if (Math.abs(gp.axes[x] || 0) >= TAKE_UP) mask |= 1 << (TAKE_UP_AXIS_BIT + x);
            }
        }
        return mask;
    }

    /**
     * Hands the slot to pad `next` (null: to none). Neither pad's controls
     * count as reported any more, so the next tick sends the new pad's whole
     * state onto the slot the switch cleared.
     * @param {number|null} next
     * @param {Gamepad|null} gp The pad at `next`.
     */
    _setActive(next, gp) {
        const switched = this.active !== null;
        for (const i of [this.active, next]) {
            this._unreport(i);
        }
        this.active = next;
        if (this.onActive) this.onActive(gp, switched);
    }

    /** Forgets what pad `i` reported, as if it had held nothing. */
    _unreport(i) {
        const s = i === null ? null : this.state[i];
        if (!s) return;
        s.buttons.fill(0);
        s.axes.fill(0);
        s.dpadAxisState = DPAD_REST();
    }

    /**
     * A pad the browser reports gone: if it drove the slot, the slot is given
     * up at once, even while polling is paused.
     * @param {number} index
     */
    padGone(index) {
        delete this._takenUpAt[index];
        delete this.state[index];
        if (this.active === index) this._setActive(null, null);
    }

    /**
     * Announces the pad that drives the slot again, and has its whole state
     * sent anew: the slot changed, or a channel reopened.
     */
    reannounce() {
        if (this.active === null) return;
        let gp;
        try {
            gp = navigator.getGamepads()[this.active];
        } catch (e) {
            return;
        }
        if (!gp) return;
        this._unreport(this.active);
        if (this.onActive) this.onActive(gp, false);
    }

    /**
     * The pad's axes with the rest noise of its sticks cut. A stick is one
     * point, so it rests while that point is within `STICK_DEADZONE` of center
     * and reads as reported past it: cutting each axis on its own pins the
     * minor axis of a push near a cardinal direction to zero, and then jumps
     * it. Only axes known to pair as a stick are taken together, a
     * standard-mapped pad's first four or what its remap profile names;
     * any other axis is cut on its own. The values past the cut are left
     * as they are, since the game applies its own deadzone to them.
     * @param {Gamepad} gp
     * @param {object} state The pad's entry in `this.state`.
     * @returns {number[]}
     */
    _deadzone(gp, state) {
        const out = state.deadzoned || (state.deadzoned = []);
        out.length = gp.axes.length;
        for (let x = 0; x < gp.axes.length; x++) {
            const v = gp.axes[x];
            out[x] = (v === undefined || Math.abs(v) < STICK_DEADZONE) ? 0 : v;
        }
        for (const [a, b] of STICK_AXES) {
            const ra = this._rawAxis(gp, state, a);
            const rb = this._rawAxis(gp, state, b);
            if (ra < 0 || rb < 0) continue;
            const rest = Math.hypot(gp.axes[ra] || 0, gp.axes[rb] || 0) < STICK_DEADZONE;
            out[ra] = rest ? 0 : (gp.axes[ra] || 0);
            out[rb] = rest ? 0 : (gp.axes[rb] || 0);
        }
        return out;
    }

    /**
     * The raw index of a standard-layout axis on this pad, or -1 where the
     * pad's layout does not say which it is.
     * @param {Gamepad} gp
     * @param {object} state
     * @param {number} standard
     * @returns {number}
     */
    _rawAxis(gp, state, standard) {
        if (gp.mapping === 'standard') return standard < gp.axes.length ? standard : -1;
        if (!state.remapProfile) return -1;
        for (const raw in state.remapProfile.axes) {
            if (state.remapProfile.axes[raw] === standard) return Number(raw);
        }
        return -1;
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

    /**
     * Plays a rumble on every connected pad that can: both motors for
     * `durationMs`, at most the Gamepad API's 5 s, a new call replacing the
     * one before; 0 on both motors stops it. Gecko's pulse has one motor,
     * which takes the stronger level. Nothing plays while rumble is off or
     * polling is paused, and a stop with nothing playing is not sent.
     * @param {number} strong Strong (low-frequency) motor, 0 to 1.
     * @param {number} weak Weak (high-frequency) motor, 0 to 1.
     * @param {number} durationMs
     */
    rumble(strong, weak, durationMs) {
        const off = !(strong > 0 || weak > 0) || !this.rumbleEnabled || !this._active;
        if (off && !this._rumbling) return;
        this._rumbling = !off;
        durationMs = Math.min(RUMBLE_MAX_MS, Math.max(0, durationMs || 0));
        let pads;
        try {
            pads = Array.from(navigator.getGamepads());
        } catch (e) {
            return;
        }
        for (const pad of pads) {
            if (!pad || !pad.connected) continue;
            const actuator = pad.vibrationActuator;
            if (actuator && typeof actuator.playEffect === 'function') {
                const done = (off && typeof actuator.reset === 'function')
                    ? actuator.reset()
                    : actuator.playEffect('dual-rumble', {
                        startDelay: 0, duration: off ? 0 : durationMs,
                        strongMagnitude: off ? 0 : strong, weakMagnitude: off ? 0 : weak,
                    });
                if (done && typeof done.catch === 'function') done.catch(() => {});
                continue;
            }
            const haptic = pad.hapticActuators && pad.hapticActuators[0];
            if (haptic && typeof haptic.pulse === 'function') {
                const done = haptic.pulse(off ? 0 : Math.max(strong, weak), off ? 0 : durationMs);
                if (done && typeof done.catch === 'function') done.catch(() => {});
            }
        }
    }

    /** Stops a rumble playing on the pads, if one is. */
    stopRumble() {
        this.rumble(0, 0, 0);
    }

    /**
     * Turns rumble on or off; off stops one playing.
     * @param {boolean} on
     */
    setRumbleEnabled(on) {
        this.rumbleEnabled = !!on;
        if (!this.rumbleEnabled) this.stopRumble();
    }

    /** Stops polling and any rumble, and forgets every pad. */
    destroy() {
        this.stopRumble();
        clearInterval(this.interval);
        window.removeEventListener('touchgamepadinput', this._onTouchInput);
        this.state = {};
        console.log("GamepadManager destroyed.");
    }
}
