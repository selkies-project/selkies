/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The desktop through Jupyter's server proxy, as selkies session
// --jupyter-config offers it.
//
// The configuration it prints, as the jupyter_server_config.json of the
// Jupyter server's configuration directory, registers a server process with
// jupyter-server-proxy ahead of any entry point an installed wheel adds, so a
// Jupyter server lists a Selkies item, its icon the web client's, and serves
// the desktop at /selkies/: it starts the launcher on a Unix socket of the
// proxy's own and carries the page, the API, and the data WebSocket behind
// Jupyter's token, its Xvfb screen started at the images' size rather than the
// whole framebuffer. Proved in a browser, where the client loads under the
// /selkies/ prefix and video flows through the proxy. The desktop is a
// stand-in the server's data directories hold, so the host's own never starts
// beside the one it runs. A Jupyter server killed outright leaves no session
// behind, the stand-in's daemonized helper included: the launcher follows its
// parent down.
//
// Needs jupyter-server and jupyter-server-proxy beside the python3 on PATH,
// the one tool of this suite that is Python's.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, mkdtempSync, openSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { request } from 'node:http';
import { createServer, type AddressInfo } from 'node:net';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { CORE_DIST, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWsVideo } from '../lib/stream.ts';

const LOG = join(WORKDIR, 'jupyter.log');
const PYTHON = 'python3';

/** [status, body] of a GET with Jupyter's token; a redirect is reported, not followed (Jupyter refuses by sending the browser to its login page). */
function get(url: string, token: string): Promise<[number, Buffer]> {
    return new Promise((resolve) => {
        const req = request(url, { headers: token ? { Authorization: `token ${token}` } : {}, timeout: 5000 }, (r) => {
            const chunks: Buffer[] = [];
            r.on('data', (c: Buffer) => chunks.push(c));
            r.on('end', () => { resolve([r.statusCode ?? -1, (r.statusCode ?? 0) >= 300 && (r.statusCode ?? 0) < 400 ? Buffer.alloc(0) : Buffer.concat(chunks)]); });
        });
        req.on('error', () => { resolve([-1, Buffer.alloc(0)]); });
        req.on('timeout', () => { req.destroy(); });
        req.end();
    });
}

const parse = (body: Buffer): unknown => {
    try {
        const value: unknown = JSON.parse(body.toString() || '{}');
        return value;
    } catch {
        return {};
    }
};
const field = (o: unknown, k: string): unknown => {
    const value: unknown = o !== null && typeof o === 'object' ? Reflect.get(o, k) : undefined;
    return value;
};

/** Whether python3 carries Jupyter's server and its proxy. */
function registered(): boolean {
    return spawnSync(PYTHON, ['-c', 'import jupyter_server, jupyter_server_proxy']).status === 0;
}

async function freePort(): Promise<number> {
    const server = createServer();
    await new Promise<void>((resolve) => { server.listen(0, '127.0.0.1', () => { resolve(); }); });
    const address: AddressInfo | string | null = server.address();
    await new Promise<void>((resolve) => { server.close(() => { resolve(); }); });
    return address !== null && typeof address === 'object' ? address.port : 0;
}

/**
 * Register the server under test with jupyter-server-proxy: the configuration
 * selkies session --jupyter-config prints, as the configuration directory's
 * jupyter_server_config.json. Jupyter reads only the extensions' switches
 * from jupyter_server_config.d, and the proxy registers the servers its
 * configuration names before those of entry points.
 */
function offerSelkies(binary: string, configDir: string): void {
    const printed = spawnSync(binary, ['session', '--jupyter-config'], { encoding: 'utf8', timeout: 30_000 });
    if (printed.status !== 0) throw new Error(`selkies session --jupyter-config exited ${printed.status}`);
    JSON.parse(printed.stdout);
    mkdirSync(configDir);
    writeFileSync(join(configDir, 'jupyter_server_config.json'), printed.stdout);
}

