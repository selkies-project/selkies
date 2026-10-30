/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The gamepads panel: the input switch, the touch gamepad switch, rumble,
 * and a live picture of every pad the core reports, plus the floating button
 * that pops the system's on-screen keyboard on a touch client.
 *
 * Pad state comes from the core's `gamepadButtonUpdate` and
 * `gamepadAxisUpdate` messages, heard while the preview is shown. Rumble on
 * this client's pads is client-only: read from storage here, posted as
 * `setGamepadRumble`, and persisted by the core as `gamepad_rumble`. Touch
 * input on the touch-gamepad host div belongs to universalTouchGamepad's own
 * overlay, which the shared state sets up.
 * @module
 */
import * as React from "react";
import { Keyboard, Vibrate } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SwitchRow, Clip } from "@/panels/controls";
import { useDashboard } from "@/state/dashboard";
import { getPrefixedKey, isMobileClient } from "@/utils";
import { t } from "@/i18n";

/** Button value above which a button is drawn as pressed. */
const PRESS_THRESHOLD = 0.1;
/** Pixels a stick top moves per unit of axis deflection. */
const STICK_TRAVEL = 10;

/** One pad's state as the core reports it: standard-layout button values and axis positions. */
interface PadState { buttons: Record<number, number>; axes: Record<number, number> }

/**
 * Draws a standard-layout pad (Xbox naming) with pressed buttons, trigger
 * pressure, and stick deflection. Button indices: 0 to 3 face (A, B, X, Y),
 * 4 and 5 bumpers, 6 and 7 triggers, 8 Back, 9 Start, 10 and 11 stick
 * clicks, 12 to 15 D-pad; axes 0 and 1 are the left stick, 2 and 3 the right.
 */
function GamepadVisualizer({ state, index }: { state: PadState; index: number }) {
  const pressed = (button: number) => (state.buttons[button] || 0) > PRESS_THRESHOLD;
  const cls = (button: number, kind: "button" | "dpad" | "bumper") => `gp-vis-${kind}${pressed(button) ? ` gp-vis-${kind}-pressed` : ""}`;
  const trigger = (button: number) => ({ opacity: 0.5 + (state.buttons[button] || 0) * 0.5 });
  const stick = (x: number, y: number) => ({ transform: `translate(${(state.axes[x] || 0) * STICK_TRAVEL}px, ${(state.axes[y] || 0) * STICK_TRAVEL}px)` });
  return (
    <div className="gamepad-visualizer-instance">
      <h4 className="mb-1 text-xs text-muted-foreground">{t("sections.gamepads.gamepadTitle", { index })}</h4>
      <svg viewBox="0 0 260 100" width="100%" height="100" className="gamepad-svg-vis">
        <rect className="gp-vis-base" x="30" y="10" width="200" height="80" rx="10" ry="10" />
        <rect className={cls(4, "bumper")} x="40" y="0" width="40" height="8" rx="2" />
        <rect className={cls(5, "bumper")} x="180" y="0" width="40" height="8" rx="2" />
        <rect className="gp-vis-trigger" style={trigger(6)} x="40" y="10" width="40" height="10" rx="2" />
        <rect className="gp-vis-trigger" style={trigger(7)} x="180" y="10" width="40" height="10" rx="2" />
        <circle className={cls(0, "button")} cx="185" cy="55" r="6" />
        <circle className={cls(1, "button")} cx="205" cy="40" r="6" />
        <circle className={cls(2, "button")} cx="165" cy="40" r="6" />
        <circle className={cls(3, "button")} cx="185" cy="25" r="6" />
        <rect className={cls(8, "button")} x="105" y="25" width="10" height="5" />
        <rect className={cls(9, "button")} x="145" y="25" width="10" height="5" />
        <rect className={cls(12, "dpad")} x="70" y="50" width="10" height="10" />
        <rect className={cls(13, "dpad")} x="70" y="70" width="10" height="10" />
        <rect className={cls(14, "dpad")} x="60" y="60" width="10" height="10" />
        <rect className={cls(15, "dpad")} x="80" y="60" width="10" height="10" />
        <g>
          <circle className="gp-vis-stick-base" cx="75" cy="30" r="12" />
          <circle className="gp-vis-stick-top" cx="75" cy="30" r="8" style={stick(0, 1)} />
          <circle className={cls(10, "button")} cx="75" cy="30" r="3" />
        </g>
        <g>
          <circle className="gp-vis-stick-base" cx="155" cy="65" r="12" />
          <circle className="gp-vis-stick-top" cx="155" cy="65" r="8" style={stick(2, 3)} />
          <circle className={cls(11, "button")} cx="155" cy="65" r="3" />
        </g>
      </svg>
    </div>
  );
}

