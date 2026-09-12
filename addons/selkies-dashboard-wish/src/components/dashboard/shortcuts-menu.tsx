/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState } from "react";
import { Alert, AlertTitle, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { computeRenderableSettings, getLastServerSettings, getPrefixedKey } from "@/utils";
import { t } from "@/i18n";

/**
 * The keyboard-shortcuts card: the core-owned chords, the switch that hands
 * them to the session instead, and the citation notice.
 * @module
 */

const shortcuts = [
	{ label: t('sections.shortcuts.fullscreen'), combo: "Ctrl + Shift + F" },
	{ label: t('sections.shortcuts.gamingMode'), combo: "Ctrl + Shift + X" },
	{ label: t('sections.shortcuts.openMenu'), combo: "Ctrl + Shift + M" },
	{ label: t('sections.shortcuts.toggleGamepad'), combo: "Ctrl + Shift + G" },
	{ label: t('sections.shortcuts.pointerLock'), combo: "Ctrl + Shift + Left Click" },
];

/** Renders the switch and the shortcut list. */
export function ShortcutsMenu() {
	const renderableSettings: any = computeRenderableSettings(getLastServerSettings());
	const [enabled, setEnabled] = useState(() => {
		const saved = localStorage.getItem(getPrefixedKey("keyboard_shortcuts"));
		return saved !== null ? saved === 'true' : true;
	});
	const toggle = () => {
		const value = !enabled;
		setEnabled(value);
		localStorage.setItem(getPrefixedKey("keyboard_shortcuts"), String(value));
		window.postMessage({ type: 'settings', settings: { keyboard_shortcuts: value } },
			window.location.origin);
	};
	return (
		<Card className="w-[320px] bg-background/95 backdrop-blur-sm border shadow-sm">
			<CardContent className="p-4">
				{(renderableSettings.keyboardShortcuts ?? true) && (
					<div className="flex items-center justify-between mb-3">
						<Label className="text-sm font-medium" title={t('sections.shortcuts.enabledDetails')}>{t('sections.shortcuts.enabledLabel')}</Label>
						<Switch checked={enabled} onCheckedChange={toggle} />
					</div>
				)}
				<Alert className="mb-3">
					<AlertTitle>{t('sections.shortcuts.title')}</AlertTitle>
					<AlertDescription>
						<ul className="space-y-3">
							{shortcuts.map((s, i) => (
								<li key={i} className="flex flex-col items-center gap-1 text-center">
									<Badge variant="secondary" className="text-xs px-2 py-0.5 font-mono bg-primary/90 text-primary-foreground whitespace-normal h-auto overflow-visible break-words max-w-full text-center">
										{s.combo}
									</Badge>
									<span className="text-foreground text-sm">
										{s.label}
									</span>
								</li>
							))}
						</ul>
					</AlertDescription>
				</Alert>
				<small className="text-foreground">
					<ul className="list-disc pl-5 text-foreground">
						<li>
							<a
								className="text-violet-600 hover:text-violet-800 dark:text-violet-400 dark:hover:text-violet-300"
								target="_blank"
								rel="noopener noreferrer"
								href="https://docs.selkies.io/citation/"
							>
								<b>{t('shortcuts.citeNotice')}{" ↗"}</b>
							</a>
						</li>
					</ul>
				</small>
			</CardContent>
		</Card>
	);
};

export default ShortcutsMenu;