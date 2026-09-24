---
title: Web Client and Dashboards
description: The bundled HTML5 client, the two reference dashboards and what each is for, the on-screen touch gamepad, and the settings that shape what the interface shows.
---

## Web Client

The HTML5 web client is bundled into the `selkies` wheel and served by the Python application on its one port. Its core, [`selkies-web-core`](https://github.com/selkies-project/selkies/tree/main/addons/selkies-web-core), is the part that streams: it decodes the video with WebCodecs over WebSockets and through the browser's own receiver over WebRTC, paints without a copy where the browser allows it, sends keyboard, pointer, touch, and gamepad input, and carries the clipboard, file transfers, printing, and the microphone and webcam uplinks. It runs on Chromium, Firefox, and Safari, and everything a page can decode is measured on the page rather than assumed of the browser: an encoder the engine cannot play is greyed out in the menus, and a stream it cannot play is reported.

The core is embeddable in any HTML5 page and is what a custom interface builds on, through `window` messaging; [Session Sharing](../usage.md#session-sharing) and [Secure Mode](../secure-mode.md) describe the URLs a page is opened with.

## Dashboards

Two reference dashboards ship with the client, and the server chooses which is served. Both are React, both put the same controls in a sidebar, and both are starting points for a dashboard of your own rather than a required component:

| Dashboard | Purpose |
| --- | --- |
| [Selkies Dashboard](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard) | The default: the classic sidebar with the display, audio, microphone, webcam, and gamepad toggles up top, and the video, screen, and audio settings, stats, shortcuts, clipboard, file transfer, apps, sharing, and gamepad sections below |
| [Selkies Dashboard (Wish)](https://github.com/selkies-project/selkies/tree/main/addons/selkies-dashboard-wish) | The TypeScript variant with the same sections in a different layout, written as the example a modern toolchain starts from |

What the shipped interface *shows* is a server setting rather than a build: `--ui-title` and `--ui-show-logo` name and brand the sidebar header, `--ui-show-sidebar` and `--ui-show-core-buttons` drop the sidebar or its device toggles entirely, and one `--ui-sidebar-show-<section>` flag per section — video, screen, and audio settings, stats, shortcuts, clipboard, files, apps, sharing, gamepads, webcam, fullscreen, gaming mode, trackpad, keyboard button, soft buttons — hides just that one. These govern the page only: the capability behind a hidden control keeps working, so the feature's own setting (`--webcam-enabled`, `--file-transfers`, `--enable-sharing`) is what actually turns it off. The [Settings Reference](../settings.md#client-interface) lists them.

The apps panel of both dashboards is backed by [proot-apps](https://github.com/linuxserver/proot-apps) in the container images, which [Desktop Container](desktop-image.md) describes.

## Universal Touch Gamepad

The [Universal Touch Gamepad](https://github.com/selkies-project/selkies/tree/main/addons/universal-touch-gamepad) is a JavaScript library that adds a customizable on-screen touch gamepad overlay to the web interface, toggled with `Control + Shift + G`. It intercepts `navigator.getGamepads()` to inject a virtual gamepad, making touch devices compatible with applications and games that expect the browser Gamepad API.
