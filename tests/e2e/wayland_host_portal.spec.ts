/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Host capture of a compositor without the wlroots protocols goes through
// xdg-desktop-portal.
//
// A KDE session offers neither screencopy nor the virtual keyboard and
// pointer protocols to ordinary clients, so SELKIES_WAYLAND_HOST_DISPLAY aimed
// at its socket has pixelflux open a RemoteDesktop portal session instead: the
// monitor's PipeWire stream carries the frames and the seat's input goes
// through the portal by keysym and stream coordinates. This suite runs a real
// KDE stack on a private bus -- `kwin_wayland --virtual`, PipeWire,
// xdg-desktop-portal, and its KDE backend -- starts the server against it over
// either transport, and checks the rung was taken, the stream negotiated, and
// that a browser's pointer moves, clicks, scrolls, and keys land on a client
// window inside KWin. Plasma 6.6's virtual backend streams its frames as GPU
// dmabufs; KWin 5's emits no output damage and streams none, and the frame
// check reports skipped there. The pointer lands where the page points: the
// browser sends the stream's pixels, which the injector takes into the
// compositor's logical space. The `scaled` block runs KWin's output at scale
// 2, where the stream's pixels are twice the logical size and an injector that
// skipped that step, or took it twice, lands twice or half as far.
//
// The `gnome` blocks run the same checks against headless Mutter, with a
// stand-in for GNOME's portal backend that forwards to Mutter's own API as
// GNOME's does (crates/selkies-suites' mutter-portal): `gnome` at scale 1,
// `gnome-scaled` at scale 2 over libei, and `gnome-notify` at scale 2 with the
// backend refusing EIS, where the client's logical point reaches Mutter as the
// stream's pixels. Mutter offers the cursor metadata at a fixed size, which
// the stream must accept for any sprite to arrive.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { accessSync, constants, copyFileSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, suitesBin, type Observer } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { realConsole, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const SIZE = [1280, 720] as const;
// The box the page maps the pointer through, with the stream's size in
// pixels: the stream element once that has a size, else the input overlay (a
// WebRTC <video> has none before a frame).
const SINK_JS = `(() => {
  const canvas = document.getElementById('videoCanvas');
  const video = document.getElementById('stream');
  const px = (canvas && canvas.width > 0) ? [canvas.width, canvas.height]
             : (video && video.videoWidth > 0) ? [video.videoWidth, video.videoHeight] : null;
  for (const id of px ? ['videoCanvas', 'videoWorkerCanvas', 'videoStream', 'stream'] : ['overlayInput']) {
    const el = document.getElementById(id);
    const r = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
    if (r && r.width > 0) return {x: r.left, y: r.top, cssW: r.width, cssH: r.height, px};
  }
  return null;
})()`;
// Record the pointer echoes the page receives, and ask for them as trackpad mode does.
const ECHO_ON_JS = `(() => {
  const input = window.webrtcInput;
  if (!input) return false;
  if (!window.__echoes) {
    window.__echoes = [];
    const draw = input.onPointerEcho.bind(input);
    input.onPointerEcho = (action) => { window.__echoes.push(action); return draw(action); };
  }
  input.send('_pointer_echo,1');
  return true;
})()`;
const ECHO_LAST_JS = '(window.__echoes || []).slice(-1)[0] || null';

type Pt = [number, number];
const r = (v: unknown): string => JSON.stringify(v);
const tail = (path: string, n: number): string => {
    try {
        return readFileSync(path, 'utf8').trimEnd().split('\n').slice(-n).join('\n');
    } catch {
        return '';
    }
};
const textTail = (text: string, n: number): string => text.trimEnd().split('\n').slice(-n).join('\n');

/**
 * The executable behind a D-Bus service name, from its activation file, the
 * way the bus itself finds it; '' when no data directory carries the file.
 */
function serviceExec(name: string): string {
    for (const base of (process.env.XDG_DATA_DIRS ?? '/usr/local/share:/usr/share').split(':')) {
        try {
            const line = readFileSync(join(base, 'dbus-1', 'services', `${name}.service`), 'utf8').split('\n').find((l) => l.startsWith('Exec='));
            if (line) return line.slice(5).trim().split(/\s+/)[0];
        } catch {
            // not in this directory
        }
    }
    return '';
}

