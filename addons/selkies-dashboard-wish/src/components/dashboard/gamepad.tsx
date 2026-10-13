/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import * as React from "react";
import GamepadVisualizer from "@dashboard/components/GamepadVisualizer.jsx";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Keyboard, X } from "lucide-react";
import { t } from "@/i18n";
import { getPrefixedKey, hardwareKeyboard, isMobileClient } from "@/utils";

/**
 * The floating Gamepads panel with its switches and the gamepad preview, one
 * visualizer per pad the core reports, and the mobile button that asks the
 * core to show the virtual keyboard.
 *
 * Pad state comes from the core's `gamepadButtonUpdate` and
 * `gamepadAxisUpdate` messages, heard while the preview is shown;
 * `showVirtualKeyboard` is posted back. The pad picture is the default
 * dashboard's own `GamepadVisualizer`, imported from its addon so a change
 * there reaches this dashboard without a second copy, and the empty and
 * touch-active states follow that dashboard's Gamepads section. Touch input on the touch-gamepad host
 * div belongs to universalTouchGamepad's own overlay, which it attaches on
 * `TOUCH_GAMEPAD_SETUP`; DashboardOverlay drives that setup and visibility
 * messaging.
 * @module
 */

interface GamepadProps {
    /**
     * Owned by DashboardOverlay, one source for the toolbar entry, the hotkey,
     * and this preview: on a mobile client the physical visualizer is hidden
     * while the touch overlay is up.
     */
    isTouchGamepadActive: boolean;
}

/** Renders a visualizer per pad that has reported, or a note that none has. */
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
            {isMobileClient && isTouchGamepadActive ? (
                <p className="text-sm text-muted-foreground">
                    {t('sections.gamepads.physicalHiddenForTouch')}
                </p>
            ) : Object.keys(gamepadStates).length > 0 ? (
                <div className="space-y-4">
                    {Object.keys(gamepadStates).sort((a, b) => parseInt(a, 10) - parseInt(b, 10)).map(gpIndexStr => {
                        const gpIndex = parseInt(gpIndexStr, 10);
                        return (
                            <GamepadVisualizer
                                key={gpIndex}
                                gamepadIndex={gpIndex}
                                gamepadState={gamepadStates[gpIndex]}
                            />
                        );
                    })}
                </div>
            ) : (
                <p className="text-sm text-muted-foreground">
                    {t(isMobileClient
                        ? 'sections.gamepads.noActivityMobileOrEnableTouch'
                        : 'sections.gamepads.noActivity')}
                </p>
            )}
        </div>
    );
}

interface GamepadPanelProps {
    /** Physical gamepad forwarding is enabled. */
    isGamepadEnabled: boolean;
    /** Toggles physical gamepad forwarding. */
    onGamepadToggle: () => void;
    /** The on-screen touch gamepad is shown. */
    isTouchGamepadActive: boolean;
    /** Toggles the on-screen touch gamepad. */
    onToggleTouchGamepad: () => void;
    /** Shows the gamepad input switch (`gamepad_enabled` is user-editable). */
    showInputToggle: boolean;
    /** Shows the touch switch, rumble, and the pad preview (`ui_sidebar_show_gamepads`). */
    showPads: boolean;
    /** Called by the header's close button. */
    onClose: () => void;
}

/**
 * The floating Gamepads panel: the input switch, the touch gamepad switch,
 * rumble, and the live preview. Its header is the drag handle the host
 * (`top-menu.tsx`) looks for. Rumble on this client's pads is client-only: it
 * is read from storage here and posted as `setGamepadRumble`, and the core
 * persists `gamepad_rumble` itself.
 * @param props The panel's props.
 * @returns The panel.
 */
