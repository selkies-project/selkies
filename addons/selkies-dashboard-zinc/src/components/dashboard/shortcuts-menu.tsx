import { DropdownMenu, DropdownMenuTrigger, DropdownMenuContent } from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { ChevronLeft } from "lucide-react";
import { Alert, AlertTitle, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";

const shortcuts = [
	{ label: "Fullscreen", combo: "Ctrl + Shift + F" },
	{ label: "(Game) Cursor Lock", combo: "Ctrl + Shift + LeftClick" },
	{ label: "Open Side Menu", combo: "Ctrl + Shift + M" },
	{ label: "Toggle Gamepad", combo: "Ctrl + Shift + G" },
	{ label: "Toggle Settings Checkbox", combo: "Ctrl + Shift + X" },
];

const ShortcutsMenu = () => {
	return (
		<DropdownMenu>
			<DropdownMenuTrigger asChild>
				<Button variant="ghost" className="flex w-full items-center px-3 py-1.5 text-sm hover:bg-accent group">
					<ChevronLeft className="h-4 w-4 mr-2 flex-shrink-0" />
					<span className="text-left break-words whitespace-normal flex-1">Shortcuts</span>
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent side="left" align="start" className="w-auto p-4 bg-background/95 backdrop-blur-sm border shadow-sm">
				<Alert className="mb-3">
					<AlertTitle>Keyboard Shortcuts</AlertTitle>
					<AlertDescription>
						<ul className="space-y-2">
							{shortcuts.map((s, i) => (
								<li key={i} className="flex items-center gap-2">
									<Badge variant="blue" className="text-xs px-2 py-0.5 font-mono bg-blue-600/90 text-white">
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
			</DropdownMenuContent>
		</DropdownMenu>
	);
};

export default ShortcutsMenu;