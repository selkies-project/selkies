/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Pelorus page, served at `pelorus/`: the agent's work in full.
 *
 * A bar across the top names the page and carries the model the next task
 * runs on, its effort, the light and dark switch the dashboard keeps (the
 * same stored choice), and the settings. Under it, tabs hold conversations
 * side by side; the one in front fills the page, and the prompt sits at the
 * bottom.
 *
 * The account uses the window's width, since what it shows is pictures of a
 * screen: a step puts what the agent did beside the screen it left, and the
 * screens before and after a task stand side by side at a size that can be
 * read without opening them.
 *
 * A task is drawn as the person's message, the screen as it stood before,
 * the agent's steps folded under one toggle (open while it works, folded
 * once it answers), and its answer with the screen as it left it. A step is
 * one turn of the model: its reasoning, what it said, each tool it called
 * with what the tool returned, and the screen after a tool that acted. Any
 * picture opens full size on a press. The pictures are for the person alone:
 * the model works from text, and they can be turned off in the settings.
 *
 * The settings are a dialog of two parts: the model configurations
 * (`settings.tsx`), which a locked server neither shows nor takes, and what
 * this browser keeps for itself: extra instructions for the agent, how many
 * steps a run may take, and whether pictures are asked for. With no model
 * configured yet the page opens on the form that adds one.
 * @module
 */
import * as React from "react";
import { ArrowUp, ChevronRight, Loader2, Moon, Plus, Settings2, Square, Sun, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { useTheme } from "@/components/ui/theme-provider";
import { SelkiesLogo } from "@/components/logo";
import { t } from "@/i18n";
import { listModels, type ModelList } from "@/pelorus/api";
import { conversationFor, describeCall, type Exchange, type Step } from "@/pelorus/conversation";
import { ModelSettings, selectClass } from "@/pelorus/settings";

/** What this browser keeps of the person's own choices. */
interface ClientSettings {
  /** Instructions added after the server's on every run. */
  suffix: string;
  /** The most steps a run may take; the server's own limit where zero. */
  maxSteps: number;
  /** Ask for the screen as a run goes. */
  screenshots: boolean;
}

const CLIENT_KEY = "pelorus.client";
const CLIENT_DEFAULTS: ClientSettings = { suffix: "", maxSteps: 0, screenshots: true };

/** The stored client settings, the defaults where none are stored or they do not read. */
function loadClient(): ClientSettings {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(CLIENT_KEY) ?? "null");
    if (stored && typeof stored === "object") return { ...CLIENT_DEFAULTS, ...(stored as Partial<ClientSettings>) };
  } catch { /* unreadable: the defaults stand */ }
  return CLIENT_DEFAULTS;
}

/** Draws the inline marks of a line: `**bold**` and `` `code` ``. */
function Inline({ text }: { text: string }) {
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

/**
 * Draws an answer written in the Markdown models reach for: paragraphs,
 * headings, bulleted and numbered lists, fenced code, and the inline marks.
 * Anything else is shown as written.
 */
function Markdown({ text }: { text: string }) {
  const blocks: React.ReactNode[] = [];
  const lines = text.split("\n");
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.startsWith("```")) {
      const code: string[] = [];
      index += 1;
      while (index < lines.length && !lines[index].startsWith("```")) code.push(lines[index++]);
      index += 1;
      blocks.push(<pre key={blocks.length} className="overflow-x-auto rounded-md bg-muted p-2 font-mono text-xs">{code.join("\n")}</pre>);
      continue;
    }
    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading) {
      blocks.push(<div key={blocks.length} className="mt-1 font-semibold"><Inline text={heading[2]} /></div>);
      index += 1;
      continue;
    }
    const bullet = /^\s*([-*]|\d+[.)])\s+/;
    if (bullet.test(line)) {
      const items: string[] = [];
      const ordered = /^\s*\d/.test(line);
      while (index < lines.length && bullet.test(lines[index])) items.push(lines[index++].replace(bullet, ""));
      const List = ordered ? "ol" : "ul";
      blocks.push(
        <List key={blocks.length} className={`ml-5 ${ordered ? "list-decimal" : "list-disc"}`}>
          {items.map((item, at) => <li key={at}><Inline text={item} /></li>)}
        </List>,
      );
      continue;
    }
    if (line.trim() === "") {
      index += 1;
      continue;
    }
    const paragraph: string[] = [];
    while (index < lines.length && lines[index].trim() !== "" && !lines[index].startsWith("```") && !bullet.test(lines[index]) && !/^#{1,4}\s/.test(lines[index])) {
      paragraph.push(lines[index++]);
    }
    blocks.push(<p key={blocks.length} className="whitespace-pre-wrap"><Inline text={paragraph.join("\n")} /></p>);
  }
  return <div className="flex flex-col gap-2 text-sm leading-relaxed">{blocks}</div>;
}

