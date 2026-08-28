/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the apps panel shows between posting a command and the server answering.
// The panel applies installs and removes optimistically, so the module tracks
// every posted command until a `command_done` message settles it or a
// `command_error` notice rolls it back, and announces both so a mounted list
// re-reads the running set.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

let failed = 0;
function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [app-commands] ${label}  ${detail}`);
}

const listeners = new Map();
const posted = [];
const store = new Map();
const origin = 'http://localhost';

globalThis.window = {
    location: { origin },
    addEventListener(type, fn) {
        if (!listeners.has(type)) listeners.set(type, []);
        listeners.get(type).push(fn);
    },
    removeEventListener(type, fn) {
        listeners.set(type, (listeners.get(type) || []).filter((f) => f !== fn));
    },
    dispatchEvent(event) {
        for (const fn of listeners.get(event.type) || []) fn(event);
        return true;
    },
    postMessage(data) { posted.push(data); },
};
globalThis.localStorage = {
    getItem: (key) => (store.has(key) ? store.get(key) : null),
    setItem: (key, value) => store.set(key, String(value)),
};
globalThis.CustomEvent = class CustomEvent {
    constructor(type, init) { this.type = type; this.detail = init && init.detail; }
};

const {
    APP_COMMAND_STATE_EVENT,
    INSTALLED_APPS_ROLLBACK_EVENT,
    pendingAppAction,
    postAppCommand,
    readInstalledApps,
    writeInstalledApps,
    resolveFailedAppCommand,
} = await import('../../addons/selkies-web-core/lib/app-commands.js');

let stateEvents = 0;
window.addEventListener(APP_COMMAND_STATE_EVENT, () => { stateEvents++; });
let rollbacks = [];
window.addEventListener(INSTALLED_APPS_ROLLBACK_EVENT, (e) => rollbacks.push(e.detail));

const deliver = (data) => {
    for (const fn of listeners.get('message') || []) fn({ source: window, origin, data });
};

postAppCommand('install', 'geany');
check('a posted install reaches the core',
      posted.at(-1) && posted.at(-1).type === 'command'
      && posted.at(-1).value === 'selkies-proot install geany', JSON.stringify(posted.at(-1)));
check('the app reads as installing while the server runs it',
      pendingAppAction('geany') === 'install', String(pendingAppAction('geany')));
check('posting announces the change', stateEvents === 1, String(stateEvents));

deliver({ type: 'commandDone', command: 'selkies-proot install geany' });
check('a clean exit clears the running state',
      pendingAppAction('geany') === null, String(pendingAppAction('geany')));
check('settling announces the change', stateEvents === 2, String(stateEvents));

writeInstalledApps(['inkscape']);
postAppCommand('install', 'inkscape');
stateEvents = 0;
const shown = resolveFailedAppCommand(
    'exited with status 1 after 0.3s -- proot error: ptrace denied: selkies-proot install inkscape');
check('a failure notice is shown to the user', shown === true, String(shown));
check('a failure clears the running state',
      pendingAppAction('inkscape') === null, String(pendingAppAction('inkscape')));
check('a failed install is rolled back',
      readInstalledApps().includes('inkscape') === false, JSON.stringify(readInstalledApps()));
check('the rollback reaches a mounted list',
      rollbacks.length === 1 && rollbacks[0].app === 'inkscape', JSON.stringify(rollbacks));
check('rolling back announces the change', stateEvents === 1, String(stateEvents));

postAppCommand('install', 'gimp');
stateEvents = 0;
const other = resolveFailedAppCommand('exited with status 1 after 0.1s: someone-elses-command');
check("another client's failure leaves this one running",
      other === true && pendingAppAction('gimp') === 'install' && stateEvents === 0,
      `${other} ${pendingAppAction('gimp')} ${stateEvents}`);

deliver({ type: 'commandDone', command: 'not-a-tracked-command' });
check('an untracked completion changes nothing',
      pendingAppAction('gimp') === 'install' && stateEvents === 0,
      `${pendingAppAction('gimp')} ${stateEvents}`);

postAppCommand('launch', 'geany');
check('a launch runs the application, not a terminal wrapped around it',
      posted.at(-1) && posted.at(-1).value === '~/.local/bin/geany-pa',
      JSON.stringify(posted.at(-1)));

process.exit(failed ? 1 : 0);
