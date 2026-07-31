# Selkies Example Dashboard (Modern UI)

## Introduction

This document describes a modern example dashboard built with React and TypeScript, designed for video streaming projects. This is an enhanced version of the original selkies-dashboard, featuring a modern UI built with shadcn/ui components, Tailwind CSS, and TypeScript for improved developer experience and type safety.

The dashboard provides the same core functionality as the original example but with a polished, modern interface that showcases contemporary web development practices. Its primary purpose is to serve as a comprehensive training tool and a starting point for developers looking to create their own custom dashboards that interact with selkies-core using window messaging. By ripping the logic out of frontend code and keeping it standardized the core screen delivery can remain constant while being a custom experience for your users down to theming and settings available to them.

While this example is implemented in React with TypeScript, the underlying principles of interaction via ```window``` messaging are universal and can be applied to any JavaScript framework (Angular, Vue, Svelte, etc.) or even vanilla JavaScript. This allows developers to use this example as a guide for building and branding their own dashboards according to their specific needs and chosen technology stack.

## Core Interaction

The dashboard communicates with selkies-core, which is responsible for the main streaming logic, hardware interactions, and communication with the remote server/service. This interaction is primarily facilitated through two mechanisms:

1.  **`window.postMessage` API:**
    *   **Dashboard to Core:** The dashboard sends JavaScript objects as messages to selkies-core component using ```window.postMessage(message, window.location.origin)```. These messages instruct selkies-core to perform actions or change settings. Examples include:
        *   Modifying video settings (encoder, bitrate, framerate).
        *   Changing audio input/output devices.
        *   Setting screen resolution.
        *   Toggling media pipelines (video, audio, microphone).
        *   Controlling gamepad input.
        *   Updating shared clipboard content.
        *   Requesting fullscreen.
    *   **Core to Dashboard:** Conversely, selkies-core component sends messages back to the dashboard via ```window.postMessage``` to provide status updates, event notifications, or synchronized data. The dashboard listens for these messages using ```window.addEventListener('message', callback)```, ensuring it only processes messages from the same origin. Examples include:
        *   Pipeline status (video/audio/microphone active).
        *   Gamepad events (button/axis updates).
        *   File upload progress and completion.
        *   Server-initiated clipboard updates.
        *   Initial client settings or available server configurations.

2.  **Global `window` Variables:**
    *   For certain real-time performance statistics, the selkies-core component exposes data directly as properties on the global ```window``` object. The dashboard reads these variables periodically (e.g., using ```setInterval```) to update its display. Examples include:
        *   ```window.fps``` (client-side rendering frames per second).
        *   ```window.system_stats``` (server CPU and system memory usage).
        *   ```window.gpu_stats``` (server GPU load and memory usage).
        *   ```window.currentAudioBufferSize``` (client-side audio buffer length).

## Key Features and selkies-core Interactions

This example dashboard showcases how to implement a wide array of functionalities by interacting with the selkies-core component:

*   **Video Settings:**
    *   Controls for Encoder, Framerate, Video Bitrate, and CRF (Constant Rate Factor) where applicable.
    *   *Interaction:* Sends ```window.postMessage({ type: 'settings', settings: { ... } })``` to selkies-core. Receives available encoder options via ```initialClientSettings``` or ```serverSettings``` messages from selkies-core. Settings are persisted in ```localStorage```.

*   **Audio Settings:**
    *   Selection of audio input (microphone) and output (speaker) devices.
    *   *Interaction:* Uses ```navigator.mediaDevices.enumerateDevices()``` to list devices (after permission). Sends ```window.postMessage({ type: 'audioDeviceSelected', context: 'input'/'output', deviceId: '...' })``` to selkies-core. Listens for ```audioDeviceStatusUpdate``` messages from selkies-core.

*   **Screen & Resolution Management:**
    *   Allows selection from resolution presets or manual input of width and height.
    *   Option to toggle local scaling of the video stream when manual resolution is active.
    *   *Interaction:* Sends ```window.postMessage({ type: 'setManualResolution', ... })```, ```window.postMessage({ type: 'resetResolutionToWindow' })```, or ```window.postMessage({ type: 'setScaleLocally', ... })``` to selkies-core.

