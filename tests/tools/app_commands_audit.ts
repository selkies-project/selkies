/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the apps panel knows between the server's reports. Nothing about the
 * session is kept in the browser: the panel asks the server by naming an
 * operation, and the store takes in what the server tells every page -- a
 * whole state (what is installed, every job and its console) or one job's
 * change (its state, phase, progress, and the console lines since the last).
 * So the checks are that a request leaves the page as an operation and not a
 * command line, that the cores' forwarding reaches the store, and that the
 * store folds both reports the way the server means them.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import type { AppsSnapshot, ServerSettings } from '../../web/selkies-web-core/lib/app-commands.ts';
import type { StandIn } from './stand_in.ts';

let failed = 0;
/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [app-commands] ${label}  ${String(detail)}`);
}

/** A message listener as the stand-in window calls it. */
type Listener = (event: StandIn<MessageEvent>) => void;

/** What the module posts to the page: a request to the core, or an action a core forwards. */
interface Posted {
    type: string;
    value?: { op: string; app?: string; id?: number };
    kind?: string;
    payload?: { app?: string };
}

const listeners = new Map<string, Listener[]>();
const posted: Posted[] = [];
const origin = 'http://localhost';

const page = {
    location: { origin },
    addEventListener(type: string, fn: Listener) {
        if (!listeners.has(type)) listeners.set(type, []);
        listeners.get(type)!.push(fn);
    },
    removeEventListener(type: string, fn: Listener) {
        listeners.set(type, (listeners.get(type) || []).filter((f) => f !== fn));
    },
    postMessage(data: Posted) { posted.push(data); },
} satisfies StandIn<Window>;
Object.assign(globalThis, { window: page });

const {
    APPS_EVENT,
    CONSOLE_LINES,
    applyAppsEvent,
    appsCatalog,
    appsSnapshot,
    forwardAppsEvent,
    jobForApp,
    sendAppsRequest,
    subscribeApps,
} = await import('../../web/selkies-web-core/lib/app-commands.ts');

const deliver = (data: unknown, source: StandIn<Window> = page): void => {
    for (const fn of listeners.get('message') || []) fn({ source, origin, data });
};

sendAppsRequest('install', 'firefox');
sendAppsRequest('clear', undefined, 7);
check('a request names an operation and an app, never a command line',
      JSON.stringify(posted[0]) === JSON.stringify({ type: 'apps', value: { op: 'install', app: 'firefox' } }),
      JSON.stringify(posted[0]));
check('a job is named by its id',
      JSON.stringify(posted[1].value) ===JSON.stringify({ op: 'clear', id: 7 }), JSON.stringify(posted[1]));

posted.length = 0;
forwardAppsEvent('apps_job,{"id":1,"app":"firefox"}');
forwardAppsEvent('apps_state,not json');
check('a core forwards an apps action as its kind and parsed body, and drops a malformed one',
      posted.length === 1 && posted[0].type === APPS_EVENT && posted[0].kind === 'job'
      && posted[0].payload?.app === 'firefox', JSON.stringify(posted));

let changes = 0;
const stop = subscribeApps(() => { changes++; });
deliver({ type: APPS_EVENT, kind: 'state', payload: { installed: ['gimp'], jobs: [] } });
check('a forwarded state reaches the store and its subscribers',
      changes === 1 && appsSnapshot().installed!.join() === 'gimp', JSON.stringify(appsSnapshot()));
deliver({ type: APPS_EVENT, kind: 'state', payload: { installed: ['x'], jobs: [] } }, {});
check('a message another window posted is not taken in', appsSnapshot().installed!.join() === 'gimp');
const before = appsSnapshot();
deliver({ type: APPS_EVENT, kind: 'unknown', payload: {} });
check('an action of no known kind changes nothing', appsSnapshot() === before && changes === 1);
stop();

let state: AppsSnapshot = { installed: null, jobs: [] };
state = applyAppsEvent(state, 'job', {
    id: 1, app: 'firefox', op: 'install', state: 'running', phase: null, progress: null, code: null,
    reset: true, lines: ['Installing firefox'], current: '',
});
state = applyAppsEvent(state, 'job', {
    id: 1, app: 'firefox', op: 'install', state: 'running', phase: 'download', progress: 40, code: null,
    append: ['Getting layer'], current: '####   40.0%',
});
const job = state.jobs[0];
check('a job change appends its new lines and replaces the line being written',
      job.lines.join('|') === 'Installing firefox|Getting layer' && job.current === '####   40.0%'
      && job.phase === 'download' && job.progress === 40, JSON.stringify(job));
state = applyAppsEvent(state, 'job', {
    id: 1, app: 'firefox', op: 'install', state: 'done', phase: 'install', progress: null, code: 0,
    append: ['Adding icon'], current: '',
});
check('a finished job keeps its console',
      state.jobs[0].state === 'done' && state.jobs[0].lines.length === 3 && state.jobs[0].current === '',
      JSON.stringify(state.jobs[0]));
state = applyAppsEvent(state, 'job', {
    id: 2, app: 'firefox', op: 'remove', state: 'queued', phase: null, progress: null, code: null,
    reset: true, lines: [], current: '',
});
check('the job shown for an app is its pending one', jobForApp(state, 'firefox')!.id === 2);
state = applyAppsEvent(state, 'job', { id: 2, app: 'firefox', op: 'remove', state: 'done', code: 0 });
check('else the last that ran for it', jobForApp(state, 'firefox')!.id === 2 && jobForApp(state, 'gimp') === null);

const flood = Array.from({ length: CONSOLE_LINES + 10 }, (_, i) => `line ${i}`);
state = applyAppsEvent(state, 'job', { id: 2, app: 'firefox', op: 'remove', state: 'done', append: flood });
check('a console keeps its last lines as the server does',
      state.jobs[1].lines.length === CONSOLE_LINES && state.jobs[1].lines.at(-1) === `line ${CONSOLE_LINES + 9}`);

state = applyAppsEvent(state, 'state', { installed: ['firefox'], jobs: [{ id: 3, app: 'gimp', op: 'install', state: 'running', lines: ['a'], current: 'b' }] });
check('a whole state replaces every job and what is installed',
      state.jobs.length === 1 && state.jobs[0].current === 'b' && state.installed!.join() === 'firefox',
      JSON.stringify(state));
state = applyAppsEvent(state, 'state', { installed: null, jobs: [] });
check('a runner that could not say what is installed leaves the last answer standing',
      state.installed!.join() === 'firefox' && state.jobs.length === 0);

// Where the catalog is read from is the server's answer: a session installing
// from a proot-apps local repository lists that folder's catalog, served by the
// server behind the session's own authentication, and never the remote one.
const tokenize = (url: string): string => `${url}?token=t`;
const remote = appsCatalog({ command_enabled: { value: true } } as ServerSettings, tokenize);
check('without a local repository the catalog is the remote one',
      remote.local === false
      && remote.metadata === 'https://raw.githubusercontent.com/linuxserver/proot-apps/master/metadata/metadata.yml'
      && remote.icon('firefox.svg') === 'https://raw.githubusercontent.com/linuxserver/proot-apps/master/metadata/img/firefox.svg',
      JSON.stringify(remote));
check('no session token ever reaches the remote catalog',
      !remote.metadata.includes('token') && !remote.icon('x.svg').includes('token'));
check('before the server settings arrive the catalog is the remote one',
      appsCatalog(null).local === false && appsCatalog(undefined).local === false);
const local = appsCatalog({ apps_local_repo: { value: true } }, tokenize);
check('a local repository is read from the server, page-relative, with the session token',
      local.local === true
      && local.metadata === 'api/apps/metadata.yml?token=t'
      && local.icon('firefox.svg') === 'api/apps/img/firefox.svg?token=t',
      JSON.stringify(local));
check('a local repository without a tokenizer is still read from the server',
      appsCatalog({ apps_local_repo: { value: true } }).metadata === 'api/apps/metadata.yml');

process.exit(failed ? 1 : 0);
