/**
 * Apps-panel contract: what the dashboard's apps panel asks the server for, and
 * the store of what the server reports back.
 *
 * The panel names an operation and an app, never a command line: a request is
 * posted to the core as `{type: "apps", value: {op, app?, id?}}` and the core
 * sends it as `apps,<json>`, held to the same `command_enabled` permission as a
 * command. `install`, `remove`, and `update` queue a job the server runs one at
 * a time; `cancel` and `clear` name a job by id (`clear` without one drops
 * every finished job); `launch` starts an installed app; `list` asks what is
 * installed now.
 *
 * Nothing about the session is kept in this browser. What is installed is the
 * runner's install directory, read by the server whenever it is asked (the
 * panel opening) and after every job; the jobs, with their consoles, live on
 * the server, so every page of the session sees the same ones, a page opened
 * mid-install replays the console so far, and a job stays until a page clears
 * it. The server tells every page with two system actions, which the cores
 * forward here unchanged (`forwardAppsEvent`): `apps_state,{installed, jobs}`
 * replaces everything, `installed` null where the runner could not say, and
 * `apps_job,{...}` is one job's change, carrying its state, phase, and download
 * progress, plus either its whole console (`reset`) or the lines committed
 * since the last update (`append`) and the line being written (`current`).
 *
 * Where the catalog comes from is the server's answer as well (`appsCatalog`):
 * the proot-apps repository on GitHub, or, for a session installing from a
 * proot-apps local repository, that folder's catalog served by the server
 * under `api/apps/`, so the panel lists exactly what the runner can install.
 * @module
 */

const REMOTE_CATALOG_BASE = "https://raw.githubusercontent.com/linuxserver/proot-apps/master/metadata/";
// Page-relative, like the file browser's `api/files/`, so a subfolder
// deployment resolves it under its prefix.
const LOCAL_CATALOG_BASE = "api/apps/";

/** The console lines a job keeps here, as the server keeps them. */
export const CONSOLE_LINES = 200;

/** The window message type the cores forward the server's apps actions as. */
export const APPS_EVENT = "appsEvent";

/** Where the panel reads its catalog. */
export interface AppsCatalog {
  /** URL of the catalog document (YAML). */
  metadata: string;
  /** URL of an entry's icon, from its `icon` file name. */
  icon: (name: string) => string;
  /** Whether this server serves it from a local repository. */
  local: boolean;
}

/** A server settings payload, as read here. */
export interface ServerSettings {
  apps_local_repo?: { value?: boolean } | null;
}

/**
 * Where the panel reads its catalog, as the server settings say.
 *
 * A server on a proot-apps local repository publishes `apps_local_repo`, and
 * its catalog is read from the server itself, through the session's own
 * authentication: `tokenize` (the dashboards' `withSessionToken`) is applied
 * to those URLs alone, since the remote catalog is another origin and never
 * sees a session token.
 * @param serverSettings The last `serverSettings` payload.
 * @param tokenize Adds the session token to a same-origin URL.
 * @returns Where the panel reads its catalog.
 */
export function appsCatalog(serverSettings: ServerSettings | null | undefined, tokenize?: (url: string) => string): AppsCatalog {
  const local = serverSettings?.apps_local_repo?.value === true;
  const base = local ? LOCAL_CATALOG_BASE : REMOTE_CATALOG_BASE;
  const url = (path: string): string => (local && tokenize ? tokenize(base + path) : base + path);
  return { metadata: url("metadata.yml"), icon: (name: string) => url(`img/${name}`), local };
}

/** What a job does to its app. */
export type AppJobOp = "install" | "remove" | "update";

/** Where a job is. */
export type AppJobState = "queued" | "running" | "done" | "failed" | "canceled";

/** Every request the panel makes. */
export type AppsOp = AppJobOp | "launch" | "list" | "cancel" | "clear";

/** One job the server runs for the panel. */
export interface AppJob {
  id: number;
  app: string;
  op: AppJobOp;
  state: AppJobState;
  /** The step the runner last named (`download`, `install`, `remove`). */
  phase: string | null;
  /** A download's percentage. */
  progress: number | null;
  /** The exit status once it ran to an end, signal deaths negative. */
  code: number | null;
  /** The committed console lines, oldest first. */
  lines: string[];
  /** The console line being written. */
  current: string;
}

/** Everything the server reported, as the panel reads it. */
export interface AppsSnapshot {
  /** The installed app names, null until the server could say. */
  installed: string[] | null;
  /** The jobs kept, in the order they were asked for. */
  jobs: AppJob[];
}

const EMPTY: AppsSnapshot = { installed: null, jobs: [] };

/**
 * Whether a job has ended, however it did.
 * @param job The job.
 * @returns Whether it is done, failed, or canceled.
 */
export function jobFinished(job: AppJob): boolean {
  return job.state === "done" || job.state === "failed" || job.state === "canceled";
}

/**
 * Forwards one `apps_*` system action from the server to the dashboard. Called
 * by both cores with the action as it arrived.
 * @param action The system action, `apps_<kind>,<json>`.
 */
export function forwardAppsEvent(action: string): void {
  const comma = action.indexOf(",");
  if (comma < 0) return;
  let payload: unknown;
  try {
    payload = JSON.parse(action.slice(comma + 1));
  } catch (e) {
    console.error("Malformed apps action from the server:", e);
    return;
  }
  const kind = action.slice("apps_".length, comma);
  window.postMessage({ type: APPS_EVENT, kind, payload }, window.location.origin);
}

