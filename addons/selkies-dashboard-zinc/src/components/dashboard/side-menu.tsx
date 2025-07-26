import * as React from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { ScrollArea } from "@/components/ui/scroll-area";
import { ModeToggle } from "@/components/ui/ModeToggle";
import {
	Volume2,
	Gamepad2,
	Monitor,
	Maximize,
	Mic,
} from "lucide-react";
import { Tooltip, TooltipTrigger, TooltipContent } from "@/components/ui/tooltip";
import { TooltipProvider } from "@/components/ui/tooltip";

import { Clipboard } from "@/components/dashboard/clipboard";
import { Files } from "@/components/dashboard/files";
import { Apps } from "@/components/dashboard/apps";
import { StatsTopBar } from "@/components/dashboard/statistics";
import { useEffect } from "react";
import ShortcutsMenu from "@/components/dashboard/shortcuts-menu";
import { Gamepad } from "@/components/dashboard/gamepad";

const menuItems = [
	"Clipboard",
	"Settings",
	"Statistics",
	"Files",
	"Apps",
];

export function MenuComponent({ isGamepadEnabled, onGamepadToggle }) {
	const [isOpen, setIsOpen] = React.useState(false);
	const [showStats, setShowStats] = React.useState(false);
	const [isVideoActive, setIsVideoActive] = React.useState(true);
	const [isAudioActive, setIsAudioActive] = React.useState(true);
	const [isMicrophoneActive, setIsMicrophoneActive] = React.useState(false);

	// Add message event listener for status updates
	React.useEffect(() => {
		const handleWindowMessage = (event: MessageEvent) => {
			if (event.origin !== window.location.origin) return;
			const message = event.data;
			if (typeof message === 'object' && message !== null) {
				if (message.type === 'pipelineStatusUpdate') {
					if (message.video !== undefined) setIsVideoActive(message.video);
					if (message.audio !== undefined) setIsAudioActive(message.audio);
					if (message.microphone !== undefined) setIsMicrophoneActive(message.microphone);
				} else if (message.type === 'sidebarButtonStatusUpdate') {
					if (message.video !== undefined) setIsVideoActive(message.video);
					if (message.audio !== undefined) setIsAudioActive(message.audio);
					if (message.microphone !== undefined) setIsMicrophoneActive(message.microphone);
					if (message.gamepad !== undefined) onGamepadToggle(message.gamepad);
				}
			}
		};

		window.addEventListener('message', handleWindowMessage);
		return () => window.removeEventListener('message', handleWindowMessage);
	}, [onGamepadToggle]);

	// Add handlers for button clicks
	const handleVideoToggle = () => {
		window.postMessage({ type: 'pipelineControl', pipeline: 'video', enabled: !isVideoActive }, window.location.origin);
		setIsVideoActive(!isVideoActive);
	};

	const handleAudioToggle = () => {
		window.postMessage({ type: 'pipelineControl', pipeline: 'audio', enabled: !isAudioActive }, window.location.origin);
		setIsAudioActive(!isAudioActive);
	};

	const handleMicrophoneToggle = () => {
		window.postMessage({ type: 'pipelineControl', pipeline: 'microphone', enabled: !isMicrophoneActive }, window.location.origin);
		setIsMicrophoneActive(!isMicrophoneActive);
	};

	const handleGamepadToggle = () => {
		window.postMessage({ type: 'gamepadControl', enabled: !isGamepadEnabled }, window.location.origin);
		onGamepadToggle(!isGamepadEnabled);
	};

	useEffect(() => {
		const handleKeyDown = (event: KeyboardEvent) => {
			if (event.ctrlKey && event.shiftKey && event.key === "M") {
				event.preventDefault();
				setIsOpen((prev) => !prev);
			}

			if (event.ctrlKey && event.shiftKey && event.key === "G") {
				event.preventDefault();
				handleGamepadToggle();
			}

			if (event.ctrlKey && event.shiftKey && event.key === "F") {
				event.preventDefault();
				if (!document.fullscreenElement) {
					document.documentElement.requestFullscreen();
				}
			}

			if (event.ctrlKey && event.shiftKey && event.key === "X") {
				event.preventDefault();
				setShowStats((prev) => !prev);
			}

			let escapeTimer: NodeJS.Timeout;
			if (event.key === "Escape") {
				escapeTimer = setTimeout(() => {
					if (document.fullscreenElement) {
						document.documentElement.requestFullscreen();
					}
				}, 500);
			}

			return () => {
				if (escapeTimer) {
					clearTimeout(escapeTimer);
				}
			};
		};

		window.addEventListener("keydown", handleKeyDown);
		return () => window.removeEventListener("keydown", handleKeyDown);
	}, [handleGamepadToggle]);

	// 1. Header Related Functions
	const renderHeaderComponents = () => {
		const renderMenuHeader = () => (
			<div className="flex items-center justify-between border-b p-2 bg-background/95">
				<div className="flex gap-1">
					<Tooltip>
						<TooltipTrigger asChild>
							<Button 
								variant={isVideoActive ? "default" : "ghost"}
								size="icon"
								onClick={handleVideoToggle}
							>
								<Monitor className="h-4 w-4" />
							</Button>
						</TooltipTrigger>
						<TooltipContent>
							{isVideoActive ? 'Disable Video Stream' : 'Enable Video Stream'}
						</TooltipContent>
					</Tooltip>
					<Tooltip>
						<TooltipTrigger asChild>
							<Button 
								variant={isAudioActive ? "default" : "ghost"}
								size="icon"
								onClick={handleAudioToggle}
							>
								<Volume2 className="h-4 w-4" />
							</Button>
						</TooltipTrigger>
						<TooltipContent>
							{isAudioActive ? 'Disable Audio Stream' : 'Enable Audio Stream'}
						</TooltipContent>
					</Tooltip>
					<Tooltip>
						<TooltipTrigger asChild>
							<Button 
								variant={isMicrophoneActive ? "default" : "ghost"}
								size="icon"
								onClick={handleMicrophoneToggle}
							>
								<Mic className="h-4 w-4" />
							</Button>
						</TooltipTrigger>
						<TooltipContent>
							{isMicrophoneActive ? 'Disable Microphone' : 'Enable Microphone'}
						</TooltipContent>
					</Tooltip>
					<Tooltip>
						<TooltipTrigger asChild>
							<Button 
								variant={isGamepadEnabled ? "default" : "ghost"}
								size="icon"								
								onClick={handleGamepadToggle}
							>
								<Gamepad2 className="h-4 w-4" />
							</Button>
						</TooltipTrigger>
						<TooltipContent>
							{isGamepadEnabled ? 'Disable Gamepad Input' : 'Enable Gamepad Input'}
						</TooltipContent>
					</Tooltip>
				</div>
				<Button
					variant="ghost"
					size="icon"
					onClick={() => {
						if (document.fullscreenElement) {
							document.exitFullscreen();
						} else {
							document.documentElement.requestFullscreen();
						}
						setIsOpen(false);
					}}
				>
					<Maximize className="h-4 w-4" />
				</Button>
			</div>
		);

		return { menuHeader: renderMenuHeader() };
	};

	// 2. Menu Related Functions
	const renderMenuComponents = () => {
		const renderStatisticsMenuItem = () => {
			const renderStatsBar = () => (
				showStats && <StatsTopBar toggleStats={() => setShowStats(false)} />
			);

			const renderCheckbox = () => (
				<div className="flex items-center px-3 py-1.5">
					<Checkbox
						checked={showStats}
						onCheckedChange={(checked) => setShowStats(checked as boolean)}
						className="border-2 border-primary"
					/>
					<span className="ml-4">Settings</span>
				</div>
			);

			return {
				checkbox: renderCheckbox(),
				statsBar: renderStatsBar()
			};
		};

		const renderMenuItem = (item: string) => {
			switch (item) {
				case "Clipboard": return <Clipboard key={item} />;
				case "Statistics": return renderStatisticsMenuItem().checkbox;
				case "Files": return <Files key={item} />;
				case "Apps": return <Apps key={item} />;
				default: return null;
			}
		};

		const renderMenuContent = () => (
			<ScrollArea className="flex-grow py-2">
				{menuItems.map((item) => (
					<React.Fragment key={item}>
						{renderMenuItem(item)}
					</React.Fragment>
				))}
				<ShortcutsMenu />
			</ScrollArea>
		);

		return {
			content: renderMenuContent(),
			statsBar: renderStatisticsMenuItem().statsBar
		};
	};

	// 3. Footer Related Functions
	const renderFooterComponents = () => {
		const renderMenuFooter = () => (
			<div className="border-t p-2">
				<div className="flex items-center justify-between">
					{/* 
					<a href="https://github.com/selkies-project/selkies" target="_blank" rel="noopener noreferrer">
						<img
							src="/horizontal.png"
							alt="footer Logo"
							className="h-7 object-contain"
						/>
					</a>
					*/}
					<span className="text-lg font-bold">JeevOps</span>
					<ModeToggle />
				</div>
			</div>
		);

		return { footer: renderMenuFooter() };
	};

	// 4. Utility Functions
	const renderMenuWrapper = () => {
		const renderMenuButton = () => (
			<Button
				variant="default"
				className="absolute -left-5 top-1/2 -translate-y-1/2 rounded-full w-14 h-14"
				onClick={() => setIsOpen(!isOpen)}
			>
			</Button>
		);

		return (
			<div className="fixed right-0 top-1/2 -translate-y-1/2 z-50">
				<div className={`transition-transform duration-300 ${
					isOpen ? "translate-x-0" : "translate-x-[calc(100%+6px)]"
				}`}>
					{renderMenuButton()}
					<div className="min-w-[200px] max-w-[300px] border bg-background/95 backdrop-blur-sm rounded-l-lg shadow-lg flex flex-col text-foreground">
						{renderHeaderComponents().menuHeader}
						{renderMenuComponents().content}
						{renderFooterComponents().footer}
					</div>
				</div>
			</div>
		);
	};

	// Main Render
	return (
		<TooltipProvider>
			<div className="h-screen w-screen">
				{renderMenuComponents().statsBar}
				{isGamepadEnabled && (
					<Gamepad isGamepadEnabled={isGamepadEnabled} onGamepadToggle={onGamepadToggle} />
				)}
				{renderMenuWrapper()}
			</div>
		</TooltipProvider>
	);
}