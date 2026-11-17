/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A virtual-keyboard keymap the compositor cannot trust leaves it serving
// (Wayland).
//
// A client of the session's compositor hands it a keymap whose size runs past
// its file (the crate's wl-vk-keymap program): the compositor ignores that
// keymap, says so in the log, and keeps serving, so a fresh client still maps
// a window.
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Observer } from '../lib/host.ts';

const WL_SOCKET = 'wayland-1';

test('wayland_virtual_keyboard', async () => {
    const res = new Checks('wl-virtual-keyboard');
    const host = new Host(suiteOf(import.meta.filename));
    let obs: Observer | null = null;
    try {
        await host.start({ label: 'vk', mode: 'websockets', wayland: true });
        const client = await host.observe(WL_SOCKET, {}, 'wl-vk-keymap');
        const sent = await client.wait('sent', 60);
        res.check('the client sends a keymap claiming 1 GiB of a 15-byte file', sent !== null, sent ?? [...await client.lines(), ...await client.output()].slice(-3));
        await client.release();
        await sleep(1000);
        res.check('the compositor ignores it and says so', (await host.log()).includes('virtual-keyboard keymap of 1073741824 bytes'), '');
        obs = await host.observe(WL_SOCKET);
        res.check("and still maps a fresh client's window", await obs.ready(15), (await obs.lines()).slice(-3));
    } finally {
        await obs?.release();
        await host.stop();
        await host.close();
    }
    res.finish();
});
