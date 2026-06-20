/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// app.js
(function() {
    'use strict';

    const toggleButton = document.getElementById('toggle-gamepad-btn');
    const targetDivId = 'touch-gamepad-container';
    let isGamepadUIVisible = false;

    // Status display elements
    const gamepadInfoIdDiv = document.getElementById('gamepad-info-id');
    const gamepadInfoConnectedDiv = document.getElementById('gamepad-info-connected');
    const gamepadInfoIndexDiv = document.getElementById('gamepad-info-index');
    const gamepadInfoMappingDiv = document.getElementById('gamepad-info-mapping');
    const axesStatusDisplay = document.getElementById('axes-status-display');
    const buttonsStatusDisplay = document.getElementById('buttons-status-display');
    const MAX_BUTTONS_DISPLAY = 18;
    const MAX_AXES_DISPLAY = 4;

    for (let i = 0; i < MAX_AXES_DISPLAY; i++) {
        const div = document.createElement('div');
        div.id = `axis-stat-${i}`;
        axesStatusDisplay.appendChild(div);
    }
    for (let i = 0; i < MAX_BUTTONS_DISPLAY; i++) {
        const span = document.createElement('span');
        span.id = `button-stat-${i}`;
        span.className = 'button-state';
        buttonsStatusDisplay.appendChild(span);
        if ((i + 1) % 6 === 0 && i < MAX_BUTTONS_DISPLAY - 1) {
            buttonsStatusDisplay.appendChild(document.createElement('br'));
        }
    }

    // Initial setup message to the library
    window.postMessage({
        type: 'TOUCH_GAMEPAD_SETUP',
        payload: {
            targetDivId: targetDivId,
            // initialProfileName: 'default', // Library uses its own default if not specified
            visible: false
        }
    }, window.location.origin);


    toggleButton.addEventListener('click', () => {
        isGamepadUIVisible = !isGamepadUIVisible;
        window.postMessage({
            type: 'TOUCH_GAMEPAD_VISIBILITY',
            payload: {
                visible: isGamepadUIVisible,
                targetDivId: targetDivId // Good to include if lib might not have it from initial SETUP
            }
        }, window.location.origin);
        toggleButton.textContent = isGamepadUIVisible ? 'Hide Touch Gamepad' : 'Show Touch Gamepad';
    });

    function updateStatusDisplay() {
        const gamepads = navigator.getGamepads();
        let activeGamepad = null;
        for (let i = 0; i < gamepads.length; i++) {
            if (gamepads[i] && gamepads[i].id === "Universal Touch Gamepad") {
                activeGamepad = gamepads[i];
                break;
            }
        }

        if (activeGamepad && activeGamepad.connected) {
            gamepadInfoIdDiv.textContent = `ID: ${activeGamepad.id}`;
            gamepadInfoConnectedDiv.textContent = `Connected: ${activeGamepad.connected}`;
            gamepadInfoIndexDiv.textContent = `Index: ${activeGamepad.index}`;
            gamepadInfoMappingDiv.textContent = `Mapping: ${activeGamepad.mapping}`;

            for (let i = 0; i < MAX_AXES_DISPLAY; i++) {
                const axisDiv = document.getElementById(`axis-stat-${i}`);
                if (axisDiv) {
                    axisDiv.textContent = `Axis ${i}: ${(activeGamepad.axes[i] || 0).toFixed(2)}`;
                }
            }
            for (let i = 0; i < MAX_BUTTONS_DISPLAY; i++) {
                const btnSpan = document.getElementById(`button-stat-${i}`);
                if (btnSpan) {
                    const button = activeGamepad.buttons[i] || { value: 0, pressed: false };
                    btnSpan.textContent = `B${i}: ${button.value.toFixed(1)} (${button.pressed ? 'T' : 'F'})`;
                    btnSpan.classList.toggle('pressed', button.pressed);
                }
            }
        } else {
            gamepadInfoIdDiv.textContent = `ID: N/A`;
            gamepadInfoConnectedDiv.textContent = `Connected: false`;
            gamepadInfoIndexDiv.textContent = `Index: N/A`;
            gamepadInfoMappingDiv.textContent = `Mapping: N/A`;

            for (let i = 0; i < MAX_AXES_DISPLAY; i++) {
                const axisDiv = document.getElementById(`axis-stat-${i}`);
                if (axisDiv) {
                    axisDiv.textContent = `Axis ${i}: 0.00`;
                }
            }
            for (let i = 0; i < MAX_BUTTONS_DISPLAY; i++) {
                const btnSpan = document.getElementById(`button-stat-${i}`);
                if (btnSpan) {
                    btnSpan.textContent = `B${i}: 0.0 (F)`;
                    btnSpan.classList.remove('pressed');
                }
            }
        }
        requestAnimationFrame(updateStatusDisplay);
    }

    requestAnimationFrame(updateStatusDisplay);
    console.log("Test App Initialized. Sent SETUP to UniversalTouchGamepad.");
})();
