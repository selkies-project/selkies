/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The settings as a card of tabs, one per settings panel of the catalog
 * (`catalog.tsx`), each tab an accordion of the panel's cards in which the
 * first visible card starts open and a card with no shown control is left
 * out. The toolbar layout hangs it under its bar.
 * @module
 */
import * as React from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { PANELS, shownCards, usePanelContext, type PanelCard } from "@/panels/catalog";
import { SectionAccordion, SectionItem } from "@/panels/section-accordion";
import { t } from "@/i18n";

/**
 * The cards of one tab as a `SectionAccordion`, the first visible card open
 * until one is picked. A picked card the server's constraints then hide
 * hands its place back to the first visible card, so a tab never sits with
 * nothing open that the user did not close.
 * @param props The cards shown, in order.
 * @returns The accordion, or nothing when no card is shown.
 */
function CardAccordion({ cards }: { cards: PanelCard[] }) {
  const [picked, setPicked] = React.useState<string[] | null>(null);
  if (cards.length === 0) return null;
  const open = picked?.filter((value) => cards.some((card) => card.value === value)) ?? [];
  const value = picked && (open.length > 0 || picked.length === 0) ? open : [cards[0].value];
  return (
    <SectionAccordion value={value} onValueChange={(next) => setPicked(next as string[])}>
      {cards.map(({ value: card, title, Body }) => (
        <SectionItem key={card} value={card} title={t(title)}><Body /></SectionItem>
      ))}
    </SectionAccordion>
  );
}

/**
 * The settings panels the page shows as a card of tabs. A picked tab the
 * server's constraints then hide hands its place to the first visible one.
 */
export function SettingsCard() {
  const ctx = usePanelContext();
  const [picked, setPicked] = React.useState<string | null>(null);
  const tabs = PANELS.filter((panel) => panel.kind === "settings" && panel.shown(ctx, "menu"));
  if (tabs.length === 0) return null;
  const value = picked && tabs.some((tab) => tab.id === picked) ? picked : tabs[0].id;
  return (
    <Card className="w-[272px] max-w-[calc(100vw-1rem)] gap-1 py-1">
      <Tabs value={value} onValueChange={(next) => setPicked(String(next))} className="w-full px-1">
        <TabsList className="w-full">
          {tabs.map((tab) => <TabsTrigger key={tab.id} value={tab.id}>{t(tab.tab ?? tab.title)}</TabsTrigger>)}
        </TabsList>
        {tabs.map((tab) => (
          <TabsContent key={tab.id} value={tab.id}>
            <CardContent className="max-h-[calc(100dvh-8rem)] overflow-x-hidden overflow-y-auto px-0 pb-2">
              <CardAccordion cards={shownCards(tab, ctx)} />
            </CardContent>
          </TabsContent>
        ))}
      </Tabs>
    </Card>
  );
}