/**
 * A desktop's compositor, PipeWire, and xdg-desktop-portal on a private
 * session bus and runtime dir, the way the desktop's session exposes them;
 * `socketPath` is the compositor's socket. A compositor's output is SIZE in
 * logical pixels at the rig's scale, so its mode is SIZE times the scale.
 */
abstract class PortalRig {
    abstract readonly name: string;
    // The processes whose logs the suite runner keeps, by the name each was spawned under.
    abstract readonly logs: readonly string[];
    // A short path: the compositor's, PipeWire's, and the bus's sockets all live under it.
    readonly root = mkdtempSync(join(tmpdir(), 'pf-portal-'));
    readonly runtime = join(this.root, 'rt');
    readonly socketName: string;
    readonly socketPath: string;
    readonly scale: number;
    readonly mode: Pt;
    readonly env: Record<string, string>;
    readonly portal = serviceExec('org.freedesktop.portal.Desktop');
    bus = '';
    private busPid = 0;
    private readonly procs: ChildProcess[] = [];

    constructor(socketName: string, scale: number, env: Record<string, string>) {
        mkdirSync(this.runtime, { mode: 0o700 });
        this.socketName = socketName;
        this.socketPath = join(this.runtime, socketName);
        this.scale = scale;
        this.mode = [SIZE[0] * scale, SIZE[1] * scale];
        this.env = {
            PATH: process.env.PATH ?? '', HOME: homedir(), XDG_RUNTIME_DIR: this.runtime, WAYLAND_DISPLAY: socketName,
            XDG_SESSION_TYPE: 'wayland', XDG_CONFIG_HOME: join(this.root, 'config'), ...env,
        };
        mkdirSync(this.env.XDG_CONFIG_HOME);
    }

    abstract start(): Promise<string>;

    log(name: string): string {
        return join(this.root, `${name}.log`);
    }

    protected spawnLogged(cmd: readonly string[], name: string, extra: Record<string, string> = {}): ChildProcess {
        const out = openSync(this.log(name), 'w');
        const proc = spawn(cmd[0], cmd.slice(1), { env: { ...this.env, ...extra }, stdio: ['ignore', out, out], detached: true });
        this.procs.push(proc);
        return proc;
    }

    /** Check for the stack's binaries, then start the bus, PipeWire, and WirePlumber; why it could not, or ''. */
    protected async startBus(binaries: readonly string[]): Promise<string> {
        for (const binary of ['dbus-daemon', 'pipewire', 'wireplumber', ...binaries]) if (!onPath(binary)) return `${binary} not installed`;
        if (!this.portal || !existsSync(this.portal)) return 'xdg-desktop-portal not installed';
        const printed = spawnSync('dbus-daemon', ['--session', '--print-address=1', '--print-pid=1', '--fork', '--address', `unix:path=${this.root}/bus`], { env: this.env, encoding: 'utf8' }).stdout ?? '';
        const words = printed.split(/\s+/).filter(Boolean);
        this.bus = words.find((w) => w.includes('=')) ?? '';
        this.busPid = Number(words.find((w) => /^\d+$/.test(w)) ?? 0);
        if (!this.bus) return 'dbus-daemon printed no address';
        this.env.DBUS_SESSION_BUS_ADDRESS = this.bus;
        this.spawnLogged(['pipewire'], 'pipewire');
        await sleep(500);
        this.spawnLogged(['wireplumber'], 'wireplumber');
        return '';
    }

    protected async waitSocket(): Promise<string> {
        for (const until = Date.now() + 20_000; !existsSync(this.socketPath);) {
            if (Date.now() > until) return `${this.name} never opened its socket: ${tail(this.log(this.logs[0]), 3)}`;
            await sleep(200);
        }
        return '';
    }