*   **Performance Statistics Display:**
    *   Visual gauges for CPU, GPU, System Memory, GPU Memory usage (from server), Client FPS, and Client Audio Buffer.
    *   Tooltips provide more detailed information on hover.
    *   *Interaction:* Periodically reads global variables such as ```window.system_stats```, ```window.gpu_stats```, ```window.fps```, and ```window.currentAudioBufferSize```.

*   **Clipboard Synchronization:**
    *   A textarea displays clipboard content shared between the client and server. Edits in the dashboard are sent to selkies-core.
    *   *Interaction:* Sends local changes via ```window.postMessage({ type: 'clipboardUpdateFromUI', ... })```. Receives remote changes via ```window.postMessage({ type: 'clipboardContentUpdate', ... })``` from selkies-core.

*   **File Management:**
    *   **Upload:** A button triggers selkies-core's file input mechanism (via ```window.dispatchEvent(new CustomEvent('requestFileUpload'))```). Upload progress and status are shown via notifications.
    *   **Download:** A modal opens an iframe to a server-side file browser for downloads.
    *   *Interaction (Upload):* Listens for ```window.postMessage({ type: 'fileUpload', payload: { ... } })``` messages from selkies-core for status updates.

*   **Application Management (Apps Modal):**
    *   A modal interface to browse, install, update, and remove applications within the remote environment.
    *   *Interaction:* Fetches app metadata from an external YAML file. Sends ```window.postMessage({ type: 'command', value: 'st ~/.local/bin/proot-apps ...' })``` to selkies-core to execute management commands.

*   **Gamepad Integration:**
    *   Toggles for enabling/disabling overall gamepad input processing and for an on-screen touch gamepad.
    *   Visualizer for connected physical gamepads, showing button presses and axis movements.
    *   *Interaction:* Toggles input via ```window.postMessage({ type: 'gamepadControl', ... })```. Touch gamepad uses ```TOUCH_GAMEPAD_SETUP``` and ```TOUCH_GAMEPAD_VISIBILITY``` messages. Visualizer updates based on ```gamepadButtonUpdate``` and ```gamepadAxisUpdate``` messages from selkies-core.

*   **General UI Controls:**
    *   **Pipeline Toggles (Video, Audio, Microphone):** Buttons to enable/disable media streams.
        *   *Interaction:* Sends ```window.postMessage({ type: 'pipelineControl', ... })```. Button states are updated based on ```pipelineStatusUpdate``` messages from selkies-core.
    *   **Fullscreen Toggle:** Requests the browser to enter fullscreen mode for the video content.
        *   *Interaction:* Sends ```window.postMessage({ type: 'requestFullscreen' })```.
    *   **Theme Toggle (Dark/Light):** Switches the dashboard's visual theme.
    *   **Virtual Keyboard (Mobile):** A button to help trigger the OS's virtual keyboard on touch devices.
        *   *Interaction:* Sends ```window.postMessage({ type: 'showVirtualKeyboard' })```.
    *   **Notifications:** A system to display temporary messages for events like file uploads.

## Modern UI Components

This dashboard leverages **shadcn/ui** for a modern, accessible, and highly customizable component system:

**shadcn/ui Integration:**
- Components are located in `src/components/ui/`
- Fully customizable through CSS variables in `src/index.css`
- TypeScript definitions for all components
- Accessible by default with ARIA attributes
- Consistent design tokens across all components

## Technology Stack

This modern dashboard is built with contemporary web technologies:

**Core Technologies:**
- **React 19** - Modern React with latest features
- **TypeScript** - Type safety and enhanced developer experience
- **Vite** - Fast build tool and development server

**UI Framework:**
- **shadcn/ui** - High-quality, accessible React components
- **Radix UI** - Unstyled, accessible UI primitives
- **Tailwind CSS** - Utility-first CSS framework
- **Lucide React** - Beautiful, customizable icons

**Additional Libraries:**
- **Framer Motion** - Smooth animations and transitions
- **Recharts** - Responsive chart library for performance monitoring
- **next-themes** - Theme switching functionality
- **Sonner** - Toast notifications
- **js-yaml** - YAML parsing for application management

## How to Use This Example as a Training Tool

