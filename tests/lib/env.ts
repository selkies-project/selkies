/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Where a browser suite finds the tree, the run's work directory and its
 * bounds, from the environment the suites' driver gives every suite.
 * @module
 */

import { mkdirSync } from 'node:fs';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The tests directory, which holds this library. */
export const TESTS: string = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** The repository the suites run from: `SELKIES_REPO`, else the tree holding this file. */
export const REPO: string = process.env.SELKIES_REPO || resolve(TESTS, '..');

/** The suites' C tools and Node audits. */
export const TOOLS: string = join(TESTS, 'tools');

/** The built web client the server serves. */
export const CORE_DIST: string = join(REPO, 'web/selkies-web-core/dist');

/**
 * Where the suites keep server logs and scratch: `E2E_WORKDIR`, else
 * `selkies-tests` under the temporary directory, as the Rust harness has it.
 * The driver copies the `.log` files a suite leaves here into its folder.
 */
export const WORKDIR: string = (() => {
    const dir = process.env.E2E_WORKDIR || join(process.env.TMPDIR || '/tmp', 'selkies-tests');
    if (!isAbsolute(dir) || dir.split('/').includes('..')) {
        throw new Error(`E2E_WORKDIR=${dir} is not an absolute path without '..'`);
    }
    mkdirSync(dir, { recursive: true });
    return dir;
})();

/**
 * How long a browser is given to answer a call that carries no deadline of its
 * own (`E2E_PAGE_CALL_TIMEOUT`, seconds; 0 turns the bound off). Every
 * legitimate one is far quicker; the bound is for an engine that stopped
 * answering altogether.
 */
export const PAGE_CALL_TIMEOUT_S: number = Number(process.env.E2E_PAGE_CALL_TIMEOUT ?? '60') || 0;

/**
 * The suite a spec file holds, its logs named after it: the file's name
 * without `.spec.ts`.
 * @param file The spec's path (`import.meta.filename`).
 * @returns `gamepad_release` for `gamepad_release.spec.ts`.
 */
export function suiteOf(file: string): string {
    return basename(file).replace(/\.spec\.ts$/, '');
}

/**
 * Wait `ms` milliseconds.
 * @param ms How long.
 * @returns A promise settled after it.
 */
export function sleep(ms: number): Promise<void> {
    return new Promise((done) => { setTimeout(done, ms); });
}