    async stop(): Promise<void> {
        for (const proc of [...this.procs].reverse()) {
            try {
                if (proc.pid !== undefined) process.kill(-proc.pid, 'SIGTERM');
            } catch {
                // already gone
            }
        }
        await sleep(1000);
        for (const proc of this.procs) {
            try {
                if (proc.pid !== undefined) process.kill(-proc.pid, 'SIGKILL');
            } catch {
                // already gone
            }
        }
        if (this.busPid) {
            try {
                process.kill(this.busPid, 'SIGTERM');
            } catch {
                // already gone
            }
        }
        // The stack's logs go where the suite runner keeps a suite's record.
        for (const name of this.logs) {
            try {
                copyFileSync(this.log(name), join(WORKDIR, `portal-${name}.log`));
            } catch {
                // never written
            }
        }
        // Best effort, as the stack's teardown is: a portal's FUSE mount under the
        // root can refuse its files while its daemon is still going away.
        try {
            rmSync(this.root, { recursive: true, force: true });
        } catch (e) {
            process.stdout.write(`note: the portal rig's ${this.root} is left behind: ${String(e)}\n`);
        }
    }
}

/**
 * kwin_wayland (virtual backend) and the KDE portal backend.
 *
 * KWin runs with KWIN_WAYLAND_NO_PERMISSION_CHECKS, as the KDE image sets
 * it: KWin 6 grants its restricted globals, the screencast one among them, by
 * the desktop file of the client's executable, which it does not resolve in
 * this bare session, and the backend then refuses every session request. KDE
 * 6's backend also asks before a remote-desktop session starts unless the
 * permission store's `kde-authorized` table grants the app `remote-desktop`,
 * so the rig grants it to a host app, whose id is empty, as a deployment does
 * ahead of time.
 *
 * KWin's virtual output takes its mode from --width and --height times
 * --scale. KWin 5 takes the output's scale from --scale as well; KWin 6 takes
 * it from the kwinoutputconfig.json it keeps, else picks its own (1 here), so
 * the rig also writes one into a configuration directory of its own.
 */
class KdeRig extends PortalRig {
    readonly name = 'KWin';
    readonly logs = ['kwin', 'pipewire', 'wireplumber', 'portal-kde', 'portal'];
    private readonly portalKde = serviceExec('org.freedesktop.impl.portal.desktop.kde');

    constructor(scale = 1) {
        super('wayland-portal', scale, { XDG_CURRENT_DESKTOP: 'KDE', QT_QPA_PLATFORM: 'wayland' });
        writeFileSync(join(this.env.XDG_CONFIG_HOME, 'kwinoutputconfig.json'), r([
            { name: 'outputs', data: [{ connectorName: 'Virtual-0', scale, mode: { width: this.mode[0], height: this.mode[1], refreshRate: 60000 } }] },
            { name: 'setups', data: [{ lidClosed: false, outputs: [{ enabled: true, outputIndex: 0, position: { x: 0, y: 0 }, priority: 0 }] }] },
        ]));
    }

