/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

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
export const GP_TIMEOUT = 16;
const MAX_GAMEPADS = 4;

export class GamepadManager {
    constructor(gamepad, onButton, onAxis) {
        this.gamepad = gamepad;
        this.onButton = onButton;
        this.onAxis = onAxis;
        this.state = {};
        this._active = true;
        this.interval = setInterval(() => {
            this._poll();
        }, GP_TIMEOUT);
    }

    enable() {
        if (!this._active) {
            this._active = true;
            console.log("GamepadManager polling activated.");
        }
    }

    disable() {
        if (this._active) {
            this._active = false;
            console.log("GamepadManager polling deactivated.");
        }
    }

    /**
     * Asynchronously loads a remap profile for a given gamepad ID.
     * @param {string} gamepadId The "vendor-product" ID of the gamepad.
     * @param {object} state The internal state object for the specific gamepad.
     */
    async _loadRemapProfile(gamepadId, state) {
        state.loadingProfile = true;
        const url = `jsdb/${gamepadId}.json`;

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

                // --- Button Polling ---
                for (let x = 0; x < currentGp.buttons.length; x++) {
                    if (currentGp.buttons[x] === undefined) continue;
                    const value = currentGp.buttons[x].value;
                    const pressed = currentGp.buttons[x].pressed;
                    let buttonIndex = x;

                    if (navigator.userAgent.includes("Firefox")) {
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

                // --- Axis Polling ---
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

                // --- D-Pad Axis Remapping for Non-Standard Controllers ---
                if (currentGp.mapping !== 'standard' && currentGp.axes.length >= 6) {
                    const axisThreshold = 0.5;
                    const dpad = {
                        up: currentGp.axes[5] < -axisThreshold,    // Standard Button 12
                        down: currentGp.axes[5] > axisThreshold,  // Standard Button 13
                        left: currentGp.axes[4] < -axisThreshold,   // Standard Button 14
                        right: currentGp.axes[4] > axisThreshold, // Standard Button 15
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
                // Gamepad disconnected
                delete this.state[i];
            }
        }
    }

    destroy() {
        clearInterval(this.interval);
        this.state = {}; // Clear state on final destruction
        console.log("GamepadManager destroyed.");
    }
}
