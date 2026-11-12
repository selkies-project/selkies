/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The drop-downs of the Pelorus page: a choice among a few fixed options
 * (`Choice`), and the model picker (`ModelPicker`).
 *
 * Both are popups drawn by the kit rather than native selects, whose option
 * lists the browser draws in its own colors and type, unreadable against the
 * dark theme. The model picker searches the provider's list as it is typed
 * into and takes any name typed, since a provider can serve a model it does
 * not list and a list can be stale; its refresh button lists the models
 * again (`names.ts`). Both popups carry `allow-native-input`, so the keys
 * they take reach them rather than a session behind them.
 * @module
 */
import * as React from "react";
import { Popover } from "@base-ui/react/popover";
import { Check, ChevronDown, Loader2, RefreshCw } from "lucide-react";
import { cn } from "cn";
import { Button } from "@/components/ui/button";
import { Menu, MenuPopup, MenuRadioGroup, MenuRadioItem, MenuTrigger } from "@/components/ui/menu";
import { t } from "@/i18n";
import type { NamesState } from "@/pelorus/names";

/** The look of a picker's trigger: a field's height and border, its text left-aligned. */
const triggerClass = "h-9 min-w-0 justify-between gap-2 px-3 font-normal dark:bg-input/30";

/** One option of a `Choice`. */
export interface ChoiceOption {
  value: string;
  label: string;
  /** A second, quieter line. */
  detail?: string;
}

interface ChoiceProps {
  value: string;
  options: ChoiceOption[];
  onChange: (value: string) => void;
  /** The control's name, for assistive technology. */
  label: string;
  /** Shown before the value on the trigger, where the trigger stands without a label of its own. */
  prefix?: string;
  className?: string;
  id?: string;
}

/** Renders a button naming the option chosen, which opens the list of them. */
export function Choice({ value, options, onChange, label, prefix, className, id }: ChoiceProps) {
  const current = options.find((option) => option.value === value);
  return (
    <Menu>
      <MenuTrigger render={
        <Button id={id} variant="outline" className={cn(triggerClass, className)} aria-label={label}>
          <span className="truncate">
            {prefix && <span className="text-muted-foreground">{prefix} </span>}
            {current?.label ?? value}
          </span>
          <ChevronDown className="size-4 text-muted-foreground" />
        </Button>
      } />
      <MenuPopup align="start" className="max-w-[min(28rem,calc(100vw-2rem))] min-w-(--anchor-width)">
        <MenuRadioGroup value={value} onValueChange={(next: string) => onChange(next)}>
          {options.map((option) => (
            <MenuRadioItem key={option.value} value={option.value}>
              <span className="flex min-w-0 flex-col py-0.5">
                <span className="truncate">{option.label}</span>
                {option.detail && <span className="truncate text-xs text-muted-foreground">{option.detail}</span>}
              </span>
            </MenuRadioItem>
          ))}
        </MenuRadioGroup>
      </MenuPopup>
    </Menu>
  );
}

interface ModelPickerProps {
  /** The model chosen; empty for the provider's own default where `defaultModel` names it. */
  value: string;
  onChange: (value: string) => void;
  /** The provider's listing (`useModelNames`). */
  names: NamesState & { refresh: () => void };
  /** The model the provider runs where none is named, offered as a choice of its own. */
  defaultModel?: string;
  className?: string;
  id?: string;
}

