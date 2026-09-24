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

/**
 * The gamepad preview of the top menu's gamepad dropdown, one visualizer per
 * pad the core reports, and the mobile button that asks the core to show the
 * virtual keyboard.
 *
 * Pad state comes from the core's `gamepadButtonUpdate` and
 * `gamepadAxisUpdate` messages, heard while the dropdown is open;
 * `showVirtualKeyboard` is posted back. Touch input on the touch-gamepad host
 * div belongs to universalTouchGamepad's own overlay, which it attaches on
 * `TOUCH_GAMEPAD_SETUP`; DashboardOverlay drives that setup and visibility
 * messaging.
 * @module
 */

interface GamepadProps {
    /**
     * Owned by DashboardOverlay, one source for the menu entry, the hotkey,
     * and this preview: while the touch overlay is up the physical visualizer
     * would only mirror it, so it is hidden.
     */
    isTouchGamepadActive: boolean;
}

/** Renders the visualizers, an idle one until a pad reports. */
export function Gamepad({ isTouchGamepadActive }: GamepadProps) {
    const [gamepadStates, setGamepadStates] = React.useState<{ [key: string]: any }>({});

    React.useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message === 'object' && message !== null) {
                if (message.type === 'gamepadButtonUpdate' || message.type === 'gamepadAxisUpdate') {
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
    }, []);

    return (
        <div className="px-2 py-2">
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
        </div>
    );
}

/** The mobile button that asks the core to show the virtual keyboard; nothing elsewhere. */
export function VirtualKeyboardButton() {
    if (!isMobileClient) return null;
    return (
        <Button
            variant="default"
            size="icon"
            className="fixed bottom-4 right-4 z-50"
            onClick={() => window.postMessage({ type: 'showVirtualKeyboard' }, window.location.origin)}
        >
            <Keyboard className="h-4 w-4" />
        </Button>
    );
}
