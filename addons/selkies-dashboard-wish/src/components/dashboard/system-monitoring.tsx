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
import { t } from "@/i18n";

/**
 * The stats panel: radial gauges for CPU, GPU, memory, FPS, audio level,
 * bandwidth and latency, in a compact strip or a detailed view.
 *
 * Every figure is polled from the `window` state the streaming cores
 * publish (`system_stats`, `gpu_stats`, `network_stats`, `fps`,
 * `currentAudioLevel`, `currentAudioBufferSize`). Gauges scale to what the
 * session is configured for rather than arbitrary ceilings: the FPS gauge to
 * the configured framerate and the bandwidth gauge to the configured video
 * plus audio bitrate, an explicit client choice in localStorage winning over
 * the server's value.
 * @module
 */

/** The stats the streaming cores publish on `window`. */
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
		/**
		 * Set by the dashboard around a transport switch so the active core
		 * suppresses the expected "Server disconnected" alert from the old peer.
		 */
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

/** One gauge: a recharts radial bar with the value in its center. */
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
/** A dashboard-owned analyser on the stream's audio track. */
type AudioMeter = { ctx: AudioContext; analyser: AnalyserNode; data: Uint8Array<ArrayBuffer>; stream: MediaStream };
/**
 * Audio level (RMS, 0 to 1) of the WebRTC stream's audio track via a
 * dashboard-owned AnalyserNode, never routed to a destination so playback is
 * unaffected. The websockets worklet path exposes `window.currentAudioLevel`
 * instead.
 * @param meterRef Holds the analyser across calls; rebuilt when the stream changes.
 * @returns The level, or null when the stream has no audio track or no analyser could be built.
 */
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
const DEFAULT_VIDEO_BITRATE_KBPS = 8000;
const DEFAULT_AUDIO_BITRATE_BPS = 128000;

/**
 * The traffic the session is configured to use, video target plus audio, in
 * Mbps: at 8 Mbps configured, 8 Mbps of traffic is a full bandwidth gauge.
 * An explicit client choice in localStorage wins over the server's value;
 * `video_bitrate` is kbps on the wire and in storage.
 */
function configuredMaxBandwidthMbps(): number {
	const settings = getLastServerSettings();
	const storedVideo = parseFloat(localStorage.getItem(getPrefixedKey('video_bitrate')) ?? '');
	const serverVideo = parseFloat(settings?.video_bitrate?.value);
	const videoKbps = !isNaN(storedVideo) ? storedVideo
		: (!isNaN(serverVideo) ? serverVideo : DEFAULT_VIDEO_BITRATE_KBPS);
	const storedAudio = parseInt(localStorage.getItem(getPrefixedKey('audio_bitrate')) ?? '', 10);
	const serverAudio = parseInt(settings?.audio_bitrate?.value, 10);
	const audioBps = !isNaN(storedAudio) ? storedAudio
		: (!isNaN(serverAudio) ? serverAudio : DEFAULT_AUDIO_BITRATE_BPS);
	return Math.max(0.1, videoKbps / 1000 + audioBps / 1_000_000);
}

/**
 * The framerate the session is configured to push, the full reading of the
 * FPS gauge. An explicit client choice in localStorage wins over the server's
 * value.
 */
function configuredFramerateMax(): number {
	const settings = getLastServerSettings();
	const stored = parseFloat(localStorage.getItem(getPrefixedKey('framerate')) ?? '');
	const server = parseFloat(settings?.framerate?.value);
	const fps = !isNaN(stored) ? stored : (!isNaN(server) ? server : 60);
	return fps > 0 ? fps : 60;
}

/**
 * Renders the gauges, polling the core's `window` stats twice a second.
 *
 * The audio level is read on one scale for both transports: the websockets
 * worklet exports a final 0 to 100 level (RMS times 141, a full-scale sine
 * reading 100), and the WebRTC analyser fallback's raw RMS gets the same
 * mapping. Only metrics with data are shown; video bitrate is omitted since
 * it duplicates the bandwidth stat.
 */
