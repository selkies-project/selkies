/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The two appearance toggles every layout carries: the theme (dark or light,
 * kept per browser under the unprefixed `theme` key the served file index
 * mirrors) and the layout, which cycles the registry and stores the pick
 * under the prefixed `ui_layout` key. Icons name the state they show, as the
 * theme toggle does: the moon while dark, the layout's own mark while shown.
 * @module
 */
import { Moon, Sun } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useTheme } from "@/components/ui/theme-provider";
import { useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";

/** Flips between dark and light. */
export function ThemeToggle() {
  const { theme, setTheme } = useTheme();
  return (
    <Button variant="outline" size="icon-xs" className="theme-toggle" aria-label={t('toggleThemeTitle')} title={t('toggleThemeTitle')}
      onClick={() => setTheme(theme === "light" ? "dark" : "light")}>
      <Sun className="size-3.5 rotate-0 scale-100 transition-all dark:-rotate-90 dark:scale-0" />
      <Moon className="absolute size-3.5 rotate-90 scale-0 transition-all dark:rotate-0 dark:scale-100" />
    </Button>
  );
}

/** The theme as a pill with the moon and the sun sliding through it, the sidebar's own control. */
export function ThemePill() {
  const { theme, setTheme } = useTheme();
  return (
    <button type="button" className="theme-toggle theme-pill" aria-label={t('toggleThemeTitle')} title={t('toggleThemeTitle')}
      onClick={() => setTheme(theme === "light" ? "dark" : "light")}>
      <Moon className="moon" fill="currentColor" strokeWidth={1} />
      <Sun className="sun" strokeWidth={2.5} />
    </button>
  );
}

/** Steps to the next layout the server allows; hidden while there is only one. */
export function LayoutToggle({ className }: { className?: string }) {
  const { layout, layoutChoices, setLayout } = useDashboard();
  if (layoutChoices.length < 2) return null;
  const index = layoutChoices.findIndex((choice) => choice.id === layout.id);
  const next = layoutChoices[(index + 1) % layoutChoices.length];
  const label = `${t('layout.title')}: ${t(layout.labelKey)}`;
  const Icon = layout.icon;
  return (
    <Tooltip>
      <TooltipTrigger render={
        <Button variant="outline" size="icon-xs" className={`layout-toggle ${className ?? ""}`} aria-label={label} data-layout={layout.id}
          onClick={() => setLayout(next.id)} />
      }>
        <Icon className="size-3.5" />
      </TooltipTrigger>
      <TooltipContent>{`${label} → ${t(next.labelKey)}`}</TooltipContent>
    </Tooltip>
  );
}