/** A picture of the screen, small in the account and full size on a press. */
function Shot({ image, type, label, onOpen, wide }: { image: string; type: "jpeg" | "png"; label: string; onOpen: (source: string) => void; wide?: boolean }) {
  const source = `data:image/${type};base64,${image}`;
  return (
    <button type="button" className={`block w-full cursor-zoom-in overflow-hidden rounded-md border border-border ${wide ? "" : "max-w-3xl"}`}
      title={label} onClick={() => onOpen(source)}>
      <img className="block w-full" alt={label} src={source} loading="lazy" />
    </button>
  );
}

/** One step: its reasoning, what the model said, and its tool calls with what each returned. */
function StepView({ step, onOpen }: { step: Step; onOpen: (source: string) => void }) {
  return (
    <div className="flex flex-col gap-1.5 border-b border-border py-2 last:border-b-0">
      <div className="text-xs font-semibold tracking-wide text-primary uppercase">{t("pelorus.step", { number: step.number })}</div>
      {step.thinking.map((text, index) => <div key={`t${index}`} className="border-l-2 border-border pl-2 text-xs whitespace-pre-wrap text-muted-foreground italic">{text}</div>)}
      {step.text.map((text, index) => <div key={`s${index}`} className="text-sm whitespace-pre-wrap"><Inline text={text} /></div>)}
      {step.tools.map((tool, index) => (
        <div key={`c${index}`} className={`grid items-start gap-2 ${tool.image || tool.screen ? "xl:grid-cols-2" : ""}`}>
          <details className="group min-w-0 rounded-md border border-border bg-muted/40 text-xs" open={!!(tool.image || tool.screen) || undefined}>
            <summary className="flex cursor-pointer list-none items-center gap-1 px-2 py-1 font-mono">
              <ChevronRight className="size-3 shrink-0 transition-transform group-open:rotate-90" />
              <span className={`truncate ${tool.failed ? "text-destructive" : ""}`}>{describeCall(tool.name, tool.arguments)}</span>
              {tool.result === undefined && <Loader2 className="ml-auto size-3 shrink-0 animate-spin text-muted-foreground" />}
            </summary>
            {tool.result !== undefined && <pre className="max-h-72 overflow-auto border-t border-border px-2 py-1 font-mono whitespace-pre-wrap text-muted-foreground">{tool.result}</pre>}
          </details>
          {tool.image && <Shot image={tool.image} type="png" label={describeCall(tool.name, tool.arguments)} onOpen={onOpen} wide />}
          {tool.screen && <Shot image={tool.screen} type="jpeg" label={describeCall(tool.name, tool.arguments)} onOpen={onOpen} wide />}
        </div>
      ))}
    </div>
  );
}

