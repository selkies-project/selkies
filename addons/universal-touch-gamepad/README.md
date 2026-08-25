# Universal Touch Gamepad

A JavaScript library that adds a highly customizable on-screen touch gamepad overlay to web pages and web games. It simulates a standard browser Gamepad API device, making it universally compatible with applications expecting gamepad input.

## Features

*   **Plug-and-Play:** Easy to integrate into any web project.
*   **Standard Gamepad Simulation:** Intercepts `navigator.getGamepads()` to inject a virtual gamepad, ensuring compatibility with games using the Gamepad API.
*   **Customizable Profiles:** Define multiple controller layouts (e.g., 8-bit, 16-bit, modern dual-stick) that users can switch between.
*   **Profile Persistence:** User's selected profile is saved in `localStorage`.
*   **Dynamic UI:** Buttons, joysticks, and analog triggers are rendered dynamically based on the selected profile.
*   **Touch Handling:** Supports multi-touch for simultaneous button presses and joystick movement. Includes logic for stick taps (for L3/R3) and analog trigger pressure.
*   **Safe Area Aware:** Positions controls considering configurable safe area padding.

## Quick Start / How to Use

### 1. Include the Script

Add the `universalTouchGamepad.js` script to your HTML file.

### 2. Provide an Anchor Div

The library needs a `div` element in your HTML to know it can initialize. While the touch controls themselves are rendered in a fullscreen overlay, this div acts as an initial target.

### 3. Send Messages to Control the Gamepad

Use `window.postMessage` to communicate with the library.

**Example `index.html`:**

```html
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0, user-scalable=no, maximum-scale=1.0">
    <title>Universal Touch Gamepad Demo</title>
    <style>
        body { font-family: sans-serif; display: flex; flex-direction: column; align-items: center; padding-top: 20px; }
        button { margin: 10px; padding: 10px 20px; font-size: 16px; }
    </style>
</head>
<body>
    <!-- This div is used by the library as an anchor. -->
    <!-- The actual gamepad UI will be a fullscreen overlay. -->
    <div id="touch-gamepad-host"></div>

    <button id="setupShowButton">Setup & Show Gamepad</button>
    <button id="hideButton">Hide Gamepad</button>

    <script src="universalTouchGamepad.js"></script>
    <script>
        const gamepadHostDivId = 'touch-gamepad-host';
        let isGamepadSetup = false;
        let isGamepadVisible = false;

        document.getElementById('setupShowButton').addEventListener('click', () => {
            if (!isGamepadSetup) {
                // Initial setup message. Send this once, typically on page load or when needed.
                window.postMessage({
                    type: 'TOUCH_GAMEPAD_SETUP',
                    payload: {
                        targetDivId: gamepadHostDivId,
                        // initialProfileName: 'modern', // Optional: specify a default profile
                        visible: true // Show immediately after setup
                    }
                }, window.location.origin);
                isGamepadSetup = true;
                isGamepadVisible = true;
            } else if (!isGamepadVisible) {
                // If already set up but hidden, just make it visible
                window.postMessage({
                    type: 'TOUCH_GAMEPAD_VISIBILITY',
                    payload: {
                        visible: true,
                        targetDivId: gamepadHostDivId // Good to include for robustness
                    }
                }, window.location.origin);
                isGamepadVisible = true;
            }
        });

        document.getElementById('hideButton').addEventListener('click', () => {
            if (isGamepadVisible) {
                window.postMessage({
                    type: 'TOUCH_GAMEPAD_VISIBILITY',
                    payload: {
                        visible: false,
                        targetDivId: gamepadHostDivId // Good to include
                    }
                }, window.location.origin);
                isGamepadVisible = false;
            }
        });

        // Optional: Listen for gamepad connection to confirm it's working
        window.addEventListener('gamepadconnected', (e) => {
            if (e.gamepad.id === "Universal Touch Gamepad") {
                console.log('Universal Touch Gamepad connected!', e.gamepad);
            }
        });
    </script>
</body>
</html>
```

### User Experience

*   **Showing/Hiding:** Use buttons in your application (like the example above) to send messages to show or hide the touch gamepad overlay.
*   **Settings Icon (⚙️):** When the gamepad is visible, a settings icon typically appears at the top-right corner. Tapping this icon opens a profile selector.
*   **Profile Selection:** Users can choose from available controller layouts (e.g., "8-bit", "Modern"). The selected profile is saved in their browser's `localStorage` and will be loaded automatically on subsequent visits.
*   **Gamepad Simulation:** The `universalTouchGamepad.js` library overrides the standard `navigator.getGamepads()` browser function. When the touch overlay is active and interacted with, it populates one of the gamepad slots with a virtual "Universal Touch Gamepad". This virtual gamepad reports button presses, joystick movements, and trigger pressure derived from the touch interactions, adhering to the standard Gamepad API. This makes it **universally compatible** with web games and applications that expect standard gamepad input, without requiring any changes to the game's input handling code.

