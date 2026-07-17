/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Alert, AlertTitle, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { t } from "@/i18n";

const shortcuts = [
	{ label: t('sections.shortcuts.fullscreen'), combo: "Ctrl + Shift + F" },
	{ label: t('sections.shortcuts.openMenu'), combo: "Ctrl + Shift + M" },
	{ label: t('sections.shortcuts.toggleGamepad'), combo: "Ctrl + Shift + G" },
	{ label: t('sections.shortcuts.pointerLock'), combo: "Ctrl + Shift + Left Click" },
];

export function ShortcutsMenu() {
	return (
		<Card className="w-[320px] bg-background/95 backdrop-blur-sm border shadow-sm">
			<CardContent className="p-4">
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
								className="text-blue-600 hover:text-blue-800 dark:text-blue-400 dark:hover:text-blue-300"
								target="_blank"
								rel="noopener noreferrer"
								href="https://github.com/selkies-project/selkies/blob/main/docs/README.md#citations-in-academic-publications"
							>
								<b>{t('shortcuts.citeNotice')}</b>
							</a>
						</li>
					</ul>
				</small>
			</CardContent>
		</Card>
	);
};

export default ShortcutsMenu;