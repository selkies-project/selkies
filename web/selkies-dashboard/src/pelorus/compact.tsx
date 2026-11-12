/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Pelorus chat as the dashboard carries it: the conversation over a
 * prompt one line tall, in the width of a sidebar or a floating card.
 *
 * The panel has a few hundred pixels across, so the conversation takes the
 * room and the prompt only what its text needs: one line across the full
 * width, growing with what is typed up to a few, over a slim row of its
 * buttons. A task is drawn as the person's message, the agent's
 * steps as one line per tool call under a toggle (open while it works,
 * folded once it answers), and its answer. Its reasoning and the screens are
 * left to the page of its own (`page.tsx`), one press away, since the person
 * asking here is watching the screen itself; runs ask for no pictures.
 *
 * The conversation is the page-scope one (`conversation.ts`), so closing the
 * panel neither stops the agent nor forgets what it said, and it runs on the
 * server's default configuration, with its key opened here where this
 * browser sealed it (`keys.ts`); one another browser sealed cannot run here. The log scrolls inside itself, following
 * its end while the person is at it (`follow.ts`), and never scrolls the sidebar: a panel
 * that moved it on every event would pull the person's place out from under
 * them. Its height is the person's to drag. The buttons are plain ones, since
 * the sidebar's skin draws every kit button as a full-width bordered one.
 * @module
 */
import * as React from "react";
import { ArrowUp, ChevronRight, ExternalLink, Loader2, Square, SquarePen } from "lucide-react";
import { cn } from "cn";
import { t } from "@/i18n";
import { listModels, type ModelConfig } from "@/pelorus/api";
import { conversationFor, describeCall, type Exchange } from "@/pelorus/conversation";
import { useFollowEnd } from "@/pelorus/follow";
import { unseal, useHeld } from "@/pelorus/keys";
import { Inline, Markdown } from "@/pelorus/markdown";

interface PelorusCompactProps {
  /** The API's address, the prefixed `/pelorus/api`. */
  base: string;
  /** The page the pop-out opens. */
  popoutHref: string;
}

/** The look of the panel's small icon buttons. */
const iconButton = "inline-flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground disabled:pointer-events-none disabled:opacity-40";

/** The most lines the prompt grows to before it scrolls. */
const PROMPT_LINES = 5;

/** One task: the message, the steps as one line per tool call, and the answer. */
function CompactExchange({ exchange }: { exchange: Exchange }) {
  const [open, setOpen] = React.useState<boolean | null>(null);
  const shown = open ?? exchange.running;
  return (
    <div className="flex flex-col gap-1.5">
      <div className="ml-6 self-end rounded-xl rounded-br-sm border border-primary/40 bg-primary/15 px-2.5 py-1.5 text-xs break-words whitespace-pre-wrap text-foreground">{exchange.prompt}</div>
      {exchange.steps.length > 0 && (
        <div className="flex flex-col">
          <button type="button" className="flex items-center gap-1 self-start rounded px-1 py-0.5 text-[11px] text-muted-foreground hover:text-foreground"
            aria-expanded={shown} onClick={() => setOpen(!shown)}>
            <ChevronRight className={cn("size-3 transition-transform", shown && "rotate-90")} />
            {t("pelorus.steps", { count: exchange.steps.length })}
            {exchange.running && <Loader2 className="size-3 animate-spin" />}
          </button>
          {shown && (
            <div className="ml-2 flex flex-col gap-0.5 border-l border-border pl-2">
              {exchange.steps.map((step) => (
                <React.Fragment key={step.number}>
                  {step.text.map((text, index) => (
                    <div key={`s${index}`} className="text-[11px] break-words whitespace-pre-wrap text-foreground/80"><Inline text={text} /></div>
                  ))}
                  {step.tools.map((tool, index) => {
                    const line = describeCall(tool.name, tool.arguments);
                    return (
                      <div key={`c${index}`} className={cn("flex min-w-0 items-center gap-1 font-mono text-[11px]", tool.failed ? "text-destructive" : "text-muted-foreground")} title={tool.result ? `${line}\n\n${tool.result}` : line}>
                        <span className="truncate">{line}</span>
                        {tool.result === undefined && <Loader2 className="size-3 shrink-0 animate-spin" />}
                      </div>
                    );
                  })}
                </React.Fragment>
              ))}
            </div>
          )}
        </div>
      )}
      {exchange.running && exchange.steps.length === 0 && (
        <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground"><Loader2 className="size-3 animate-spin" />{t("pelorus.working")}</div>
      )}
      {exchange.answer && (
        <div className={cn("rounded-lg border px-2.5 py-2 break-words", exchange.answer.success ? "border-border bg-background/60" : "border-destructive/50 bg-destructive/10")}>
          <Markdown className="text-xs" text={exchange.answer.text} />
        </div>
      )}
      {exchange.error && !exchange.answer && (
        <div className="rounded-lg border border-destructive/50 bg-destructive/10 px-2.5 py-2 text-xs break-words whitespace-pre-wrap">{exchange.error}</div>
      )}
    </div>
  );
}

