---
title: Web Client and Dashboards
description: The bundled HTML5 client, the two dashboards and how a session picks one, the on-screen touch gamepad, and the settings that shape what the interface shows.
---

## Web Client

The HTML5 web client is bundled into the `selkies` wheel and served by the Python application on its one port. Its core, [`selkies-web-core`](https://github.com/selkies-project/selkies/tree/main/addons/selkies-web-core), is the part that streams: it decodes the video with WebCodecs over WebSockets and through the browser's own receiver over WebRTC, paints without a copy where the browser allows it, sends keyboard, pointer, touch, and gamepad input, and carries the clipboard, file transfers, printing, and the microphone and webcam uplinks. It runs on Chromium, Firefox, and Safari, and everything a page can decode is measured on the page rather than assumed of the browser: an encoder the engine cannot play is greyed out in the menus, and a stream it cannot play is reported.

Over WebSockets a browser short of WebCodecs still gets the whole session. Where it has no video decoder the screen comes as striped JPEG, and where it has no WebCodecs audio (Safari before 26, Firefox before 130, and Firefox for Android) Opus is decoded for the speakers and encoded from the microphone by libopus compiled to WebAssembly, [libopus-wasm](https://github.com/openclaw/libopus-wasm). The web build ships it in `codecs/`, about 170 KB compressed, and only such a browser loads it; it needs `import()` in a worker, which Safari has from 15 and Firefox from 114. The webcam goes up as JPEG drawn on a canvas, and a browser that cannot capture the microphone into a 24 kHz context (Firefox before 148) captures it at its own rate and the client resamples it. WebRTC needs none of this: the browser's own media stack carries the sound and the camera.

The core is embeddable in any HTML5 page and is what a custom interface builds on, through `window` messaging; [Session Sharing](../usage.md#session-sharing) and [Secure Mode](../secure-mode.md) describe the URLs a page is opened with.

## Dashboards

Two dashboards ship in one package, [`selkies-dashboard`](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard), and a page loads the one it is set to. Both are React, both put the same controls over the stream, both are starting points for a dashboard of your own rather than a required component, and the logic they share that is not about drawing (the storage keys the streaming cores read, the settings ladder, which controls the server's settings leave to the user) lives once in the core's library:

| Dashboard | Purpose |
| --- | --- |
| Classic (`src/classic`) | The default: the sidebar with the display, audio, microphone, webcam, and gamepad toggles up top, and the video, screen, and audio settings, stats, shortcuts, clipboard, file transfer, apps, sharing, gamepad, and appearance sections below |
| Wish (`src/components`) | The TypeScript variant on shadcn/ui with the same controls behind a floating top bar, a settings card, and floating stats and gamepad panels |

Which one a page shows, and in which color mode, are two client settings kept per browser like the stream settings and offered, narrowed, or locked by the server the same way: `--ui-shell` (`classic` or `wish`) and `--ui-mode` (`dark`, `light`, or `system`, which follows the browser's preference). A single value locks the choice and hides its control, as `--encoder` does for the encoder menu, and a comma-separated list makes the first the default. Each dashboard offers both choices as two buttons beside each other, the classic sidebar in its header and the wish top bar in its overflow menu's footer: a sun and moon toggle between dark and light, and a button that reloads the page into the other dashboard. The toggle picks dark or light only, so `system` is a default an operator sets with `--ui-mode`, kept until the user flips the toggle.

What the shipped interface *shows* is a server setting rather than a build: `--ui-title` and `--ui-show-logo` name and brand the header, `--ui-show-sidebar` and `--ui-show-core-buttons` drop the dashboard or its device toggles entirely, and one `--ui-sidebar-show-<section>` flag per section — video, screen, and audio settings, stats, shortcuts, clipboard, files, apps, sharing, gamepads, webcam, fullscreen, gaming mode, trackpad, keyboard button, soft buttons — hides just that one in either dashboard. These govern the page only: the capability behind a hidden control keeps working, so the feature's own setting (`--webcam-enabled`, `--file-transfers`, `--enable-sharing`) is what actually turns it off. The [Settings Reference](../settings.md#client-interface) lists them.

The apps panel of both dashboards is backed by [proot-apps](https://github.com/linuxserver/proot-apps) in the container images, which [Desktop Container](desktop-image.md) describes.

## Universal Touch Gamepad

The [Universal Touch Gamepad](https://github.com/selkies-project/selkies/tree/main/addons/universal-touch-gamepad) is a JavaScript library that adds a customizable on-screen touch gamepad overlay to the web interface, toggled with `Control + Shift + G`. It intercepts `navigator.getGamepads()` to inject a virtual gamepad, making touch devices compatible with applications and games that expect the browser Gamepad API.