export function GamepadPanel({
    isGamepadEnabled, onGamepadToggle, isTouchGamepadActive, onToggleTouchGamepad,
    showInputToggle, showPads, onClose,
}: GamepadPanelProps) {
    const [rumble, setRumble] = React.useState(() => {
        try {
            return localStorage.getItem(getPrefixedKey("gamepad_rumble")) !== "false";
        } catch {
            return true;
        }
    });
    const toggleRumble = () => {
        const next = !rumble;
        setRumble(next);
        window.postMessage({ type: "setGamepadRumble", value: next }, window.location.origin);
    };

    return (
        <div className="flex min-h-0 w-[300px] flex-col rounded-lg border bg-background text-xs shadow-lg">
            <div data-drag-handle className="flex cursor-grab select-none items-center justify-between px-3 py-2 active:cursor-grabbing">
                <h3 className="pointer-events-none text-sm font-semibold text-card-foreground">{t('sections.gamepads.title')}</h3>
                <Button
                    variant="ghost"
                    size="sm"
                    className="h-7 w-7 min-w-0 p-0"
                    onClick={onClose}
                    aria-label={t('common.close')}
                >
                    <X className="h-3 w-3" />
                </Button>
            </div>
            <div className="flex min-h-0 cursor-default flex-col gap-3 overflow-y-auto border-t px-3 py-3">
                {showInputToggle && (
                    <div className="flex items-center justify-between" data-testid="gamepad-input-toggle">
                        <Label className="text-sm font-medium">{t('topMenu.gamepadInput')}</Label>
                        <Switch checked={isGamepadEnabled} onCheckedChange={onGamepadToggle} />
                    </div>
                )}
                {showPads && (
                    <>
                        <div className="flex items-center justify-between" data-testid="touch-gamepad-toggle">
                            <Label className="text-sm font-medium">{t('topMenu.touchGamepad')}</Label>
                            <Switch checked={isTouchGamepadActive} onCheckedChange={onToggleTouchGamepad} />
                        </div>
                        <div className="flex items-center justify-between" data-testid="gamepad-rumble-toggle">
                            <Label className="text-sm font-medium">{t('topMenu.rumble')}</Label>
                            <Switch checked={rumble} onCheckedChange={toggleRumble} />
                        </div>
                        <Gamepad isTouchGamepadActive={isTouchGamepadActive} />
                    </>
                )}
            </div>
        </div>
    );
}

/** The page's attached-keyboard verdict, listening from load. */
export const keyboardWatch = hardwareKeyboard();

/** Whether a hardware keyboard is attached, which keeps the system's on-screen one down. */
export function useKeyboardAttached(): boolean {
    return React.useSyncExternalStore(keyboardWatch.subscribe, keyboardWatch.attached);
}

/** The primary pointer, which a convertible turns coarse when its keyboard is detached. */
const coarsePointer = typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(pointer: coarse)")
    : null;
const subscribeCoarsePointer = (onChange: () => void) => {
    coarsePointer?.addEventListener("change", onChange);
    return () => coarsePointer?.removeEventListener("change", onChange);
};
const isCoarsePointer = () => coarsePointer?.matches ?? false;

/**
 * The button that asks the core to show the virtual keyboard, while the primary pointer is
 * coarse (a phone, a tablet, a convertible in its tablet posture) and no keyboard is
 * attached, which keeps the system's on-screen one down. The toolbar's keyboard
 * toggle can hide it, as the default dashboard's keyboard tile does.
 * @param props `visible` is false once the user has hidden the button.
 */
export function VirtualKeyboardButton({ visible = true }: { visible?: boolean }) {
    const keyboardAttached = useKeyboardAttached();
    const coarse = React.useSyncExternalStore(subscribeCoarsePointer, isCoarsePointer);
    if (!visible || !coarse || keyboardAttached) return null;
    return (
        <Button
            variant="default"
            size="icon"
            className="fixed bottom-4 right-4 z-50"
            aria-label={t("topMenu.virtualKeyboard")}
            title={t("topMenu.virtualKeyboard")}
            data-virtual-keyboard-button=""
            onClick={() => window.postMessage({ type: 'showVirtualKeyboard' }, window.location.origin)}
        >
            <Keyboard className="h-4 w-4" />
        </Button>
    );
}
