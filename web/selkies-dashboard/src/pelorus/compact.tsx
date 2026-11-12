/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Pelorus chat as the dashboard carries it: a box to ask in and one line
 * of what the agent is doing or answered, with the page of its own one press
 * away.
 *
 * A panel beside the stream has no room for the account of a run, and the
 * person asking is watching the stream itself, so it draws no steps, no
 * pictures, and no settings: those are the page's (`page.tsx`). It runs on
 * the server's default configuration and shares the page-scope conversation
 * (`conversation.ts`), so closing the panel neither stops the agent nor
 * forgets what it said. The box is tall, since it is all the panel has, and
 * nothing here scrolls the sidebar: a panel that moved it on every event
 * would pull the person's place out from under them.
 * @module
 */
import * as React from "react";
import { ArrowUp, ExternalLink, Square } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { t } from "@/i18n";
import { conversationFor, describeCall } from "@/pelorus/conversation";

interface PelorusCompactProps {
  /** The API's address, the prefixed `/pelorus/api`. */
  base: string;
  /** The page the pop-out opens. */
  popoutHref: string;
}

/** Renders the prompt, the send or stop button, the pop-out, and one line of status. */
export function PelorusCompact({ base, popoutHref }: PelorusCompactProps) {
  const conversation = conversationFor(base);
  const { exchanges, running } = React.useSyncExternalStore(conversation.subscribe, conversation.read);
  const [prompt, setPrompt] = React.useState("");

  const send = () => {
    const text = prompt.trim();
    if (!text || running) return;
    setPrompt("");
    void conversation.send(text, {}, t("pelorus.stopped"));
  };

  const last = exchanges[exchanges.length - 1];
  const lastStep = last?.steps[last.steps.length - 1];
  const lastTool = lastStep?.tools[lastStep.tools.length - 1];
  let status = "";
  if (last?.running) status = lastTool ? describeCall(lastTool.name, lastTool.arguments) : t("pelorus.working");
  else if (last?.answer) status = last.answer.text;
  else if (last?.error) status = last.error;

  return (
    <div className="flex w-full flex-col gap-2" data-testid="pelorus-compact">
      <div className="flex items-end gap-1">
        <Textarea className="max-h-[50dvh] min-h-40 flex-1 resize-y" rows={7} value={prompt} placeholder={t("pelorus.placeholder")} aria-label={t("pelorus.placeholder")}
          onChange={(event) => setPrompt(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              send();
            }
          }} />
        <div className="flex flex-col gap-1">
          <Button size="icon-sm" variant="ghost" title={t("pelorus.popout")} aria-label={t("pelorus.popout")} onClick={() => window.open(popoutHref, "_blank", "noopener")}><ExternalLink /></Button>
          {running ? (
            <Button size="icon-sm" variant="destructive" onClick={() => conversation.stop()} title={t("pelorus.stop")} aria-label={t("pelorus.stop")}><Square /></Button>
          ) : (
            <Button size="icon-sm" onClick={send} disabled={!prompt.trim()} title={t("pelorus.send")} aria-label={t("pelorus.send")}><ArrowUp /></Button>
          )}
        </div>
      </div>
      {status && (
        <div className={`max-h-[30dvh] overflow-y-auto text-xs break-words whitespace-pre-wrap ${last?.running ? "animate-pulse font-mono text-muted-foreground" : last?.error && !last.answer ? "text-destructive" : "text-foreground"}`}
          aria-live="polite">{status}</div>
      )}
    </div>
  );
}