/** One task: the message, the screen before, the steps under their toggle, and the answer with the screen after. */
function ExchangeView({ exchange, onOpen }: { exchange: Exchange; onOpen: (source: string) => void }) {
  const [open, setOpen] = React.useState<boolean | null>(null);
  const shown = open ?? exchange.running;
  return (
    <div className="flex flex-col gap-3">
      <div className="ml-12 max-w-4xl self-end rounded-2xl bg-primary px-4 py-2 text-sm whitespace-pre-wrap text-primary-foreground">{exchange.prompt}</div>
      {exchange.startScreen && (!exchange.endScreen || !exchange.answer) && (
        <figure className="flex max-w-3xl flex-col gap-1">
          <figcaption className="text-xs text-muted-foreground">{t("pelorus.before")}</figcaption>
          <Shot image={exchange.startScreen} type="jpeg" label={t("pelorus.before")} onOpen={onOpen} wide />
        </figure>
      )}
      {exchange.steps.length > 0 && (
        <div className="rounded-lg border border-border">
          <button type="button" className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs font-medium text-muted-foreground hover:text-foreground"
            aria-expanded={shown} onClick={() => setOpen(!shown)}>
            <ChevronRight className={`size-3 transition-transform ${shown ? "rotate-90" : ""}`} />
            {t("pelorus.thoughtProcess")} · {t("pelorus.steps", { count: exchange.steps.length })}
            {exchange.running && <Loader2 className="ml-auto size-3.5 animate-spin" />}
          </button>
          {shown && <div className="border-t border-border px-3">{exchange.steps.map((step) => <StepView key={step.number} step={step} onOpen={onOpen} />)}</div>}
        </div>
      )}
      {exchange.running && exchange.steps.length === 0 && <div className="flex items-center gap-2 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" />{t("pelorus.working")}</div>}
      {exchange.answer && (
        <div className={`flex flex-col gap-3 rounded-xl border p-4 ${exchange.answer.success ? "border-border bg-card" : "border-destructive/50 bg-destructive/10"}`}>
          <Markdown text={exchange.answer.text} />
          {exchange.endScreen && (
            <div className="grid gap-3 lg:grid-cols-2">
              {exchange.startScreen && (
                <figure className="flex flex-col gap-1">
                  <figcaption className="text-xs text-muted-foreground">{t("pelorus.before")}</figcaption>
                  <Shot image={exchange.startScreen} type="jpeg" label={t("pelorus.before")} onOpen={onOpen} wide />
                </figure>
              )}
              <figure className="flex flex-col gap-1">
                <figcaption className="text-xs text-muted-foreground">{t("pelorus.after")}</figcaption>
                <Shot image={exchange.endScreen} type="jpeg" label={t("pelorus.after")} onOpen={onOpen} wide />
              </figure>
            </div>
          )}
        </div>
      )}
      {exchange.error && !exchange.answer && <div className="rounded-xl border border-destructive/50 bg-destructive/10 px-4 py-2 text-sm whitespace-pre-wrap">{exchange.error}</div>}
    </div>
  );
}

/** The tab of one conversation, named by its first task. */
function TabLabel({ base, id, number, active, closable, onSelect, onClose }: { base: string; id: number; number: number; active: boolean; closable: boolean; onSelect: () => void; onClose: () => void }) {
  const conversation = conversationFor(base, String(id));
  const { exchanges, running } = React.useSyncExternalStore(conversation.subscribe, conversation.read);
  const title = exchanges[0]?.prompt ?? t("pelorus.chat", { number });
  return (
    <div className={`flex max-w-56 shrink-0 items-center gap-1 border-b-2 px-3 py-2 text-xs ${active ? "border-primary text-foreground" : "border-transparent text-muted-foreground hover:text-foreground"}`}>
      <button type="button" className="truncate" onClick={onSelect} title={title}>{title}</button>
      {running && <Loader2 className="size-3 shrink-0 animate-spin" />}
      {closable && <button type="button" className="shrink-0 rounded p-0.5 hover:bg-muted hover:text-destructive" onClick={onClose} title={t("pelorus.closeTab")} aria-label={t("pelorus.closeTab")}><X className="size-3" /></button>}
    </div>
  );
}

/** The conversation in front, with the prompt under it. */
function Pane({ base, id, list, config, effort, client, onOpen }: { base: string; id: number; list: ModelList; config: string; effort: string; client: ClientSettings; onOpen: (source: string) => void }) {
  const conversation = conversationFor(base, String(id));
  const { exchanges, running } = React.useSyncExternalStore(conversation.subscribe, conversation.read);
  const [prompt, setPrompt] = React.useState("");
  const scroller = React.useRef<HTMLDivElement>(null);
  const follow = React.useRef(true);

  // The account follows its own end while the person is at it, and stays put once they scroll up to read.
  React.useEffect(() => {
    const element = scroller.current;
    if (element && follow.current) element.scrollTop = element.scrollHeight;
  }, [exchanges]);

  const selected = list.models.find((m) => m.id === config);
  const canRun = !!selected?.usable && !running && prompt.trim().length > 0;
  const send = () => {
    if (!canRun) return;
    const text = prompt.trim();
    setPrompt("");
    follow.current = true;
    void conversation.send(text, {
      config: config || undefined,
      effort: effort || undefined,
      screenshots: client.screenshots,
      suffix: client.suffix || undefined,
      max_steps: client.maxSteps > 0 ? client.maxSteps : undefined,
    }, t("pelorus.stopped"));
  };

  return (
    <>
      <div ref={scroller} className="min-h-0 flex-1 overflow-y-auto"
        onScroll={(event) => {
          const element = event.currentTarget;
          follow.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
        }}>
        <div className="mx-auto flex w-full max-w-[1800px] flex-col gap-6 px-6 py-6">
          {exchanges.length === 0 && (
            <div className="mx-auto mt-16 flex max-w-md flex-col items-center gap-3 text-center text-sm text-muted-foreground">
              <SelkiesLogo width={44} height={44} />
              <div>{t("pelorus.hint")}</div>
              {!list.windows && <div>{t("pelorus.noWindows")}</div>}
            </div>
          )}
          {exchanges.map((exchange, index) => <ExchangeView key={index} exchange={exchange} onOpen={onOpen} />)}
        </div>
      </div>
      <div className="border-t border-border bg-card/60">
        <div className="mx-auto flex w-full max-w-[1800px] items-end gap-2 px-6 py-3">
          <Textarea className="max-h-64 min-h-16 flex-1 resize-none" rows={2} value={prompt} placeholder={t("pelorus.placeholder")} aria-label={t("pelorus.placeholder")}
            onChange={(event) => setPrompt(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                send();
              }
            }} />
          {running ? (
            <Button size="icon" variant="destructive" onClick={() => conversation.stop()} title={t("pelorus.stop")} aria-label={t("pelorus.stop")}><Square /></Button>
          ) : (
            <Button size="icon" onClick={send} disabled={!canRun} title={t("pelorus.send")} aria-label={t("pelorus.send")}><ArrowUp /></Button>
          )}
        </div>
      </div>
    </>
  );
}