## Developer Section

### Controller Profiles

The core of the gamepad's layout and functionality is defined by "profiles". A profile is a JavaScript object that describes the visual elements (buttons, joysticks, triggers) and their mapping to standard gamepad inputs.

**How Profiles Work:**

*   Profiles are defined within an object (e.g., `profiles`) in `universalTouchGamepad.js`.
*   Each key in this object is a profile ID (e.g., `eightBit`, `modern`), and its value is the profile configuration object.
*   The library renders UI elements based on the currently active profile.
*   Interactions with these UI elements update the state of the virtual gamepad.

**Example: 8-bit Controller Profile**

Here's a snippet of the `eightBit` profile from `universalTouchGamepad.js` to illustrate the structure:

```javascript
// Inside universalTouchGamepad.js
const profiles = {
    // ... other profiles ...
    eightBit: {
        name: "8-bit", // Display name for the profile selector UI
        buttons: [     // Array of digital button definitions
            { 
                id: 'dpadUp',        // Unique string ID for this button element
                index: 12,           // Gamepad API button index (12 is D-pad Up)
                label: '▲',          // Text or emoji to display on the button
                style: {             // CSS-in-JS for styling and positioning
                    left: '70px', 
                    bottom: '130px', 
                    width: '50px', 
                    height: '50px' 
                } 
            },
            { 
                id: 'dpadDown', 
                index: 13,           // 13 is D-pad Down
                label: '▼', 
                style: { left: '70px', bottom: '50px', width: '50px', height: '50px' } 
            },
            // ... more d-pad buttons ...
            { 
                id: 'buttonA_nes', 
                index: 0,            // 0 is typically 'A' or primary action
                label: 'A', 
                style: { right: '110px', bottom: '90px', width: '60px', height: '60px', borderRadius: '15px' } 
            },
            { 
                id: 'select', 
                index: 8,            // 8 is Select/Back
                label: 'SELECT', 
                shape: 'squircle',   // Applies a specific squircle shape style
                style: { left: 'calc(50% - 70px)', bottom: '30px', width: '60px', height: '30px'} 
            },
            // ... more buttons ...
        ],
        clusters: [ // Array of interactive touch areas (for digital buttons)
            { 
                id: 'dpadCluster_8bit', // Unique string ID for this cluster element
                style: {                // CSS-in-JS for the cluster's bounding box
                    left: '10px', 
                    bottom: '40px', 
                    width: '170px', 
                    height: '150px' 
                }, 
                // Links buttons by their 'id' to this touchable area
                buttonIds: ['dpadUp', 'dpadDown', 'dpadLeft', 'dpadRight'] 
            },
            // ... other clusters for face buttons, system buttons etc. ...
        ]
    },
    // ... more profiles like 'sixteenBit', 'modern' ...
};
```

### Profile Configuration Details

A profile object can contain the following top-level keys:

*   `name`: (String) The user-friendly name displayed in the profile selector.
*   `buttons`: (Array of Objects) Defines individual digital buttons. Each button object has:
    *   `id`: (String, **Required**) A unique identifier for this specific button element within the profile. Used for internal tracking and linking to clusters.
    *   `index`: (Number, **Required**) The button index (0-17) that this touch button will trigger in the simulated Gamepad API state. Refer to the "Standard Gamepad Mapping" section below.
    *   `label`: (String, Optional) Text, symbol, or emoji to display on the button.
    *   `style`: (Object, **Required**) A CSS-in-JS style object for positioning and appearance (e.g., `left`, `bottom`, `width`, `height`, `borderRadius`, `fontSize`, `backgroundColor`).
        *   Positioning properties (`left`, `right`, `top`, `bottom`) are applied relative to the fullscreen overlay. The library automatically adds `SAFE_AREA_PADDING` (defined internally) to these values.
        *   Supports values like `'50px'` or `'calc(50% - 25px)'`.
    *   `shape`: (String, Optional) A predefined shape class. Example: `'squircle'` applies a specific rounded rectangle style.
    *   `type`: (String, Optional) A predefined type class for styling. Example: `'digitalShoulder'` for styling shoulder-like buttons.
*   `joysticks`: (Array of Objects, Optional) Defines analog-like joystick controls. Each joystick object has:
    *   `id`: (String, **Required**) A unique identifier for the joystick element.
    *   `axes`: (Array of two Numbers, **Required**) Specifies the Gamepad API axis indices for the X and Y movements of this joystick (e.g., `[0, 1]` for Left Stick X and Y). Values range from -1.0 to 1.0.
    *   `clickButtonIndex`: (Number, Optional) The Gamepad API button index to trigger if the joystick is tapped (e.g., `10` for L3, `11` for R3).
    *   `style`: (Object, **Required**) CSS-in-JS for positioning and base size.
        *   `size`: (String, e.g., `'90px'`) Defines the diameter of the joystick's circular base.
        *   Other properties like `left`, `bottom` for positioning the joystick base.