/**
 * Asks the server for one apps operation, through the core.
 * @param op The operation.
 * @param app The app it applies to, for every operation that names one.
 * @param id The job `cancel` and `clear` apply to; `clear` without one drops every finished job.
 */
export function sendAppsRequest(op: AppsOp, app?: string, id?: number): void {
  const value: { op: AppsOp; app?: string; id?: number } = { op };
  if (app !== undefined) value.app = app;
  if (id !== undefined) value.id = id;
  window.postMessage({ type: "apps", value }, window.location.origin);
}

/** A job as the server describes it, read field by field. */
type JobPayload = Partial<AppJob> & { reset?: boolean; append?: unknown; lines?: unknown; current?: unknown };

const strings = (value: unknown): string[] =>
  Array.isArray(value) ? value.filter((line): line is string => typeof line === "string") : [];

const capped = (lines: string[]): string[] =>
  lines.length > CONSOLE_LINES ? lines.slice(lines.length - CONSOLE_LINES) : lines;

/**
 * A job built from a payload carrying its whole console.
 * @param payload The server's description.
 * @returns The job, or null for a payload that names none.
 */
function jobFrom(payload: JobPayload): AppJob | null {
  if (typeof payload.id !== "number" || typeof payload.app !== "string") return null;
  return {
    id: payload.id,
    app: payload.app,
    op: (payload.op ?? "install") as AppJobOp,
    state: (payload.state ?? "queued") as AppJobState,
    phase: payload.phase ?? null,
    progress: typeof payload.progress === "number" ? payload.progress : null,
    code: typeof payload.code === "number" ? payload.code : null,
    lines: capped(strings(payload.lines)),
    current: typeof payload.current === "string" ? payload.current : "",
  };
}

/**
 * Applies one forwarded action to a snapshot, returning a new snapshot (the
 * given one where the action changes nothing).
 * @param snapshot What was known before.
 * @param kind The action's kind, `state` or `job`.
 * @param payload Its parsed body.
 * @returns What is known after.
 */
export function applyAppsEvent(snapshot: AppsSnapshot, kind: string, payload: unknown): AppsSnapshot {
  if (!payload || typeof payload !== "object") return snapshot;
  if (kind === "state") {
    const body = payload as { installed?: unknown; jobs?: unknown };
    const installed = Array.isArray(body.installed) ? strings(body.installed) : snapshot.installed;
    const jobs = (Array.isArray(body.jobs) ? body.jobs : [])
      .map((job) => jobFrom(job as JobPayload))
      .filter((job): job is AppJob => job !== null);
    return { installed, jobs };
  }
  if (kind !== "job") return snapshot;
  const body = payload as JobPayload;
  const at = snapshot.jobs.findIndex((job) => job.id === body.id);
  if (at < 0 || body.reset) {
    const job = jobFrom(body);
    if (!job) return snapshot;
    const jobs = at < 0 ? [...snapshot.jobs, job] : snapshot.jobs.map((old, i) => (i === at ? job : old));
    return { ...snapshot, jobs };
  }
  const old = snapshot.jobs[at];
  const job: AppJob = {
    ...old,
    state: (body.state ?? old.state) as AppJobState,
    phase: body.phase !== undefined ? body.phase : old.phase,
    progress: body.progress !== undefined ? body.progress : old.progress,
    code: body.code !== undefined ? body.code : old.code,
    lines: body.append !== undefined ? capped([...old.lines, ...strings(body.append)]) : old.lines,
    current: typeof body.current === "string" ? body.current : old.current,
  };
  return { ...snapshot, jobs: snapshot.jobs.map((each, i) => (i === at ? job : each)) };
}

let snapshot: AppsSnapshot = EMPTY;
const listeners = new Set<() => void>();

/**
 * Takes in a forwarded action, posted by this page alone.
 * @param event The window message.
 */
const onAppsEvent = (event: MessageEvent): void => {
  if (event.source !== window || event.origin !== window.location.origin) return;
  const data = event.data as { type?: string; kind?: unknown; payload?: unknown } | null;
  if (!data || data.type !== APPS_EVENT || typeof data.kind !== "string") return;
  const next = applyAppsEvent(snapshot, data.kind, data.payload);
  if (next === snapshot) return;
  snapshot = next;
  for (const listener of listeners) listener();
};

if (typeof window !== "undefined") {
  window.addEventListener("message", onAppsEvent);
}

/**
 * What the server last reported.
 * @returns The current snapshot, the same object until something changes.
 */
export function appsSnapshot(): AppsSnapshot {
  return snapshot;
}

/**
 * Calls `listener` whenever the snapshot changes.
 * @param listener Called with no arguments; reads `appsSnapshot()`.
 * @returns A function that stops the calls.
 */
export function subscribeApps(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/**
 * The job shown for an app: its pending one, else the last that ran for it.
 * @param state The snapshot.
 * @param app The app name.
 * @returns The job, or null where none is kept.
 */
export function jobForApp(state: AppsSnapshot, app: string): AppJob | null {
  let last: AppJob | null = null;
  for (const job of state.jobs) {
    if (job.app !== app) continue;
    if (!jobFinished(job)) return job;
    last = job;
  }
  return last;
}
