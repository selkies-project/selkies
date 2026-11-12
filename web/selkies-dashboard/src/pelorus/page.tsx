/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Pelorus page, served at `pelorus/`: the agent's work in full.
 *
 * A bar across the top names the page and carries what the next task runs
 * on: the configuration, the model of its provider (picked from the
 * provider's list, `names.ts`, or typed), and its effort, each remembered by
 * this browser, then the light and dark switch the dashboard keeps (the same
 * stored choice) and the settings. Under it, tabs hold conversations
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
 * picture opens full size on a press. The account keeps its newest step in
 * view as it grows, pictures included, until the person scrolls up to read
 * (`follow.ts`). The pictures are for the person alone:
 * the model works from text, and they can be turned off in the settings.
 *
 * A configuration whose key a browser sealed (`keys.ts`) runs with the key
 * this browser opens for each run; one sealed by another browser cannot run
 * here, and the prompt says so.
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
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { useTheme } from "@/components/ui/theme-provider";
import { t } from "@/i18n";
import { listModels, type ModelConfig, type ModelList } from "@/pelorus/api";
import { conversationFor, describeCall, type Exchange, type Step } from "@/pelorus/conversation";
import { useFollowEnd } from "@/pelorus/follow";
import { Inline, Markdown } from "@/pelorus/markdown";
import logoUrl from "@/pelorus/logo.svg";
import { unseal, useHeld } from "@/pelorus/keys";
import { listingKey, useModelNames } from "@/pelorus/names";
import { Choice, ModelPicker } from "@/pelorus/pickers";
import { ModelSettings } from "@/pelorus/settings";

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

/** What the bar was last set to run on. */
interface Choices {
  /** The configuration chosen; the default one where it is gone. */
  config: string;
  /** The effort asked for; the configuration's own where empty. */
  effort: string;
  /** The model asked for in place of each configuration's own, by configuration. */
  models: Record<string, string>;
}

const CHOICES_KEY = "pelorus.choices";

/**
 * A stored object over its defaults.
 * @param key The storage key.
 * @param defaults What stands where nothing is stored or it does not read.
 * @returns The stored fields over the defaults.
 */
function loadStored<T extends object>(key: string, defaults: T): T {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(key) ?? "null");
    if (stored && typeof stored === "object") return { ...defaults, ...(stored as Partial<T>) };
  } catch { /* unreadable: the defaults stand */ }
  return defaults;
}

/**
 * Keeps an object for the next visit.
 * @param key The storage key.
 * @param value The object.
 */
function store(key: string, value: object): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch { /* storage refused: the choice lasts for the page */ }
}

/** The Pelorus mark, a compass rose. */
function PelorusMark({ size }: { size: number }) {
  return <img src={logoUrl} width={size} height={size} alt="" aria-hidden className="shrink-0 select-none" draggable={false} />;
}

