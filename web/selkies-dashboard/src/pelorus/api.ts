/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The client of the server's Pelorus API (`/api/pelorus`), for the chat panel
 * and the standalone page alike. Every request carries the page's session
 * token where it holds one, as the server's other API requests do.
 *
 * A run is one `POST run` answered as a server-sent stream: each `data:` line
 * is one event of the agent's work (its reasoning, what it said, a tool call,
 * what the tool returned, the end), handed to the caller as it arrives.
 * Aborting the request is how a run is stopped: the server ends it at its
 * next step. Model configurations are listed without their keys, which the
 * server takes and never gives back; a key this browser sealed (`keys.ts`) is
 * listed sealed, and sent opened with each run and model listing.
 * @module
 */
import { fields, hasFields, isRecord, stringOr, type FieldKind } from "../../../selkies-web-core/lib/narrow.ts";
import { sessionAuthHeaders } from "../../../selkies-web-core/lib/session-token.ts";

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
  /** The key as the browser that saved it sealed it; empty where the server keeps the key. */
  sealed: string;
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
  /** A key this browser sealed, kept in place of `api_key`; empty to drop a sealed one. */
  sealed?: string;
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

/** Each run event's fields, by its `type`. */
const RUN_EVENT_FIELDS: Readonly<Record<RunEvent["type"], Readonly<Record<string, FieldKind | readonly FieldKind[]>>>> = {
  start: { session: "string", model: "string" },
  step: { number: "number" },
  screen: { when: "string", image: "string" },
  thinking: { text: "string" },
  text: { text: "string" },
  call: { name: "string", arguments: "object" },
  result: { name: "string", text: "string", failed: "boolean", image: ["string", "null"] },
  done: { success: "boolean", text: "string" },
  error: { message: "string" },
};

/**
 * Whether a streamed frame is a run event: a known `type` with its fields.
 * @param value What the frame parsed to.
 * @returns True for an event the panel shows.
 */
function isRunEvent(value: unknown): value is RunEvent {
  if (!isRecord(value)) return false;
  const kinds = Object.entries(RUN_EVENT_FIELDS).find(([type]) => type === value.type)?.[1];
  return kinds !== undefined && hasFields(value, kinds)
    && (value.type !== "screen" || value.when === "start" || value.when === "step" || value.when === "end");
}

/** Whether an answer is a saved model configuration. */
function isModelConfig(value: unknown): value is ModelConfig {
  return hasFields(value, { id: "string", name: "string", provider: "string", endpoint: "string", model: "string",
    effort: "string", vision: "boolean", has_key: "boolean", sealed: "string", default: "boolean", editable: "boolean",
    usable: "boolean" });
}

/** Whether an entry is a provider's defaults. */
function isProviderDefaults(value: unknown): value is ProviderDefaults {
  return hasFields(value, { name: "string", label: "string", endpoint: "string", model: "string", key: "string", local: "boolean" })
    && (value.key === "required" || value.key === "optional" || value.key === "none");
}

/** Whether an answer is the model list. */
function isModelList(value: unknown): value is ModelList {
  return hasFields(value, { locked: "boolean", models: "object", providers: "object", efforts: "object", shell: "boolean",
    windows: "boolean", max_steps: "number" })
    && Array.isArray(value.models) && value.models.every(isModelConfig)
    && Array.isArray(value.providers) && value.providers.every(isProviderDefaults)
    && Array.isArray(value.efforts) && value.efforts.every((e: unknown): boolean => typeof e === "string");
}

/**
 * Reads an answer of the shape a guard names, or refuses it.
 * @param guard The shape's guard.
 * @param what The shape, for the error.
 * @returns The answer, typed.
 */
const read = <T>(guard: (value: unknown) => value is T, what: string) => (value: unknown): T => {
  if (!guard(value)) throw new Error(`the agent answered with something other than ${what}`);
  return value;
};

/** What a run is started with. */
export interface RunRequest {
  prompt: string;
  session?: string;
  config?: string;
  model?: string;
  effort?: string;
  /** The opened key of a sealed configuration, for this run alone. */
  api_key?: string;
  /** Ask for the screen as the run goes. */
  screenshots?: boolean;
  /** Instructions of the person's own, added after the server's. */
  suffix?: string;
  /** End the run after this many model turns, within the server's own limit. */
  max_steps?: number;
}

/**
 * Sends a JSON request and reads a JSON answer.
 * @param base The API's address, ending in `/api/pelorus`.
 * @param path The route under it.
 * @param method The HTTP method.
 * @param body The request body, where the method takes one.
 * @returns The answer.
 * @throws With the server's message when the request is refused.
 */
async function call(base: string, path: string, method = "GET", body?: unknown): Promise<unknown> {
  const response = await fetch(`${base}/${path}`, {
    method,
    credentials: "same-origin",
    headers: sessionAuthHeaders(body === undefined ? undefined : { "Content-Type": "application/json" }),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data: unknown = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(stringOr(fields(data).message) ?? `HTTP ${response.status}`);
  return data;
}

/** Lists the model configurations and what the session offers. */
export const listModels = (base: string): Promise<ModelList> => call(base, "models").then(read(isModelList, "a model list"));

/** Saves a new configuration. */
export const createModel = (base: string, draft: ModelDraft): Promise<ModelConfig> =>
  call(base, "models", "POST", draft).then(read(isModelConfig, "a model configuration"));

/** Changes a saved configuration. */
export const updateModel = (base: string, id: string, draft: ModelDraft): Promise<ModelConfig> =>
  call(base, `models/${encodeURIComponent(id)}`, "PUT", draft).then(read(isModelConfig, "a model configuration"));

/** Forgets a saved configuration. */
export const deleteModel = (base: string, id: string): Promise<unknown> => call(base, `models/${encodeURIComponent(id)}`, "DELETE");

/**
 * Asks a provider which models it offers.
 * @param base The API's address.
 * @param probe The configuration to ask as, by `id`, with any field typed and not yet saved.
 * @returns The model names, sorted.
 */
export async function modelNames(base: string, probe: ModelDraft & { id?: string }): Promise<string[]> {
  const models = fields(await call(base, "models/names", "POST", probe)).models;
  return Array.isArray(models) ? models.filter((name): name is string => typeof name === "string") : [];
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
    headers: sessionAuthHeaders({ "Content-Type": "application/json" }),
    body: JSON.stringify({ ...request, stream: true }),
    signal,
  });
  if (!response.ok || !response.body) {
    const data: unknown = await response.json().catch(() => ({}));
    throw new Error(stringOr(fields(data).message) ?? `HTTP ${response.status}`);
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
          const event: unknown = JSON.parse(data);
          if (isRunEvent(event)) onEvent(event);
        } catch { /* a frame that is not an event (a keep-alive) is skipped */ }
      }
      end = buffered.indexOf("\n\n");
    }
  }
}
