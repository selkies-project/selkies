/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * One conversation with the Pelorus agent, kept for the page's life.
 *
 * A conversation is a list of exchanges: a task the person gave, the steps
 * the agent took on it, and its answer. A step is one turn of the model: what
 * it reasoned, what it said, the tools it called with what each returned,
 * and, where the person asked for pictures, the screen after each tool that
 * acted. The run's events (`api.ts`) are folded into that shape as they
 * arrive, so the sidebar's one line and the page's full account draw the same
 * thing.
 *
 * The conversation and its run belong to the page, not to a component: a
 * layout unmounts a panel the moment its menu closes, and a run that ended
 * with its panel would stop the agent mid-task and lose what it had done. A
 * component subscribes and draws what the page holds.
 * @module
 */
import { forgetSession, run, type RunEvent, type RunRequest } from "@/pelorus/api";

/** One tool call of a step. */
export interface ToolCall {
  name: string;
  arguments: Record<string, unknown>;
  /** What the tool returned; absent while it runs. */
  result?: string;
  failed?: boolean;
  /** A picture the tool itself returned, as base64 PNG. */
  image?: string | null;
  /** The screen after the tool acted, as base64 JPEG. */
  screen?: string;
}

/** One turn of the model. */
export interface Step {
  number: number;
  thinking: string[];
  text: string[];
  tools: ToolCall[];
}

/** One task and what came of it. */
export interface Exchange {
  prompt: string;
  /** The screen before the run and after it, as base64 JPEG. */
  startScreen?: string;
  endScreen?: string;
  steps: Step[];
  /** The agent's answer, and whether it finished the task itself. */
  answer?: { text: string; success: boolean };
  /** Why the run ended without an answer. */
  error?: string;
  running: boolean;
}

/** What a component draws. */
export interface Snapshot {
  exchanges: Exchange[];
  running: boolean;
}

/** What a run takes beside its task. */
export type RunOptions = Omit<RunRequest, "prompt" | "session">;

/**
 * An exchange after one more event of its run.
 * @param exchange The exchange so far.
 * @param event The event.
 * @returns The exchange with the event folded in.
 */
export function applyEvent(exchange: Exchange, event: RunEvent): Exchange {
  const steps = exchange.steps.slice();
  const current = (): Step => {
    if (steps.length === 0) steps.push({ number: 1, thinking: [], text: [], tools: [] });
    const step = { ...steps[steps.length - 1] };
    steps[steps.length - 1] = step;
    return step;
  };
  switch (event.type) {
    case "step":
      return { ...exchange, steps: [...steps, { number: event.number, thinking: [], text: [], tools: [] }] };
    case "screen": {
      if (event.when === "start") return { ...exchange, startScreen: event.image };
      if (event.when === "end") return { ...exchange, endScreen: event.image };
      const step = current();
      if (step.tools.length === 0) return exchange;
      step.tools = step.tools.slice();
      step.tools[step.tools.length - 1] = { ...step.tools[step.tools.length - 1], screen: event.image };
      return { ...exchange, steps };
    }
    case "thinking": {
      const step = current();
      step.thinking = [...step.thinking, event.text];
      return { ...exchange, steps };
    }
    case "text": {
      const step = current();
      step.text = [...step.text, event.text];
      return { ...exchange, steps };
    }
    case "call": {
      const step = current();
      step.tools = [...step.tools, { name: event.name, arguments: event.arguments }];
      return { ...exchange, steps };
    }
    case "result": {
      const step = current();
      const index = step.tools.findIndex((tool) => tool.name === event.name && tool.result === undefined);
      if (index < 0) return exchange;
      step.tools = step.tools.slice();
      step.tools[index] = { ...step.tools[index], result: event.text, failed: event.failed, image: event.image };
      return { ...exchange, steps };
    }
    case "done": {
      // The answer was already folded in as the last step's text; it is drawn once, as the answer.
      const last = steps[steps.length - 1];
      if (last && last.text[last.text.length - 1] === event.text) {
        steps[steps.length - 1] = { ...last, text: last.text.slice(0, -1) };
      }
      const kept = steps.filter((step) => step.thinking.length + step.text.length + step.tools.length > 0);
      return { ...exchange, steps: kept, answer: { text: event.text, success: event.success } };
    }
    case "error":
      return { ...exchange, error: event.message };
    default:
      return exchange;
  }
}

/** A conversation: its exchanges, the run in progress, and the session the server keeps for it. */
export class Conversation {
  private snapshot: Snapshot = { exchanges: [], running: false };
  private listeners = new Set<() => void>();
  private session: string | undefined;
  private abort: AbortController | null = null;

  constructor(private readonly base: string) {}

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  };

  read = (): Snapshot => this.snapshot;

  private update(change: (exchange: Exchange) => Exchange, running: boolean): void {
    const exchanges = this.snapshot.exchanges.slice();
    if (exchanges.length > 0) exchanges[exchanges.length - 1] = change(exchanges[exchanges.length - 1]);
    this.snapshot = { exchanges, running };
    this.listeners.forEach((listener) => listener());
  }

  /**
   * Gives the agent a task and folds its work in until it ends or is stopped.
   * @param prompt The task.
   * @param options The configuration, effort, and what else the run takes.
   * @param stoppedText What an exchange the person stopped says.
   */
  async send(prompt: string, options: RunOptions, stoppedText: string): Promise<void> {
    if (this.snapshot.running) return;
    this.snapshot = { exchanges: [...this.snapshot.exchanges, { prompt, steps: [], running: true }], running: true };
    this.listeners.forEach((listener) => listener());
    const controller = new AbortController();
    this.abort = controller;
    let failure: string | undefined;
    try {
      await run(this.base, { ...options, prompt, session: this.session }, (event) => {
        if (event.type === "start") this.session = event.session;
        this.update((exchange) => applyEvent(exchange, event), true);
      }, controller.signal);
    } catch (error) {
      failure = controller.signal.aborted ? stoppedText : (error instanceof Error ? error.message : String(error));
    }
    if (controller.signal.aborted) failure = stoppedText;
    this.abort = null;
    this.update((exchange) => ({ ...exchange, running: false, error: exchange.answer ? exchange.error : (failure ?? exchange.error) }), false);
  }

  /** Stops the run in progress. */
  stop(): void {
    this.abort?.abort();
  }

  /** Stops any run, forgets the conversation here and on the server, and starts an empty one. */
  reset(): void {
    this.stop();
    if (this.session) void forgetSession(this.base, this.session);
    this.session = undefined;
    this.snapshot = { exchanges: [], running: false };
    this.listeners.forEach((listener) => listener());
  }
}

const conversations = new Map<string, Conversation>();

/**
 * The page's conversation under a key.
 * @param base The API's address.
 * @param key Which conversation of the page, where it holds several.
 * @returns The conversation, made on first use.
 */
export function conversationFor(base: string, key = ""): Conversation {
  const id = `${base}#${key}`;
  let conversation = conversations.get(id);
  if (!conversation) {
    conversation = new Conversation(base);
    conversations.set(id, conversation);
  }
  return conversation;
}

/**
 * A tool call as one short line: its name and its arguments, as a person
 * would say them.
 * @param name The tool.
 * @param args Its arguments.
 * @returns The line.
 */
export function describeCall(name: string, args: Record<string, unknown>): string {
  const parts = Object.entries(args).map(([key, value]) => {
    const shown = typeof value === "string" ? value : JSON.stringify(value);
    return `${key}=${shown.length > 60 ? `${shown.slice(0, 60)}…` : shown}`;
  });
  return [name, ...parts].join(" ");
}