    async start(): Promise<string> {
        if (!this.portalKde || !existsSync(this.portalKde)) return 'xdg-desktop-portal-kde not installed';
        let why = await this.startBus(['kwin_wayland', 'gdbus']);
        if (why) return why;
        // KWin runs from a copy: distributions give kwin_wayland CAP_SYS_NICE
        // as a file capability, which a container whose bounding set lacks it
        // cannot exec, and the rig needs none (a copy leaves the capability's
        // attribute behind). The copy keeps the name, which KWin's platform
        // plugin requires of the executable.
        const kwin = join(this.root, 'kwin_wayland');
        copyFileSync(onPath('kwin_wayland') ?? 'kwin_wayland', kwin);
        this.spawnLogged([kwin, '--virtual', '--width', String(SIZE[0]), '--height', String(SIZE[1]), '--scale', String(this.scale), '--no-lockscreen', '--no-global-shortcuts', '--no-kactivities', '--socket', this.socketName],
            'kwin', { KWIN_WAYLAND_NO_PERMISSION_CHECKS: '1',
                // KWin 6's Qt would register with the host portal as it starts,
                // which has the bus start the portal and a KDE backend before
                // KWin is up: that backend holds the name the rig's own needs
                // and refuses every session.
                QT_NO_XDG_DESKTOP_PORTAL: '1' });
        why = await this.waitSocket();
        if (why) return why;
        await sleep(1000);
        // KWin casts the screen through its screencast plugin, and only while
        // it composites with OpenGL: KWin 5 does on Mesa's surfaceless
        // platform, KWin 6 on a render node alone, which a host without a GPU
        // gets from the vgem module.
        let plugins: string;
        let compositing: string;
        for (const until = Date.now() + 10_000; ;) {
            plugins = this.kwinProperty('/Plugins', 'org.kde.KWin.Plugins', 'LoadedPlugins');
            compositing = this.kwinProperty('/Compositor', 'org.kde.kwin.Compositing', 'compositingType');
            if ((plugins && compositing && !compositing.includes("'none'")) || Date.now() > until) break;
            await sleep(500);
        }
        if (!plugins.includes("screencast'")) return "KWin loaded no screencast plugin (Debian and Ubuntu ship KWin 6's in kwin-common)";
        if (!compositing.includes('gl')) {
            const kind = compositing.replace(/^[(<>,')]+|[(<>,')]+$/g, '') || 'nothing';
            return `KWin composites with ${kind} and casts the screen only with OpenGL, which needs a render node (the vgem module gives one)`;
        }
        this.spawnLogged([this.portalKde], 'portal-kde', { KDE_FULL_SESSION: 'true' });
        this.spawnLogged([this.portal, '-r'], 'portal');
        await sleep(2000);
        const grant = spawnSync('gdbus', ['call', '--session', '--dest', 'org.freedesktop.impl.portal.PermissionStore', '--object-path', '/org/freedesktop/impl/portal/PermissionStore',
            '--method', 'org.freedesktop.impl.portal.PermissionStore.SetPermission', 'kde-authorized', 'true', 'remote-desktop', '', "['yes']"], { env: this.env, encoding: 'utf8', timeout: 15_000 });
        if (grant.status !== 0) return `the permission store took no remote-desktop grant: ${(grant.stderr ?? '').trim()}`;
        return '';
    }

    /** A property KWin publishes on the rig's bus, as gdbus prints it; '' when none answers. */
    private kwinProperty(path: string, iface: string, name: string): string {
        const q = spawnSync('gdbus', ['call', '--session', '--dest', 'org.kde.KWin', '--object-path', path, '--method', 'org.freedesktop.DBus.Properties.Get', iface, name], { env: this.env, encoding: 'utf8', timeout: 10_000 });
        return q.status === 0 ? (q.stdout ?? '').trim() : '';
    }
}

/**
 * Headless Mutter, with the crate's mutter-portal as the portal's
 * RemoteDesktop and ScreenCast backend. GNOME's own backend asks through a
 * dialog that nothing answers headless; the stand-in grants every session and
 * forwards it to Mutter's D-Bus API as GNOME's does, but converts an absolute
 * position from the stream's logical size to the pixels Mutter reads it in,
 * which GNOME's does not. `eis=false` has it refuse ConnectToEIS, which leaves
 * the client on the Notify methods.
 *
 * Mutter's virtual monitor takes the mode; the stand-in then sets the scale,
 * under `scale-monitor-framebuffer`, the layout GNOME sessions run scaled
 * with. E2E_MUTTER names another mutter executable.
 */
class GnomeRig extends PortalRig {
    readonly name = 'Mutter';
    readonly logs = ['mutter', 'pipewire', 'wireplumber', 'portal-gnome', 'portal'];
    version: [number, number] = [0, 0];
    private readonly eis: boolean;
    private readonly mutter: string;

