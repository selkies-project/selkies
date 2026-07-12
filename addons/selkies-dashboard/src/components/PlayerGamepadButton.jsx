/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// src/components/PlayerGamepadButton.jsx
import React from "react"; 

const TOUCH_GAMEPAD_HOST_DIV_ID = "touch-gamepad-host";
const DRAG_THRESHOLD = 10;

const GamepadIcon = () => (
    <svg viewBox="0 0 24 24" fill="currentColor" width="28" height="28">
      <path d="M15 7.5V2H9v5.5l3 3 3-3zM7.5 9H2v6h5.5l3-3-3-3zM9 16.5V22h6v-5.5l-3-3-3 3zM16.5 9l-3 3 3 3H22V9h-5.5z" />
    </svg>
);

// Floating, draggable touch-gamepad toggle for the #player2..#player4
// clients, which render no dashboard. Always visible: these slots exist to
// contribute gamepad input, so the toggle must be reachable on any device
// without depending on touch detection.
function PlayerGamepadButton() {
    const [isTouchGamepadActive, setIsTouchGamepadActive] = React.useState(false);
    const [isTouchGamepadSetup, setIsTouchGamepadSetup] = React.useState(false);

    const [buttonPosition, setButtonPosition] = React.useState({ bottom: 20, right: 20 });
    const dragInfo = React.useRef({
        isDragging: false,
        hasDragged: false,
        pointerId: null,
        startX: 0,
        startY: 0,
        initialBottom: 0,
        initialRight: 0,
    });

    const handleToggleTouchGamepad = React.useCallback(() => {
        const newActiveState = !isTouchGamepadActive;
        setIsTouchGamepadActive(newActiveState);

        if (newActiveState && !isTouchGamepadSetup) {
            window.postMessage({
                type: "TOUCH_GAMEPAD_SETUP",
                payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true },
            }, window.location.origin);
            setIsTouchGamepadSetup(true);
        } else if (isTouchGamepadSetup) {
            window.postMessage({
                type: "TOUCH_GAMEPAD_VISIBILITY",
                payload: { visible: newActiveState, targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID },
            }, window.location.origin);
        }
    }, [isTouchGamepadActive, isTouchGamepadSetup]);

    const handlePointerDown = (e) => {
        dragInfo.current = {
            isDragging: true,
            hasDragged: false,
            pointerId: e.pointerId,
            startX: e.clientX,
            startY: e.clientY,
            initialBottom: buttonPosition.bottom,
            initialRight: buttonPosition.right,
        };
        e.currentTarget.setPointerCapture(e.pointerId);
    };

    const handlePointerMove = (e) => {
        if (!dragInfo.current.isDragging) return;

        const dx = e.clientX - dragInfo.current.startX;
        const dy = e.clientY - dragInfo.current.startY;

        if (!dragInfo.current.hasDragged && (Math.abs(dx) > DRAG_THRESHOLD || Math.abs(dy) > DRAG_THRESHOLD)) {
            dragInfo.current.hasDragged = true;
        }

        if (dragInfo.current.hasDragged) {
            setButtonPosition({
                bottom: dragInfo.current.initialBottom - dy,
                right: dragInfo.current.initialRight - dx,
            });
        }
    };

    const handlePointerUp = (e) => {
        if (e.currentTarget.hasPointerCapture(dragInfo.current.pointerId)) {
            e.currentTarget.releasePointerCapture(dragInfo.current.pointerId);
        }
        dragInfo.current.isDragging = false;
        dragInfo.current.pointerId = null;
    };

    const onButtonClick = (e) => {
        if (dragInfo.current.hasDragged) {
            e.preventDefault();
            e.stopPropagation();
            dragInfo.current.hasDragged = false;
            return;
        }
        handleToggleTouchGamepad();
    };

    return (
        <button
            className={`player-gamepad-button ${isTouchGamepadActive ? "active" : ""}`}
            onClick={onButtonClick}
            onPointerDown={handlePointerDown}
            onPointerMove={handlePointerMove}
            onPointerUp={handlePointerUp}
            onPointerCancel={handlePointerUp}
            style={{
                position: 'fixed',
                right: `${buttonPosition.right}px`,
                bottom: `${buttonPosition.bottom}px`,
                touchAction: 'none',
                zIndex: 10000,
                width: '60px',
                height: '60px',
                borderRadius: '50%',
                backgroundColor: 'rgba(0, 0, 0, 0.6)',
                border: '2px solid rgba(255, 255, 255, 0.7)',
                color: 'white',
                display: 'flex',
                justifyContent: 'center',
                alignItems: 'center',
                cursor: 'pointer',
                boxShadow: '0 4px 12px rgba(0,0,0,0.4)',
                transition: 'background-color 0.2s ease-in-out',
            }}
            title="Toggle Touch Gamepad"
            aria-label="Toggle Touch Gamepad"
        >
            <GamepadIcon />
        </button>
    );
}

export default PlayerGamepadButton;