/** A visualizer per pad that has reported, or a note that none has. */
function GamepadPreview() {
  const { touchGamepad } = useDashboard();
  const [pads, setPads] = React.useState<Record<string, PadState>>({});
  React.useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (typeof message !== "object" || message === null) return;
      if (message.type !== "gamepadButtonUpdate" && message.type !== "gamepadAxisUpdate") return;
      const index = message.gamepadIndex;
      if (index === undefined || index === null) return;
      setPads((previous) => {
        const pad: PadState = { buttons: { ...(previous[index]?.buttons || {}) }, axes: { ...(previous[index]?.axes || {}) } };
        if (message.type === "gamepadButtonUpdate") pad.buttons[message.buttonIndex] = message.value || 0;
        else pad.axes[message.axisIndex] = Math.max(-1, Math.min(1, message.value || 0));
        return { ...previous, [index]: pad };
      });
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, []);
  if (isMobileClient && touchGamepad) {
    return <p className="text-xs text-muted-foreground">{t('sections.gamepads.physicalHiddenForTouch')}</p>;
  }
  const indexes = Object.keys(pads).sort((a, b) => parseInt(a, 10) - parseInt(b, 10));
  if (indexes.length === 0) {
    return <p className="text-xs text-muted-foreground">{t(isMobileClient ? 'sections.gamepads.noActivityMobileOrEnableTouch' : 'sections.gamepads.noActivity')}</p>;
  }
  return <div className="space-y-3">{indexes.map((index) => <GamepadVisualizer key={index} index={parseInt(index, 10)} state={pads[index]} />)}</div>;
}

interface GamepadControlsProps {
  /** Draws the gamepad input switch too; off where the layout has a tile for it. */
  showInput?: boolean;
  /** Draws rumble as a pressed button with its icon rather than a switch row. */
  rumbleButton?: boolean;
}

/** The gamepad switches and the preview; `ui_sidebar_show_gamepads` hides everything but the input switch. */
export function GamepadControls({ showInput = false, rumbleButton = false }: GamepadControlsProps) {
  const d = useDashboard();
  const [rumble, setRumble] = React.useState(() => localStorage.getItem(getPrefixedKey("gamepad_rumble")) !== "false");
  const toggleRumble = () => {
    const next = !rumble;
    setRumble(next);
    window.postMessage({ type: "setGamepadRumble", value: next }, window.location.origin);
  };
  return (
    <div className="flex flex-col gap-3">
      {showInput && d.renderable.coreButtons && d.renderable.gamepadToggle && <SwitchRow id="gamepadInputToggle" label={t('topMenu.gamepadInput')} checked={d.gamepad} onToggle={d.toggleGamepad} />}
      {d.renderable.gamepads && (
        <>
          <SwitchRow id="touchGamepadToggle" label={t('topMenu.touchGamepad')} checked={d.touchGamepad} onToggle={d.toggleTouchGamepad} />
          {rumbleButton ? (
            <Button id="gamepadRumbleToggle" variant="outline" className="w-full min-w-0" aria-pressed={rumble}
              title={t('sections.gamepads.rumbleTitle')} onClick={toggleRumble}>
              <Vibrate /><Clip>{t(rumble ? 'sections.gamepads.rumbleActiveLabel' : 'sections.gamepads.rumbleInactiveLabel')}</Clip>
            </Button>
          ) : (
            <SwitchRow id="gamepadRumbleToggle" label={t('topMenu.rumble')} title={t('sections.gamepads.rumbleTitle')} checked={rumble} onToggle={toggleRumble} />
          )}
          <GamepadPreview />
        </>
      )}
    </div>
  );
}

/** The primary pointer, which a convertible turns coarse when its keyboard is detached. */
const coarsePointer = typeof window !== "undefined" && typeof window.matchMedia === "function" ? window.matchMedia("(pointer: coarse)") : null;
const subscribeCoarsePointer = (onChange: () => void) => {
  coarsePointer?.addEventListener("change", onChange);
  return () => coarsePointer?.removeEventListener("change", onChange);
};
const isCoarsePointer = () => coarsePointer?.matches ?? false;

/**
 * The button that asks the core to show the virtual keyboard, while the
 * primary pointer is coarse (a phone, a tablet, a convertible in its tablet
 * posture), no keyboard is attached, and the keyboard toggle has not hidden it.
 */
export function VirtualKeyboardButton() {
  const d = useDashboard();
  const coarse = React.useSyncExternalStore(subscribeCoarsePointer, isCoarsePointer);
  if (!d.keyboardButton || !coarse || !d.gamepad || d.isSecondaryDisplay || d.isViewer || !d.renderable.chrome || !d.renderable.gamepads) return null;
  return (
    <Button variant="default" size="icon" className="virtual-keyboard-button fixed right-4 bottom-4 z-[2050]" data-virtual-keyboard-button=""
      aria-label={t("topMenu.virtualKeyboard")} title={t("topMenu.virtualKeyboard")}
      onClick={() => window.postMessage({ type: 'showVirtualKeyboard' }, window.location.origin)}>
      <Keyboard className="size-4" />
    </Button>
  );
}
