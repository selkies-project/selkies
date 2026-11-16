/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The browser suites: one spec file per suite, one test per selector, run one
// at a time, since every suite drives the one test display and the one
// server port range. The suites' driver runs a spec as one case per selector
// (tests/suites.py) and gives it its budget in SELKIES_SUITE_DEADLINE, which
// is the test's timeout here; a suite prints its checks itself.
import { defineConfig } from '@playwright/test';

import { WORKDIR } from './lib/env.ts';

const budget = Number(process.env.SELKIES_SUITE_DEADLINE ?? '0');

export default defineConfig({
    testDir: '.',
    testMatch: ['integration/*.spec.ts', 'e2e/*.spec.ts'],
    workers: 1,
    fullyParallel: false,
    retries: 0,
    timeout: budget > 0 ? budget * 1000 : 0,
    reporter: [['list']],
    // A click or a navigation waits 30 s, as the Python suites' did: past it
    // the step fails where it stuck instead of holding the suite to its budget.
    use: { actionTimeout: 30_000, navigationTimeout: 30_000 },
    outputDir: `${WORKDIR}/playwright-results`,
    preserveOutput: 'failures-only',
});