async function startJupyter(binary: string, token: string, runtime: string, wayland: boolean): Promise<[ChildProcess, number]> {
    const port = await freePort();
    const share = join(runtime, 'share');
    const config = join(runtime, 'config');
    for (const kind of ['xsessions', 'wayland-sessions']) {
        mkdirSync(join(share, kind), { recursive: true });
        writeFileSync(join(share, kind, 'standin.desktop'), "[Desktop Entry]\nName=Stand-in\nExec=sh -c 'setsid sleep 3600 & exec sleep 3600'\n");
    }
    const env = {
        PATH: process.env.PATH ?? '', HOME: process.env.HOME ?? '', XDG_RUNTIME_DIR: runtime, XDG_DATA_DIRS: share, XDG_DATA_HOME: share,
        JUPYTER_CONFIG_DIR: config, JUPYTER_DATA_DIR: join(runtime, 'data'), JUPYTER_RUNTIME_DIR: join(runtime, 'jupyter'),
        SELKIES_WEB_ROOT: CORE_DIST, SELKIES_WAYLAND: wayland ? 'true' : 'false',
    };
    offerSelkies(binary, config);
    const out = openSync(LOG, 'w');
    const proc = spawn(PYTHON, ['-m', 'jupyter_server', '--ServerApp.ip=127.0.0.1', `--ServerApp.port=${port}`, '--ServerApp.port_retries=0',
        `--ServerApp.token=${token}`, `--ServerApp.root_dir=${runtime}`, '--ServerApp.open_browser=False'],
    { env, cwd: WORKDIR, stdio: ['ignore', out, out], detached: true });
    for (const until = Date.now() + 90_000; Date.now() < until;) {
        if ((await get(`http://127.0.0.1:${port}/api/status`, token))[0] === 200) return [proc, port];
        if (proc.exitCode !== null) throw new Error(`jupyter exited ${proc.exitCode}; see ${LOG}`);
        await sleep(500);
    }
    throw new Error(`jupyter did not answer within 90 s; see ${LOG}`);
}

/** A process's command line, spaces for its separators; empty once it is gone. */
function cmdline(pid: number): string {
    try {
        return readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').join(' ').trim();
    } catch {
        return '';
    }
}

/** The launcher the proxy started, as the child of the Jupyter server running selkies session. */
function launcherOf(jupyter: ChildProcess): number | null {
    const server = jupyter.pid ?? 0;
    for (const pid of readFileSync(`/proc/${server}/task/${server}/children`, 'utf8').split(/\s+/).filter(Boolean)) {
        if (cmdline(Number(pid)).includes('session --follow')) return Number(pid);
    }
    return null;
}

/** The processes whose runtime directory is a session's the launcher made under `runtime`. */
function sessionProcesses(runtime: string): number[] {
    const marker = `XDG_RUNTIME_DIR=${runtime}/selkies-session-`;
    const found: number[] = [];
    for (const entry of readdirSync('/proc')) {
        if (!/^\d+$/.test(entry)) continue;
        try {
            if (readFileSync(`/proc/${entry}/environ`, 'latin1').split('\0').some((v) => v.startsWith(marker))) found.push(Number(entry));
        } catch {
            // gone, or not ours
        }
    }
    return found;
}

const logLines = (needle: string): string[] => readFileSync(LOG, 'utf8').split('\n').filter((l) => l.includes(needle));

