---
title: Web Client and Dashboards
description: The bundled HTML5 client, the dashboard and its three layouts, the on-screen touch gamepad, and the settings that shape what the interface shows.
---

## Web Client

The HTML5 web client is installed beside the `selkies` executable in every package, image, and wheel, and served by the server on its one port. Its core, [`selkies-web-core`](https://github.com/selkies-project/selkies/tree/main/web/selkies-web-core), is the part that streams: it decodes the video with WebCodecs over WebSockets and through the browser's own receiver over WebRTC, paints without a copy where the browser allows it, sends keyboard, pointer, touch, and gamepad input, and carries the clipboard, file transfers, printing, and the microphone and webcam uplinks. It runs on Chromium, Firefox, and Safari, and everything a page can decode is measured on the page rather than assumed of the browser: an encoder the engine cannot play is greyed out in the menus, and a stream it cannot play is reported.

Over WebSockets a browser short of WebCodecs still gets the whole session. Where it has no video decoder the screen comes as striped JPEG, and where it has no WebCodecs audio (Safari before 26, Firefox before 130, and Firefox for Android) Opus is decoded for the speakers and encoded from the microphone by libopus compiled to WebAssembly, [libopus-wasm](https://github.com/openclaw/libopus-wasm). The web build ships it in `codecs/`, about 170 KB compressed, and only such a browser loads it; it needs `import()` in a worker, which Safari has from 15 and Firefox from 114. The webcam goes up as JPEG drawn on a canvas, and a browser that cannot capture the microphone into a 24 kHz context (Firefox before 148) captures it at its own rate and the client resamples it. WebRTC needs none of this: the browser's own media stack carries the sound and the camera.

The core is embeddable in any HTML5 page and is what a custom interface builds on, through `window` messaging; [Session Sharing](../usage.md#session-sharing) and [Secure Mode](../secure-mode.md) describe the URLs a page is opened with.

## Dashboard

One dashboard ships with the client, [`selkies-dashboard`](https://github.com/selkies-project/selkies/tree/main/web/selkies-dashboard): React and TypeScript over the core's `window` messaging, with one set of panels (the video, screen, and audio settings, stats, shortcuts, clipboard, files, printing, apps, sharing, and gamepads) and the stream toggles for the display, audio, microphone, webcam, and gamepad, painted by one of three layouts:

| Layout | `ui_layout` | What it draws |
| --- | --- | --- |
| Sidebar | `default` | A drawer on the left edge, opened by the handle there or by `Control + Shift + M`, with the toggles as a row of tiles and every panel as a section below |
| Toolbar | `wish` | A floating bar along the top edge, dragged where it suits, with the panels in its menus, the settings hanging under it as tabs, and the stats and gamepads as floating panels |
| Edge | `edge` | One round button docked on an edge of the window, the top right corner to begin with, its arrow pointing back into the window, away from where it sinks. It sinks half into its edge at rest, glowing from within in the logo's pink and purple while it does, and comes fully out for a hover, a keyboard focus, or a drag. A tap opens a fan of the toolbar's tools as round buttons on an arc around it (the settings hanging against it first, then the menu, which has the stream switches, then the stats, fullscreen, and the rest, ending with the layout toggle), each named by its tooltip. The buttons, like the panels, turn translucent with the translucency knob. The buttons sit in at most two rows of arcs facing into the window, on a phone as on a desktop: a half circle on an edge, a quarter in a corner, less in a narrow window, a single arc for a few buttons. When there are more buttons than two rows hold, as when a touch client adds its own, the last button of each page, the dots, turns to the next page. A drag carries it to the nearest edge, where it docks at the spot it was dropped (kept per browser). Dropped near a corner it snaps into the corner and tucks in diagonally, hiding behind both edges. While a mouse hovers it or the keyboard focuses it, a smaller button with the Selkies logo slides out beside it and opens the project's page. The stream's live figures (the status marks, the frame rate, the video rate, and the time a frame takes to the wire) ride top-center over the stream |

Every layout carries a toggle that steps to the next, beside the dark and light theme toggle; the pick is kept per browser. `--ui-layout` names the layout a page starts with, and listing one value alone locks it. A layout is a component that arranges the panels, so another is one entry in the dashboard's layout registry; the panels know nothing of where they are painted.

What the shipped interface *shows* is a server setting rather than a build: `--ui-title` and `--ui-show-logo` name and brand the dashboard's header, `--ui-show-sidebar` and `--ui-show-core-buttons` drop the chrome or its device toggles entirely, and one `--ui-sidebar-show-<section>` flag per panel — video, screen, and audio settings, stats, shortcuts, clipboard, files, apps, sharing, gamepads, webcam, fullscreen, gaming mode, trackpad, keyboard button, soft buttons — hides just that one in any layout. These govern the page only: the capability behind a hidden control keeps working, so the feature's own setting (`--webcam-enabled`, `--file-transfers`, `--enable-sharing`) is what actually turns it off. The [Settings Reference](../settings.md#client-interface) lists them.

The apps panel is backed by [proot-apps](https://github.com/linuxserver/proot-apps) in the container images, which [Desktop Container](desktop-image.md) describes.

## Universal Touch Gamepad

The [Universal Touch Gamepad](https://github.com/selkies-project/selkies/tree/main/web/universal-touch-gamepad) is a JavaScript library that adds a customizable on-screen touch gamepad overlay to the web interface, toggled with `Control + Shift + G`. It intercepts `navigator.getGamepads()` to inject a virtual gamepad, making touch devices compatible with applications and games that expect the browser Gamepad API.
