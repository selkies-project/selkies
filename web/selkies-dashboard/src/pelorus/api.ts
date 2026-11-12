/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The client of the server's Pelorus API (`/pelorus/api`), for the chat panel
 * and the standalone page alike.
 *
 * A run is one `POST run` answered as a server-sent stream: each `data:` line
 * is one event of the agent's work (its reasoning, what it said, a tool call,
 * what the tool returned, the end), handed to the caller as it arrives.
 * Aborting the request is how a run is stopped: the server ends it at its
 * next step. Model configurations are listed without their keys, which the
 * server takes and never gives back.
 * @module
 */

/** One model configuration as the server lists it. */
export interface ModelConfig {
  id: string;
  name: string;
  provider: string;
  endpoint: string;
  model: string;
  effort: string;
  vision: boolean;
  /** Whether a key is set; the key itself is never sent. */
  has_key: boolean;
  default: boolean;
  /** False for the server's own configuration, which only its settings change. */
  editable: boolean;
  /** Whether it names enough to run. */
  usable: boolean;
}

/** One provider the server knows, with what a form need not ask for. */
export interface ProviderDefaults {
  /** The id a configuration names it by. */
  name: string;
  /** The name a person is shown. */
  label: string;
  /** Its own address; empty where a person has to give one. */
  endpoint: string;
  /** The model used where none is named; empty where one has to be picked. */
  model: string;
  /** Whether it takes an API key. */
  key: "required" | "optional" | "none";
  /** A server on the person's own machine or network, whose address is theirs to set. */
  local: boolean;
}

/** What `GET models` answers. */
export interface ModelList {
  /** The operator fixed the configuration: nothing of it is shown or changed. */
  locked: boolean;
  models: ModelConfig[];
  providers: ProviderDefaults[];
  efforts: string[];
  shell: boolean;
  windows: boolean;
  max_steps: number;
}

/** The fields a configuration is saved with; a field left out keeps its value. */
export interface ModelDraft {
  name?: string;
  provider?: string;
  endpoint?: string;
  model?: string;
  api_key?: string;
  effort?: string;
  vision?: boolean;
  default?: boolean;
}

/** One event of a run. */
export type RunEvent =
  | { type: "start"; session: string; model: string }
  /** A model turn begins, counted from one. */
  | { type: "step"; number: number }
  /** The screen as a base64 JPEG, for the person watching: before the run, after a tool that acted, at the end. */
  | { type: "screen"; when: "start" | "step" | "end"; image: string }
  | { type: "thinking"; text: string }
  | { type: "text"; text: string }
  | { type: "call"; name: string; arguments: Record<string, unknown> }
  | { type: "result"; name: string; text: string; failed: boolean; image: string | null }
  | { type: "done"; success: boolean; text: string }
  | { type: "error"; message: string };

/** What a run is started with. */
export interface RunRequest {
  prompt: string;
  session?: string;
  config?: string;
  model?: string;
  effort?: string;
  /** Ask for the screen as the run goes. */
  screenshots?: boolean;
  /** Instructions of the person's own, added after the server's. */
  suffix?: string;
  /** End the run after this many model turns, within the server's own limit. */
  max_steps?: number;
}

/**
 * Sends a JSON request and reads a JSON answer.
 * @param base The API's address, ending in `/pelorus/api`.
 * @param path The route under it.
 * @param method The HTTP method.
 * @param body The request body, where the method takes one.
 * @returns The answer.
 * @throws With the server's message when the request is refused.
 */
async function call<T>(base: string, path: string, method = "GET", body?: unknown): Promise<T> {
  const response = await fetch(`${base}/${path}`, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(typeof data?.message === "string" ? data.message : `HTTP ${response.status}`);
  return data as T;
}

/** Lists the model configurations and what the session offers. */
export const listModels = (base: string): Promise<ModelList> => call(base, "models");

/** Saves a new configuration. */
export const createModel = (base: string, draft: ModelDraft): Promise<ModelConfig> => call(base, "models", "POST", draft);

/** Changes a saved configuration. */
export const updateModel = (base: string, id: string, draft: ModelDraft): Promise<ModelConfig> =>
  call(base, `models/${encodeURIComponent(id)}`, "PUT", draft);

/** Forgets a saved configuration. */
export const deleteModel = (base: string, id: string): Promise<unknown> => call(base, `models/${encodeURIComponent(id)}`, "DELETE");

/**
 * Asks a provider which models it offers.
 * @param base The API's address.
 * @param probe The configuration to ask as, by `id`, with any field typed and not yet saved.
 * @returns The model names, sorted.
 */
export async function modelNames(base: string, probe: ModelDraft & { id?: string }): Promise<string[]> {
  return (await call<{ models: string[] }>(base, "models/names", "POST", probe)).models;
}

/** Forgets a conversation on the server. */
export const forgetSession = (base: string, session: string): Promise<unknown> =>
  call(base, `session/${encodeURIComponent(session)}`, "DELETE").catch(() => undefined);

/**
 * Starts a run and hands each of its events to `onEvent` as it arrives.
 * @param base The API's address.
 * @param request The task and what to run it on.
 * @param onEvent Receives every event, in order.
 * @param signal Aborting it stops the run.
 * @throws When the server refuses the run or the stream breaks; an abort is not an error.
 */
export async function run(base: string, request: RunRequest, onEvent: (event: RunEvent) => void, signal: AbortSignal): Promise<void> {
  const response = await fetch(`${base}/run`, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...request, stream: true }),
    signal,
  });
  if (!response.ok || !response.body) {
    const data = await response.json().catch(() => ({}));
    throw new Error(typeof data?.message === "string" ? data.message : `HTTP ${response.status}`);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;
    buffered += decoder.decode(value, { stream: true });
    let end = buffered.indexOf("\n\n");
    while (end >= 0) {
      const frame = buffered.slice(0, end);
      buffered = buffered.slice(end + 2);
      const data = frame.split("\n").filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trimStart()).join("\n");
      if (data) {
        try {
          onEvent(JSON.parse(data) as RunEvent);
        } catch { /* a frame that is not an event (a keep-alive) is skipped */ }
      }
      end = buffered.indexOf("\n\n");
    }
  }
}
