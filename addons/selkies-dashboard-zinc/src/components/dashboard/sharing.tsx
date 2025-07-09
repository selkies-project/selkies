import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Card, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Info } from "lucide-react";

const sharingLinks = [
	{
		id: "shared",
		label: "Viewer",
		badge: "Read Only",
		hash: "#shared",
	},
	{
		id: "player2",
		label: "Controller 2",
		badge: "Gamepad 2",
		hash: "#player2",
	},
	{
		id: "player3",
		label: "Controller 3",
		badge: "Gamepad 3",
		hash: "#player3",
	},
	{
		id: "player4",
		label: "Controller 4",
		badge: "Gamepad 4",
		hash: "#player4",
	},
];

interface SharingProps {
	show: boolean;
	onClose: () => void;
}

export const Sharing = ({ show, onClose }: SharingProps) => {
	const [copiedId, setCopiedId] = useState<string | null>(null);
	const baseUrl =
		typeof window !== "undefined" ? window.location.href.split("#")[0] : "";

	const handleCopyLink = async (fullUrl: string, id: string) => {
		if (!navigator.clipboard) return;
		try {
			await navigator.clipboard.writeText(fullUrl);
			setCopiedId(id);
			setTimeout(() => setCopiedId(null), 2000);
		} catch (err) {
			// Optionally handle error
		}
	};

	if (!show) return null;

	return (
		<Card className="w-[320px] bg-background/95 backdrop-blur-sm border shadow-lg rounded-lg relative p-2">
			<div className="px-2 py-0 flex flex-col gap-2">
				<div className="flex items-center justify-between mb-1">
					<CardTitle className="text-xs font-bold">Share Session Links</CardTitle>
					<Tooltip>
						<TooltipTrigger asChild>
							<span className="inline-block cursor-help">
								<Info className="h-4 w-4 text-muted-foreground" />
							</span>
						</TooltipTrigger>
						<TooltipContent className="text-sm bg-primary text-primary-foreground">
							Share these links to allow others to join your session as a viewer or as a controller.<br />
							Each link grants access to a specific role.
						</TooltipContent>
					</Tooltip>
				</div>
				<div className="grid grid-cols-2 gap-3">
					{sharingLinks.map((link) => {
						const fullUrl = `${baseUrl}${link.hash}`;
						return (
							<div key={link.id} className="flex flex-col items-start justify-between bg-muted/60 rounded-md p-3 gap-2 shadow-sm">
								<span className="font-medium text-xs text-foreground text-left w-full">{link.label}</span>
								<Badge variant="green" className="text-[10px] px-2 py-0.5 mb-1">{link.badge}</Badge>
								<div className="flex w-full justify-end">
									<Button
										variant="outline"
										size="sm"
										className="px-2 py-0.5 h-6 text-xs font-medium"
										onClick={() => handleCopyLink(fullUrl, link.id)}
										aria-label={`Copy ${link.label} link`}
									>
										{copiedId === link.id ? (
											<span className="text-green-600">Copied!</span>
										) : (
											<span>Copy</span>
										)}
									</Button>
								</div>
							</div>
						);
					})}
				</div>
			</div>
		</Card>
	);
};