/** Renders a button naming the model, which opens a searchable list of the provider's models. */
export function ModelPicker({ value, onChange, names, defaultModel, className, id }: ModelPickerProps) {
  const [open, setOpen] = React.useState(false);
  const [query, setQuery] = React.useState("");
  const [highlight, setHighlight] = React.useState(0);
  const search = React.useRef<HTMLInputElement>(null);
  const listRef = React.useRef<HTMLDivElement>(null);

  const all = names.listing?.names ?? [];
  const needle = query.trim().toLowerCase();
  const matches = needle ? all.filter((name) => name.toLowerCase().includes(needle)) : all;
  const typed = query.trim();
  const rows: { value: string; label: string; detail?: string }[] = [];
  if (!needle && defaultModel) rows.push({ value: "", label: t("pelorus.providerDefault"), detail: defaultModel });
  for (const name of matches) rows.push({ value: name, label: name });
  if (typed && !all.includes(typed)) rows.push({ value: typed, label: t("pelorus.useTyped", { name: typed }) });

  const pick = (next: string) => {
    onChange(next);
    setOpen(false);
  };
  const shown = value || defaultModel || "";

  React.useEffect(() => {
    listRef.current?.querySelector(`[data-row="${highlight}"]`)?.scrollIntoView({ block: "nearest" });
  }, [highlight]);

  return (
    <Popover.Root open={open} onOpenChange={(next) => { setOpen(next); setHighlight(0); if (!next) setQuery(""); }}>
      <Popover.Trigger render={
        <Button id={id} variant="outline" className={cn(triggerClass, className)} aria-label={t("pelorus.model")}>
          <span className={cn("truncate", !shown && "text-muted-foreground")}>{shown || t("pelorus.pickModel")}</span>
          <ChevronDown className="size-4 text-muted-foreground" />
        </Button>
      } />
      <Popover.Portal>
        <Popover.Positioner className="z-50" align="start" sideOffset={4}>
          <Popover.Popup initialFocus={search}
            className="allow-native-input flex max-h-[min(28rem,var(--available-height))] w-[max(var(--anchor-width),20rem)] max-w-[calc(100vw-2rem)] flex-col overflow-hidden rounded-lg border bg-popover text-popover-foreground shadow-lg outline-none">
            <div className="flex items-center gap-1 border-b border-border p-1.5">
              <input ref={search} value={query} placeholder={t("pelorus.searchModels")} aria-label={t("pelorus.searchModels")}
                className="h-8 min-w-0 flex-1 rounded-md bg-transparent px-2 text-sm outline-none placeholder:text-muted-foreground"
                onChange={(event) => { setQuery(event.target.value); setHighlight(0); }}
                onKeyDown={(event) => {
                  if (event.key === "ArrowDown") { event.preventDefault(); setHighlight((h) => Math.min(rows.length - 1, h + 1)); }
                  else if (event.key === "ArrowUp") { event.preventDefault(); setHighlight((h) => Math.max(0, h - 1)); }
                  else if (event.key === "Enter") { event.preventDefault(); const row = rows[highlight]; if (row) pick(row.value); }
                }} />
              <Button type="button" variant="ghost" size="icon-sm" disabled={names.loading} onClick={names.refresh}
                title={t("pelorus.refreshModels")} aria-label={t("pelorus.refreshModels")}>
                <RefreshCw className={cn("size-4", names.loading && "animate-spin")} />
              </Button>
            </div>
            <div ref={listRef} className="min-h-0 flex-1 overflow-y-auto p-1" role="listbox" aria-label={t("pelorus.model")}>
              {rows.map((row, index) => (
                <button key={`${row.value}#${index}`} type="button" data-row={index} role="option" aria-selected={row.value === value}
                  className={cn("flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm", index === highlight && "bg-accent text-accent-foreground")}
                  onMouseMove={() => setHighlight(index)} onClick={() => pick(row.value)}>
                  <Check className={cn("size-4 shrink-0", row.value === value ? "opacity-100" : "opacity-0")} />
                  <span className="flex min-w-0 flex-col">
                    <span className="truncate">{row.label}</span>
                    {row.detail && <span className="truncate text-xs text-muted-foreground">{row.detail}</span>}
                  </span>
                </button>
              ))}
              {rows.length === 0 && (
                <div className="flex items-center justify-center gap-2 px-2 py-6 text-center text-sm text-muted-foreground">
                  {names.loading ? <><Loader2 className="size-4 animate-spin" />{t("pelorus.listingModels")}</> : t("pelorus.noModelsListed")}
                </div>
              )}
            </div>
            <div className="border-t border-border px-3 py-1.5 text-xs text-muted-foreground">
              {names.error && !names.loading
                ? <span className="text-destructive">{names.error}</span>
                : t("pelorus.modelsFound", { count: all.length })}
            </div>
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}
