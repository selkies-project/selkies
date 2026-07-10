/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// universalTouchGamepad.js
(function() {
    'use strict';

    const GAMEPAD_ID = "Universal Touch Gamepad";
    const MAX_BUTTONS = 18;
    const MAX_AXES = 4;
    const PREVIEW_SCALE = 0.15;

    const SAFE_AREA_PADDING = { top: 10, right: 15, bottom: 10, left: 15 };
    const HIT_TEST_SLOP = 10;

    const STICK_TAP_DURATION_THRESHOLD = 250;
    const STICK_TAP_MOVEMENT_THRESHOLD_FACTOR = 0.25;
    const STICK_BUTTON_PRESS_DURATION = 60;

    const L3_BUTTON_INDEX = 10;
    const R3_BUTTON_INDEX = 11;

    const SETTINGS_ICON_SVG = `
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" fill="currentColor">
            <path d="M424.5 216.5h-15.2c-12.4 0-22.8-10.7-22.8-23.4 0-6.4 2.7-12.2 7.5-16.5l9.8-9.6c9.7-9.6 9.7-25.3 0-34.9L381.5 110c-4.4-4.4-10.9-7-17.5-7s-13 2.6-17.5 7l-9.4 9.4c-4.5 5-10.5 7.7-17 7.7-12.8 0-23.5-10.4-23.5-22.7V89.1c0-13.5-10.9-25.1-24.5-25.1h-30.4c-13.6 0-24.4 11.5-24.4 25.1v15.2c0 12.3-10.7 22.7-23.5 22.7-6.4 0-12.3-2.7-16.6-7.4l-9.7-9.6c-4.4-4.5-10.9-7-17.5-7s-13 2.6-17.5 7L110 132c-9.6 9.6-9.6 25.3 0 34.8l9.4 9.4c5 4.5 7.8 10.5 7.8 16.9 0 12.8-10.4 23.4-22.8 23.4H89.2c-13.7 0-25.2 10.7-25.2 24.3V256v15.2c0 13.5 11.5 24.3 25.2 24.3h15.2c12.4 0 22.8 10.7 22.8 23.4 0 6.4-2.8 12.4-7.8 16.9l-9.4 9.3c-9.6 9.6-9.6 25.3 0 34.8l22.3 22.2c4.4 4.5 10.9 7 17.5 7s13-2.6 17.5-7l9.7-9.6c4.2-4.7 10.2-7.4 16.6-7.4 12.8 0 23.5 10.4 23.5 22.7V423c0 13.5 10.8 25.1 24.5 25.1H272c13.6 0 24.4-11.5 24.4-25.1v-15.2c0-12.3 10.7-22.7 23.5-22.7 6.4 0 12.4 2.8 17 7.7l9.4 9.4c4.5 4.4 10.9 7 17.5 7s13-2.6 17.5-7l22.3-22.2c9.6-9.6 9.6-25.3 0-34.9l-9.8-9.6c-4.8-4.3-7.5-10.2-7.5-16.5 0-12.8 10.4-23.4 22.8-23.4h15.2c13.6 0 23.3-10.7 23.3-24.3V256v-15.2c.2-13.6-9.5-24.3-23.1-24.3zM336.8 256h0c0 44.1-35.7 80-80 80s-80-35.9-80-80h0 0c0-44.1 35.7-80 80-80s80 35.9 80 80h0z"/>
        </svg>
    `;

    const UP_ARROW_SVG = `
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor">
            <path d="M4 14h16l-8-8z"/>
        </svg>
    `;

    const DOWN_ARROW_SVG = `
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor">
            <path d="M4 10h16l-8 8z"/>
        </svg>
    `;

    const LEFT_ARROW_SVG = `
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor">
            <path d="M14 4v16l-8-8z"/>
        </svg>
    `;

    const RIGHT_ARROW_SVG = `
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor">
            <path d="M10 4v16l8-8z"/>
        </svg>
    `;

    const HOME_ICON_SVG = `
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor">
            <path d="M10 20v-6h4v6h5v-8h3L12 3 2 12h3v8h5z"/>
        </svg>
    `;

    function setRealViewportHeight() {
      const vh = window.innerHeight * 0.01;
      document.documentElement.style.setProperty('--vh', `${vh}px`);
    }
    window.addEventListener('resize', setRealViewportHeight);
    window.addEventListener('orientationchange', setRealViewportHeight);
    setRealViewportHeight();

    let hostAnchorElement = null;
    let currentProfileName = 'modern';
    let isGamepadVisible = false;
    let activeTouchControls = [];
    let buttonElementsToTrack = {};
    let analogTriggersToTrack = {};

    let gamepadControlsOverlayElement = null;
    let settingsIconElement = null;
    let profileSelectorOverlayElement = null;
    let isProfileSelectorVisible = false;
    let styleSheet = null;

    let gamepadState = {
        id: GAMEPAD_ID,
        index: 0,
        connected: false,
        mapping: "standard",
        axes: new Array(MAX_AXES).fill(0.0),
        buttons: Array.from({ length: MAX_BUTTONS }, () => ({ pressed: false, touched: false, value: 0.0 })),
        timestamp: Date.now(),
    };

    const originalGetGamepads = navigator.getGamepads ? navigator.getGamepads.bind(navigator) : () => [];

    function overrideGamepadAPI() {
        navigator.getGamepads = function() {
            const nativeGamepads = originalGetGamepads();
            const allGamepads = [null, null, null, null];
            for (let i = 0; i < nativeGamepads.length && i < 4; i++) {
                if (nativeGamepads[i] && nativeGamepads[i].id !== GAMEPAD_ID) {
                    allGamepads[i] = nativeGamepads[i];
                }
            }
            if (gamepadState.connected) {
                let targetIndex = gamepadState.index;
                if (allGamepads[targetIndex] && allGamepads[targetIndex].id !== GAMEPAD_ID) {
                    targetIndex = allGamepads.findIndex(p => p === null || (p && p.id === GAMEPAD_ID));
                    if (targetIndex === -1 && allGamepads.length < 4) {
                        targetIndex = allGamepads.length;
                    } else if (targetIndex === -1) {
                         targetIndex = gamepadState.index; 
                    }
                }
                if(targetIndex >=0 && targetIndex < 4){
                    allGamepads[targetIndex] = gamepadState;
                    gamepadState.index = targetIndex;
                }
            }
            return allGamepads;
        };
    }

    function dispatchGamepadEvent(type) {
        const event = new Event(type);
        event.gamepad = gamepadState;
        window.dispatchEvent(event);
    }

    function updateGamepadButton(buttonIndex, pressed, analogValue = null) {
        if (buttonIndex < 0 || buttonIndex >= MAX_BUTTONS) return;
        const buttonState = gamepadState.buttons[buttonIndex];
        
        let newPressedState = pressed;
        let newValue = analogValue !== null ? Math.max(0, Math.min(1, analogValue)) : (pressed ? 1.0 : 0.0);

        const currentProfile = profiles[currentProfileName];
        const isAnalogButtonConfig = currentProfile?.analogTriggers?.find(t => t.buttonIndex === buttonIndex);
        
        if (isAnalogButtonConfig && analogValue !== null) {
            newPressedState = newValue > 0.05; 
        }

        if (buttonState.pressed !== newPressedState || buttonState.value !== newValue) {
            buttonState.pressed = newPressedState;
            buttonState.touched = newPressedState; 
            buttonState.value = newValue;
            gamepadState.timestamp = Date.now();
            if (isGamepadVisible && !gamepadState.connected) connectGamepad();
        }
    }

    function updateGamepadAxis(index, value) {
        if (index < 0 || index >= MAX_AXES) return;
        const clampedValue = Math.max(-1, Math.min(1, value));
        if (gamepadState.axes[index] !== clampedValue) {
            gamepadState.axes[index] = clampedValue;
            gamepadState.timestamp = Date.now();
            if (isGamepadVisible && !gamepadState.connected) connectGamepad();
        }
    }

    function connectGamepad() {
        if (!gamepadState.connected) {
            gamepadState.connected = true;
            dispatchGamepadEvent("gamepadconnected");
        }
    }

    function disconnectGamepad() {
         if (gamepadState.connected) {
            gamepadState.connected = false;
            gamepadState.axes.fill(0.0);
            gamepadState.buttons.forEach(b => { b.pressed = false; b.touched = false; b.value = 0.0; });
            dispatchGamepadEvent("gamepaddisconnected");
        }
        Object.values(buttonElementsToTrack).forEach(btnTrack => {
            if (btnTrack.element) { 
                btnTrack.activeTouchIds.clear();
                btnTrack.element.classList.remove('pressed');
            }
        });
        Object.values(analogTriggersToTrack).forEach(triggerTrack => {
            if (triggerTrack.element) {
                triggerTrack.activeTouchId = null;
                triggerTrack.element.classList.remove('pressed');
                if(triggerTrack.fillElement) triggerTrack.fillElement.style.height = '0%';
            }
        });
    }

    const profiles = {
        eightBit: {
            name: "8-bit",
            buttons: [
                { id: 'dpadUp', index: 12, label: UP_ARROW_SVG, style: { left: '70px', bottom: '130px', width: '50px', height: '50px' } },
                { id: 'dpadDown', index: 13, label: DOWN_ARROW_SVG, style: { left: '70px', bottom: '50px', width: '50px', height: '50px' } },
                { id: 'dpadLeft', index: 14, label: LEFT_ARROW_SVG, style: { left: '20px', bottom: '90px', width: '50px', height: '50px' } },
                { id: 'dpadRight', index: 15, label: RIGHT_ARROW_SVG, style: { left: '120px', bottom: '90px', width: '50px', height: '50px' } },
                { id: 'select', index: 8, label: 'SELECT', shape: 'squircle', style: { left: 'calc(50% - 70px)', bottom: '30px', width: '60px', height: '30px'} },
                { id: 'start', index: 9, label: 'START', shape: 'squircle', style: { right: 'calc(50% - 90px)', bottom: '30px', width: '60px', height: '30px' } },
                { id: 'buttonB_nes', index: 1, label: 'B', style: { right: '30px', bottom: '90px', width: '60px', height: '60px', borderRadius: '15px' } },
                { id: 'buttonA_nes', index: 0, label: 'A', style: { right: '110px', bottom: '90px', width: '60px', height: '60px', borderRadius: '15px' } },
            ],
            clusters: [
                { id: 'dpadCluster_8bit', style: { left: '10px', bottom: '40px', width: '170px', height: '150px' }, buttonIds: ['dpadUp', 'dpadDown', 'dpadLeft', 'dpadRight'] },
                { id: 'faceCluster_8bit', style: { right: '10px', bottom: '70px', width: '170px', height: '100px' }, buttonIds: ['buttonA_nes', 'buttonB_nes'] },
                { id: 'systemCluster_8bit', style: { left: 'calc(50% - 80px)', bottom: '20px', width: '160px', height: '50px' }, buttonIds: ['select', 'start'] }
            ]
        },
        sixteenBit: {
            name: "16-bit",
            buttons: [
                { id: 'dpadUp_snes', index: 12, label: UP_ARROW_SVG, style: { left: '70px', bottom: '130px', width: '50px', height: '50px' } },
                { id: 'dpadDown_snes', index: 13, label: DOWN_ARROW_SVG, style: { left: '70px', bottom: '50px', width: '50px', height: '50px' } },
                { id: 'dpadLeft_snes', index: 14, label: LEFT_ARROW_SVG, style: { left: '20px', bottom: '90px', width: '50px', height: '50px' } },
                { id: 'dpadRight_snes', index: 15, label: RIGHT_ARROW_SVG, style: { left: '120px', bottom: '90px', width: '50px', height: '50px' } },
                { id: 'select_snes', index: 8, label: 'SELECT', shape: 'squircle', style: { left: 'calc(50% - 70px)', bottom: '30px', width: '60px', height: '30px'} },
                { id: 'start_snes', index: 9, label: 'START', shape: 'squircle', style: { right: 'calc(50% - 90px)', bottom: '30px', width: '60px', height: '30px' } },
                { id: 'buttonY_snes', index: 3, label: 'Y', style: { right: '80px', bottom: '130px', width: '50px', height: '50px', borderRadius: '50%' } },
                { id: 'buttonX_snes', index: 2, label: 'X', style: { right: '130px', bottom: '90px', width: '50px', height: '50px', borderRadius: '50%' } },
                { id: 'buttonB_snes', index: 1, label: 'B', style: { right: '30px', bottom: '90px', width: '50px', height: '50px', borderRadius: '50%' } },
                { id: 'buttonA_snes', index: 0, label: 'A', style: { right: '80px', bottom: '50px', width: '50px', height: '50px', borderRadius: '50%' } },
                { id: 'L_snes', index: 4, label: 'L', type: 'digitalShoulder', style: { left: '40px', bottom: '220px', width: '100px', height: '35px' } },
                { id: 'R_snes', index: 5, label: 'R', type: 'digitalShoulder', style: { right: '40px', bottom: '220px', width: '100px', height: '35px' } },
            ],
            clusters: [
                { id: 'dpadCluster_snes', style: { left: '10px', bottom: '40px', width: '170px', height: '150px' }, buttonIds: ['dpadUp_snes', 'dpadDown_snes', 'dpadLeft_snes', 'dpadRight_snes'] },
                { id: 'faceCluster_snes', style: { right: '10px', bottom: '30px', width: '180px', height: '160px' }, buttonIds: ['buttonY_snes', 'buttonX_snes', 'buttonB_snes', 'buttonA_snes'] },
                { id: 'systemCluster_snes', style: { left: 'calc(50% - 80px)', bottom: '20px', width: '160px', height: '50px' }, buttonIds: ['select_snes', 'start_snes'] },
                { id: 'shoulderL_snes', style: { left: '30px', bottom: '210px', width: '120px', height: '55px' }, buttonIds: ['L_snes'] },
                { id: 'shoulderR_snes', style: { right: '30px', bottom: '210px', width: '120px', height: '55px' }, buttonIds: ['R_snes'] },
            ]
        },
        modern: {
            name: "Modern",
            joysticks: [
                { id: 'leftStick', axes: [0, 1], clickButtonIndex: L3_BUTTON_INDEX, style: { left: '50px', bottom: '120px', size: '90px' } },
                { id: 'rightStick', axes: [2, 3], clickButtonIndex: R3_BUTTON_INDEX, style: { right: '200px', bottom: '35px', size: '90px' } }
            ],
            buttons: [
                { id: 'dpadUp_mod', index: 12, label: UP_ARROW_SVG, style: { left: '160px', bottom: '90px', width: '40px', height: '40px' } },
                { id: 'dpadDown_mod', index: 13, label: DOWN_ARROW_SVG, style: { left: '160px', bottom: '30px', width: '40px', height: '40px' } },
                { id: 'dpadLeft_mod', index: 14, label: LEFT_ARROW_SVG, style: { left: '120px', bottom: '60px', width: '40px', height: '40px' } },
                { id: 'dpadRight_mod', index: 15, label: RIGHT_ARROW_SVG, style: { left: '200px', bottom: '60px', width: '40px', height: '40px' } },
                { id: 'home_mod', index: 16, label: HOME_ICON_SVG, style: { left: 'calc(50% - 32px)', bottom: '100px', width: '40px', height: '40px', borderRadius: '50%'} },
                { id: 'select_mod', index: 8, label: 'VIEW', shape: 'squircle', style: { left: 'calc(50% - 85px)', bottom: '50px', width: '60px', height: '30px'} },
                { id: 'start_mod', index: 9, label: 'MENU', shape: 'squircle', style: { right: 'calc(50% - 90px)', bottom: '50px', width: '60px', height: '30px' } },
                { id: 'buttonY_mod', index: 3, label: 'Y', style: { right: '125px', bottom: '180px', width: '45px', height: '45px', borderRadius: '50%' } },
                { id: 'buttonX_mod', index: 2, label: 'X', style: { right: '175px', bottom: '140px', width: '45px', height: '45px', borderRadius: '50%' } },
                { id: 'buttonB_mod', index: 1, label: 'B', style: { right: '75px', bottom: '140px', width: '45px', height: '45px', borderRadius: '50%' } },
                { id: 'buttonA_mod', index: 0, label: 'A', style: { right: '125px', bottom: '100px', width: '45px', height: '45px', borderRadius: '50%' } },
                { id: 'L1_mod', index: 4, label: 'L1', type: 'digitalShoulder', style: { left: '40px', bottom: '240px', width: '110px', height: '35px' } },
                { id: 'R1_mod', index: 5, label: 'R1', type: 'digitalShoulder', style: { right: '40px', bottom: '240px', width: '110px', height: '35px' } },
            ],
            analogTriggers: [
                { id: 'L2_mod', buttonIndex: 6, label: 'L2', style: { left: '40px', bottom: '285px', width: '110px', height: '45px' } },
                { id: 'R2_mod', buttonIndex: 7, label: 'R2', style: { right: '40px', bottom: '285px', width: '110px', height: '45px' } },
            ],
            clusters: [
                { id: 'dpadCluster_mod', style: { left: '110px', bottom: '20px', width: '140px', height: '120px' }, buttonIds: ['dpadUp_mod', 'dpadDown_mod', 'dpadLeft_mod', 'dpadRight_mod'] },
                { id: 'faceCluster_mod', style: { right: '55px', bottom: '80px', width: '175px', height: '155px' }, buttonIds: ['buttonY_mod', 'buttonX_mod', 'buttonB_mod', 'buttonA_mod'] },
                { id: 'systemCluster_mod', style: { left: 'calc(50% - 95px)', bottom: '40px', width: '190px', height: '120px' }, buttonIds: ['home_mod', 'select_mod', 'start_mod'] },
                { id: 'shoulderL1_mod', style: { left: '30px', bottom: '230px', width: '130px', height: '55px' }, buttonIds: ['L1_mod'] },
                { id: 'shoulderR1_mod', style: { right: '30px', bottom: '230px', width: '130px', height: '55px' }, buttonIds: ['R1_mod'] },
            ]
        }
    };

    if (Object.keys(profiles).length > 0 && !profiles[currentProfileName]) {
        currentProfileName = Object.keys(profiles)[0];
    }

    function injectBaseStyles() {
        if (styleSheet) return;
        const css = `
            .touch-gamepad-control {
                position: absolute;
                box-sizing: border-box;
                user-select: none; -webkit-user-select: none; -webkit-tap-highlight-color: transparent;
                display: flex; align-items: center; justify-content: center;
                font-family: Arial, sans-serif; font-weight: bold; color: white;
                transition: transform 0.05s ease-out, background-color 0.05s ease-out, box-shadow 0.1s ease-out;
                border: none;
                box-shadow: 0 2px 5px rgba(0,0,0,0.2), 0 0 0 1px rgba(255,255,255,0.1) inset;
                font-size: 14px;
                pointer-events: none;
            }
            .touch-button { background-color: rgba(80, 80, 80, 0.8); border-radius: 8px; }
            .touch-button.pressed, .touch-joystick-base.pressed, .touch-analog-trigger.pressed {
                background-color: rgba(50, 50, 50, 0.9) !important;
                transform: scale(0.96);
                box-shadow: 0 1px 2px rgba(0,0,0,0.3), 0 0 0 1px rgba(0,0,0,0.05) inset;
            }
            .touch-button.shape-squircle { border-radius: 20% / 35%; font-size: 9px; padding: 0 5px; }
            .touch-button.type-digitalShoulder { border-radius: 6px; font-size: 12px; }

            .touch-joystick-base { background-color: rgba(80, 80, 80, 0.6); border-radius: 50%; pointer-events: auto !important; }
            .touch-joystick-handle { background-color: rgba(50, 50, 50, 0.8); border-radius: 50%; position: absolute; }
            
            .touch-analog-trigger { background-color: rgba(70, 70, 70, 0.8); border-radius: 6px; overflow: hidden; font-size: 12px; pointer-events: auto !important; }
            .touch-analog-trigger-fill { position: absolute; bottom: 0; left: 0; width: 100%; background-color: rgba(150, 150, 150, 0.7); }
            
            .settings-icon-host {
                position: absolute; width: 36px; height: 36px; padding: 6px;
                background-color: rgba(0, 0, 0, 0.3); border-radius: 50%;
                display: flex; align-items: center; justify-content: center;
                cursor: pointer; z-index: 2010;
                pointer-events: auto; transition: background-color 0.1s ease-out;
            }
            .settings-icon-host:hover { background-color: rgba(0, 0, 0, 0.5); }
            .settings-icon-host svg { width: 24px; height: 24px; fill: rgba(255, 255, 255, 0.8); }

            .touch-gamepad-cluster {
                position: absolute;
                box-sizing: border-box;
                pointer-events: auto;
            }
            .touch-button svg {
                width: 70%; 
                height: 70%;
            }
        `;
        styleSheet = document.createElement('style');
        styleSheet.type = 'text/css';
        styleSheet.innerText = css;
        document.head.appendChild(styleSheet);
    }

    function parseStyleValue(valueStr, scale) {
        if (typeof valueStr !== 'string') {
            return (valueStr * scale) + 'px';
        }
        if (valueStr.toLowerCase().startsWith('calc(')) {
            if (scale !== 1.0 && !valueStr.includes('%')) {
                 return valueStr.replace(/(\d+(\.\d+)?)px/g, (match, p1) => (parseFloat(p1) * scale) + 'px');
            }
            return valueStr;
        }
        const numericalPart = parseFloat(valueStr);
        const unit = valueStr.replace(String(numericalPart), '');
        return (numericalPart * scale) + (unit || 'px');
    }

    function renderControlElements(profile, parentEl, scale = 1.0, isPreview = false) {
        if (!isPreview) {
            buttonElementsToTrack = {};
            analogTriggersToTrack = {};
            activeTouchControls = [];
        }

        if (profile.joysticks) {
            profile.joysticks.forEach(joyConfig => {
                const baseSizeUnscaled = parseFloat(joyConfig.style.size);
                const baseSize = baseSizeUnscaled * scale;
                const handleRelSizeFactor = 0.6;
                const handleSize = baseSize * handleRelSizeFactor;

                const base = document.createElement('div');
                base.className = 'touch-gamepad-control touch-joystick-base';
                
                const baseStyles = { width: `${baseSize}px`, height: `${baseSize}px` };
                for (const key in joyConfig.style) {
                    if (key === 'size') continue;
                    let finalValue = parseStyleValue(joyConfig.style[key], scale);
                    if (!isPreview) { 
                        if (key === 'left') finalValue = `calc(${parseStyleValue(joyConfig.style[key], 1.0)} + ${SAFE_AREA_PADDING.left}px)`;
                        else if (key === 'right') finalValue = `calc(${parseStyleValue(joyConfig.style[key], 1.0)} + ${SAFE_AREA_PADDING.right}px)`;
                        else if (key === 'bottom') finalValue = `calc(${parseStyleValue(joyConfig.style[key], 1.0)} + ${SAFE_AREA_PADDING.bottom}px)`;
                        else if (key === 'top') finalValue = `calc(${parseStyleValue(joyConfig.style[key], 1.0)} + ${SAFE_AREA_PADDING.top}px)`;
                    }
                    baseStyles[key] = finalValue;
                }
                Object.assign(base.style, baseStyles);

                const handle = document.createElement('div');
                handle.className = 'touch-joystick-handle'; 
                Object.assign(handle.style, { width: `${handleSize}px`, height: `${handleSize}px`, top: `${(baseSize - handleSize) / 2}px`, left: `${(baseSize - handleSize) / 2}px` });
                base.appendChild(handle);
                parentEl.appendChild(base);
                
                if (!isPreview) {
                    activeTouchControls.push({ element: base, type: 'joystick', config: joyConfig });
                    let activeTouchId = null;
                    
                    const onJoystickTouchStart = (e) => { 
                        e.preventDefault(); e.stopPropagation(); base.classList.add('pressed');
                        if (activeTouchId !== null && e.changedTouches[0].identifier !== activeTouchId) return;
                        activeTouchId = e.changedTouches[0].identifier;
                        base.dataset.touchStartTime = Date.now();
                        base.dataset.touchInitialClientX = e.changedTouches[0].clientX;
                        base.dataset.touchInitialClientY = e.changedTouches[0].clientY;
                        base.dataset.movedSignificant = "false";
                        updateStick(e.changedTouches[0], base, handle, baseSizeUnscaled, handleRelSizeFactor, scale);
                    };
                    const onJoystickTouchMove = (e) => { 
                        e.preventDefault(); e.stopPropagation(); if (activeTouchId === null) return;
                        for (let i = 0; i < e.changedTouches.length; i++) {
                            if (e.changedTouches[i].identifier === activeTouchId) {
                                updateStick(e.changedTouches[i], base, handle, baseSizeUnscaled, handleRelSizeFactor, scale);
                                const initialX = parseFloat(base.dataset.touchInitialClientX);
                                const initialY = parseFloat(base.dataset.touchInitialClientY);
                                const currentX = e.changedTouches[i].clientX;
                                const currentY = e.changedTouches[i].clientY;
                                const deltaX = Math.abs(currentX - initialX);
                                const deltaY = Math.abs(currentY - initialY);
                                const renderedBaseSize = base.getBoundingClientRect().width;
                                if (deltaX > renderedBaseSize * STICK_TAP_MOVEMENT_THRESHOLD_FACTOR || deltaY > renderedBaseSize * STICK_TAP_MOVEMENT_THRESHOLD_FACTOR) {
                                    base.dataset.movedSignificant = "true";
                                }
                                break;
                            }
                        }
                    };
                    const onJoystickTouchEnd = (e) => { 
                        e.preventDefault(); e.stopPropagation(); base.classList.remove('pressed');
                        if (activeTouchId === null) return;
                        for (let i = 0; i < e.changedTouches.length; i++) {
                            if (e.changedTouches[i].identifier === activeTouchId) {
                                const touchStartTime = parseInt(base.dataset.touchStartTime || '0');
                                const touchDuration = Date.now() - touchStartTime;
                                const movedSignificant = base.dataset.movedSignificant === "true";
                                if (joyConfig.clickButtonIndex !== undefined && touchDuration < STICK_TAP_DURATION_THRESHOLD && !movedSignificant) {
                                    const clickButtonIndex = joyConfig.clickButtonIndex;
                                    updateGamepadButton(clickButtonIndex, true);
                                    setTimeout(() => { updateGamepadButton(clickButtonIndex, false); }, STICK_BUTTON_PRESS_DURATION);
                                }
                                const currentBaseSizeScaled = base.getBoundingClientRect().width;
                                const currentHandleSizeScaled = currentBaseSizeScaled * handleRelSizeFactor;
                                handle.style.left = `${(currentBaseSizeScaled - currentHandleSizeScaled) / 2}px`;
                                handle.style.top = `${(currentBaseSizeScaled - currentHandleSizeScaled) / 2}px`;
                                updateGamepadAxis(joyConfig.axes[0], 0); updateGamepadAxis(joyConfig.axes[1], 0);
                                activeTouchId = null;
                                Object.assign(base.dataset, { touchStartTime: '0', movedSignificant: "false", touchInitialClientX: '0', touchInitialClientY: '0' });
                                break;
                            }
                        }
                    };
                    function updateStick(touch, stickBaseElement, handleElement, unscaledBaseSize, handleRelFactor, currentScale) { 
                        const rect = stickBaseElement.getBoundingClientRect();
                        const touchX = touch.clientX - rect.left; const touchY = touch.clientY - rect.top;
                        const currentBaseSize = rect.width; const currentHandleSize = currentBaseSize * handleRelFactor;
                        let x = touchX - currentBaseSize / 2; let y = touchY - currentBaseSize / 2;
                        const distance = Math.sqrt(x * x + y * y);
                        const maxDistance = (currentBaseSize - currentHandleSize) / 2;
                        if (distance > maxDistance && maxDistance > 0) { x = (x / distance) * maxDistance; y = (y / distance) * maxDistance; }
                        else if (maxDistance <= 0) { x = 0; y = 0;}
                        handleElement.style.left = `${x + (currentBaseSize - currentHandleSize) / 2}px`;
                        handleElement.style.top = `${y + (currentBaseSize - currentHandleSize) / 2}px`;
                        if (maxDistance > 0) { updateGamepadAxis(joyConfig.axes[0], x / maxDistance); updateGamepadAxis(joyConfig.axes[1], y / maxDistance * -1); }
                        else { updateGamepadAxis(joyConfig.axes[0], 0); updateGamepadAxis(joyConfig.axes[1], 0); }
                    }
                    base.addEventListener('touchstart', onJoystickTouchStart, { passive: false });
                    base.addEventListener('touchmove', onJoystickTouchMove, { passive: false });
                    base.addEventListener('touchend', onJoystickTouchEnd, { passive: false });
                    base.addEventListener('touchcancel', onJoystickTouchEnd, { passive: false });
                }
            });
        }

        if (profile.analogTriggers && !isPreview) {
            profile.analogTriggers.forEach(triggerConfig => {
                const trigger = document.createElement('div');
                trigger.className = 'touch-gamepad-control touch-analog-trigger';
                trigger.textContent = triggerConfig.label || '';
                const fillElement = document.createElement('div');
                fillElement.className = 'touch-analog-trigger-fill';
                trigger.appendChild(fillElement);
                const triggerStyles = {};
                for (const key in triggerConfig.style) {
                    let finalValue = parseStyleValue(triggerConfig.style[key], 1.0);
                    if (key === 'left') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.left}px)`;
                    else if (key === 'right') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.right}px)`;
                    else if (key === 'bottom') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.bottom}px)`;
                    else if (key === 'top') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.top}px)`;
                    triggerStyles[key] = finalValue;
                }
                Object.assign(trigger.style, triggerStyles);
                parentEl.appendChild(trigger);
                activeTouchControls.push({ element: trigger, type: 'analogTrigger', config: triggerConfig });
                analogTriggersToTrack[triggerConfig.buttonIndex] = { element: trigger, fillElement: fillElement, config: triggerConfig, activeTouchId: null };
                let activeId = null;
                trigger.addEventListener('touchstart', (e) => { 
                    e.preventDefault(); e.stopPropagation(); if (activeId !== null && e.changedTouches[0].identifier !== activeId) return;
                    activeId = e.changedTouches[0].identifier; trigger.classList.add('pressed');
                    analogTriggersToTrack[triggerConfig.buttonIndex].activeTouchId = activeId;
                    updateAnalogTriggerVisuals(triggerConfig.buttonIndex, e.changedTouches[0].clientY, true);
                }, { passive: false });
                trigger.addEventListener('touchmove', (e) => { 
                    e.preventDefault(); e.stopPropagation(); if (activeId === null) return;
                    for (let i = 0; i < e.changedTouches.length; i++) {
                        if (e.changedTouches[i].identifier === activeId) {
                            updateAnalogTriggerVisuals(triggerConfig.buttonIndex, e.changedTouches[i].clientY, true); break;
                        }
                    }
                }, { passive: false });
                const onTriggerEnd = (e) => {
                    e.preventDefault(); e.stopPropagation(); if (activeId === null) return;
                    for (let i = 0; i < e.changedTouches.length; i++) {
                        if (e.changedTouches[i].identifier === activeId) {
                            activeId = null; trigger.classList.remove('pressed');
                            analogTriggersToTrack[triggerConfig.buttonIndex].activeTouchId = null;
                            updateAnalogTriggerVisuals(triggerConfig.buttonIndex, 0, false); break;
                        }
                    }
                };
                trigger.addEventListener('touchend', onTriggerEnd, { passive: false });
                trigger.addEventListener('touchcancel', onTriggerEnd, { passive: false });
            });
        }
        
        if (profile.buttons) {
            profile.buttons.forEach(btnConfig => {
                const button = document.createElement('div');
                button.className = 'touch-gamepad-control touch-button';
                if (btnConfig.shape === 'squircle') button.classList.add('shape-squircle');
                if (btnConfig.type === 'digitalShoulder') button.classList.add('type-digitalShoulder');
                
                if (typeof btnConfig.label === 'string' && btnConfig.label.trim().startsWith('<svg')) {
                    button.innerHTML = btnConfig.label;
                } else {
                    button.textContent = isPreview ? (btnConfig.label && btnConfig.label.length > 2 && btnConfig.shape !== 'squircle' ? btnConfig.label[0] : btnConfig.label || '') : btnConfig.label || '';
                }
                
                const btnStyles = {};
                for (const key in btnConfig.style) {
                    let finalValue = parseStyleValue(btnConfig.style[key], scale);
                    if (!isPreview) {
                        if (key === 'left') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.left}px)`;
                        else if (key === 'right') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.right}px)`;
                        else if (key === 'bottom') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.bottom}px)`;
                        else if (key === 'top') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.top}px)`;
                    }
                    btnStyles[key] = finalValue;
                }

                if (isPreview && btnConfig.label && scale < 0.5) {
                    if (!(typeof btnConfig.label === 'string' && btnConfig.label.trim().startsWith('<svg'))) {
                        let previewFontSize = parseFloat(btnConfig.style.fontSize || '14px') * 0.7;
                        if (btnStyles.fontSize) previewFontSize = parseFloat(btnStyles.fontSize) * 0.7;
                        if (btnConfig.shape === 'squircle') previewFontSize *= 0.8;
                        btnStyles.fontSize = `${previewFontSize}px`;
                        if (btnConfig.label.length > 3 && btnConfig.shape !== 'squircle') {
                             button.textContent = btnConfig.label[0];
                        }
                    }
                }
                Object.assign(button.style, btnStyles);
                parentEl.appendChild(button);
                
                if (!isPreview) {
                    buttonElementsToTrack[btnConfig.id] = { element: button, config: btnConfig, activeTouchIds: new Set() };
                }
            });
        }

        if (profile.clusters && !isPreview) {
            profile.clusters.forEach(clusterConfig => {
                const clusterDiv = document.createElement('div');
                clusterDiv.className = 'touch-gamepad-cluster';
                clusterDiv.id = `cluster-${clusterConfig.id}`;

                const clusterStyles = {};
                for (const key in clusterConfig.style) {
                    let finalValue = parseStyleValue(clusterConfig.style[key], 1.0);
                    if (key === 'left') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.left}px)`;
                    else if (key === 'right') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.right}px)`;
                    else if (key === 'bottom') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.bottom}px)`;
                    else if (key === 'top') finalValue = `calc(${finalValue} + ${SAFE_AREA_PADDING.top}px)`;
                    clusterStyles[key] = finalValue;
                }
                Object.assign(clusterDiv.style, clusterStyles);
                parentEl.appendChild(clusterDiv);

                const handleClusterTouch = (event) => {
                    let interactionOccurred = false;
                    const touches = event.touches;
                    const changedTouches = event.changedTouches;
                    const buttonsInThisCluster = clusterConfig.buttonIds.map(id => buttonElementsToTrack[id]).filter(Boolean);

                    if (event.type === 'touchend' || event.type === 'touchcancel') {
                        for (let i = 0; i < changedTouches.length; i++) {
                            const touch = changedTouches[i];
                            buttonsInThisCluster.forEach(btnTrack => {
                                if (btnTrack.activeTouchIds.has(touch.identifier)) {
                                    btnTrack.activeTouchIds.delete(touch.identifier);
                                    if (btnTrack.activeTouchIds.size === 0) {
                                        btnTrack.element.classList.remove('pressed');
                                        updateGamepadButton(btnTrack.config.index, false);
                                    }
                                    interactionOccurred = true;
                                }
                            });
                        }
                    } else {
                        for (let i = 0; i < touches.length; i++) {
                            const touch = touches[i];
                            const x = touch.clientX;
                            const y = touch.clientY;

                            buttonsInThisCluster.forEach(btnTrack => {
                                const rect = btnTrack.element.getBoundingClientRect();
                                if (x >= rect.left - HIT_TEST_SLOP && x <= rect.right + HIT_TEST_SLOP &&
                                    y >= rect.top - HIT_TEST_SLOP && y <= rect.bottom + HIT_TEST_SLOP) {
                                    
                                    if (!btnTrack.activeTouchIds.has(touch.identifier)) {
                                        btnTrack.activeTouchIds.add(touch.identifier);
                                        if (!btnTrack.element.classList.contains('pressed')) {
                                            btnTrack.element.classList.add('pressed');
                                            updateGamepadButton(btnTrack.config.index, true);
                                        }
                                    }
                                    interactionOccurred = true;
                                } else {
                                    if (btnTrack.activeTouchIds.has(touch.identifier)) {
                                        btnTrack.activeTouchIds.delete(touch.identifier);
                                        if (btnTrack.activeTouchIds.size === 0) {
                                            btnTrack.element.classList.remove('pressed');
                                            updateGamepadButton(btnTrack.config.index, false);
                                        }
                                    }
                                }
                            });
                        }
                    }

                    if (interactionOccurred && event.cancelable) {
                        event.preventDefault();
                        event.stopPropagation();
                    }
                };
                clusterDiv.addEventListener('touchstart', handleClusterTouch, { passive: false });
                clusterDiv.addEventListener('touchmove', handleClusterTouch, { passive: false });
                clusterDiv.addEventListener('touchend', handleClusterTouch, { passive: false });
                clusterDiv.addEventListener('touchcancel', handleClusterTouch, { passive: false });
            });
        }
    }
    
    function updateAnalogTriggerVisuals(buttonIndex, currentClientY, isActive) {
        const triggerTrack = analogTriggersToTrack[buttonIndex];
        if (!triggerTrack || !triggerTrack.element) return;

        let value = 0;
        const rect = triggerTrack.element.getBoundingClientRect();
        const triggerHeight = rect.height;

        if (isActive && triggerTrack.activeTouchId !== null && triggerHeight > 0) {
            const relativeY = currentClientY - rect.top;
            value = Math.max(0, Math.min(1, relativeY / triggerHeight));
        }
        
        updateGamepadButton(buttonIndex, isActive && value > 0.05, value);
        if (triggerTrack.fillElement) {
            triggerTrack.fillElement.style.height = `${value * 100}%`;
        }
    }

    function createSettingsIcon() {
        if (!gamepadControlsOverlayElement) return;
        if (settingsIconElement && settingsIconElement.parentElement) settingsIconElement.remove();

        settingsIconElement = document.createElement('div');
        settingsIconElement.className = 'settings-icon-host';
        settingsIconElement.innerHTML = SETTINGS_ICON_SVG;

        settingsIconElement.style.top = `${SAFE_AREA_PADDING.top}px`;
        settingsIconElement.style.right = `${SAFE_AREA_PADDING.right}px`;
        
        settingsIconElement.addEventListener('click', (e) => {
            e.stopPropagation();
            toggleProfileSelector();
        });
        gamepadControlsOverlayElement.appendChild(settingsIconElement);
    }

    function toggleProfileSelector() {
        if (isProfileSelectorVisible) {
            hideProfileSelector();
        } else {
            showProfileSelector();
        }
    }

    function showProfileSelector() {
        if (profileSelectorOverlayElement) profileSelectorOverlayElement.remove();
        isProfileSelectorVisible = true;
        profileSelectorOverlayElement = document.createElement('div');
        Object.assign(profileSelectorOverlayElement.style, {
            position: 'fixed', top: '0', left: '0', width: '100vw', height: 'calc(var(--vh, 1vh) * 100)',
            backgroundColor: 'rgba(0, 0, 0, 0.75)',
            display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center',
            zIndex: '2147483640', pointerEvents: 'auto',
            padding: '20px', boxSizing: 'border-box', overflowY: 'auto'
        });

        const selectorContainer = document.createElement('div');
        Object.assign(selectorContainer.style, {
            background: '#2c2c2c', 
            color: '#e0e0e0', 
            padding: '20px', borderRadius: '8px',
            display: 'flex', flexWrap: 'wrap', gap: '15px',
            justifyContent: 'center',
            maxHeight: '80vh', overflowY: 'auto',
            border: '1px solid #444'
        });

        for (const profileKey in profiles) {
            const profile = profiles[profileKey];
            const previewBox = document.createElement('div');
            previewBox.dataset.profileKey = profileKey;
            const isActiveProfile = profileKey === currentProfileName;
            Object.assign(previewBox.style, {
                width: '200px', height: '120px', 
                border: `2px solid ${isActiveProfile ? '#0096ff' : '#555'}`,
                borderRadius: '5px', padding: '5px', cursor: 'pointer',
                position: 'relative', 
                backgroundColor: '#3b3b3b', 
                color: '#d0d0d0', 
                overflow: 'hidden', 
                display: 'flex', flexDirection: 'column', alignItems: 'center'
            });

            const title = document.createElement('div');
            title.textContent = profile.name || profileKey;
            Object.assign(title.style, { 
                marginBottom: '5px', fontWeight: 'bold', fontSize: '12px', textAlign: 'center',
                width: '100%', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis',
                color: isActiveProfile ? '#fff' : '#c0c0c0'
            });
            previewBox.appendChild(title);
            
            const previewContentArea = document.createElement('div');
            Object.assign(previewContentArea.style, { width: '100%', flexGrow: 1, position: 'relative', overflow: 'hidden' });
            const scaleWrapper = document.createElement('div');
            Object.assign(scaleWrapper.style, { width: `${100 / PREVIEW_SCALE}%`, height: `${100 / PREVIEW_SCALE}%`, transform: `scale(${PREVIEW_SCALE})`, transformOrigin: 'top left', position: 'absolute', top: '0px', left: '0px' });
            const controlsRenderContainer = document.createElement('div');
            Object.assign(controlsRenderContainer.style, { width: '100%', height: '100%', position: 'relative' });
            scaleWrapper.appendChild(controlsRenderContainer);
            previewContentArea.appendChild(scaleWrapper);
            previewBox.appendChild(previewContentArea);

            renderControlElements(profile, controlsRenderContainer, 1.0, true);

            previewBox.addEventListener('click', () => {
                currentProfileName = profileKey;
                localStorage.setItem('universalTouchGamepad_currentProfile', currentProfileName);
                hideProfileSelector();
                if (isGamepadVisible) {
                    renderMainGamepadUI();
                }
            });
            selectorContainer.appendChild(previewBox);
        }
        profileSelectorOverlayElement.appendChild(selectorContainer);
        document.body.appendChild(profileSelectorOverlayElement);
        profileSelectorOverlayElement.addEventListener('click', function(e) { if (e.target === profileSelectorOverlayElement) hideProfileSelector(); });
    }

    function hideProfileSelector() {
        if (profileSelectorOverlayElement) {
            profileSelectorOverlayElement.remove();
            profileSelectorOverlayElement = null;
        }
        isProfileSelectorVisible = false;
    }

    function renderMainGamepadUI() {
        if (!gamepadControlsOverlayElement || !profiles[currentProfileName]) return;
        injectBaseStyles();
        gamepadControlsOverlayElement.innerHTML = '';
        
        renderControlElements(profiles[currentProfileName], gamepadControlsOverlayElement, 1.0, false);
        createSettingsIcon();
        if (!gamepadState.connected && isGamepadVisible) connectGamepad();
    }

    function createGamepadControlsOverlay() {
        if (!gamepadControlsOverlayElement) {
            gamepadControlsOverlayElement = document.createElement('div');
            gamepadControlsOverlayElement.id = 'universal-touch-gamepad-controls-overlay';
            Object.assign(gamepadControlsOverlayElement.style, {
                position: 'fixed', top: '0', left: '0',
                width: '100vw', height: 'calc(var(--vh, 1vh) * 100)',
                zIndex: '2000',
                pointerEvents: 'none',
                overflow: 'hidden' 
            });
            document.body.appendChild(gamepadControlsOverlayElement);
        }
    }

    function showGamepad() {
        if (!hostAnchorElement) {
            console.error(GAMEPAD_ID + ": Host anchor element not set. Call SETUP first.");
            return;
        }
        createGamepadControlsOverlay();
        gamepadControlsOverlayElement.style.display = 'block'; 
        isGamepadVisible = true;
        const savedProfile = localStorage.getItem('universalTouchGamepad_currentProfile');
        if (savedProfile && profiles[savedProfile]) {
            currentProfileName = savedProfile;
        }
        renderMainGamepadUI();
    }

    function hideGamepad() {
        if (gamepadControlsOverlayElement) {
            gamepadControlsOverlayElement.innerHTML = ''; 
            gamepadControlsOverlayElement.style.display = 'none';
        }
        hideProfileSelector();
        isGamepadVisible = false;
        disconnectGamepad();
    }

    window.addEventListener('message', (event) => {
        const { data } = event;
        if (!data || typeof data !== 'object') return;

        switch (data.type) {
            case 'TOUCH_GAMEPAD_SETUP':
                if (data.payload && data.payload.targetDivId) {
                    const div = document.getElementById(data.payload.targetDivId);
                    if (div) {
                        hostAnchorElement = div;
                        console.log(GAMEPAD_ID + ": Host anchor element set to #" + data.payload.targetDivId);
                        const savedProfile = localStorage.getItem('universalTouchGamepad_currentProfile');
                        if (savedProfile && profiles[savedProfile]) {
                             currentProfileName = savedProfile;
                        } else if (data.payload.initialProfileName && profiles[data.payload.initialProfileName]) {
                            currentProfileName = data.payload.initialProfileName;
                        } else if (!profiles[currentProfileName]) {
                            currentProfileName = Object.keys(profiles)[0];
                        }

                        if (data.payload.visible === true) {
                            showGamepad();
                        }
                    } else {
                        console.error(GAMEPAD_ID + ": Host anchor DIV #" + data.payload.targetDivId + " not found.");
                        hostAnchorElement = null;
                    }
                }
                break;
            case 'TOUCH_GAMEPAD_VISIBILITY':
                 if (!hostAnchorElement && data.payload && data.payload.targetDivId) {
                     const div = document.getElementById(data.payload.targetDivId);
                     if(div) hostAnchorElement = div;
                }
                if (!hostAnchorElement) {
                    console.error(GAMEPAD_ID + ": Host anchor not set. Call SETUP or provide targetDivId in visibility message.");
                    return;
                }
                if (data.payload && typeof data.payload.visible === 'boolean') {
                    if (data.payload.visible) {
                        showGamepad();
                    } else {
                        hideGamepad();
                    }
                }
                break;
        }
    });

    overrideGamepadAPI();
    console.log(GAMEPAD_ID + " library loaded. Send 'TOUCH_GAMEPAD_SETUP' message to initialize.");
})();
