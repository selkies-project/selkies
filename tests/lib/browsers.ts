/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The browsers the suites drive, launched as every suite launches them.
 * @module
 */

import { spawnSync } from 'node:child_process';

import { chromium, firefox, webkit, type Browser } from '@playwright/test';

/** An engine a suite drives. */
export type Engine = 'chromium' | 'firefox' | 'webkit';

/** The switches every Chromium of the suites runs with. */
export const BROWSER_ARGS: readonly string[] = [
    '--no-sandbox', '--use-gl=swiftshader', '--mute-audio',
    '--autoplay-policy=no-user-gesture-required',
];

/**
 * GStreamer's hardware VP8 decoders, ranked out of the suites' WebKit so it
 * decodes VP8 as CI's GPU-less runners do: ranked above vp8dec, they would
 * paint WebRTC VP8 with GStreamer's default matrix for its size whatever its
 * RTP color space declares, a WebKit fault that fails the color checks on a
 * host with NVDEC or VA-API.
 */
const WEBKIT_SOFTWARE_VP8 = 'nvvp8dec:0,vavp8dec:0,v4l2slvp8dec:0';

/**
 * A headless Chromium: Playwright's own, or the system Chrome `E2E_CHROME`
 * names, which is how a release Chrome build gets covered (its media stack
 * differs from Chromium's).
 * @param extraArgs Switches appended to the shared set.
 * @returns The browser.
 */
export function launchChromium(extraArgs: readonly string[] = []): Promise<Browser> {
    const executablePath = process.env.E2E_CHROME || undefined;
    return chromium.launch({ headless: true, args: [...BROWSER_ARGS, ...extraArgs], executablePath });
}

let glSink: boolean | undefined;

/**
 * Whether GStreamer carries the `opengl` plugin WebKit's video sink needs.
 * Playwright's Linux WebKit shows every `<video>` through its GL sink, and
 * without the plugin falls back, silently, to a software sink that paints
 * WebRTC streams differently and can stall them. Probed once through
 * `gst-inspect-1.0`; a host without that tool is taken at its word.
 * @returns Whether the plugin is there.
 */
export function webkitGlSinkReady(): boolean {
    if (glSink === undefined) {
        const probe = spawnSync('gst-inspect-1.0', ['opengl'], { stdio: 'ignore' });
        glSink = probe.error !== undefined || probe.status === 0;
    }
    return glSink;
}

/**
 * A headless browser of `engine`. A release Firefox (`E2E_FIREFOX`) has no
 * Juggler, the protocol Playwright's own Firefox build speaks, so it is
 * driven over WebDriver BiDi, which the `moz-firefox` channel selects; a
 * profile last opened by a newer build needs `--allow-downgrade`. The
 * `<video>` carries the audio, so Firefox is given the autoplay allowance
 * Chromium gets through its switches.
 * @param engine The engine.
 * @returns The browser.
 * @throws {Error} For WebKit where GStreamer lacks its sink's plugin.
 */
export function launchBrowser(engine: Engine = 'chromium'): Promise<Browser> {
    if (engine === 'firefox') {
        const executablePath = process.env.E2E_FIREFOX || undefined;
        return firefox.launch({
            headless: true,
            args: ['--allow-downgrade'],
            firefoxUserPrefs: {
                'media.autoplay.default': 0,
                'media.autoplay.blocking_policy': 0,
                'media.autoplay.block-webaudio': false,
            },
            ...(executablePath ? { executablePath, channel: 'moz-firefox' } : {}),
        });
    }
    if (engine === 'webkit') {
        if (!webkitGlSinkReady()) {
            throw new Error('GStreamer has no opengl plugin, so WebKit would paint through its software fallback sink; '
                + 'install gstreamer1.0-gl (playwright install --with-deps webkit)');
        }
        const ranks = [process.env.GST_PLUGIN_FEATURE_RANK ?? '', WEBKIT_SOFTWARE_VP8].filter(Boolean).join(',');
        return webkit.launch({ headless: true, env: { ...process.env, GST_PLUGIN_FEATURE_RANK: ranks } });
    }
    return launchChromium();
}

/**
 * The engine `E2E_ENGINE` names, Chromium by default.
 * @returns The engine.
 * @throws {Error} When it names none of them.
 */
export function engineFromEnv(): Engine {
    const named = process.env.E2E_ENGINE || 'chromium';
    if (named === 'chromium' || named === 'firefox' || named === 'webkit') return named;
    throw new Error(`E2E_ENGINE=${named} is not chromium, firefox or webkit`);
}