async function block(res: Checks, binary: string, wayland: boolean): Promise<void> {
    const tag = wayland ? '[wayland]' : '[x11]';
    const token = randomBytes(16).toString('hex');
    const runtime = mkdtempSync(join(WORKDIR, 'jupyter-'));
    let jupyter: ChildProcess;
    let port: number;
    try {
        [jupyter, port] = await startJupyter(binary, token, runtime, wayland);
    } catch (e) {
        res.check(`${tag} the Jupyter server starts`, false, String(e));
        return;
    }
    res.check(`${tag} the Jupyter server starts`, true, port);
    const base = `http://127.0.0.1:${port}`;
    let launcher: number | null;
    try {
        let [code, body] = await get(`${base}/server-proxy/servers-info`, token);
        const processes = code === 200 ? field(parse(body), 'server_processes') : [];
        const entry: unknown = Array.isArray(processes) ? (processes.find((s: unknown) => field(s, 'name') === 'selkies') ?? {}) : {};
        const launcherEntry = field(entry, 'launcher_entry');
        res.check(`${tag} the proxy lists the Selkies desktop`, field(launcherEntry, 'title') === 'Selkies', entry);
        res.check(`${tag} the desktop opens in a tab of its own`, field(entry, 'new_browser_tab') === true);
        const rawIcon = field(launcherEntry, 'icon_url');
        const iconUrl = typeof rawIcon === 'string' ? rawIcon : '';
        let icon: Buffer = Buffer.alloc(0);
        let iconCode: number | null = null;
        if (iconUrl) [iconCode, icon] = await get(`${base}${iconUrl}`, token);
        res.check(`${tag} the launcher's icon is the web client's`, iconCode === 200 && icon.includes('<svg'), [iconUrl, iconCode, icon.subarray(0, 40).toString()]);
        const browser = await launchBrowser('chromium');
        try {
            const context = await browser.newContext({ viewport: { width: 1280, height: 720 } });
            await context.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            const page = await context.newPage();
            const errors: string[] = [];
            page.on('pageerror', (e) => errors.push(String(e)));
            await page.goto(`${base}/selkies/?token=${token}`, { waitUntil: 'load' });
            res.check(`${tag} the client loads under the proxy's prefix`, page.url().startsWith(`${base}/selkies/`), page.url());
            const info = await waitWsVideo(page, 120);
            res.check(`${tag} video flows through the proxy`, info !== null, info);
            res.check(`${tag} the page raised no error`, errors.length === 0, errors.slice(0, 2));
        } finally {
            await browser.close();
        }
        [code, body] = await get(`${base}/selkies/api/status`, token);
        const mode = code === 200 ? field(parse(body), 'current_mode') : null;
        res.check(`${tag} the API answers behind the proxy, on the WebSocket transport`, code === 200 && mode === 'websockets', [code, mode]);
        const refused = (await get(`${base}/selkies/api/status`, ''))[0];
        res.check(`${tag} without Jupyter's token the desktop is not reachable`, [302, 401, 403].includes(refused), refused);
        launcher = launcherOf(jupyter);
        res.check(`${tag} the proxy runs the launcher as its child`, launcher !== null, launcher);
        const desktop = logLines('[selkies-session] desktop ');
        res.check(`${tag} the session the server's data directories hold runs`, desktop.length > 0 && desktop[0].includes('desktop standin on '), desktop.slice(0, 1));
        if (!wayland) {
            const sized = logLines('[selkies-session] the screen ');
            res.check(`${tag} the Xvfb screen starts at the images' size, not the whole framebuffer`, sized.length > 0 && sized[0].trimEnd().endsWith('starts at 1920x1080'), sized.slice(0, 1));
        }
        const started = logLines(' starting: ');
        res.check(`${tag} Selkies streams the backend SELKIES_WAYLAND names`, started.length > 0 && started[0].includes(' transport, Wayland capture') === wayland, started.slice(0, 1));
    } finally {
        jupyter.kill('SIGKILL');
        await new Promise<void>((resolve) => {
            if (jupyter.exitCode !== null || jupyter.signalCode !== null) resolve();
            else jupyter.on('exit', () => { resolve(); });
        });
    }
    for (const until = Date.now() + 30_000; Date.now() < until && (sessionProcesses(runtime).length || (launcher !== null && cmdline(launcher)));) await sleep(500);
    const left = sessionProcesses(runtime);
    res.check(`${tag} a Jupyter server killed outright takes the session with it`, launcher !== null && !cmdline(launcher) && left.length === 0, left.slice(0, 5));
}

test('jupyter_proxy', async () => {
    const res = new Checks('jupyter-proxy');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        if (!registered()) {
            res.skip('jupyter-proxy', `jupyter-server and jupyter-server-proxy are not installed beside ${PYTHON}`);
        } else {
            const binary = await host.serverBinary();
            await block(res, binary, false);
            await block(res, binary, true);
        }
    } finally {
        await host.close();
    }
    res.finish();
});
