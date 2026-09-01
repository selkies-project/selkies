/**
 * Apps-panel command contract, shared by both dashboards.
 *
 * The UI applies install and remove results optimistically, so every posted
 * command is tracked here until the server's command watch settles it: a
 * clean exit comes back as `command_done` and only clears the running state,
 * a failure as a `command_error` system action whose message ends with the
 * echoed command string, and matching that rolls the optimistic update back. Commands run through a shell on the server, in the environment of
 * the session the applications use, so `~` in a launch command is the
 * session user's home.
 *
 * Which apps are installed is the server's answer, arriving with the server
 * settings and again as an `apps_installed` system action when a command
 * changes it. What is stored locally is a cache of it, so a panel opened before
 * the server speaks shows the last answer rather than nothing.
 * @module
 */

const INSTALLED_APPS_STORAGE_KEY = "prootInstalledApps";

/** Window event dispatched when a failed install or remove is rolled back. */
export const INSTALLED_APPS_ROLLBACK_EVENT = "installedAppsRollback";

/** Window event dispatched when the server's installed-apps list replaces the stored one. */
export const INSTALLED_APPS_SERVER_EVENT = "installedAppsFromServer";

/** Window event dispatched whenever the set of running commands changes. */
export const APP_COMMAND_STATE_EVENT = "appCommandState";

const PENDING_COMMAND_TTL_MS = 10 * 60 * 1000;
const LAUNCH_FAILURE_WINDOW_MS = 15 * 1000;

/** Shell command per action; the `selkies-proot` wrapper is on PATH in the image.
 *
 * A launch runs the application directly, the way its desktop entry does. It
 * draws its own window, so a terminal wrapped around it is the only thing the
 * user sees come up. Every action goes through the same wrapper, which is on
 * PATH: where the runner keeps an application is the runner's business, and
 * nothing here needs a path to it.
 */
const appCommandBuilders = {
    install: (app) => `selkies-proot install ${app}`,
    remove: (app) => `selkies-proot remove ${app}`,
    update: (app) => `selkies-proot update ${app}`,
    launch: (app) => `selkies-proot run ${app}`,
};

const pendingAppCommands = new Map();

const announceState = () =>
    window.dispatchEvent(new CustomEvent(APP_COMMAND_STATE_EVENT));

/**
 * The action running for an app, so a list can show it and hold its buttons.
 * @param {string} app The app name.
 * @returns {'install'|'remove'|'update'|'launch'|null}
 */
export function pendingAppAction(app) {
    for (const entry of pendingAppCommands.values()) {
        if (entry.app === app) return entry.action;
    }
    return null;
}

/**
 * Settles a command the server reported finishing cleanly. The optimistic
 * update already holds, so this only stops it reading as still running.
 * @param {string} command The echoed command string.
 */
export function resolveFinishedAppCommand(command) {
    if (!pendingAppCommands.delete(command)) return;
    announceState();
}

if (typeof window !== "undefined") {
    window.addEventListener("message", (event) => {
        if (event.source !== window || event.origin !== window.location.origin) return;
        const data = event.data;
        if (data && data.type === "commandDone" && typeof data.command === "string") {
            resolveFinishedAppCommand(data.command);
        }
    });
}

/**
 * Posts an app command to the core and records it as pending.
 * @param {'install'|'remove'|'update'|'launch'} action The command to build.
 * @param {string} app The app name the command applies to.
 */
export function postAppCommand(action, app) {
    const command = appCommandBuilders[action](app);
    const now = Date.now();
    for (const [cmd, entry] of pendingAppCommands) {
        if (now - entry.at > PENDING_COMMAND_TTL_MS) pendingAppCommands.delete(cmd);
    }
    pendingAppCommands.set(command, { app, action, at: now });
    window.postMessage({ type: "command", value: command }, window.location.origin);
    announceState();
}

/**
 * Adopts the installed-apps list the server reported.
 *
 * A command still in flight is left alone: its optimistic update is newer than
 * a list read before it ran.
 * @param {string[]|undefined} apps App names the server reports installed; a
 *   payload that carries none at all is left to the stored list.
 * @returns {boolean} Whether the stored list changed.
 */
export function applyServerInstalledApps(apps) {
    if (!Array.isArray(apps)) return false;
    const names = apps.filter((item) => typeof item === "string");
    for (const entry of pendingAppCommands.values()) {
        if (entry.action === "install" && !names.includes(entry.app)) names.push(entry.app);
        if (entry.action === "remove") {
            const at = names.indexOf(entry.app);
            if (at >= 0) names.splice(at, 1);
        }
    }
    const stored = readInstalledApps();
    if (stored.length === names.length && stored.every((name) => names.includes(name))) {
        return false;
    }
    writeInstalledApps(names);
    window.dispatchEvent(
        new CustomEvent(INSTALLED_APPS_SERVER_EVENT, { detail: { apps: names } })
    );
    return true;
}

/**
 * Reads the stored installed-apps list.
 * @returns {string[]} The app names, empty when nothing valid is stored.
 */
export function readInstalledApps() {
    try {
        const saved = JSON.parse(
            localStorage.getItem(INSTALLED_APPS_STORAGE_KEY) || "[]"
        );
        if (Array.isArray(saved)) {
            return saved.filter((item) => typeof item === "string");
        }
    } catch (e) {
        console.error("Failed to parse installed apps from localStorage:", e);
    }
    return [];
}

/** @param {string[]} apps The installed-apps list to store. */
export function writeInstalledApps(apps) {
    localStorage.setItem(INSTALLED_APPS_STORAGE_KEY, JSON.stringify(apps));
}

/**
 * Settles a failed-command notification against the pending map.
 *
 * A matched launch command older than the failure window is application
 * lifecycle (the app ran, then exited nonzero), so it is suppressed. A
 * matched install or remove rolls back the stored installed-apps list and
 * notifies mounted lists through `INSTALLED_APPS_ROLLBACK_EVENT`; an
 * unmatched failure is someone else's command and passes through untouched.
 * @param {string} errMsg The `command_error` message.
 * @returns {boolean} Whether the notice should be shown.
 */
export function resolveFailedAppCommand(errMsg) {
    if (typeof errMsg !== "string") return true;
    for (const [command, entry] of pendingAppCommands) {
        if (!errMsg.endsWith(`: ${command}`)) continue;
        pendingAppCommands.delete(command);
        if (
            entry.action === "launch" &&
            Date.now() - entry.at > LAUNCH_FAILURE_WINDOW_MS
        ) {
            return false;
        }
        announceState();
        if (entry.action === "install" || entry.action === "remove") {
            const apps = readInstalledApps();
            const rolledBack =
                entry.action === "install"
                    ? apps.filter((name) => name !== entry.app)
                    : apps.includes(entry.app)
                        ? apps
                        : [...apps, entry.app];
            writeInstalledApps(rolledBack);
            window.dispatchEvent(
                new CustomEvent(INSTALLED_APPS_ROLLBACK_EVENT, {
                    detail: { app: entry.app, action: entry.action },
                })
            );
        }
        return true;
    }
    return true;
}
