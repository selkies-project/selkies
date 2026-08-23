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
import { isMobileClient } from "@/utils";

interface GamepadProps {
    isGamepadEnabled: boolean;
    // Owned by DashboardOverlay (one source for the menu entry, the hotkey
    // and this card): while the touch overlay is up the physical visualizer
    // would only mirror it, so it is hidden.
    isTouchGamepadActive: boolean;
}

export function Gamepad({ isGamepadEnabled, isTouchGamepadActive }: GamepadProps) {
    const isMobile = isMobileClient;
    const [gamepadStates, setGamepadStates] = React.useState<{ [key: string]: any }>({});
    const [hasReceivedGamepadData, setHasReceivedGamepadData] = React.useState(false);

    // Add message event listener for status updates
    React.useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message === 'object' && message !== null) {
                if (message.type === 'gamepadButtonUpdate' || message.type === 'gamepadAxisUpdate') {
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
                }
            }
        };

        window.addEventListener('message', handleWindowMessage);
        return () => window.removeEventListener('message', handleWindowMessage);
    }, [hasReceivedGamepadData]);

    // Touch input on the host div belongs to universalTouchGamepad's own overlay,
    // which it attaches on TOUCH_GAMEPAD_SETUP; DashboardOverlay drives the
    // setup/visibility messaging.

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