/** Renders the page; reads and writes only the Pelorus API under `base`. */
export function PelorusPage({ base }: { base: string }) {
  const { theme, setTheme } = useTheme();
  const [list, setList] = React.useState<ModelList | null>(null);
  const [loadError, setLoadError] = React.useState<string | null>(null);
  const [config, setConfig] = React.useState("");
  const [effort, setEffort] = React.useState("");
  const [tabs, setTabs] = React.useState<number[]>([1]);
  const [active, setActive] = React.useState(1);
  const [settingsOpen, setSettingsOpen] = React.useState(false);
  const [client, setClient] = React.useState<ClientSettings>(loadClient);
  const [enlarged, setEnlarged] = React.useState<string | null>(null);
  const nextTab = React.useRef(2);

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

  const saveClient = (change: Partial<ClientSettings>) => {
    const next = { ...client, ...change };
    setClient(next);
    try {
      localStorage.setItem(CLIENT_KEY, JSON.stringify(next));
    } catch { /* storage refused: the choice lasts for the page */ }
  };

  const addTab = () => {
    const id = nextTab.current;
    nextTab.current += 1;
    setTabs((current) => [...current, id]);
    setActive(id);
  };
  const closeTab = (id: number) => {
    conversationFor(base, String(id)).reset();
    const rest = tabs.filter((tab) => tab !== id);
    setTabs(rest);
    if (active === id) setActive(rest[rest.length - 1]);
  };

  const themeToggle = (
    <Button variant="ghost" size="icon-sm" className="relative" aria-label={t("toggleThemeTitle")} title={t("toggleThemeTitle")}
      onClick={() => setTheme(theme === "light" ? "dark" : "light")}>
      <Sun className="size-4 scale-100 rotate-0 transition-all dark:scale-0 dark:-rotate-90" />
      <Moon className="absolute size-4 scale-0 rotate-90 transition-all dark:scale-100 dark:rotate-0" />
    </Button>
  );

  if (!list) {
    return <div className="flex h-full items-center justify-center text-sm text-muted-foreground">{loadError ?? <Loader2 className="size-5 animate-spin" />}</div>;
  }

  if (list.models.length === 0) {
    return (
      <div className="flex h-full items-center justify-center overflow-y-auto p-4">
        <div className="flex w-full max-w-md flex-col gap-4 rounded-2xl border border-border bg-card p-6">
          <div className="flex flex-col items-center gap-1 text-center">
            <SelkiesLogo width={48} height={48} />
            <h1 className="text-xl font-bold">{t("pelorus.title")}</h1>
            <p className="text-sm text-muted-foreground">{t("pelorus.tagline")}</p>
          </div>
          {list.locked
            ? <p className="text-center text-sm text-muted-foreground">{t("pelorus.noModelLocked")}</p>
            : <div className="flex min-h-96 flex-col"><ModelSettings base={base} list={list} onChanged={reload} onClose={() => undefined} /></div>}
          <div className="flex justify-center">{themeToggle}</div>
        </div>
      </div>
    );
  }

  const selected = list.models.find((m) => m.id === config);
  return (
    <div className="flex h-full flex-col" data-testid="pelorus-page">
      <div className="flex items-center gap-3 border-b border-border bg-card/60 px-4 py-2">
        <div className="flex items-center gap-2 font-semibold"><SelkiesLogo width={22} height={22} />{t("pelorus.title")}</div>
        {list.locked ? (
          selected && <span className="truncate rounded-md border border-border px-2 py-1 text-xs text-muted-foreground">{selected.model}</span>
        ) : (
          <>
            <select className={selectClass} value={config} onChange={(event) => setConfig(event.target.value)} aria-label={t("pelorus.model")}>
              {list.models.map((m) => <option key={m.id} value={m.id}>{m.name} · {m.model}</option>)}
            </select>
            <select className={selectClass} value={effort} onChange={(event) => setEffort(event.target.value)} aria-label={t("pelorus.effort")}>
              <option value="">{t("pelorus.effort")}: {selected?.effort ?? "default"}</option>
              {list.efforts.map((level) => <option key={level} value={level}>{t("pelorus.effort")}: {level}</option>)}
            </select>
          </>
        )}
        <div className="flex-1" />
        {themeToggle}
        <Button variant="ghost" size="icon-sm" onClick={() => setSettingsOpen(true)} title={t("pelorus.settings")} aria-label={t("pelorus.settings")}><Settings2 className="size-4" /></Button>
      </div>

      <div className="flex items-center overflow-x-auto border-b border-border px-2">
        {tabs.map((id, index) => (
          <TabLabel key={id} base={base} id={id} number={index + 1} active={id === active} closable={tabs.length > 1} onSelect={() => setActive(id)} onClose={() => closeTab(id)} />
        ))}
        <button type="button" className="shrink-0 rounded p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground" onClick={addTab} title={t("pelorus.newChat")} aria-label={t("pelorus.newChat")}>
          <Plus className="size-3.5" />
        </button>
      </div>

      <Pane key={active} base={base} id={active} list={list} config={config} effort={effort} client={client} onOpen={setEnlarged} />

      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90dvh] gap-4 overflow-y-auto sm:max-w-xl">
          <DialogHeader><DialogTitle>{t("pelorus.settings")}</DialogTitle></DialogHeader>
          <Tabs defaultValue={list.locked ? "client" : "models"}>
            <TabsList>
              {!list.locked && <TabsTrigger value="models">{t("pelorus.models")}</TabsTrigger>}
              <TabsTrigger value="client">{t("pelorus.client")}</TabsTrigger>
            </TabsList>
            {!list.locked && (
              <TabsContent value="models">
                <div className="flex min-h-96 flex-col"><ModelSettings base={base} list={list} onChanged={reload} onClose={() => setSettingsOpen(false)} /></div>
              </TabsContent>
            )}
            <TabsContent value="client">
              <div className="flex flex-col gap-4 pt-2 text-sm">
                <div className="flex flex-col gap-1.5">
                  <Label htmlFor="pelorus-suffix">{t("pelorus.suffix")}</Label>
                  <Textarea id="pelorus-suffix" rows={3} value={client.suffix} placeholder={t("pelorus.suffixHint")} onChange={(event) => saveClient({ suffix: event.target.value })} />
                </div>
                <div className="flex flex-col gap-1.5">
                  <Label htmlFor="pelorus-steps">{t("pelorus.maxSteps")}</Label>
                  <Input id="pelorus-steps" type="number" min={1} max={list.max_steps} value={client.maxSteps > 0 ? client.maxSteps : list.max_steps}
                    onChange={(event) => saveClient({ maxSteps: Math.max(0, Math.min(list.max_steps, Number(event.target.value) || 0)) })} />
                </div>
                <div className="flex items-center justify-between gap-2">
                  <Label htmlFor="pelorus-shots">{t("pelorus.screenshots")}</Label>
                  <Switch id="pelorus-shots" checked={client.screenshots} onCheckedChange={(screenshots) => saveClient({ screenshots })} />
                </div>
              </div>
            </TabsContent>
          </Tabs>
        </DialogContent>
      </Dialog>

      {enlarged && (
        <div className="fixed inset-0 z-[3000] flex cursor-zoom-out items-center justify-center bg-black/85 p-4" role="dialog" aria-modal="true" onClick={() => setEnlarged(null)}>
          <img className="max-h-full max-w-full rounded-md" alt="" src={enlarged} />
        </div>
      )}
    </div>
  );
}
