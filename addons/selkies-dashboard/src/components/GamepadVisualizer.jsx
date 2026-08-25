/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * SVG picture of one gamepad's live state for the sidebar's gamepads section.
 * @module
 */
import { getTranslator } from "../translations";

/** Button value above which a button is drawn as pressed. */
const GAMEPAD_VIS_THRESHOLD = 0.1;
/** Pixels a stick top moves per unit of axis deflection. */
const STICK_VIS_MULTIPLIER = 10;

/** Resolved once: the browser language is fixed for the life of the document. */
const { t } = getTranslator(typeof navigator !== "undefined" ? navigator.language : "en");

/**
 * Draws a standard-layout pad (Xbox naming) with pressed buttons, trigger
 * pressure and stick deflection taken from the core's gamepad state. Button
 * indices: 0 to 3 face (A, B, X, Y), 4 and 5 bumpers, 6 and 7 triggers, 8
 * Back, 9 Start, 10 and 11 stick clicks, 12 to 15 D-pad; axes 0 and 1 are the
 * left stick, 2 and 3 the right.
 * @param {object} props
 * @param {{buttons?: Object<number, number>, axes?: Object<number, number>}|null} props.gamepadState
 *     Standard-layout button values and axis positions; `null` until the pad
 *     has reported.
 * @param {number} props.gamepadIndex Pad slot, used in the title and element ids.
 */
function GamepadVisualizer({ gamepadState, gamepadIndex }) {
  if (!gamepadState) {
    return <div>{t("sections.gamepads.loadingGamepad", { index: gamepadIndex })}</div>;
  }

  const buttons = gamepadState.buttons || {};
  const axes = gamepadState.axes || {};

  /** Pressed-state class for a button: D-pad (12 to 15), bumpers (4, 5), or any other. */
  const getButtonClass = (index) => {
    const value = buttons[index] || 0;
    const pressed = value > GAMEPAD_VIS_THRESHOLD;
    if (!pressed) return '';

    if (index >= 12 && index <= 15) return 'gp-vis-dpad-pressed';
    if (index === 4 || index === 5) return 'gp-vis-bumper-pressed';
    return 'gp-vis-button-pressed';
  };

  /** Opacity of a trigger (6, 7) rising with its pressure. */
  const getTriggerStyle = (index) => {
    if (index !== 6 && index !== 7) return {};
    const value = buttons[index] || 0;
    return { opacity: 0.5 + (value * 0.5) };
  };

  /** CSS transform moving a stick top by its two axes. */
  const getStickTransform = (xAxisIndex, yAxisIndex) => {
    const x = axes[xAxisIndex] || 0;
    const y = axes[yAxisIndex] || 0;
    const translateX = x * STICK_VIS_MULTIPLIER;
    const translateY = y * STICK_VIS_MULTIPLIER;
    return `translate(${translateX}px, ${translateY}px)`;
  };

  const leftStickTransform = getStickTransform(0, 1);
  const rightStickTransform = getStickTransform(2, 3);

  return (
    <div className="gamepad-visualizer-instance">
      <h4>{t("sections.gamepads.gamepadTitle", { index: gamepadIndex })}</h4>
      <svg viewBox="0 0 260 100" width="100%" height="100" className="gamepad-svg-vis">
        <rect className="gp-vis-base" x="30" y="10" width="200" height="80" rx="10" ry="10" />

        <rect id={`gp-${gamepadIndex}-btn-4`} className={`gp-vis-bumper ${getButtonClass(4)}`} x="40" y="0" width="40" height="8" rx="2" />
        <rect id={`gp-${gamepadIndex}-btn-5`} className={`gp-vis-bumper ${getButtonClass(5)}`} x="180" y="0" width="40" height="8" rx="2" />

        <rect id={`gp-${gamepadIndex}-btn-6`} className="gp-vis-trigger" style={getTriggerStyle(6)} x="40" y="10" width="40" height="10" rx="2" />
        <rect id={`gp-${gamepadIndex}-btn-7`} className="gp-vis-trigger" style={getTriggerStyle(7)} x="180" y="10" width="40" height="10" rx="2" />

        <circle id={`gp-${gamepadIndex}-btn-0`} className={`gp-vis-button ${getButtonClass(0)}`} cx="185" cy="55" r="6" /> {/* A */}
        <circle id={`gp-${gamepadIndex}-btn-1`} className={`gp-vis-button ${getButtonClass(1)}`} cx="205" cy="40" r="6" /> {/* B */}
        <circle id={`gp-${gamepadIndex}-btn-2`} className={`gp-vis-button ${getButtonClass(2)}`} cx="165" cy="40" r="6" /> {/* X */}
        <circle id={`gp-${gamepadIndex}-btn-3`} className={`gp-vis-button ${getButtonClass(3)}`} cx="185" cy="25" r="6" /> {/* Y */}

        <rect id={`gp-${gamepadIndex}-btn-8`} className={`gp-vis-button ${getButtonClass(8)}`} x="105" y="25" width="10" height="5" /> {/* Back */}
        <rect id={`gp-${gamepadIndex}-btn-9`} className={`gp-vis-button ${getButtonClass(9)}`} x="145" y="25" width="10" height="5" /> {/* Start */}

        <rect id={`gp-${gamepadIndex}-btn-12`} className={`gp-vis-dpad ${getButtonClass(12)}`} x="70" y="50" width="10" height="10" /> {/* Up */}
        <rect id={`gp-${gamepadIndex}-btn-13`} className={`gp-vis-dpad ${getButtonClass(13)}`} x="70" y="70" width="10" height="10" /> {/* Down */}
        <rect id={`gp-${gamepadIndex}-btn-14`} className={`gp-vis-dpad ${getButtonClass(14)}`} x="60" y="60" width="10" height="10" /> {/* Left */}
        <rect id={`gp-${gamepadIndex}-btn-15`} className={`gp-vis-dpad ${getButtonClass(15)}`} x="80" y="60" width="10" height="10" /> {/* Right */}

        <g> {/* Left */}
          <circle className="gp-vis-stick-base" cx="75" cy="30" r="12" />
          <circle id={`gp-${gamepadIndex}-stick-left`} className="gp-vis-stick-top" cx="75" cy="30" r="8" style={{ transform: leftStickTransform }} />
          <circle id={`gp-${gamepadIndex}-btn-10`} className={`gp-vis-button ${getButtonClass(10)}`} cx="75" cy="30" r="3" /> {/* L3 */}
        </g>
        <g> {/* Right */}
          <circle className="gp-vis-stick-base" cx="155" cy="65" r="12" />
          <circle id={`gp-${gamepadIndex}-stick-right`} className="gp-vis-stick-top" cx="155" cy="65" r="8" style={{ transform: rightStickTransform }}/>
          <circle id={`gp-${gamepadIndex}-btn-11`} className={`gp-vis-button ${getButtonClass(11)}`} cx="155" cy="65" r="3" /> {/* R3 */}
        </g>
      </svg>
    </div>
  );
}

export default GamepadVisualizer;
