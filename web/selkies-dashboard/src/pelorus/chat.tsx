/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Pelorus chat: a person gives the agent a task and watches it work.
 *
 * One component serves the dashboard's panel and the page at `pelorus/`
 * (`standalone`). It talks to the server's Pelorus API alone (`api.ts`): it
 * lists the model configurations, starts a run as a stream, and draws each
 * event as it arrives: the model's reasoning, what it says, each tool call
 * with what the tool returned folded under it, and the answer. A follow-up
 * continues the same conversation on the server until the person starts a
 * new one. While a run is going the send button is the stop button.
 *
 * The conversation and its run belong to the page, not to the component
 * (`Conversation`): a layout unmounts a panel the moment its menu closes, and
 * a run that ended with its panel would stop the agent mid-task and lose what
 * it had done. The component only draws what the page holds.
 *
 * Which configuration, model, and effort a run uses are chosen above the
 * prompt, and the configurations are edited in `ModelSettings`, unless the
 * operator locked them, in which case none of that is drawn.
 * @module
 */
import * as React from "react";
import { ArrowUp, ChevronRight, ExternalLink, Plus, Settings2, Square } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { t } from "@/i18n";
import { forgetSession, listModels, run, type ModelList, type RunEvent } from "@/pelorus/api";
import { ModelSettings, selectClass } from "@/pelorus/settings";

/** One entry of the conversation as it is drawn. */
type Entry =
  | { kind: "user"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "text"; text: string }
  | { kind: "tool"; name: string; arguments: Record<string, unknown>; result?: string; failed?: boolean; image?: string | null }
  | { kind: "final"; text: string; success: boolean }
  | { kind: "error"; text: string };

interface PelorusChatProps {
  /** The API's address: `api` from the page at `pelorus/`, the prefixed `/pelorus/api` from the dashboard. */
  base: string;
  /** The page of its own: fills the window and offers no pop-out. */
  standalone?: boolean;
  /** The page a pop-out opens, where the chat is a panel. */
  popoutHref?: string;
}

/**
 * A tool call as one short line: its name and its arguments, as a person
 * would say them.
 * @param name The tool.
 * @param args Its arguments.
 * @returns The line.
 */
function describeCall(name: string, args: Record<string, unknown>): string {
  const parts = Object.entries(args).map(([key, value]) => {
    const shown = typeof value === "string" ? value : JSON.stringify(value);
    return `${key}=${shown.length > 60 ? `${shown.slice(0, 60)}…` : shown}`;
  });
  return [name, ...parts].join(" ");
}

/**
 * Draws text with the two marks models reach for in a sentence, `**bold**`
 * and `` `code` ``, and nothing else of Markdown.
 */