/** Renders the conversation over the prompt, and under the prompt the buttons that pop out, start over, and send or stop. */
export function PelorusCompact({ base, popoutHref }: PelorusCompactProps) {
  const conversation = conversationFor(base);
  const { exchanges, running } = React.useSyncExternalStore(conversation.subscribe, conversation.read);
  const [prompt, setPrompt] = React.useState("");
  const [config, setConfig] = React.useState<ModelConfig | undefined>();
  const held = useHeld(config?.sealed ?? "");
  const { scroller, content, stick } = useFollowEnd(exchanges);
  const field = React.useRef<HTMLTextAreaElement>(null);

  // The prompt is as tall as its text, from one line up to `PROMPT_LINES`, measured again when its width changes.
  React.useLayoutEffect(() => {
    const element = field.current;
    if (!element) return;
    const fit = () => {
      element.style.height = "auto";
      const style = getComputedStyle(element);
      const limit = (parseFloat(style.lineHeight) || 18) * PROMPT_LINES + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
      element.style.height = `${Math.min(element.scrollHeight, limit)}px`;
    };
    fit();
    let width = element.clientWidth;
    const observer = new ResizeObserver(() => {
      if (element.clientWidth !== width) {
        width = element.clientWidth;
        fit();
      }
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [prompt]);

  React.useEffect(() => {
    let live = true;
    listModels(base).then((list) => { if (live) setConfig(list.models.find((m) => m.default)); }, () => undefined);
    return () => { live = false; };
  }, [base]);

  const send = async () => {
    const text = prompt.trim();
    if (!text || running || held === false) return;
    setPrompt("");
    stick();
    const apiKey = config?.sealed ? await unseal(config.sealed) : undefined;
    void conversation.send(text, { api_key: apiKey }, t("pelorus.stopped"));
  };

  return (
    <div className="flex w-full min-w-0 flex-col gap-1.5" data-testid="pelorus-compact">
      <div ref={scroller} className={cn("h-[min(55dvh,26rem)] min-h-24 resize-y overflow-y-auto rounded-lg border border-border bg-background/40 p-2", exchanges.length === 0 && "hidden")}>
        <div ref={content} className="flex flex-col gap-3">
          {exchanges.map((exchange, index) => <CompactExchange key={index} exchange={exchange} />)}
        </div>
      </div>
      {held === false && <div className="rounded-lg border border-destructive/50 bg-destructive/10 px-2.5 py-1.5 text-xs">{t("pelorus.heldElsewhere")}</div>}
      <div className="flex flex-col rounded-lg border border-input bg-background/60 focus-within:border-ring">
        <textarea ref={field} rows={1} value={prompt} placeholder={t("pelorus.placeholderShort")} aria-label={t("pelorus.placeholder")}
          className="max-h-none! min-h-0! w-full resize-none! border-0! bg-transparent! px-2.5! pt-2! pb-0.5! font-sans! text-[13px]! leading-[18px]! text-foreground shadow-none! outline-none placeholder:text-muted-foreground"
          onChange={(event) => setPrompt(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              void send();
            }
          }} />
        <div className="flex items-center gap-0.5 px-1 pb-1">
          <button type="button" className={iconButton} onClick={() => window.open(popoutHref, "_blank", "noopener")} title={t("pelorus.popout")} aria-label={t("pelorus.popout")}>
            <ExternalLink className="size-3.5" />
          </button>
          {exchanges.length > 0 && !running && (
            <button type="button" className={iconButton} onClick={() => conversation.reset()} title={t("pelorus.newChat")} aria-label={t("pelorus.newChat")}>
              <SquarePen className="size-3.5" />
            </button>
          )}
          <div className="flex-1" />
          {running ? (
            <button type="button" className={cn(iconButton, "bg-destructive text-white hover:bg-destructive/90 hover:text-white")} onClick={() => conversation.stop()} title={t("pelorus.stop")} aria-label={t("pelorus.stop")}>
              <Square className="size-3.5" />
            </button>
          ) : (
            <button type="button" className={cn(iconButton, "bg-primary text-primary-foreground hover:bg-primary/90 hover:text-primary-foreground")} onClick={() => void send()} disabled={!prompt.trim() || held === false} title={t("pelorus.send")} aria-label={t("pelorus.send")}>
              <ArrowUp className="size-4" />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