    constructor(scale = 1, eis = true) {
        super('wayland-gnome', scale, { XDG_CURRENT_DESKTOP: 'GNOME', GSETTINGS_BACKEND: 'keyfile' });
        this.eis = eis;
        const mutter = resolve(process.env.E2E_MUTTER || onPath('mutter') || '/nonexistent');
        let runnable: boolean;
        try {
            accessSync(mutter, constants.X_OK);
            runnable = statSync(mutter).isFile();
        } catch {
            runnable = false;
        }
        this.mutter = runnable ? mutter : '';
        // The portal takes its backends, and with this set its portals.conf
        // too, from this directory alone.
        this.env.XDG_DESKTOP_PORTAL_DIR = join(this.root, 'portals');
        const files: Record<string, string> = {
            [join(this.env.XDG_CONFIG_HOME, 'glib-2.0', 'settings', 'keyfile')]: "[org/gnome/mutter]\nexperimental-features=['scale-monitor-framebuffer']\n",
            [join(this.env.XDG_DESKTOP_PORTAL_DIR, 'portals.conf')]: '[preferred]\ndefault=mutter_test\n',
            [join(this.env.XDG_DESKTOP_PORTAL_DIR, 'mutter_test.portal')]: '[portal]\nDBusName=org.freedesktop.impl.portal.desktop.mutter_test\nInterfaces=org.freedesktop.impl.portal.RemoteDesktop;org.freedesktop.impl.portal.ScreenCast;\n',
        };
        for (const [path, text] of Object.entries(files)) {
            mkdirSync(dirname(path), { recursive: true });
            writeFileSync(path, text);
        }
    }

    async start(): Promise<string> {
        if (!this.mutter) return 'mutter not installed';
        const printed = spawnSync(this.mutter, ['--version'], { encoding: 'utf8' }).stdout ?? '';
        const found = /(\d+)\.(\d+)/.exec(printed);
        this.version = found ? [Number(found[1]), Number(found[2])] : [0, 0];
        let why = await this.startBus([]);
        if (why) return why;
        this.spawnLogged([this.mutter, '--wayland', '--no-x11', '--headless', '--wayland-display', this.socketName, '--virtual-monitor', `${this.mode[0]}x${this.mode[1]}`], 'mutter');
        why = await this.waitSocket();
        if (why) return why;
        this.spawnLogged([suitesBin('mutter-portal'), ...(this.eis ? [] : ['--no-eis']), ...(this.scale !== 1 ? ['--scale', String(this.scale)] : [])], 'portal-gnome');
        // Up to the backend's own wait for Mutter's bus names, and some.
        for (const until = Date.now() + 40_000; !readFileSync(this.log('portal-gnome'), 'utf8').includes('ready');) {
            if (Date.now() > until) return `the Mutter portal backend never came up: ${tail(this.log('portal-gnome'), 3)}`;
            await sleep(200);
        }
        this.spawnLogged([this.portal, '-r'], 'portal');
        await sleep(2000);
        return '';
    }
}

interface Sink {
    x: number;
    y: number;
    cssW: number;
    cssH: number;
    px: Pt | null;
}

async function sinkOf(page: Page): Promise<Sink | null> {
    const got = await within(page.evaluate(SINK_JS));
    if (got === null || typeof got !== 'object') return null;
    const px: unknown = Reflect.get(got, 'px');
    return {
        x: Number(Reflect.get(got, 'x')), y: Number(Reflect.get(got, 'y')), cssW: Number(Reflect.get(got, 'cssW')), cssH: Number(Reflect.get(got, 'cssH')),
        px: Array.isArray(px) && px.length === 2 ? [Number(px[0]), Number(px[1])] : null,
    };
}

/**
 * Move the page's pointer to (x, y); where the session's client saw it last,
 * where it should have: the stream pixel the page point shows, in the
 * output's `logical` size, and that point in the stream's own pixels, where
 * the server's pointer echo should put it. The page maps by the stream's size
 * once it has one, so that has to be the host's `mode` first.
 */
async function landing(page: Page, obs: Observer, x: number, y: number, logical: Pt, mode: Pt): Promise<[Pt | null, Pt | null, Pt | null]> {
    let sink = await sinkOf(page);
    for (const until = Date.now() + 10_000; sink?.px && r(sink.px) !== r(mode) && Date.now() < until;) {
        await sleep(200);
        sink = await sinkOf(page);
    }
    const mark = (await obs.lines()).length;
    await page.mouse.move(x, y);
    await sleep(600);
    const seen = (await obs.lines(mark)).filter((l) => l.kind === 'ptr_motion' || l.kind === 'ptr_enter');
    const last = seen.at(-1);
    const got: Pt | null = last ? [Math.round(Number(last.x)), Math.round(Number(last.y))] : null;
    if (!sink) return [got, null, null];
    const want: Pt = [Math.round((x - sink.x) / sink.cssW * logical[0]), Math.round((y - sink.y) / sink.cssH * logical[1])];
    const px = sink.px ?? mode;
    const at: Pt = [Math.round((x - sink.x) / sink.cssW * px[0]), Math.round((y - sink.y) / sink.cssH * px[1])];
    return [got, want, at];
}

