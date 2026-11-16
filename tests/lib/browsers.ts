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
import { existsSync, readdirSync } from 'node:fs';
import { arch } from 'node:os';
import { join } from 'node:path';

import { chromium, firefox, webkit, type Browser, type BrowserContext } from '@playwright/test';

import { WORKDIR } from './env.ts';

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

/**
 * The persistent Firefox profile: Firefox grants clipboard access only to a
 * profile that carries the permission, and accepts an H.264 video m-line only
 * with the OpenH264 GMP plugin tests/tools/fetch-openh264.sh side-loads.
 */
export const FF_E2E_PROFILE: string = process.env.E2E_FIREFOX_PROFILE || join(WORKDIR, 'firefox-profile');

/**
 * The version of the OpenH264 GMP side-loaded into the persistent profile.
 * Without it Firefox answers a WebRTC offer with the video m-line rejected.
 * @returns The newest version there, or null.
 */
export function openh264Version(): string | null {
    const root = join(FF_E2E_PROFILE, 'gmp-gmpopenh264');
    if (!existsSync(root)) return null;
    for (const version of readdirSync(root).sort().reverse()) {
        if (existsSync(join(root, version, 'libgmpopenh264.so'))) return version;
    }
    return null;
}

/**
 * The prefs that point Firefox at the side-loaded plugin: it scans
 * `gmp-gmpopenh264/<version>` only once a pref names that version, and the
 * GMP updater is turned off so it does not replace it mid-run.
 * @returns The prefs; none without the plugin.
 */
function openh264Prefs(): Record<string, string | number | boolean> {
    const version = openh264Version();
    if (!version) return {};
    return {
        'media.gmp-gmpopenh264.version': version,
        'media.gmp-gmpopenh264.abi': arch() === 'arm64' ? 'aarch64-gcc3' : 'x86_64-gcc3',
        'media.gmp-gmpopenh264.lastUpdate': 1,
        'media.gmp-manager.updateEnabled': false,
    };
}

/**
 * A headless Firefox context on the persistent profile, with the autoplay
 * allowance `launchBrowser` gives Firefox, the clipboard testing pref, and
 * the OpenH264 prefs; the caller closes it.
 * @param options The viewport, further prefs, and further context options.
 * @returns The context.
 */
export function firefoxPersistentContext(options: {
    viewport?: { width: number; height: number };
    prefs?: Record<string, string | number | boolean>;
    hasTouch?: boolean;
} = {}): Promise<BrowserContext> {
    const executablePath = process.env.E2E_FIREFOX || undefined;
    return firefox.launchPersistentContext(FF_E2E_PROFILE, {
        headless: true,
        args: ['--allow-downgrade'],
        firefoxUserPrefs: {
            'media.gmp-gmpopenh264.enabled': true,
            'media.autoplay.default': 0,
            'media.autoplay.blocking_policy': 0,
            'media.autoplay.block-webaudio': false,
            'dom.events.testing.asyncClipboard': true,
            ...openh264Prefs(),
            ...(options.prefs ?? {}),
        },
        ...(options.viewport ? { viewport: options.viewport } : {}),
        ...(options.hasTouch ? { hasTouch: true } : {}),
        ...(executablePath ? { executablePath, channel: 'moz-firefox' } : {}),
    });
}
