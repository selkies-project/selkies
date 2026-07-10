/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { PolarAngleAxis, RadialBar, RadialBarChart } from "recharts";
import { useEffect, useRef, useState } from "react";
import {
	ChevronDown,
	ChevronUp
} from "lucide-react";
import { getLastServerSettings, getPrefixedKey } from "@/utils";

// Declare global window properties
declare global {
	interface Window {
		system_stats?: {
			cpu_percent?: number;
			mem_used?: number;
			mem_total?: number;
		};
		gpu_stats?: {
			gpu_percent?: number;
			utilization_gpu?: number;
			mem_used?: number;
			memory_used?: number;
			used_gpu_memory_bytes?: number;
			mem_total?: number;
			memory_total?: number;
			total_gpu_memory_bytes?: number;
		};
		fps?: number;
		currentAudioBufferSize?: number;
		network_stats?: {
			bandwidth_mbps?: number;
			latency_ms?: number;
		};
		// Set by the dashboard around a transport switch so the active core
		// suppresses the expected "Server disconnected" alert from the old peer.
		__selkiesModeSwitching?: boolean;
	}
}

interface RadialGaugeProps {
	metric: {
		name: string;
		current: number;
		max: number;
		fill: string;
	};
	size: number;
}

function RadialGauge({ metric, size }: RadialGaugeProps) {
	const percentage = (metric.current / metric.max) * 100;
	const scaleFactor = size / 100;

	return (
		<div
			className="flex flex-col items-center"
			style={{
				width: size * 0.6,
				height: size * 0.7,
			}}
		>
			<div style={{ width: size * 0.8, height: size * 0.7 }}>
				<RadialBarChart
					width={size * 0.8}
					height={size * 0.7}
					cx={(size * 0.8) / 2}
					cy={(size * 0.7) / 2}
					innerRadius={20 * scaleFactor}
					outerRadius={30 * scaleFactor}
					barSize={4 * scaleFactor}
					data={[{ ...metric, percentage }]}
					startAngle={180}
					endAngle={0}
				>
					<PolarAngleAxis
						type="number"
						domain={[0, 100]}
						angleAxisId={0}
						tick={false}
					/>
					<RadialBar
						background
						dataKey="percentage"
						cornerRadius={5 * scaleFactor}
						fill={metric.fill}
						className="stroke-transparent stroke-2"
					/>
					<text
						x={(size * 0.8) / 2}
						y={(size * 0.7) / 2}
						textAnchor="middle"
						dominantBaseline="middle"
						className="fill-foreground font-bold"
						style={{ fontSize: `${0.9 * scaleFactor}rem` }}
					>
						{metric.current}
					</text>
					<text
						x={(size * 0.8) / 2}
						y={(size * 0.7) / 2 + 18 * scaleFactor}
						textAnchor="middle"
						dominantBaseline="middle"
						className="fill-muted-foreground font-medium"
						style={{ fontSize: `${0.65 * scaleFactor}rem` }}
					>
						{metric.name}
					</text>
				</RadialBarChart>
			</div>
		</div>
	);
}

const STATS_READ_INTERVAL_MS = 500;
// Audio level (RMS, 0..1) for the WebRTC stream's audio track via a dashboard-owned
// AnalyserNode (never routed to a destination, so playback is unaffected). The
// websockets worklet path exposes window.currentAudioLevel instead.
type AudioMeter = { ctx: AudioContext; analyser: AnalyserNode; data: Uint8Array<ArrayBuffer>; stream: MediaStream };
function readStreamAudioLevel(meterRef: { current: AudioMeter | null }): number | null {
	const el = document.getElementById("stream") as HTMLVideoElement | null;
	const ms = el && (el.srcObject as MediaStream | null);
	if (!ms || typeof ms.getAudioTracks !== "function" || ms.getAudioTracks().length === 0) {
		return null;
	}
	let m = meterRef.current;
	if (!m || m.stream !== ms) {
		try {
			if (m && m.ctx) m.ctx.close();
			const ctx = new AudioContext();
			const analyser = ctx.createAnalyser();
			analyser.fftSize = 512;
			ctx.createMediaStreamSource(ms).connect(analyser);
			m = { ctx, analyser, data: new Uint8Array(analyser.fftSize), stream: ms };
			meterRef.current = m;
		} catch {
			return null;
		}
	}
	m.analyser.getByteTimeDomainData(m.data);
	let sum = 0;
	for (let i = 0; i < m.data.length; i++) {
		const v = (m.data[i] - 128) / 128;
		sum += v * v;
	}
	return Math.sqrt(sum / m.data.length);
}

