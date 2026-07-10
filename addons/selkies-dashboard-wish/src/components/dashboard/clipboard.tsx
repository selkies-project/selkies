/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect, useRef } from "react";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { computeRenderableSettings, getLastServerSettings, getPrefixedKey } from "@/utils";

export function Clipboard() {
	const [dashboardClipboardContent, setDashboardClipboardContent] = useState('');
	const [clipboardImageUrl, setClipboardImageUrl] = useState<string | null>(null);
	const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));
	const [enableBinaryClipboard, setEnableBinaryClipboard] = useState(() => {
		const saved = localStorage.getItem(getPrefixedKey("enable_binary_clipboard"));
		return saved !== null ? saved === 'true' : true;
	});
	const fileInputRef = useRef<HTMLInputElement>(null);

	const handleBinaryClipboardToggle = () => {
		const newState = !enableBinaryClipboard;
		setEnableBinaryClipboard(newState);
		// Core persists enable_binary_clipboard when handling the settings message.
		window.postMessage(
			{ type: 'settings', settings: { enable_binary_clipboard: newState } },
			window.location.origin
		);
	};

	// --- Message Listener for Clipboard and Server Settings Updates ---
	useEffect(() => {
		const handleWindowMessage = (event: MessageEvent) => {
			if (event.origin !== window.location.origin) return;
			const message = event.data;

			if (typeof message !== 'object' || message === null) return;

			if (message.type === 'clipboardContentUpdate') {
				if (typeof message.text === 'string') {
					setDashboardClipboardContent(message.text);
				}
			}

			if (message.type === 'serverSettings') {
				const payload = message.payload;
				setRenderableSettings(computeRenderableSettings(payload));
				const s = payload?.enable_binary_clipboard;
				if (s) {
					const saved = localStorage.getItem(getPrefixedKey('enable_binary_clipboard'));
					const final = s.locked ? s.value : (saved !== null ? saved === 'true' : s.value);
					setEnableBinaryClipboard(final);
				}
			}
		};

		window.addEventListener('message', handleWindowMessage);
		return () => window.removeEventListener('message', handleWindowMessage);
	}, []);

	const handleClipboardChange = (event: React.ChangeEvent<HTMLTextAreaElement>) => {
		setDashboardClipboardContent(event.target.value);
	};

	const handleClipboardBlur = (event: React.FocusEvent<HTMLTextAreaElement>) => {
		window.postMessage({ type: 'clipboardUpdateFromUI', text: event.target.value }, window.location.origin);
	};

	const handleImageUpload = (event: React.ChangeEvent<HTMLInputElement>) => {
		const file = event.target.files?.[0];
		if (file && file.type.startsWith('image/')) {
			const reader = new FileReader();
			reader.onload = (e) => {
				const result = e.target?.result as string;
				setClipboardImageUrl(result);

				// Convert to blob and send to clipboard
				fetch(result)
					.then(res => res.blob())
					.then(blob => {
						window.postMessage({
							type: 'clipboardImageUpdate',
							imageBlob: blob
						}, window.location.origin);
					})
					.catch(err => console.error('Error processing image:', err));
			};
			reader.readAsDataURL(file);
		}
	};

	const handleImageButtonClick = () => {
		fileInputRef.current?.click();
	};

	const handleClearImage = () => {
		setClipboardImageUrl(null);
		if (fileInputRef.current) {
			fileInputRef.current.value = '';
		}
	};

	return (
		<div className="w-[300px] p-4 flex flex-col gap-2">
			{(renderableSettings.binaryClipboard ?? true) && (
				<div className="flex items-center justify-between">
					<Label className="text-sm font-medium">Binary Clipboard</Label>
					<Switch
						checked={enableBinaryClipboard}
						onCheckedChange={handleBinaryClipboardToggle}
					/>
				</div>
			)}

			<Label htmlFor="dashboardClipboardTextarea">Clipboard</Label>
			<Textarea
				id="dashboardClipboardTextarea"
				value={dashboardClipboardContent}
				onChange={handleClipboardChange}
				onBlur={handleClipboardBlur}
				rows={5}
				placeholder="Enter text to copy to remote clipboard..."
				className="allow-native-input resize-none bg-background/95 overflow-y-auto max-h-[150px]"
			/>

			{/* Image Upload Section */}
			<div className="flex flex-col gap-2">
				<div className="flex gap-2">
					<Button
						variant="outline"
						size="sm"
						onClick={handleImageButtonClick}
						className="flex-1"
					>
						Upload Image
					</Button>
					{clipboardImageUrl && (
						<Button
							variant="outline"
							size="sm"
							onClick={handleClearImage}
							className="flex-1"
						>
							Clear Image
						</Button>
					)}
				</div>

				<input
					ref={fileInputRef}
					type="file"
					accept="image/*"
					onChange={handleImageUpload}
					className="hidden"
				/>

				{clipboardImageUrl && (
					<div className="mt-2">
						<img
							src={clipboardImageUrl}
							alt="Clipboard preview"
							className="max-w-full max-h-32 object-contain rounded border"
						/>
					</div>
				)}
			</div>
		</div>
	);
}
