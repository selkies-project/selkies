/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Alert, AlertTitle, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";

const shortcuts = [
	{ label: "Fullscreen", combo: "Ctrl + Shift + F" },
	{ label: "(Game) Cursor Lock", combo: "Ctrl + Shift + LeftClick" },
	{ label: "Open Side Menu", combo: "Ctrl + Shift + M" },
	{ label: "Toggle Gamepad", combo: "Ctrl + Shift + G" },
	{ label: "Toggle Settings Checkbox", combo: "Ctrl + Shift + X" },
];

export function ShortcutsMenu() {
	return (
		<Card className="w-[320px] bg-background/95 backdrop-blur-sm border shadow-sm">
			<CardContent className="p-4">
				<Alert className="mb-3">
					<AlertTitle>Keyboard Shortcuts</AlertTitle>
					<AlertDescription>
						<ul className="space-y-2">
							{shortcuts.map((s, i) => (
								<li key={i} className="flex items-center gap-2">
									<Badge variant="secondary" className="text-xs px-2 py-0.5 font-mono bg-primary/90 text-primary-foreground">
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
								<b>Please cite within your publication for academic usage</b>
							</a>
						</li>
					</ul>
				</small>
			</CardContent>
		</Card>
	);
};

export default ShortcutsMenu;