const MAX_LATENCY_MS = 1000;
const DEFAULT_VIDEO_BITRATE_MBPS = 8;
const DEFAULT_AUDIO_BITRATE_BPS = 128000;

// The bandwidth gauge reads full at the traffic the session is CONFIGURED to
// use (video target + audio), not an arbitrary link speed — at 8 Mbps
// configured, 8 Mbps of traffic is a full circle. Explicit client choice
// (localStorage) wins over the server's configured value.
function configuredMaxBandwidthMbps(): number {
	const settings = getLastServerSettings();
	const storedVideo = parseFloat(localStorage.getItem(getPrefixedKey('video_bitrate')) ?? '');
	const serverVideo = parseFloat(settings?.video_bitrate?.value);
	const videoMbps = !isNaN(storedVideo) ? storedVideo
		: (!isNaN(serverVideo) ? serverVideo : DEFAULT_VIDEO_BITRATE_MBPS);
	const storedAudio = parseInt(localStorage.getItem(getPrefixedKey('audio_bitrate')) ?? '', 10);
	const serverAudio = parseInt(settings?.audio_bitrate?.value, 10);
	const audioBps = !isNaN(storedAudio) ? storedAudio
		: (!isNaN(serverAudio) ? serverAudio : DEFAULT_AUDIO_BITRATE_BPS);
	return Math.max(0.1, videoMbps + audioBps / 1_000_000);
}