This dashboard is specifically designed as a learning resource. Developers can:

1.  **Examine the Code:** The main logic is distributed across TypeScript components in ```src/components/dashboard/```. Study how React state (```useState```, ```useEffect```, ```useCallback```) is used with TypeScript types to manage the dashboard's UI and respond to events.
2.  **Identify Core Interactions:**
    *   Look for instances of ```window.postMessage(...)```. These demonstrate how the dashboard sends commands or data *to* selkies-core.
    *   Locate the ```useEffect``` hooks that set up event listeners for ```'message'``` events on the ```window``` object. This is how the dashboard receives messages *from* selkies-core. Pay attention to the origin check (```event.origin === window.location.origin```).
    *   Note where global variables (e.g., ```window.fps```, ```window.system_stats```) are accessed, usually within a ```useEffect``` hook with a timer or a stats-reading interval.
3.  **Study Modern UI Patterns:** Explore how shadcn/ui components are used to create a polished interface:
    *   Component composition with Radix UI primitives
    *   Tailwind CSS for styling and responsive design
    *   TypeScript interfaces for type safety
    *   Theme switching with CSS variables
4.  **Adapt to Other Frameworks:** The core communication patterns are standard JavaScript:
    *   **Sending Messages:** Use ```window.postMessage(yourMessageObject, window.location.origin)```.
    *   **Receiving Messages:** Use ```window.addEventListener('message', (event) => { if (event.origin === window.location.origin) { /* process event.data */ } })```.
    *   **Reading Globals:** Access variables like ```window.fps``` directly.
    This allows the logic seen in the React/TypeScript example to be translated into Angular services, Vuex actions/mutations, Svelte stores, or vanilla JS modules.
5.  **Consult selkies-core API Documentation:** Cross-reference the interactions implemented in this dashboard with the "Selkies Core API" documentation. This will provide a deeper understanding of the expected message formats, data structures, and selkies-core's capabilities.

## Customization and Branding

This modern example dashboard is a robust foundation built with contemporary technologies. Developers are encouraged to:

*   **Modify UI/UX:** Leverage the shadcn/ui component system and Tailwind CSS to easily customize the visual appearance, layout, and specific components to align with their project's branding and user experience goals. The modular component structure makes it easy to swap out or modify individual UI elements.
*   **Extend with shadcn/ui:** Add new shadcn/ui components to expand functionality. Use the CLI command `npx shadcn@latest add [component]` to add new components to your project.
*   **Customize Themes:** Modify the CSS variables in `src/index.css` to create custom color schemes, or extend the theme configuration to match your brand colors.
*   **TypeScript Benefits:** Take advantage of TypeScript's type safety when adding new features or modifying existing ones. The existing type definitions provide a solid foundation for extending functionality.
*   **Tailor Features:** Add new controls specific to their application's needs or remove features that are not relevant. The modular component architecture makes this straightforward.
*   **Adapt Communication:** If interacting with a different backend or a modified Core component, adjust the message types and payloads accordingly, while leveraging the established pattern of ```window``` messaging.

## Development Setup

**Prerequisites:**
- Node.js 18+ and npm/yarn/pnpm
- Modern browser with WebRTC support

**Getting Started:**
```bash
# Install dependencies
npm install

# Start development server
npm run dev

# Build for production
npm run build

# Preview production build
npm run preview

# Lint code
npm run lint
```

**Project Structure:**
```
src/
├── components/
│   ├── dashboard/          # Dashboard-specific components
│   │   ├── settings.tsx    # Video/audio settings
│   │   ├── system-monitoring.tsx  # Performance stats
│   │   ├── files.tsx       # File management
│   │   ├── apps.tsx        # Application management
│   │   ├── gamepad.tsx     # Gamepad controls
│   │   └── ...
│   └── ui/                 # shadcn/ui components
│       ├── button.tsx
│       ├── card.tsx
│       ├── dialog.tsx
│       └── ...
├── lib/
│   └── utils.ts           # Utility functions
└── styles/
    └── globals.css        # Global styles and CSS variables
```

By studying and experimenting with this modern example, developers can gain practical knowledge of contemporary React development practices and a solid starting point for building their own sophisticated, interactive, and branded dashboards for video streaming applications.