function Marked({ text }: { text: string }) {
  const pieces = text.split(/(\*\*[^*]+\*\*|`[^`]+`)/g);
  return (
    <>
      {pieces.map((piece, index) => {
        if (piece.startsWith("**") && piece.endsWith("**") && piece.length > 4) return <strong key={index}>{piece.slice(2, -2)}</strong>;
        if (piece.startsWith("`") && piece.endsWith("`") && piece.length > 2) {
          return <code key={index} className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em]">{piece.slice(1, -1)}</code>;
        }
        return <React.Fragment key={index}>{piece}</React.Fragment>;
      })}
    </>
  );
}

/** One entry of the conversation. A tool call folds what it returned under its line. */
function EntryView({ entry }: { entry: Entry }) {
  switch (entry.kind) {
    case "user":
      return <div className="ml-8 self-end rounded-xl bg-primary px-3 py-2 text-sm whitespace-pre-wrap text-primary-foreground">{entry.text}</div>;
    case "thinking":
      return <div className="border-l-2 border-border pl-2 text-xs whitespace-pre-wrap text-muted-foreground italic">{entry.text}</div>;
    case "text":
      return <div className="text-sm whitespace-pre-wrap"><Marked text={entry.text} /></div>;
    case "tool":
      return (
        <details className="group rounded-md border border-border bg-muted/40 text-xs">
          <summary className="flex cursor-pointer list-none items-center gap-1 px-2 py-1 font-mono">
            <ChevronRight className="size-3 shrink-0 transition-transform group-open:rotate-90" />
            <span className={`truncate ${entry.failed ? "text-destructive" : ""}`}>{describeCall(entry.name, entry.arguments)}</span>
            {entry.result === undefined && <span className="ml-auto shrink-0 animate-pulse text-muted-foreground">…</span>}
          </summary>
          {entry.result !== undefined && (
            <div className="border-t border-border px-2 py-1">
              <pre className="max-h-64 overflow-auto font-mono whitespace-pre-wrap text-muted-foreground">{entry.result}</pre>
              {entry.image && <img className="mt-1 max-h-64 rounded border border-border" alt="" src={`data:image/png;base64,${entry.image}`} />}
            </div>
          )}
        </details>
      );
    case "final":
      return (
        <div className={`rounded-xl border px-3 py-2 text-sm whitespace-pre-wrap ${entry.success ? "border-border bg-card" : "border-destructive/50 bg-destructive/10"}`}>
          <Marked text={entry.text} />
        </div>
      );
    case "error":
      return <div className="rounded-xl border border-destructive/50 bg-destructive/10 px-3 py-2 text-sm whitespace-pre-wrap">{entry.text}</div>;
  }
}

/**
 * The conversation after one more event of a run.
 * @param entries The conversation so far.
 * @param event The event.
 * @returns The conversation with the event drawn in.
 */
function applyEvent(entries: Entry[], event: RunEvent): Entry[] {
  switch (event.type) {
    case "thinking":
      return [...entries, { kind: "thinking", text: event.text }];
    case "text":
      return [...entries, { kind: "text", text: event.text }];
    case "call":
      return [...entries, { kind: "tool", name: event.name, arguments: event.arguments }];
    case "result": {
      let index = entries.length - 1;
      while (index >= 0) {
        const entry = entries[index];
        if (entry.kind === "tool" && entry.name === event.name && entry.result === undefined) break;
        index -= 1;
      }
      if (index < 0) return entries;
      const next = entries.slice();
      next[index] = { ...(next[index] as Extract<Entry, { kind: "tool" }>), result: event.text, failed: event.failed, image: event.image };
      return next;
    }
    case "done": {
      // The answer was already drawn as the run's last text; it is drawn once, as the answer.
      const last = entries[entries.length - 1];
      const rest = last?.kind === "text" && last.text === event.text ? entries.slice(0, -1) : entries;
      return [...rest, { kind: "final", text: event.text, success: event.success }];
    }
    case "error":
      return [...entries, { kind: "error", text: event.message }];
    default:
      return entries;
  }
}

/** What the page holds of one conversation: what is drawn, and the run in progress. */
interface Snapshot {
  entries: Entry[];
  running: boolean;
}

/**
 * One conversation with the agent, kept for the page's life under the API
 * address it talks to, so every mount of the chat draws the same one.
 */
class Conversation {
  private snapshot: Snapshot = { entries: [], running: false };
  private listeners = new Set<() => void>();
  private session: string | undefined;
  private abort: AbortController | null = null;

  constructor(private readonly base: string) {}

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  };

  read = (): Snapshot => this.snapshot;

  private update(change: Partial<Snapshot>): void {
    this.snapshot = { ...this.snapshot, ...change };
    this.listeners.forEach((listener) => listener());
  }

  private add(entry: Entry): void {
    this.update({ entries: [...this.snapshot.entries, entry] });
  }

  /**
   * Gives the agent a task and draws its work until it ends or is stopped.
   * @param text The task.
   * @param config The configuration to run on.
   * @param effort The effort in place of the configuration's, or none.
   */
  async send(text: string, config: string | undefined, effort: string | undefined): Promise<void> {
    if (this.snapshot.running) return;
    this.update({ entries: [...this.snapshot.entries, { kind: "user", text }], running: true });
    const controller = new AbortController();
    this.abort = controller;
    try {
      await run(this.base, { prompt: text, session: this.session, config, effort }, (event) => {
        if (event.type === "start") this.session = event.session;
        this.update({ entries: applyEvent(this.snapshot.entries, event) });
      }, controller.signal);
    } catch (error) {
      if (!controller.signal.aborted) this.add({ kind: "error", text: error instanceof Error ? error.message : String(error) });
    } finally {
      if (controller.signal.aborted) this.add({ kind: "error", text: t("pelorus.stopped") });
      this.abort = null;
      this.update({ running: false });
    }
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
    this.update({ entries: [] });
  }
}

const conversations = new Map<string, Conversation>();

/** The page's conversation for an API address. */
function conversationFor(base: string): Conversation {
  let conversation = conversations.get(base);
  if (!conversation) {
    conversation = new Conversation(base);
    conversations.set(base, conversation);
  }
  return conversation;
}

/**
 * Renders the chat: the conversation, the choice of configuration, model, and
 * effort, and the prompt. Reads and writes only the Pelorus API under `base`.
 */
export function PelorusChat({ base, standalone, popoutHref }: PelorusChatProps) {
  const [list, setList] = React.useState<ModelList | null>(null);
  const [loadError, setLoadError] = React.useState<string | null>(null);
  const conversation = conversationFor(base);
  const { entries, running } = React.useSyncExternalStore(conversation.subscribe, conversation.read);
  const [prompt, setPrompt] = React.useState("");
  const [settingsOpen, setSettingsOpen] = React.useState(false);
  const [config, setConfig] = React.useState("");
  const [effort, setEffort] = React.useState("");
  const bottom = React.useRef<HTMLDivElement>(null);

  const apply = React.useCallback((next: ModelList) => {
    setList(next);
    setLoadError(null);
    setConfig((current) => (next.models.some((m) => m.id === current) ? current : next.models.find((m) => m.default)?.id ?? next.models[0]?.id ?? ""));
  }, []);
  const failed = React.useCallback((error: unknown) => setLoadError(error instanceof Error ? error.message : String(error)), []);
  const reload = React.useCallback(() => listModels(base).then(apply, failed), [base, apply, failed]);

  React.useEffect(() => {
    let mounted = true;
    listModels(base).then((next) => { if (mounted) apply(next); }, (error) => { if (mounted) failed(error); });
    return () => { mounted = false; };
  }, [base, apply, failed]);
  React.useEffect(() => { bottom.current?.scrollIntoView({ block: "end" }); }, [entries]);

  const selected = list?.models.find((m) => m.id === config);
  const canRun = !!selected?.usable && !running && prompt.trim().length > 0;

  const send = () => {
    if (!canRun) return;
    const text = prompt.trim();
    setPrompt("");
    void conversation.send(text, config || undefined, effort || undefined);
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      send();
    }
  };

  const configurable = !!list && !list.locked;

  return (
    <div className={`flex min-h-0 flex-col gap-2 ${standalone ? "mx-auto h-full w-full max-w-3xl p-3" : "h-[min(70dvh,560px)] w-full"}`} data-testid="pelorus-chat">
      <div className="flex items-center gap-1">
        <span className="mr-auto text-sm font-medium">{standalone && t("pelorus.title")}</span>
        <Button variant="ghost" size="xs" onClick={() => conversation.reset()} title={t("pelorus.newChat")}><Plus />{standalone && t("pelorus.newChat")}</Button>
        {configurable && (
          <Button variant="ghost" size="xs" onClick={() => setSettingsOpen((open) => !open)} title={t("pelorus.models")} aria-pressed={settingsOpen}>
            <Settings2 />{standalone && t("pelorus.models")}
          </Button>
        )}
        {!standalone && popoutHref && (
          <Button variant="ghost" size="xs" title={t("pelorus.popout")} onClick={() => window.open(popoutHref, "_blank", "noopener")}><ExternalLink /></Button>
        )}
      </div>

      {settingsOpen && configurable && list ? (
        <ModelSettings base={base} list={list} onChanged={reload} onClose={() => setSettingsOpen(false)} />
      ) : (
        <>
          <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto rounded-lg border border-border bg-background/60 p-2">
            {entries.length === 0 && (
              <div className="m-auto max-w-sm text-center text-sm text-muted-foreground">
                {loadError ?? (list && list.models.length === 0
                  ? (list.locked ? t("pelorus.noModelLocked") : t("pelorus.noModel"))
                  : t("pelorus.hint"))}
                {list && !list.windows && <div className="mt-2">{t("pelorus.noWindows")}</div>}
              </div>
            )}
            {entries.map((entry, index) => <EntryView key={index} entry={entry} />)}
            {running && <div className="animate-pulse text-xs text-muted-foreground">{t("pelorus.working")}</div>}
            <div ref={bottom} />
          </div>

          {configurable && list && list.models.length > 0 && (
            <div className="flex flex-wrap items-center gap-1 text-xs">
              <select className={selectClass} value={config} onChange={(event) => setConfig(event.target.value)} aria-label={t("pelorus.model")} disabled={running}>
                {list.models.map((m) => <option key={m.id} value={m.id}>{m.name} · {m.model}</option>)}
              </select>
              <select className={selectClass} value={effort} onChange={(event) => setEffort(event.target.value)} aria-label={t("pelorus.effort")} disabled={running}>
                <option value="">{t("pelorus.effort")}: {selected?.effort ?? "default"}</option>
                {list.efforts.map((level) => <option key={level} value={level}>{t("pelorus.effort")}: {level}</option>)}
              </select>
            </div>
          )}

          <div className="flex items-end gap-1">
            <Textarea className="max-h-40 min-h-10 flex-1 resize-none" rows={standalone ? 2 : 1} value={prompt} placeholder={t("pelorus.placeholder")}
              onChange={(event) => setPrompt(event.target.value)} onKeyDown={onKeyDown} aria-label={t("pelorus.placeholder")} />
            {running ? (
              <Button size="icon" variant="destructive" onClick={() => conversation.stop()} title={t("pelorus.stop")} aria-label={t("pelorus.stop")}><Square /></Button>
            ) : (
              <Button size="icon" onClick={send} disabled={!canRun} title={t("pelorus.send")} aria-label={t("pelorus.send")}><ArrowUp /></Button>
            )}
          </div>
        </>
      )}
    </div>
  );
}
