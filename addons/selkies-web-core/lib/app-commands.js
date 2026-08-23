// Apps-panel command contract, shared by both dashboards. The UI applies
// install/remove results optimistically, so every posted command is tracked
// here until the server's command watch settles it: a failure comes back as
// a command_error system action whose message ends with the echoed command
// string, and matching it rolls the optimistic update back.

const INSTALLED_APPS_STORAGE_KEY = "prootInstalledApps";

export const INSTALLED_APPS_ROLLBACK_EVENT = "installedAppsRollback";

const PENDING_COMMAND_TTL_MS = 10 * 60 * 1000;
const LAUNCH_FAILURE_WINDOW_MS = 15 * 1000;

// The wrapper is on PATH in the image, and the server runs these through a
// shell, so ~ in the launch command is the session user's home. The launch
// terminal is the one the server publishes as app_terminal for the session's
// windowing system (foot on a Wayland session, st on X11); the server runs the
// command in that session's environment (display, compositor socket, session bus).
const DEFAULT_APP_TERMINAL = "st";
const appTerminal = () =>
    typeof window.app_terminal === "string" && window.app_terminal
        ? window.app_terminal
        : DEFAULT_APP_TERMINAL;
const appCommandBuilders = {
    install: (app) => `selkies-proot install ${app}`,
    remove: (app) => `selkies-proot remove ${app}`,
    update: (app) => `selkies-proot update ${app}`,
    launch: (app) => `${appTerminal()} ~/.local/bin/${app}-pa`,
};

const pendingAppCommands = new Map();

export function postAppCommand(action, app) {
    const command = appCommandBuilders[action](app);
    const now = Date.now();
    for (const [cmd, entry] of pendingAppCommands) {
        if (now - entry.at > PENDING_COMMAND_TTL_MS) pendingAppCommands.delete(cmd);
    }
    pendingAppCommands.set(command, { app, action, at: now });
    window.postMessage({ type: "command", value: command }, window.location.origin);
}

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

export function writeInstalledApps(apps) {
    localStorage.setItem(INSTALLED_APPS_STORAGE_KEY, JSON.stringify(apps));
}

/**
 * Settle a failed-command notification against the pending map. Returns
 * whether the notice should be shown: a matched launch command older than
 * the failure window is application lifecycle (the app ran, then exited
 * nonzero), so it is suppressed. A matched install/remove rolls back the
 * stored installed-apps list and notifies mounted lists through
 * INSTALLED_APPS_ROLLBACK_EVENT; an unmatched failure is someone else's
 * command and passes through untouched.
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