const close2 = (a: Pt | null, b: Pt | null): boolean => a !== null && b !== null && Math.abs(a[0] - b[0]) <= 2 && Math.abs(a[1] - b[1]) <= 2;

async function run(host: Host, res: Checks, mode: string, scale: number, desktop: string, eis: boolean): Promise<void> {
    const gnome = desktop === 'gnome';
    const rig: PortalRig = gnome ? new GnomeRig(scale, eis) : new KdeRig(scale);
    const why = await rig.start();
    if (why) {
        await rig.stop();
        res.skip('the suite', `${rig.name} portal stack unavailable: ${why}`);
        return;
    }
    let obs: Observer | null = null;
    try {
        // The observer is the session's own client: fullscreen, so every
        // injected pointer event lands on it, and it reports the seat events
        // it receives. Under Mutter, which shows no cursor over a surface that
        // sets none, it sets a 24-pixel one drawn at the output's scale, as an
        // application does; KWin shows its own 24-pixel cursor.
        obs = await host.observe(rig.socketPath, { XDG_RUNTIME_DIR: rig.runtime, WLOBS_FILL: 'ff2878dc', WLOBS_CURSOR: gnome ? `24x${scale}` : '' });
        res.check(`observer client maps inside ${rig.name}`, await obs.ready(15), tail(rig.log(rig.logs[0]), 3));
        const mapped = (await obs.lines()).find((l) => l.kind === 'mapped') ?? {};
        res.check(`${rig.name}'s output is SIZE in logical pixels at the rig's scale`, mapped.w === SIZE[0] && mapped.h === SIZE[1], mapped);
        const { url } = await host.start({ label: res.block, mode, wayland: true, env: { SELKIES_WAYLAND_HOST_DISPLAY: rig.socketPath, DBUS_SESSION_BUS_ADDRESS: rig.bus, XDG_CURRENT_DESKTOP: rig.env.XDG_CURRENT_DESKTOP } });
        const client = await launchClient(url, { mode });
        const { page } = client;
        try {
            const log = (): Promise<string> => host.log();
            res.check('the portal rung is taken for the frames', await host.saw('frames come through the xdg-desktop-portal ScreenCast', 60), tail(rig.log('portal'), 2));
            res.check('keyboard and pointer go through the portal', (await host.saw('keyboard goes through the portal by keysym', 10)) && (await host.saw('pointer goes through the portal', 10)), textTail(await log(), 5));
            res.check('the portal session opens with the monitor stream and both devices', await host.saw('portal session open: 1 stream(s), devices keyboard pointer', 60), textTail(await log(), 8));
            res.check("the stream negotiates the host's mode", await host.saw(`portal stream: ${rig.mode[0]}x${rig.mode[1]}`, 30), textTail(await log(), 8));
            res.check('the stream reaches the streaming state', await host.saw('portal stream streaming', 30), textTail(await log(), 8));
            res.check('no host connect failure', !(await log()).includes('connect failed'), '');

            // Input from the browser: pointer motion, a click, a wheel notch,
            // and a key, each of which the session's client must report. It
            // goes before the wait for frames: a WebRTC page that shows none
            // reconnects 12 s after connecting, and input sent meanwhile is lost.
            await page.mouse.move(640, 360);
            // A click in the same instant as the session's first pointer
            // motion reaches no client under GNOME Shell (and under Mutter
            // over Notify, where that motion creates the pointer): the client
            // gets the pointer only once the motion is handled.
            await sleep(300);
            await page.mouse.click(640, 360);
            await sleep(300);
            await page.mouse.move(300, 200);
            await sleep(300);
            await page.mouse.wheel(0, 120);
            await sleep(300);
            if (gnome && !eis) {
                // Headless Mutter adds the seat's keyboard with the first key
                // a session notifies, which reaches no client: one bound none
                // yet. A desktop with a keyboard has the device already; a
                // Control tap stands in for it.
                await page.keyboard.press('Control');
                await sleep(300);
            }
            await page.keyboard.press('a');
            await sleep(300);
            await page.keyboard.press('Shift+B');
            res.check("pointer motion reaches the session's client", (await obs.wait('ptr_motion', 8)) !== null, (await obs.lines()).filter((l) => l.kind === 'ptr_enter').slice(0, 2));
            const path = (await log()).includes('input via libei') ? 'libei' : 'portal notify';
            if (!eis) res.check("with EIS refused, input goes through the portal's Notify methods", path === 'portal notify' && (await log()).includes('portal offers no EIS socket'), textTail(await log(), 5));
            // A trackpad page draws the pointer where the server echoes it:
            // from the host's cursor metas under host capture, in the display's pixels.
            const echoOn = (await within(page.evaluate(ECHO_ON_JS))) === true;
            const logical: Pt = [Number(mapped.w ?? SIZE[0]) || SIZE[0], Number(mapped.h ?? SIZE[1]) || SIZE[1]];
            for (const [x, y] of [[900, 500], [200, 120]] as const) {
                const [got, want, at] = await landing(page, obs, x, y, logical, rig.mode);
                res.check(`the pointer lands where the page points (${x}, ${y}), over ${path}`, close2(got, want), `at ${r(got)}, the page points at ${r(want)}`);
                // The echo sent with the move may still carry the host's
                // previous cursor meta; the resting pointer is read again on
                // the echo's own poll.
                let echo: unknown = null;
                let echoed: Pt | null = null;
                for (const until = Date.now() + 3000; Date.now() < until;) {
                    echo = await within(page.evaluate(ECHO_LAST_JS));
                    const parts = typeof echo === 'string' ? echo.split(',') : [];
                    echoed = parts.length >= 6 ? [Math.round(Number(parts[2])), Math.round(Number(parts[3]))] : null;
                    if (close2(echoed, at)) break;
                    await sleep(200);
                }
                res.check(`the pointer echo follows the host's pointer (${x}, ${y})`, echoOn && close2(echoed, at), `echo ${r(echo)}, the page points at ${r(at)} in the stream's pixels`);
            }
            res.check("a click reaches the session's client", (await obs.wait('ptr_button', 8, { state: 1 })) !== null && (await obs.wait('ptr_button', 8, { state: 0 })) !== null, '');
            res.check("a wheel notch reaches the session's client", (await obs.wait('ptr_axis', 8)) !== null, '');
            const keys = (await obs.lines()).filter((l) => l.kind === 'kbd_key' && l.state === 1).map((l) => l.key);
            res.check("keys reach the session's client by keysym (a, Shift, b)",
                (await obs.wait('kbd_key', 8, { key: 30, state: 1 })) !== null && (await obs.wait('kbd_key', 8, { key: 48, state: 1 })) !== null && (await obs.wait('kbd_key', 8, { key: 42, state: 1 })) !== null, keys);
            // Past the page's one-second wheel reset, a stroke of small pixel
            // deltas is a touchpad's, which goes to the portal as smooth
            // scroll ending in a finish.
            const strokeChecks = ["a touchpad's stroke reaches the session's client unstepped, as far as the page scrolled", 'and stops once the stroke pauses'];
            if (gnome && path === 'libei') {
                // Mutter 46 through 50 hand every EIS scroll delta to the seat
                // as a wheel's (meta-eis-client.c handle_scroll), which steps it by tens.
                for (const name of strokeChecks) res.skip(name, "Mutter takes libei's scroll deltas as a wheel's");
            } else {
                await sleep(1300);
                const mark = (await obs.lines()).length;
                const stroke = [3.5, 6.25, 9.0, 4.75, 2.5];
                for (const d of stroke) {
                    await page.mouse.wheel(0, d);
                    await sleep(16);
                }
                // The finish follows the stroke's pause; give it a few seconds
                // to cross the portal on a loaded host, and the rest of its frame.
                await sleep(1000);
                for (const until = Date.now() + 3000; Date.now() < until && !(await obs.lines(mark)).some((l) => l.kind === 'ptr_axis_stop');) await sleep(100);
                await sleep(200);
                const seen = (await obs.lines(mark)).filter((l) => l.kind === 'ptr_axis' || l.kind === 'ptr_axis_source' || l.kind === 'ptr_axis_stop');
                const values = seen.filter((l) => l.kind === 'ptr_axis' && l.axis === 0).map((l) => Number(l.value));
                let lastAxis = -1;
                seen.forEach((l, i) => { if (l.kind === 'ptr_axis') lastAxis = i; });
                const strokeSum = stroke.reduce((a, b) => a + b, 0);
                res.check(strokeChecks[0], values.some((v) => !Number.isInteger(v)) && Math.abs(values.reduce((a, b) => a + b, 0) - strokeSum) < 0.5, `${r(values)} against ${strokeSum}`);
                if (!gnome && (await log()).includes('portal offers no EIS socket')) {
                    // Without EIS the stroke goes through NotifyPointerAxis,
                    // whose finish KDE's backend drops (5.27 through master):
                    // the client is never told it stopped.
                    res.skip(strokeChecks[1], "KDE's portal takes no finish without EIS");
                } else if (gnome && rig instanceof GnomeRig && (rig.version[0] < 48)) {
                    // Mutter 46 and 47 drop a virtual pointer's scroll finish
                    // on the way to the seat; 48 forwards it (mutter f76b5932).
                    res.skip(strokeChecks[1], `Mutter ${rig.version[0]} drops a scroll's finish`);
                } else {
                    const after = seen.slice(Math.max(lastAxis, 0)).map((l) => [String(l.kind).slice(4), l.axis ?? l.source ?? null, l.value ?? null]);
                    res.check(strokeChecks[1], seen.slice(lastAxis + 1).some((l) => l.kind === 'ptr_axis_stop'), after);
                }
            }
            if (await host.saw('first portal frame', 8)) {
                res.check('video reaches the browser', Boolean(mode === 'websockets' ? await waitWsVideo(page, 30) : await waitWrVideo(page, 30)), '');
            } else {
                res.skip('video reaches the browser', 'the compositor emitted no screencast frame');
            }
            res.check('the portal cursor sprite is delivered when the client draws the cursor', await host.saw('portal cursor sprites arrive', 10), textTail(await log(), 4));
            // Either 24-pixel cursor comes from a buffer scale times as large.
            const sprite = /portal cursor sprites arrive \((\d+)x(\d+)/.exec(await log());
            res.check("the sprite is the cursor's size at the output's scale", sprite !== null && Number(sprite[1]) === Number(sprite[2]) && Number(sprite[1]) === 24 * scale, sprite ? sprite[0] : 'no sprite');
            const real = realConsole(client.consoleErrors, client.notFound);
            res.check('no browser console errors', real.errors.length === 0 && real.notFound.length === 0, [...real.errors, ...real.notFound].slice(0, 3));
        } finally {
            await client.browser.close();
        }
    } finally {
        await obs?.release();
        await host.stop();
        await rig.stop();
    }
}

const RUNS: Record<string, { mode: string; scale: number; desktop: string; eis: boolean }> = {
    websockets: { mode: 'websockets', scale: 1, desktop: 'kde', eis: true },
    webrtc: { mode: 'webrtc', scale: 1, desktop: 'kde', eis: true },
    scaled: { mode: 'websockets', scale: 2, desktop: 'kde', eis: true },
    gnome: { mode: 'websockets', scale: 1, desktop: 'gnome', eis: true },
    'gnome-scaled': { mode: 'websockets', scale: 2, desktop: 'gnome', eis: true },
    'gnome-notify': { mode: 'websockets', scale: 2, desktop: 'gnome', eis: false },
    // The registry's `wayland` case runs the default block, as the suite runs with no selector.
    wayland: { mode: 'websockets', scale: 1, desktop: 'kde', eis: true },
};

for (const [which, run_] of Object.entries(RUNS)) {
    test(which, async () => {
        const { mode, scale, desktop, eis } = run_;
        const res = new Checks(`wl-host-portal-${mode}${desktop === 'gnome' ? '-gnome' : ''}${scale !== 1 ? `-scale${scale}` : ''}${eis ? '' : '-noeis'}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await run(host, res, mode, scale, desktop, eis);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