export function SystemMonitoring() {
	const [isDetailedView, setIsDetailedView] = useState(false);
	const [clientFps, setClientFps] = useState(0);
	const [audioLevel, setAudioLevel] = useState(0);
	const audioMeterRef = useRef<AudioMeter | null>(null);
	const [cpuPercent, setCpuPercent] = useState(0);
	const [gpuPercent, setGpuPercent] = useState(0);
	const [sysMemPercent, setSysMemPercent] = useState(0);
	const [gpuMemPercent, setGpuMemPercent] = useState(0);
	const [sysMemUsed, setSysMemUsed] = useState<number | null>(null);
	const [sysMemTotal, setSysMemTotal] = useState<number | null>(null);
	const [gpuMemUsed, setGpuMemUsed] = useState<number | null>(null);
	const [gpuMemTotal, setGpuMemTotal] = useState<number | null>(null);
	const [bandwidthMbps, setBandwidthMbps] = useState(0);
	const [maxBandwidthMbps, setMaxBandwidthMbps] = useState(configuredMaxBandwidthMbps);
	const [latencyMs, setLatencyMs] = useState(0);
	const [isWebrtc, setIsWebrtc] = useState(() =>
		localStorage.getItem(getPrefixedKey('stream_mode')) === 'webrtc'
	);

	// Track live streaming-mode switches (the loader reloads shortly after,
	// but reflect the change immediately).
	useEffect(() => {
		const handleMessage = (event: MessageEvent) => {
			if (event.origin !== window.location.origin) return;
			if (event.data?.type === 'mode') {
				setIsWebrtc(event.data.mode === 'webrtc');
			}
		};
		window.addEventListener('message', handleMessage);
		return () => window.removeEventListener('message', handleMessage);
	}, []);

	// Read stats periodically
	useEffect(() => {
		const readStats = () => {
			const currentSystemStats = window.system_stats;
			const sysMemUsed = currentSystemStats?.mem_used ?? null;
			const sysMemTotal = currentSystemStats?.mem_total ?? null;
			setCpuPercent(currentSystemStats?.cpu_percent ?? 0);
			setSysMemUsed(sysMemUsed);
			setSysMemTotal(sysMemTotal);
			setSysMemPercent((sysMemUsed !== null && sysMemTotal !== null && sysMemTotal > 0) ? (sysMemUsed / sysMemTotal) * 100 : 0);

			const currentGpuStats = window.gpu_stats;
			const gpuPercent = currentGpuStats?.gpu_percent ?? currentGpuStats?.utilization_gpu ?? 0;
			setGpuPercent(gpuPercent);
			const gpuMemUsed = currentGpuStats?.mem_used ?? currentGpuStats?.memory_used ?? currentGpuStats?.used_gpu_memory_bytes ?? null;
			const gpuMemTotal = currentGpuStats?.mem_total ?? currentGpuStats?.memory_total ?? currentGpuStats?.total_gpu_memory_bytes ?? null;
			setGpuMemUsed(gpuMemUsed);
			setGpuMemTotal(gpuMemTotal);
			setGpuMemPercent((gpuMemUsed !== null && gpuMemTotal !== null && gpuMemTotal > 0) ? (gpuMemUsed / gpuMemTotal) * 100 : 0);

			setClientFps(window.fps ?? 0);
			// The websockets worklet exports a FINAL 0-100 level (RMS ×141, full-scale
			// sine = 100); the analyser fallback (WebRTC) returns raw RMS 0..1 — apply
			// the same ×141 mapping so both transports read on one scale.
			const coreLevel = (window as unknown as { currentAudioLevel?: number }).currentAudioLevel;
			const level = typeof coreLevel === "number"
				? coreLevel
				: (readStreamAudioLevel(audioMeterRef) ?? 0) * 141;
			setAudioLevel(Math.min(100, Math.round(level)));

			const netStats = window.network_stats;
			setBandwidthMbps(netStats?.bandwidth_mbps ?? 0);
			setMaxBandwidthMbps(configuredMaxBandwidthMbps());
			setLatencyMs(netStats?.latency_ms ?? 0);
		};
		const intervalId = setInterval(readStats, STATS_READ_INTERVAL_MS);
		return () => clearInterval(intervalId);
	}, []);

	const formatMemory = (bytes: number | null): string => {
		if (bytes === null) return "N/A";
		const gb = bytes / (1024 * 1024 * 1024);
		return gb >= 1 ? `${gb.toFixed(1)}GB` : `${(bytes / (1024 * 1024)).toFixed(0)}MB`;
	};

	// Performance status helper functions
	const getPerformanceStatus = (value: number, type: 'percentage' | 'fps' | 'latency' | 'audio' | 'bandwidth') => {
		switch (type) {
			case 'percentage': // For CPU, GPU, Memory usage
				if (value <= 60) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value <= 80) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'high', color: 'text-red-500', bg: 'bg-red-500/10' };

			case 'fps': // For frame rate
				if (value >= 50) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value >= 30) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'low', color: 'text-red-500', bg: 'bg-red-500/10' };

			case 'latency': // For network latency (ms)
				if (value <= 50) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value <= 100) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'high', color: 'text-red-500', bg: 'bg-red-500/10' };

			case 'audio': // Output level (0-100%): activity indicator, not a pressure gauge
				if (value >= 95) return { status: 'clipping', color: 'text-red-500', bg: 'bg-red-500/10' };
				return { status: 'ok', color: 'text-green-500', bg: 'bg-green-500/10' };

			case 'bandwidth': // For bandwidth (Mbps)
				if (value >= 50) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value >= 25) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'low', color: 'text-red-500', bg: 'bg-red-500/10' };

			default:
				return { status: 'unknown', color: 'text-muted-foreground', bg: 'bg-muted/10' };
		}
	};

	// Check which metrics have data available (same logic as detailed view)
	const hasCpuData = true;
	const hasGpuData = window.gpu_stats?.gpu_percent !== undefined || window.gpu_stats?.utilization_gpu !== undefined || gpuPercent > 0;
	const hasSysMemData = window.system_stats?.mem_used !== undefined && window.system_stats?.mem_total !== undefined && sysMemUsed !== null && sysMemTotal !== null;
	const hasGpuMemData = window.gpu_stats?.mem_used !== undefined || window.gpu_stats?.memory_used !== undefined || window.gpu_stats?.used_gpu_memory_bytes !== undefined || gpuMemUsed !== null;
	const hasFpsData = true;
	// Audio buffer: the websockets worklet frame count, or in WebRTC a proxy from the
	// RTCInboundRtpStreamStats de-jitter depth. Video bitrate is omitted — it duplicates
	// the Bandwidth stat.
	const hasAudioData = true;
	const hasBandwidthData = true;
	const hasLatencyData = true;

	// Create metrics array for recharts - only include metrics that have data
	const allMetrics = [
		{
			name: "CPU",
			current: Math.round(cpuPercent),
			max: 100,
			fill: "hsl(250, 100%, 60%)",
			hasData: hasCpuData
		},
		{
			name: "GPU",
			current: Math.round(gpuPercent),
			max: 100,
			fill: "hsl(260, 100%, 50%)",
			hasData: hasGpuData
		},
		{
			name: "Sys Mem",
			current: Math.round(sysMemPercent),
			max: 100,
			fill: "hsl(240, 100%, 60%)",
			hasData: hasSysMemData
		},
		{
			name: "GPU Mem",
			current: Math.round(gpuMemPercent),
			max: 100,
			fill: "hsl(240, 100%, 60%)",
			hasData: hasGpuMemData
		},
		{
			name: "FPS",
			current: Math.round(clientFps),
			max: 60,
			fill: "hsl(220, 100%, 50%)",
			hasData: hasFpsData
		},
		{
			name: "Audio",
			current: audioLevel,
			max: 100,
			fill: "hsl(230, 100%, 60%)",
			hasData: hasAudioData
		},
		{
			name: "Bandwidth",
			current: Math.round(bandwidthMbps * 100) / 100,
			max: maxBandwidthMbps,
			fill: "hsl(200, 100%, 60%)",
			hasData: hasBandwidthData
		},
		{
			name: "Latency",
			current: Math.round(latencyMs * 10) / 10,
			max: MAX_LATENCY_MS,
			fill: "hsl(180, 100%, 60%)",
			hasData: hasLatencyData
		}
	];

	// Filter to only show metrics that have data available
	const metrics = allMetrics.filter(metric => metric.hasData);

	// Detailed view as separate draggable panel
	if (isDetailedView) {
		return (
			<div className="p-3 rounded-lg bg-card backdrop-blur-sm border shadow-sm w-auto cursor-grab hover:cursor-grab active:cursor-grabbing border bg-background/95 backdrop-blur-sm shadow-lg opacity-30 hover:opacity-100 transition-opacity duration-300">
				<div className="flex items-center justify-between mb-4">
					<h3 className="text-sm font-semibold text-card-foreground pointer-events-none">System Performance Monitor</h3>
					<div className="flex items-center gap-2 pointer-events-auto">
						{/* Toggle View Button */}
						<Tooltip>
							<TooltipTrigger asChild>
								<Button
									variant="outline"
									size="sm"
									className="h-8 w-8 p-0 pointer-events-auto"
									onClick={() => setIsDetailedView(false)}
								>
									<ChevronUp className="h-3 w-3" />
								</Button>
							</TooltipTrigger>
							<TooltipContent side="bottom">
								<p>Compact View</p>
							</TooltipContent>
						</Tooltip>
					</div>
				</div>

				<div className="space-y-2 pointer-events-none">
					{hasCpuData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">CPU</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{Math.round(cpuPercent)}%</span>
								{(() => {
									const status = getPerformanceStatus(cpuPercent, 'percentage');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasGpuData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">GPU</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{Math.round(gpuPercent)}%</span>
								{(() => {
									const status = getPerformanceStatus(gpuPercent, 'percentage');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasSysMemData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">System Memory</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{Math.round(sysMemPercent)}% ({formatMemory(sysMemUsed)}/{formatMemory(sysMemTotal)})</span>
								{(() => {
									const status = getPerformanceStatus(sysMemPercent, 'percentage');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasGpuMemData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">GPU Memory</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{Math.round(gpuMemPercent)}% ({formatMemory(gpuMemUsed)}/{formatMemory(gpuMemTotal)})</span>
								{(() => {
									const status = getPerformanceStatus(gpuMemPercent, 'percentage');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasFpsData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">FPS</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{Math.round(clientFps)}</span>
								{(() => {
									const status = getPerformanceStatus(clientFps, 'fps');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasAudioData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">Audio level</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{audioLevel}%</span>
								{(() => {
									const status = getPerformanceStatus(audioLevel, 'audio');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasBandwidthData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">Bandwidth</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{(Math.round(bandwidthMbps * 100) / 100)} Mbps</span>
								{(() => {
									const status = getPerformanceStatus(bandwidthMbps, 'bandwidth');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}

					{hasLatencyData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">Latency</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{(Math.round(latencyMs * 10) / 10)} ms</span>
								{(() => {
									const status = getPerformanceStatus(latencyMs, 'latency');
									return (
										<div className={`w-2 h-2 rounded-full ${status.color.replace('text-', 'bg-')}`} />
									);
								})()}
							</div>
						</div>
					)}
				</div>
			</div>
		);
	}

	// Compact view with recharts
	return (
		<div className="w-full bg-card backdrop-blur-sm border shadow-sm rounded-lg px-2 py-1 cursor-grab hover:cursor-grab active:cursor-grabbing">
			<div className="flex items-center justify-between">
				<div className="grid grid-flow-col auto-cols-max gap-2 pointer-events-none">
					{metrics.map((metric) => (
						<RadialGauge
							key={metric.name}
							metric={metric}
							size={80}
						/>
					))}
				</div>
				<div className="flex items-center gap-1 ml-2 pointer-events-auto">
					{/* Toggle to Detailed View Button */}
					<Tooltip>
						<TooltipTrigger asChild>
							<Button
								variant="ghost"
								size="sm"
								className="h-8 w-6 p-0 min-w-0 pointer-events-auto"
								onClick={() => setIsDetailedView(true)}
							>
								<ChevronDown className="h-3 w-3" />
							</Button>
						</TooltipTrigger>
						<TooltipContent side="bottom">
							<p>Detailed View</p>
						</TooltipContent>
					</Tooltip>
				</div>
			</div>
		</div>
	);
}

export default SystemMonitoring;
