/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The browsers the suites drive, launched as every suite launches them.
 * @module
 */

import { chromium, type Browser } from '@playwright/test';

/** The switches every Chromium of the suites runs with. */
export const BROWSER_ARGS: readonly string[] = [
    '--no-sandbox', '--use-gl=swiftshader', '--mute-audio',
    '--autoplay-policy=no-user-gesture-required',
];

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
