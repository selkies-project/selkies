/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Floating touch-gamepad toggle for the player clients.
 * @module
 */
import React from "react";
import { getTranslator } from "../translations";
import { isMobileClient } from "../../../selkies-web-core/lib/util.js";

/** Id of the element the touch gamepad overlay is mounted in. */
const TOUCH_GAMEPAD_HOST_DIV_ID = "touch-gamepad-host";
/** Pointer travel in pixels beyond which a press is a drag rather than a click. */
const DRAG_THRESHOLD = 10;

/** Resolved once: the browser language is fixed for the life of the document. */
const { t } = getTranslator(typeof navigator !== "undefined" ? navigator.language : "en");

/** Four-way arrow glyph drawn on the toggle. */
const GamepadIcon = () => (
    <svg viewBox="0 0 24 24" fill="currentColor" width="28" height="28">
      <path d="M15 7.5V2H9v5.5l3 3 3-3zM7.5 9H2v6h5.5l3-3-3-3zM9 16.5V22h6v-5.5l-3-3-3 3zM16.5 9l-3 3 3 3H22V9h-5.5z" />
    </svg>
);

/**
 * Draggable button that shows and hides the touch gamepad for every client
 * that is not the primary controller: the `#player2` to `#player4` and
 * `#shared` hashes, which render no dashboard, and a token-authenticated
 * viewer, whose sidebar is withdrawn once the server assigns the role.
 *
 * The player slots exist to contribute gamepad input, so their toggle stays
 * reachable on any device; a shared viewer only sees it once the client
 * looks like a touch device (`touchOnly`: a mobile user agent or a first
 * `touchstart`), the same gate the sidebar applies to its own touch tiles.
 * Uncontrolled, the button owns the overlay state: the first activation posts
 * `TOUCH_GAMEPAD_SETUP` to the window, later toggles post
 * `TOUCH_GAMEPAD_VISIBILITY`. A host that already owns that state (the
 * sidebar, whose Ctrl+Shift+G handler must agree with the button) passes
 * `isActive` and `onToggle` instead. A press that travels further than
 * `DRAG_THRESHOLD` moves the button instead of toggling.
 * @param {object} props
 * @param {boolean} [props.touchOnly=false] Render only on a mobile or touch-detected client.
 * @param {boolean} [props.isActive] Overlay state when controlled by the host.
 * @param {() => void} [props.onToggle] Host toggle, replacing the internal one.
 */
function PlayerGamepadButton({ touchOnly = false, isActive, onToggle }) {
    const [ownActive, setOwnActive] = React.useState(false);
    const [isTouchGamepadSetup, setIsTouchGamepadSetup] = React.useState(false);
    const [hasDetectedTouch, setHasDetectedTouch] = React.useState(isMobileClient);
    const isControlled = typeof onToggle === "function";
    const isTouchGamepadActive = isControlled ? !!isActive : ownActive;

    React.useEffect(() => {
        if (hasDetectedTouch) return undefined;
        const detectTouch = () => setHasDetectedTouch(true);
        window.addEventListener("touchstart", detectTouch, { once: true, passive: true });
        return () => window.removeEventListener("touchstart", detectTouch);
    }, [hasDetectedTouch]);

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
        if (isControlled) {
            onToggle();
            return;
        }
        const newActiveState = !isTouchGamepadActive;
        setOwnActive(newActiveState);

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
    }, [isControlled, onToggle, isTouchGamepadActive, isTouchGamepadSetup]);

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

    if (touchOnly && !hasDetectedTouch) return null;

    const title = t(isTouchGamepadActive
        ? "sections.gamepads.touchDisableTitle"
        : "sections.gamepads.touchEnableTitle");

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
            title={title}
            aria-label={title}
        >
            <GamepadIcon />
        </button>
    );
}

export default PlayerGamepadButton;
