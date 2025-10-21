import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { PolarAngleAxis, RadialBar, RadialBarChart } from "recharts";
import { useEffect, useState } from "react";
import {
	ChevronDown,
	ChevronUp
} from "lucide-react";

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

const STATS_READ_INTERVAL_MS = 100;
const MAX_AUDIO_BUFFER = 10;
const MAX_BANDWIDTH_MBPS = 1000;
const MAX_LATENCY_MS = 1000;

export function SystemMonitoring() {
	const [isDetailedView, setIsDetailedView] = useState(false);
	const [clientFps, setClientFps] = useState(0);
	const [audioBuffer, setAudioBuffer] = useState(0);
	const [cpuPercent, setCpuPercent] = useState(0);
	const [gpuPercent, setGpuPercent] = useState(0);
	const [sysMemPercent, setSysMemPercent] = useState(0);
	const [gpuMemPercent, setGpuMemPercent] = useState(0);
	const [sysMemUsed, setSysMemUsed] = useState<number | null>(null);
	const [sysMemTotal, setSysMemTotal] = useState<number | null>(null);
	const [gpuMemUsed, setGpuMemUsed] = useState<number | null>(null);
	const [gpuMemTotal, setGpuMemTotal] = useState<number | null>(null);
	const [bandwidthMbps, setBandwidthMbps] = useState(0);
	const [latencyMs, setLatencyMs] = useState(0);

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
			setAudioBuffer(window.currentAudioBufferSize ?? 0);

			const netStats = window.network_stats;
			setBandwidthMbps(netStats?.bandwidth_mbps ?? 0);
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

			case 'audio': // For audio buffer
				if (value <= 3) return { status: 'excellent', color: 'text-green-500', bg: 'bg-green-500/10' };
				if (value <= 6) return { status: 'good', color: 'text-yellow-500', bg: 'bg-yellow-500/10' };
				return { status: 'high', color: 'text-red-500', bg: 'bg-red-500/10' };

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
			current: audioBuffer,
			max: MAX_AUDIO_BUFFER,
			fill: "hsl(230, 100%, 60%)",
			hasData: hasAudioData
		},
		{
			name: "Bandwidth",
			current: Math.round(bandwidthMbps * 100) / 100,
			max: MAX_BANDWIDTH_MBPS,
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
							<span className="text-sm text-muted-foreground">Audio Buffer</span>
							<div className="flex items-center gap-2">
								<span className="text-sm font-medium text-card-foreground">{audioBuffer}/{MAX_AUDIO_BUFFER}</span>
								{(() => {
									const status = getPerformanceStatus(audioBuffer, 'audio');
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