/** The model picker of the bar, over the list of the chosen configuration's provider. */
function BarModelPicker({ base, config, value, onChange }: { base: string; config: ModelConfig; value: string; onChange: (model: string) => void }) {
  const probe = { id: config.id, provider: config.provider, endpoint: config.endpoint };
  const names = useModelNames(base, config.endpoint ? listingKey(config.provider, config.endpoint) : "",
    config.sealed ? async () => ({ ...probe, api_key: await unseal(config.sealed) }) : probe);
  return <ModelPicker className="order-first w-full md:order-none md:w-72" value={value} names={names} onChange={(model) => onChange(model || config.model)} />;
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
function Pane({ base, id, list, config, model, effort, client, onOpen }: { base: string; id: number; list: ModelList; config: string; model: string; effort: string; client: ClientSettings; onOpen: (source: string) => void }) {
  const conversation = conversationFor(base, String(id));
  const { exchanges, running } = React.useSyncExternalStore(conversation.subscribe, conversation.read);
  const [prompt, setPrompt] = React.useState("");
  const { scroller, content, stick } = useFollowEnd(exchanges);

  const selected = list.models.find((m) => m.id === config);
  const held = useHeld(selected?.sealed ?? "");
  const canRun = !!selected?.usable && held !== false && !running && prompt.trim().length > 0;
  const send = async () => {
    if (!canRun) return;
    const text = prompt.trim();
    setPrompt("");
    stick();
    const apiKey = selected?.sealed ? await unseal(selected.sealed) : undefined;
    void conversation.send(text, {
      config: config || undefined,
      api_key: apiKey,
      model: model && model !== selected?.model ? model : undefined,
      effort: effort || undefined,
      screenshots: client.screenshots,
      suffix: client.suffix || undefined,
      max_steps: client.maxSteps > 0 ? client.maxSteps : undefined,
    }, t("pelorus.stopped"));
  };

  return (
    <>
      <div ref={scroller} className="min-h-0 flex-1 overflow-y-auto">
        <div ref={content} className="mx-auto flex w-full max-w-[1800px] flex-col gap-6 px-6 py-6">
          {exchanges.length === 0 && (
            <div className="mx-auto mt-[12vh] flex max-w-lg flex-col items-center gap-4 text-center">
              <PelorusMark size={88} />
              <div className="flex flex-col gap-1">
                <h1 className="text-2xl font-semibold tracking-tight">{t("pelorus.title")}</h1>
                <p className="text-base text-muted-foreground">{t("pelorus.tagline")}</p>
              </div>
              <p className="text-sm leading-relaxed text-muted-foreground">{t("pelorus.hint")}</p>
              {!list.windows && <p className="rounded-lg border border-border bg-muted/40 px-3 py-2 text-sm text-muted-foreground">{t("pelorus.noWindows")}</p>}
            </div>
          )}
          {exchanges.map((exchange, index) => <ExchangeView key={index} exchange={exchange} onOpen={onOpen} />)}
        </div>
      </div>
      <div className="px-4 pb-4 sm:px-6">
        {held === false && <p className="mx-auto mb-2 w-full max-w-4xl rounded-lg border border-destructive/50 bg-destructive/10 px-3 py-2 text-sm">{t("pelorus.heldElsewhere")}</p>}
        <div className="mx-auto flex w-full max-w-4xl items-end gap-2 rounded-2xl border border-border bg-card p-2 shadow-sm focus-within:border-ring">
          <Textarea className="max-h-64 min-h-14 flex-1 resize-none border-0 bg-transparent text-base shadow-none focus-visible:ring-0 md:text-sm dark:bg-transparent" rows={2} value={prompt} placeholder={t("pelorus.placeholder")} aria-label={t("pelorus.placeholder")}
            onChange={(event) => setPrompt(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                void send();
              }
            }} />
          {running ? (
            <Button size="icon" className="rounded-xl" variant="destructive" onClick={() => conversation.stop()} title={t("pelorus.stop")} aria-label={t("pelorus.stop")}><Square /></Button>
          ) : (
            <Button size="icon" className="rounded-xl" onClick={() => void send()} disabled={!canRun} title={t("pelorus.send")} aria-label={t("pelorus.send")}><ArrowUp /></Button>
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
  const [choices, setChoices] = React.useState<Choices>(() => loadStored(CHOICES_KEY, { config: "", effort: "", models: {} }));
  const [tabs, setTabs] = React.useState<number[]>([1]);
  const [active, setActive] = React.useState(1);
  const [settingsOpen, setSettingsOpen] = React.useState(false);
  const [client, setClient] = React.useState<ClientSettings>(() => loadStored(CLIENT_KEY, CLIENT_DEFAULTS));
  const [enlarged, setEnlarged] = React.useState<string | null>(null);
  const nextTab = React.useRef(2);

  const apply = React.useCallback((next: ModelList) => {
    setList(next);
    setLoadError(null);
  }, []);
  const failed = React.useCallback((error: unknown) => setLoadError(error instanceof Error ? error.message : String(error)), []);
  const reload = React.useCallback(() => listModels(base).then(apply, failed), [base, apply, failed]);
  React.useEffect(() => {
    let mounted = true;
    listModels(base).then((next) => { if (mounted) apply(next); }, (error) => { if (mounted) failed(error); });
    return () => { mounted = false; };
  }, [base, apply, failed]);

  const choose = (change: Partial<Choices>) => {
    const next = { ...choices, ...change };
    setChoices(next);
    store(CHOICES_KEY, next);
  };
  const saveClient = (change: Partial<ClientSettings>) => {
    const next = { ...client, ...change };
    setClient(next);
    store(CLIENT_KEY, next);
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
      <div className="flex h-full justify-center overflow-y-auto px-4 py-10 sm:items-center">
        <div className="flex h-fit w-full max-w-xl flex-col gap-6 rounded-2xl border border-border bg-card p-6 shadow-sm sm:p-8">
          <div className="flex flex-col items-center gap-3 text-center">
            <PelorusMark size={80} />
            <div className="flex flex-col gap-1">
              <h1 className="text-2xl font-semibold tracking-tight">{t("pelorus.title")}</h1>
              <p className="text-sm text-muted-foreground">{t("pelorus.tagline")}</p>
            </div>
          </div>
          {list.locked ? (
            <p className="text-center text-sm text-muted-foreground">{t("pelorus.noModelLocked")}</p>
          ) : (
            <div className="flex flex-col gap-4 border-t border-border pt-6">
              <div className="flex flex-col gap-1">
                <h2 className="text-base font-semibold">{t("pelorus.add")}</h2>
                <p className="text-sm text-muted-foreground">{t("pelorus.setupHint")}</p>
              </div>
              <ModelSettings base={base} list={list} onChanged={reload} />
            </div>
          )}
          <div className="flex justify-center">{themeToggle}</div>
        </div>
      </div>
    );
  }

  const selected = list.models.find((m) => m.id === choices.config) ?? list.models.find((m) => m.default) ?? list.models[0];
  const model = (selected && choices.models[selected.id]) || selected?.model || "";
  const effort = list.efforts.includes(choices.effort) ? choices.effort : "";
  return (
    <div className="flex h-full flex-col" data-testid="pelorus-page">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card/60 px-4 py-2.5">
        <div className="flex items-center gap-2 pr-2 text-lg font-semibold tracking-tight"><PelorusMark size={30} />{t("pelorus.title")}</div>
        {list.locked ? (
          selected && <Badge variant="secondary" className="h-7 px-3 text-xs">{selected.model}</Badge>
        ) : selected && (
          <div className="order-last flex w-full min-w-0 flex-wrap items-center gap-2 md:order-none md:w-auto">
            {list.models.length > 1 && (
              <Choice className="flex-1 md:w-48 md:flex-none" label={t("pelorus.configuration")} value={selected.id}
                options={list.models.map((m) => ({ value: m.id, label: m.name, detail: list.providers.find((p) => p.name === m.provider)?.label ?? m.provider }))}
                onChange={(config) => choose({ config })} />
            )}
            <BarModelPicker base={base} config={selected} value={model}
              onChange={(next) => {
                const models = { ...choices.models };
                if (next === selected.model) delete models[selected.id];
                else models[selected.id] = next;
                choose({ config: selected.id, models });
              }} />
            <Choice className="flex-1 md:w-40 md:flex-none" label={t("pelorus.effort")} prefix={`${t("pelorus.effort")}:`} value={effort}
              options={[{ value: "", label: selected.effort }, ...list.efforts.filter((level) => level !== selected.effort).map((level) => ({ value: level, label: level }))]}
              onChange={(next) => choose({ effort: next })} />
          </div>
        )}
        <div className="ml-auto flex items-center gap-1">
          {themeToggle}
          <Button variant="ghost" size="icon-sm" onClick={() => setSettingsOpen(true)} title={t("pelorus.settings")} aria-label={t("pelorus.settings")}><Settings2 className="size-4" /></Button>
        </div>
      </div>

      <div className="flex items-center overflow-x-auto border-b border-border px-2">
        {tabs.map((id, index) => (
          <TabLabel key={id} base={base} id={id} number={index + 1} active={id === active} closable={tabs.length > 1} onSelect={() => setActive(id)} onClose={() => closeTab(id)} />
        ))}
        <button type="button" className="shrink-0 rounded p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground" onClick={addTab} title={t("pelorus.newChat")} aria-label={t("pelorus.newChat")}>
          <Plus className="size-3.5" />
        </button>
      </div>

      <Pane key={active} base={base} id={active} list={list} config={selected?.id ?? ""} model={model} effort={effort} client={client} onOpen={setEnlarged} />

      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-h-[90dvh] gap-5 overflow-y-auto sm:max-w-2xl sm:p-8">
          <DialogHeader><DialogTitle className="text-lg">{t("pelorus.settings")}</DialogTitle></DialogHeader>
          <Tabs defaultValue={list.locked ? "client" : "models"} className="gap-5">
            <TabsList>
              {!list.locked && <TabsTrigger value="models">{t("pelorus.models")}</TabsTrigger>}
              <TabsTrigger value="client">{t("pelorus.client")}</TabsTrigger>
            </TabsList>
            {!list.locked && (
              <TabsContent value="models">
                <ModelSettings base={base} list={list} onChanged={reload} onClose={() => setSettingsOpen(false)} />
              </TabsContent>
            )}
            <TabsContent value="client">
              <div className="flex flex-col gap-5 text-sm">
                <div className="flex flex-col gap-1.5">
                  <Label htmlFor="pelorus-suffix">{t("pelorus.suffix")}</Label>
                  <Textarea id="pelorus-suffix" rows={4} value={client.suffix} placeholder={t("pelorus.suffixHint")} onChange={(event) => saveClient({ suffix: event.target.value })} />
                </div>
                <div className="flex flex-col gap-1.5">
                  <Label htmlFor="pelorus-steps">{t("pelorus.maxSteps")}</Label>
                  <Input id="pelorus-steps" className="w-32" type="number" min={1} max={list.max_steps} value={client.maxSteps > 0 ? client.maxSteps : list.max_steps}
                    onChange={(event) => saveClient({ maxSteps: Math.max(0, Math.min(list.max_steps, Number(event.target.value) || 0)) })} />
                </div>
                <label htmlFor="pelorus-shots" className="flex cursor-pointer items-center justify-between gap-3 rounded-lg border border-border px-3 py-2.5">
                  <span className="font-medium">{t("pelorus.screenshots")}</span>
                  <Switch id="pelorus-shots" checked={client.screenshots} onCheckedChange={(screenshots) => saveClient({ screenshots })} />
                </label>
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
