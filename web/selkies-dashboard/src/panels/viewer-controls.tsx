/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A viewer's floating buttons, which stand in for the dashboard it does not
 * get: the touch gamepad's toggle while it holds a gamepad slot, on a touch
 * client with no physical pad in use, and the on-screen keyboard's while it
 * holds input (`viewerGrants`, from `lib/viewer-grants.ts`). The session's
 * controller hands both around live, so each comes and goes with its grant.
 *
 * Both start in the bottom right corner, side by side: the gamepad's in the
 * corner and the keyboard's to its left, centered on it. Either can be
 * dragged, so whenever the grants or the set of buttons shown changes, both
 * go back to those places, and a button that arrives never lands on one the
 * user moved.
 * @module
 */
import { VirtualKeyboardButton, type Corner } from "@/panels/gamepads";
import PlayerGamepadButton from "@/panels/player-gamepad-button";
import { useDashboard } from "@/state/dashboard";
import { usePhysicalGamepad } from "@/state/physical-gamepad";

/** The gamepad toggle's place: its 60 pixels, 20 in from the corner. */
const PAD_START: Corner = { right: 20, bottom: 20 };
/** The keyboard button's place alone. */
const KEYS_ALONE: Corner = { right: 16, bottom: 16 };
/** The keyboard button's place beside the gamepad toggle: 12 pixels left of it, its 36 pixels centered on its 60. */
const KEYS_BESIDE_PAD: Corner = { right: 92, bottom: 32 };

/** The viewer's gamepad and keyboard buttons, placed apart. */
export function ViewerControls() {
  const d = useDashboard();
  const physicalPad = usePhysicalGamepad();
  const { pad, input } = d.viewerGrants;
  const padShown = pad && d.isTouch && (!physicalPad || d.touchGamepad);
  const grants = `${pad}:${input}`;
  return (
    <>
      {padShown && <PlayerGamepadButton key={grants} isActive={d.touchGamepad} onToggle={d.toggleTouchGamepad} start={PAD_START} />}
      {input && <VirtualKeyboardButton key={`${grants}:${padShown}`} start={padShown ? KEYS_BESIDE_PAD : KEYS_ALONE} />}
    </>
  );
}