export function SystemMonitoring() {
	const [isDetailedView, setIsDetailedView] = useState(false);
	const [clientFps, setClientFps] = useState(0);
	const [framerateMax, setFramerateMax] = useState(configuredFramerateMax);
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
			const coreLevel = (window as unknown as { currentAudioLevel?: number }).currentAudioLevel;
			const level = typeof coreLevel === "number"
				? coreLevel
				: (readStreamAudioLevel(audioMeterRef) ?? 0) * 141;
			setAudioLevel(Math.min(100, Math.round(level)));

			const netStats = window.network_stats;
			setBandwidthMbps(netStats?.bandwidth_mbps ?? 0);
			setMaxBandwidthMbps(configuredMaxBandwidthMbps());
			setLatencyMs(netStats?.latency_ms ?? 0);
			setFramerateMax(configuredFramerateMax());
		};
		const intervalId = setInterval(readStats, STATS_READ_INTERVAL_MS);
		return () => clearInterval(intervalId);
	}, []);

	const formatMemory = (bytes: number | null): string => {
		if (bytes === null) return t('sections.stats.tooltipMemoryNA');
		const gb = bytes / (1024 * 1024 * 1024);
		return gb >= 1 ? `${gb.toFixed(1)} GB` : `${(bytes / (1024 * 1024)).toFixed(0)} MB`;
	};

	/** Status label and colors for a reading; the audio level is an activity indicator, not a pressure gauge. */
	const getPerformanceStatus = (value: number, type: 'percentage' | 'fps' | 'latency' | 'audio' | 'bandwidth') => {
		switch (type) {
			case 'percentage':
				if (value <= 60) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value <= 80) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'high', color: 'text-red-500', bg: 'bg-red-500/10' };

			case 'fps':
				if (value >= 50) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value >= 30) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'low', color: 'text-red-500', bg: 'bg-red-500/10' };

			case 'latency':
				if (value <= 50) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value <= 100) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'high', color: 'text-red-500', bg: 'bg-red-500/10' };

			case 'audio':
				if (value >= 95) return { status: 'clipping', color: 'text-red-500', bg: 'bg-red-500/10' };
				return { status: 'ok', color: 'text-green-500', bg: 'bg-green-500/10' };

			case 'bandwidth':
				if (value >= 50) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value >= 25) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'low', color: 'text-red-500', bg: 'bg-red-500/10' };

			default:
				return { status: 'unknown', color: 'text-muted-foreground', bg: 'bg-muted/10' };
		}
	};

	const hasCpuData = true;
	const hasGpuData = window.gpu_stats?.gpu_percent !== undefined || window.gpu_stats?.utilization_gpu !== undefined || gpuPercent > 0;
	const hasSysMemData = window.system_stats?.mem_used !== undefined && window.system_stats?.mem_total !== undefined && sysMemUsed !== null && sysMemTotal !== null;
	const hasGpuMemData = window.gpu_stats?.mem_used !== undefined || window.gpu_stats?.memory_used !== undefined || window.gpu_stats?.used_gpu_memory_bytes !== undefined || gpuMemUsed !== null;
	const hasFpsData = true;
	const hasAudioData = true;
	const hasBandwidthData = true;
	const hasLatencyData = true;

	const allMetrics = [
		{
			name: t('sections.stats.cpuLabel'),
			current: Math.round(cpuPercent),
			max: 100,
			fill: "hsl(250, 100%, 60%)",
			hasData: hasCpuData
		},
		{
			name: t('sections.stats.gpuLabel'),
			current: Math.round(gpuPercent),
			max: 100,
			fill: "hsl(260, 100%, 50%)",
			hasData: hasGpuData
		},
		{
			name: t('sections.stats.sysMemLabel'),
			current: Math.round(sysMemPercent),
			max: 100,
			fill: "hsl(240, 100%, 60%)",
			hasData: hasSysMemData
		},
		{
			name: t('sections.stats.gpuMemLabel'),
			current: Math.round(gpuMemPercent),
			max: 100,
			fill: "hsl(240, 100%, 60%)",
			hasData: hasGpuMemData
		},
		{
			name: t('sections.stats.fpsLabel'),
			current: Math.round(clientFps),
			max: framerateMax,
			fill: "hsl(220, 100%, 50%)",
			hasData: hasFpsData
		},
		{
			name: t('sections.stats.audioLabel'),
			current: audioLevel,
			max: 100,
			fill: "hsl(230, 100%, 60%)",
			hasData: hasAudioData
		},
		{
			name: t('sections.stats.bandwidthLabel'),
			current: Math.round(bandwidthMbps * 100) / 100,
			max: maxBandwidthMbps,
			fill: "hsl(200, 100%, 60%)",
			hasData: hasBandwidthData
		},
		{
			name: t('sections.stats.latencyLabel'),
			current: Math.round(latencyMs * 10) / 10,
			max: MAX_LATENCY_MS,
			fill: "hsl(180, 100%, 60%)",
			hasData: hasLatencyData
		}
	];

	const metrics = allMetrics.filter(metric => metric.hasData);

	if (isDetailedView) {
		return (
			<div className="p-3 rounded-lg bg-card backdrop-blur-sm border shadow-sm w-auto cursor-grab hover:cursor-grab active:cursor-grabbing border bg-background/95 backdrop-blur-sm shadow-lg opacity-30 hover:opacity-100 transition-opacity duration-300">
				<div className="flex items-center justify-between mb-4">
					<h3 className="text-sm font-semibold text-card-foreground pointer-events-none">{t('stats.monitorTitle')}</h3>
					<div className="flex items-center gap-2 pointer-events-auto">
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
								<p>{t('stats.compactView')}</p>
							</TooltipContent>
						</Tooltip>
					</div>
				</div>

				<div className="space-y-2 pointer-events-none">
					{hasCpuData && (
						<div className="flex justify-between items-center py-1">
							<span className="text-sm text-muted-foreground">{t('sections.stats.cpuLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.gpuLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.sysMemLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.gpuMemLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.fpsLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.audioLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.bandwidthLabel')}</span>
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
							<span className="text-sm text-muted-foreground">{t('sections.stats.latencyLabel')}</span>
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
							<p>{t('stats.detailedView')}</p>
						</TooltipContent>
					</Tooltip>
				</div>
			</div>
		</div>
	);
}

export default SystemMonitoring;
