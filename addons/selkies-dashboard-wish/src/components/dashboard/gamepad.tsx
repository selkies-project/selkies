/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import * as React from "react";
import { GamepadVisualizer } from "@/components/dashboard/GamepadVisualizer";
import { Button } from "@/components/ui/button";
import { Keyboard } from "lucide-react";
import { t } from "@/i18n";

const TOUCH_GAMEPAD_HOST_DIV_ID = 'touch-gamepad-host';

interface GamepadProps {
    isGamepadEnabled: boolean;
    onGamepadToggle: (enabled: boolean) => void;
}

export function Gamepad({ isGamepadEnabled, onGamepadToggle }: GamepadProps) {
    const [isTouchGamepadActive, setIsTouchGamepadActive] = React.useState(false);
    const [isTouchGamepadSetup, setIsTouchGamepadSetup] = React.useState(false);
    const [isMobile, setIsMobile] = React.useState(false);
    const [gamepadStates, setGamepadStates] = React.useState<{ [key: string]: any }>({});
    const [hasReceivedGamepadData, setHasReceivedGamepadData] = React.useState(false);

    // Mobile detection effect
    React.useEffect(() => {
        const mobileCheck = typeof window !== 'undefined' && (
            (navigator as any).userAgentData?.mobile ||
            /Mobi|Android|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(navigator.userAgent)
        );
        setIsMobile(!!mobileCheck);

        if (!!mobileCheck) {
            // If mobile, ensure the gamepads section is open by default
            // Note: This should be handled by the parent component
                    // If mobile, ensure the gamepads section is open by default
            window.postMessage({ type: 'OPEN_GAMEPADS_SECTION' }, window.location.origin);
        }

        // Log mobile detection details
        if ((navigator as any).userAgentData && (navigator as any).userAgentData.mobile !== undefined) {
            console.log('Dashboard: Mobile detected via userAgentData.mobile:', (navigator as any).userAgentData.mobile);
        } else if (typeof navigator.userAgent === 'string') {
            console.log('Dashboard: Mobile detected via userAgent string match:', /Mobi|Android/i.test(navigator.userAgent));
        } else {
            console.warn('Dashboard: Mobile detection methods not fully available. Mobile status set to:', !!mobileCheck);
        }
    }, []);

    // Add message event listener for status updates
    React.useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message === 'object' && message !== null) {
                if (message.type === 'gamepadControl') {
                    if (message.enabled !== undefined) {
                        onGamepadToggle(message.enabled);
                        // If gamepad is disabled, also disable touch gamepad
                        if (!message.enabled && isTouchGamepadActive) {
                            setIsTouchGamepadActive(false);
                            window.postMessage({
                                type: 'TOUCH_GAMEPAD_VISIBILITY',
                                payload: { visible: false, targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID }
                            }, window.location.origin);
                        }
                    }
                } else if (message.type === 'gamepadButtonUpdate' || message.type === 'gamepadAxisUpdate') {
                    if (!hasReceivedGamepadData) setHasReceivedGamepadData(true);
                    const gpIndex = message.gamepadIndex;
                    if (gpIndex === undefined || gpIndex === null) return;
                    setGamepadStates(prev => {
                        const ns = { ...prev };
                        if (!ns[gpIndex]) ns[gpIndex] = { buttons: {}, axes: {} };
                        else ns[gpIndex] = { buttons: { ...(ns[gpIndex].buttons || {}) }, axes: { ...(ns[gpIndex].axes || {}) } };
                        if (message.type === 'gamepadButtonUpdate') ns[gpIndex].buttons[message.buttonIndex] = message.value || 0;
                        else ns[gpIndex].axes[message.axisIndex] = Math.max(-1, Math.min(1, message.value || 0));
                        return ns;
                    });
                } else if (message.type === 'TOUCH_GAMEPAD_VISIBILITY') {
                    setIsTouchGamepadActive(message.payload.visible);
                    if (!isTouchGamepadSetup && message.payload.visible) {
                        window.postMessage({
                            type: 'TOUCH_GAMEPAD_SETUP',
                            payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true }
                        }, window.location.origin);
                        setIsTouchGamepadSetup(true);
                    }
                } else if (message.type === 'trackpadModeUpdate') {
                    // Handle trackpad mode updates if needed
                    console.log('Gamepad: Received trackpad mode update:', message.enabled);
                }
            }
        };

        window.addEventListener('message', handleWindowMessage);
        return () => window.removeEventListener('message', handleWindowMessage);
    }, [hasReceivedGamepadData, onGamepadToggle, isTouchGamepadActive, isTouchGamepadSetup]);

    // Touch Gamepad Handler
    const handleToggleTouchGamepad = React.useCallback(() => {
        const newActiveState = !isTouchGamepadActive;
        setIsTouchGamepadActive(newActiveState);

        if (newActiveState && !isTouchGamepadSetup) {
            window.postMessage({
                type: 'TOUCH_GAMEPAD_SETUP',
                payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true }
            }, window.location.origin);
            setIsTouchGamepadSetup(true);
            console.log("Dashboard: Touch Gamepad SETUP sent, targetDivId:", TOUCH_GAMEPAD_HOST_DIV_ID, "visible: true");
        } else if (isTouchGamepadSetup) { // Only send visibility if setup has occurred
            window.postMessage({
                type: 'TOUCH_GAMEPAD_VISIBILITY',
                payload: { visible: newActiveState, targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID }
            }, window.location.origin);
            console.log(`Dashboard: Touch Gamepad VISIBILITY sent, targetDivId:`, TOUCH_GAMEPAD_HOST_DIV_ID, `visible: ${newActiveState}`);
        }
    }, [isTouchGamepadActive, isTouchGamepadSetup]);

    // Add touch gamepad handler
    const handleTouchGamepad = React.useCallback((e: TouchEvent) => {
        if (!isGamepadEnabled) return;
        const touch = e.touches[0];
        const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
        const x = (touch.clientX - rect.left) / rect.width;
        const y = (touch.clientY - rect.top) / rect.height;
        const data = {
            type: 'gamepad',
            data: {
                leftStick: { x, y },
                rightStick: { x: 0, y: 0 },
                buttons: { A: false, B: false, X: false, Y: false }
            }
        };
        window.parent.postMessage(data, '*');
    }, [isGamepadEnabled]);

    // Add touch event listeners
    React.useEffect(() => {
        const handleTouchStart = (e: TouchEvent) => handleTouchGamepad(e);
        const handleTouchMove = (e: TouchEvent) => handleTouchGamepad(e);
        const handleTouchEnd = () => {
            const data = {
                type: 'gamepad',
                data: {
                    leftStick: { x: 0, y: 0 },
                    rightStick: { x: 0, y: 0 },
                    buttons: { A: false, B: false, X: false, Y: false }
                }
            };
            window.parent.postMessage(data, '*');
        };

        const touchGamepadHost = document.getElementById(TOUCH_GAMEPAD_HOST_DIV_ID);
        if (touchGamepadHost) {
            touchGamepadHost.addEventListener('touchstart', handleTouchStart);
            touchGamepadHost.addEventListener('touchmove', handleTouchMove);
            touchGamepadHost.addEventListener('touchend', handleTouchEnd);
        }

        return () => {
            if (touchGamepadHost) {
                touchGamepadHost.removeEventListener('touchstart', handleTouchStart);
                touchGamepadHost.removeEventListener('touchmove', handleTouchMove);
                touchGamepadHost.removeEventListener('touchend', handleTouchEnd);
            }
        };
    }, [handleTouchGamepad]);

    const handleShowVirtualKeyboard = () => {
        window.postMessage({ type: 'showVirtualKeyboard' }, window.location.origin);
        console.log("Dashboard: Sending postMessage: { type: 'showVirtualKeyboard' }");
    };

    // Show UI when gamepad is enabled, regardless of other conditions
    if (!isGamepadEnabled && !isMobile && !isTouchGamepadActive && !hasReceivedGamepadData) return null;

    return (
        <div className="px-3 py-2">
            {isTouchGamepadActive && (
                <p className="text-sm text-muted-foreground">
                    {t('sections.gamepads.physicalHiddenForTouch')}
                </p>
            )}
            {!isTouchGamepadActive && (
                <div className="space-y-4">
                    {Object.keys(gamepadStates).length > 0 ? (
                        Object.keys(gamepadStates).sort((a, b) => parseInt(a, 10) - parseInt(b, 10)).map(gpIndexStr => {
                            const gpIndex = parseInt(gpIndexStr, 10);
                            return (
                                <GamepadVisualizer
                                    key={gpIndex}
                                    gamepadIndex={gpIndex}
                                    gamepadState={gamepadStates[gpIndex]}
                                />
                            );
                        })
                    ) : (
                        <GamepadVisualizer
                            key="default"
                            gamepadIndex={0}
                            gamepadState={{ buttons: {}, axes: {} }}
                        />
                    )}
                </div>
            )}
            {isMobile && (
                <Button
                    variant="default"
                    size="icon"
                    className="fixed bottom-4 right-4 z-50"
                    onClick={handleShowVirtualKeyboard}
                >
                    <Keyboard className="h-4 w-4" />
                </Button>
            )}
        </div>
    );
} 