*   `analogTriggers`: (Array of Objects, Optional) Defines analog-like trigger buttons that report pressure.
    *   `id`: (String, **Required**) A unique identifier for the trigger element.
    *   `buttonIndex`: (Number, **Required**) The Gamepad API button index this trigger corresponds to (e.g., `6` for L2, `7` for R2). This button will report both a `pressed` state (boolean) and an analog `value` (0.0 to 1.0) in the Gamepad API.
    *   `label`: (String, Optional) Text to display on the trigger.
    *   `style`: (Object, **Required**) CSS-in-JS for positioning and appearance.
*   `clusters`: (Array of Objects, **Required if `buttons` are defined and not handled by joysticks/triggers**) Defines invisible touch-sensitive rectangular areas. Clusters manage touch events for a group of digital buttons.
    *   `id`: (String, **Required**) A unique identifier for the cluster element.
    *   `style`: (Object, **Required**) CSS-in-JS to define the position and dimensions of this touchable area.
    *   `buttonIds`: (Array of Strings, **Required**) An array of `id`s (matching the `id`s from the `buttons` array) that this cluster will manage. Touches occurring within the cluster's bounds are evaluated against the bounding boxes of these specified buttons (with a small `HIT_TEST_SLOP`).
        *   **Important:** Ensure that the visual `style` of each button in `buttonIds` places it *within* the area defined by its cluster's `style`. If a button is visually outside its designated cluster, it will not be interactive.

### Creating Your Own Profiles

1.  **Edit `universalTouchGamepad.js`:** Locate the `profiles` object.
2.  **Add a New Profile Entry:** Create a new key-value pair, where the key is your new profile's ID (e.g., `myCustomProfile`) and the value is its configuration object.
3.  **Define Elements:**
    *   Specify `name` for the UI.
    *   Add `buttons`, `joysticks`, and/or `analogTriggers` arrays.
    *   For each element, provide a unique `id`, the correct Gamepad API `index` or `axes`, and `style` for its position and look.
    *   If you have digital `buttons`, define `clusters` to make them interactive. Ensure each cluster's `style` (its touchable area) correctly encompasses all buttons listed in its `buttonIds`.
4.  **Test Thoroughly:** Use the in-browser settings icon to switch to your new profile and test all interactions.

### Standard Gamepad Mapping Reference

When defining `index` for buttons, `axes` for joysticks, or `buttonIndex` for analog triggers, use the W3C Standard Gamepad mapping for best compatibility:

**Buttons ( `index` or `clickButtonIndex` or `buttonIndex` ):**

*   `0`: A / Cross (Face button, typically bottom)
*   `1`: B / Circle (Face button, typically right)
*   `2`: X / Square (Face button, typically left)
*   `3`: Y / Triangle (Face button, typically top)
*   `4`: L1 / LB (Left shoulder)
*   `5`: R1 / RB (Right shoulder)
*   `6`: L2 / LT (Left trigger - can be analog)
*   `7`: R2 / RT (Right trigger - can be analog)
*   `8`: Select / Back / View / Share (Often left of center)
*   `9`: Start / Menu / Options (Often right of center)
*   `10`: L3 / LS (Left stick press)
*   `11`: R3 / RS (Right stick press)
*   `12`: D-pad Up
*   `13`: D-pad Down
*   `14`: D-pad Left
*   `15`: D-pad Right
*   `16`: Home / Guide / PS Button
*   `17`: Touchpad / Mute / Extra (less common, e.g., PS5 Mute or Steam Deck extra buttons)

**Axes ( `axes: [xAxisIndex, yAxisIndex]` ):**

*   `0`: Left Stick X-axis (-1.0 left, +1.0 right)
*   `1`: Left Stick Y-axis (-1.0 up, +1.0 down) *Note: The library internally inverts the Y-axis for intuitive touch control, so positive touch Y maps to negative gamepad Y.*
*   `2`: Right Stick X-axis (-1.0 left, +1.0 right)
*   `3`: Right Stick Y-axis (-1.0 up, +1.0 down) *Note: Same Y-axis inversion as above.*

By adhering to these standard mappings, your custom touch gamepad layouts will work seamlessly with most web games and applications that support the Gamepad API.

## Contributing

Contributions are welcome! Please feel free to submit pull requests or open issues for bugs and feature requests.

## License

This project is licensed under the Mozilla Public License 2.0, like the rest of Selkies.
See the repository [LICENSE](../../LICENSE) file for details.
A copy of the MPL 2.0 license text can also be found at [https://mozilla.org/MPL/2.0/](https://mozilla.org/MPL/2.0/).
