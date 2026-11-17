/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Host-capture virtual-pointer events drive labwc's release-gated root menu.
//
// The suite sends the same browser input a remote user sends, through
// Selkies' Wayland-host capture path into a private headless labwc. It checks
// the actual virtual-pointer wire timestamps as well as labwc's ShowMenu
// actions and the Terminal command. Labwc ignores a menu-item release whose
// timestamp follows the press within its ignore period; an all-zero
// virtual-pointer clock therefore opens the root menu but cannot activate its
// item.
import { spawn } from 'node:child_process';
import { chmodSync, existsSync, lstatSync, mkdirSync, openSync, readFileSync, rmSync, statSync, writeFileSync, closeSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { realConsole, waitWsVideo } from '../lib/stream.ts';

const CONFIG_XML = `<?xml version="1.0"?>
<labwc_config>
  <core><decoration>server</decoration></core>
  <keyboard>
    <keybind key="C-A-t"><action name="Execute"><command>sh -c 'echo key >> MARKER'</command></action></keybind>
  </keyboard>
  <mouse>
    <default />
    <context name="Root">
      <mousebind button="Left" action="Press" />
      <mousebind button="Middle" action="Press"><action name="ShowMenu" menu="client-list-combined-menu" /></mousebind>
    </context>
  </mouse>
</labwc_config>
`;
const MENU_XML = `<?xml version="1.0"?>
<openbox_menu>
  <menu id="root-menu" label="Root">
    <item label="Terminal"><action name="Execute"><command>sh -c 'echo menu >> MARKER'</command></action></item>
  </menu>
</openbox_menu>
`;

const isSocket = (path: string): boolean => existsSync(path) && lstatSync(path).isSocket();
const tail = (text: string, n: number): string => text.trimEnd().split('\n').slice(-n).join('\n');

async function run(host: Host, res: Checks): Promise<void> {
    const root = join(WORKDIR, 'wl-host-menu-rig');
    const config = join(root, 'config', 'labwc');
    // The private compositor's own runtime directory: the server is handed its socket by path.
    const runtime = join(root, 'run');
    rmSync(runtime, { recursive: true, force: true });
    mkdirSync(config, { recursive: true });
    mkdirSync(runtime, { recursive: true });
    chmodSync(runtime, 0o700);
    const marker = join(root, 'actions.log');
    const labwcLog = join(root, 'labwc.log');
    writeFileSync(join(config, 'rc.xml'), CONFIG_XML.replaceAll('MARKER', marker));
    writeFileSync(join(config, 'menu.xml'), MENU_XML.replaceAll('MARKER', marker));
    rmSync(marker, { force: true });
    const env: NodeJS.ProcessEnv = {
        PATH: process.env.PATH ?? '', HOME: homedir(), XDG_RUNTIME_DIR: runtime, XDG_CONFIG_HOME: join(root, 'config'), XDG_CONFIG_DIRS: '/etc/xdg',
        WLR_BACKENDS: 'headless', WLR_RENDERER: 'pixman', WLR_HEADLESS_OUTPUTS: '1', WLR_LIBINPUT_NO_DEVICES: '1', LIBGL_ALWAYS_SOFTWARE: '1',
    };
    const hostSocket = join(runtime, 'wayland-0');
    const out = openSync(labwcLog, 'w');
    const labwc = spawn('labwc', ['--debug', '--config-dir', config], { env, stdio: ['ignore', out, out], detached: true });
    const labwcText = (): string => readFileSync(labwcLog, { encoding: 'utf8' });
    const labwcSince = (offset: number): string => readFileSync(labwcLog).subarray(offset).toString('utf8');
    const actions = (): string[] => (existsSync(marker) ? readFileSync(marker, 'utf8').split('\n').filter(Boolean) : []);
    let client: Awaited<ReturnType<typeof launchClient>> | null = null;
    try {
        for (const until = Date.now() + 15_000; !isSocket(hostSocket) && Date.now() < until && labwc.exitCode === null;) await sleep(50);
        res.check('private headless labwc publishes a socket', isSocket(hostSocket), tail(labwcText(), 10));
        if (!isSocket(hostSocket)) return;

        const { url } = await host.start({ label: 'menu', mode: 'websockets', wayland: true, env: {
            SELKIES_WAYLAND_HOST_DISPLAY: hostSocket, SELKIES_WIDTH: '1280', SELKIES_HEIGHT: '720', WAYLAND_DEBUG: '1', LIBGL_ALWAYS_SOFTWARE: '1',
        } });
        client = await launchClient(url, { mode: 'websockets' });
        const { page } = client;
        const video = await waitWsVideo(page, 60);
        res.check('the remote page displays the host desktop', video !== null, tail(await host.log(), 15));
        if (video === null) return;
        res.check("host capture binds labwc's virtual-input protocols", await host.saw('[HostCapture] capturing host compositor', 5), tail(await host.log(), 10));
        await sleep(1000);

        /** One 40 ms browser click, and the compositor given time to settle. */
        const click = async (p: Page, x: number, y: number, button: 'left' | 'right' | 'middle'): Promise<void> => {
            await p.mouse.click(x, y, { button, delay: 40 });
            await sleep(600);
        };

        const leftOffset = statSync(labwcLog).size;
        await click(page, 640, 300, 'left');
        await click(page, 680, 315, 'left');
        res.check('left root clicks open no menu and execute nothing', !labwcSince(leftOffset).includes('ShowMenu') && actions().length === 0, labwcSince(leftOffset).slice(-500));

        const wireMark = await host.mark();
        const rightOffset = statSync(labwcLog).size;
        await click(page, 640, 300, 'right');
        res.check("right root click opens labwc's root menu", labwcSince(rightOffset).includes('ShowMenu'), labwcSince(rightOffset).slice(-500));
        await page.screenshot({ path: join(root, 'root-menu.png') });
        await click(page, 680, 315, 'left');
        await sleep(800);
        const wireText = await host.log(wireMark);
        const wire = [...wireText.matchAll(/zwlr_virtual_pointer_v1@\d+\.button\((\d+), (\d+), ([01])\)/g)].map((m) => ({ time: Number(m[1]), button: Number(m[2]), state: Number(m[3]) }));
        const durations: number[] = [];
        for (const button of [273, 272]) {
            const states = wire.filter((e) => e.button === button);
            if (states.length === 2 && states[0].state === 1 && states[1].state === 0) durations.push(((states[1].time - states[0].time) >>> 0));
        }
        res.check('right open and item selection each carry one press/release pair', wire.length === 4, JSON.stringify(wire));
        res.check('button times are live, ordered, and leave the item-release gate', wire.length === 4 && wire.every((e) => e.time !== 0) && durations.length === 2 && durations.every((n) => n > 0 && n < 250),
            JSON.stringify({ events: wire, durations_ms: durations }));
        res.check('selecting Terminal executes its real labwc action', actions().includes('menu'), actions());

        await click(page, 100, 400, 'left');
        const middleOffset = statSync(labwcLog).size;
        await click(page, 100, 300, 'middle');
        const middleLog = labwcSince(middleOffset);
        res.check('middle root click opens the configured client list', middleLog.includes('ShowMenu'), middleLog.slice(-500));
        await page.screenshot({ path: join(root, 'client-list.png') });
        await click(page, 100, 400, 'left');

        await page.keyboard.down('Control');
        await page.keyboard.down('Alt');
        await page.keyboard.press('t');
        await page.keyboard.up('Alt');
        await page.keyboard.up('Control');
        await sleep(800);
        res.check('adjacent virtual-keyboard input still executes Ctrl+Alt+T', actions().includes('key'), actions());
        const real = realConsole(client.consoleErrors, client.notFound);
        res.check('the browser reports no errors', real.errors.length === 0 && real.notFound.length === 0, [...real.errors, ...real.notFound].slice(0, 3));
    } finally {
        await client?.browser.close();
        await host.stop();
        if (labwc.pid !== undefined) {
            try {
                process.kill(-labwc.pid, 'SIGTERM');
            } catch {
                // already gone
            }
            for (const until = Date.now() + 5000; labwc.exitCode === null && labwc.signalCode === null && Date.now() < until;) await sleep(50);
            if (labwc.exitCode === null && labwc.signalCode === null) labwc.kill('SIGKILL');
        }
        closeSync(out);
    }
}

test('wayland_host_menu', async () => {
    const res = new Checks('wl-host-menu');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        if (onPath('labwc') === null) res.skip('the suite', 'labwc is not installed');
        else await run(host, res);
    } finally {
        await host.close();
    }
    res.finish();
});
