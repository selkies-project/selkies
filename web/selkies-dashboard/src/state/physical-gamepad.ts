/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Whether a physical gamepad is in use, which makes the touch gamepad's
 * floating toggle redundant.
 *
 * Browsers hand a page no pad until a button on it is pressed, so a pad
 * plugged in before the page loads counts from its first press, not from the
 * load; `gamepadconnected` fires then, and `gamepaddisconnected` when it goes.
 * The touch gamepad's own synthetic pad is told apart by its id. Only a pad
 * the browser maps to the standard layout counts: some Android devices report
 * remotes and sensors as pads with no mapping, and a pad left uncounted only
 * leaves the toggle up.
 * @module
 */
import * as React from "react";
import { TOUCH_GAMEPAD_ID } from "../../../universal-touch-gamepad/universalTouchGamepad.ts";

/** Whether the browser lists a connected standard-layout pad other than the touch gamepad. */
function physicalPadPresent(): boolean {
  if (typeof navigator === "undefined" || typeof navigator.getGamepads !== "function") return false;
  return Array.from(navigator.getGamepads()).some((pad) =>
    pad !== null && pad.connected && pad.id !== TOUCH_GAMEPAD_ID && pad.mapping === "standard");
}

/** Follows pads arriving and leaving. */
function subscribe(onChange: () => void): () => void {
  window.addEventListener("gamepadconnected", onChange);
  window.addEventListener("gamepaddisconnected", onChange);
  return () => {
    window.removeEventListener("gamepadconnected", onChange);
    window.removeEventListener("gamepaddisconnected", onChange);
  };
}

/** Whether a physical gamepad is in use, live. */
export function usePhysicalGamepad(): boolean {
  return React.useSyncExternalStore(subscribe, physicalPadPresent);
}
