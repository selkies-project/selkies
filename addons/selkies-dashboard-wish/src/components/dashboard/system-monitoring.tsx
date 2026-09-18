/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useEffect, useMemo, useState } from "react";
import {
	Check,
	ChevronDown,
	ChevronUp,
	CircleCheck,
	Copy,
	Dot,
	Mic,
	TriangleAlert,
	Video,
} from "lucide-react";
import { getLastServerSettings, getPrefixedKey } from "@/utils";
import { t } from "@/i18n";
import type { StreamClient, StreamInfo, StreamSample } from "../../../../selkies-web-core/lib/stream-stats.js";
import {
	graphPath,
	seriesOf,
	streamMeters,
	streamReport,
	streamRows,
	streamTiles,
} from "../../../../selkies-web-core/lib/stream-stats-view.js";

/**
 * The stats overlay: what the stream runs on, the graphs that grow while it
 * stays open, the figures under them and the host's meters, as a compact strip
 * or in full.
 *
 * Everything drawn comes from the core's `window.stream_info`,
 * `window.stream_client` and `window.stream_stats`
 * (`selkies-web-core/lib/stream-stats.js`), read once a second while the
 * overlay is mounted and the tab is visible; what a row says and when it warns
 * is `lib/stream-stats-view.js`, shared with the default dashboard. Being on
 * screen is what turns the numbers on: the component posts `statsOpen` to the
 * core, which asks the server for them, and posts `open: false` on the way out.
 * The fps axis starts at the configured framerate, an explicit client choice in
 * localStorage winning over the server's value.
 * @module
 */

/** The stream state the cores publish on `window`. */
declare global {
	interface Window {
		stream_info?: StreamInfo | null;
		stream_client?: StreamClient;
		stream_stats?: { open: boolean; latest: StreamSample | null; history: StreamSample[] };
		fps?: number;
		currentAudioBufferSize?: number;
		/**
		 * Set by the dashboard around a transport switch so the active core
		 * suppresses the expected "Server disconnected" alert from the old peer.
		 */
		__selkiesModeSwitching?: boolean;
	}
}

const READ_INTERVAL_MS = 1000;
const GRAPH_WIDTH = 240;
const GRAPH_HEIGHT = 44;

/** What one read of the core's state holds. */
interface Snapshot {
	/** The server's description of the stream, null until it arrives. */
	info: StreamInfo | null;
	/** This page's description of the stream. */
	client: StreamClient | null;
	/** The last second's figures. */
	latest: StreamSample | null;
	/** Every second since the overlay opened, appended to in place. */
	history: StreamSample[];
	/** The history's length at the read, which is what changes between reads. */
	length: number;
}

/**
 * How many overlays are on screen. The panel and the floating overlay can both
 * be mounted, and the core is told the stats shut only when the last one goes.
 */
let mounted = 0;

/** Counts one overlay in or out and tells the core when the count crosses zero. */
function countOverlay(delta: 1 | -1): void {
	const before = mounted;
	mounted += delta;
	if ((before === 0) !== (mounted === 0)) {
		window.postMessage({ type: 'statsOpen', open: mounted > 0 }, window.location.origin);
	}
}

/** The framerate the session is configured to push, the floor of the fps axis. */
function configuredFramerate(): number {
	const settings = getLastServerSettings();
	const stored = parseFloat(localStorage.getItem(getPrefixedKey('framerate')) ?? '');
	const server = parseFloat(settings?.framerate?.value);
	const fps = !isNaN(stored) ? stored : (!isNaN(server) ? server : 60);
	return fps > 0 ? fps : 60;
}

/** The top of a graph's axis: the largest value drawn with a little headroom, never under `floor`. */
const axisTop = (floor: number, ...lists: number[][]): number =>
	Math.max(floor, Math.ceil(Math.max(0, ...lists.flat()) * 1.1));

interface StatusMarkProps {
	/** The row's state; the icon's shape carries it as well as its color. */
	status: 'good' | 'warn' | 'neutral';
}

/** The mark beside a row. */
function StatusMark({ status }: StatusMarkProps) {
	if (status === 'good') return <CircleCheck className="h-3.5 w-3.5 shrink-0 text-[var(--stat-good)]" aria-hidden />;
	if (status === 'warn') return <TriangleAlert className="h-3.5 w-3.5 shrink-0 text-[var(--stat-warn)]" aria-hidden />;
	return <Dot className="h-3.5 w-3.5 shrink-0 text-muted-foreground" aria-hidden />;
}

interface GraphProps {
	/** The graph's name. */
	label: string;
	/** The unit of its axis. */
	unit: string;
	/** One or two series against that one axis. */
	series: Array<{ name: string; values: number[] }>;
	/** The value at the top edge. */
	max: number;
	/** Drawn without its head, for the compact strip. */
	bare?: boolean;
}

const SERIES_STROKES = ['var(--stat-series-1)', 'var(--stat-series-2)'];

/** One graph: its name, the value under the pointer or else the newest, and its series. */
function Graph({ label, unit, series, max, bare }: GraphProps) {
	const [hover, setHover] = useState<number | null>(null);
	const paths = useMemo(
		() => series.map((s) => graphPath(s.values, GRAPH_WIDTH, GRAPH_HEIGHT, max)),
		[series, max],
	);
	const count = series[0].values.length;
	const at = hover !== null && hover < count ? hover : count - 1;
	const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
		const box = e.currentTarget.getBoundingClientRect();
		const xs = paths[0].xs;
		if (!xs.length || box.width <= 0) return;
		const x = ((e.clientX - box.left) / box.width) * GRAPH_WIDTH;
		setHover(xs.reduce((best, px, i) => (Math.abs(px - x) < Math.abs(xs[best] - x) ? i : best), 0));
	};
	return (
		<div className="relative">
			{!bare && (
				<div className="mb-0.5 flex items-baseline justify-between gap-2">
					<span className="text-[11px] uppercase tracking-wide text-muted-foreground">{label}</span>
					<span className="flex gap-2.5 text-xs text-muted-foreground">
						{series.map((s, i) => (
							<span key={s.name || i}>
								{series.length > 1 && (
									<i className="mr-1 inline-block h-0.5 w-2.5 rounded-sm align-middle" style={{ background: SERIES_STROKES[i] }} />
								)}
								<b className="text-sm text-foreground">{at >= 0 ? s.values[at] : 0}</b>
								{series.length > 1 ? ` ${s.name}` : ` ${unit}`}
							</span>
						))}
					</span>
				</div>
			)}
			<svg
				className={`block w-full rounded-md bg-muted/50 pointer-events-auto ${bare ? 'h-6' : 'h-11'}`}
				viewBox={`0 0 ${GRAPH_WIDTH} ${GRAPH_HEIGHT}`}
				preserveAspectRatio="none"
				onPointerMove={bare ? undefined : onMove}
				onPointerLeave={() => setHover(null)}
				role="img"
				aria-label={`${label}: ${series.map((s) => `${at >= 0 ? s.values[at] : 0} ${s.name || unit}`).join(', ')}`}
			>
				{paths.map((p, i) => (
					<g key={i}>
						{i === 0 && <path d={p.area} fill={SERIES_STROKES[0]} opacity={0.12} />}
						<path d={p.line} fill="none" stroke={SERIES_STROKES[i]} strokeWidth={2} strokeLinejoin="round"
							strokeLinecap="round" vectorEffect="non-scaling-stroke" />
					</g>
				))}
				{hover !== null && at >= 0 && (
					<line x1={paths[0].xs[at]} x2={paths[0].xs[at]} y1={0} y2={GRAPH_HEIGHT}
						stroke="var(--muted-foreground)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
				)}
			</svg>
			{!bare && (
				<span className="pointer-events-none absolute bottom-7 right-1 rounded-sm bg-muted px-1 text-[11px] text-muted-foreground">
					{`${Math.round(max)} ${unit}`}
				</span>
			)}
		</div>
	);
}

/**
 * Renders the overlay, reading the core's `window` stream state once a second
 * and telling the core the stats are on screen for as long as it is mounted
 * in a visible tab.
 */
export function SystemMonitoring() {
	const [isDetailedView, setIsDetailedView] = useState(false);
	const [visible, setVisible] = useState(!document.hidden);
	const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
	const [copied, setCopied] = useState(false);
	const [framerate, setFramerate] = useState(configuredFramerate);

	useEffect(() => {
		const onVisibility = () => setVisible(!document.hidden);
		document.addEventListener('visibilitychange', onVisibility);
		return () => document.removeEventListener('visibilitychange', onVisibility);
	}, []);

	useEffect(() => {
		if (!visible) return undefined;
		countOverlay(1);
		const read = () => {
			const stats = window.stream_stats || { open: false, latest: null, history: [] };
			setSnapshot({
				info: window.stream_info || null,
				client: window.stream_client ? { ...window.stream_client } : null,
				latest: stats.latest,
				history: stats.history,
				length: stats.history.length,
			});
			setFramerate(configuredFramerate());
		};
		read();
		const id = setInterval(read, READ_INTERVAL_MS);
		return () => {
			clearInterval(id);
			countOverlay(-1);
		};
	}, [visible]);

	const historyLength = snapshot ? snapshot.length : 0;
	const history = snapshot ? snapshot.history : null;
	const graphs = useMemo(() => {
		const samples = history || [];
		const fps = seriesOf(samples, 'fps');
		const encoded = seriesOf(samples, 'encoded_fps');
		const mbps = seriesOf(samples, 'mbps');
		const latency = seriesOf(samples, 'latency_ms');
		// The round trip alone where no stage of the path was measured, so the graph
		// keeps a reading rather than flattening to zero.
		const rtt = latency.some((value) => value > 0) ? latency : seriesOf(samples, 'rtt_ms');
		const hasEncoded = samples.some((s) => typeof s.encoded_fps === 'number');
		return {
			fps: {
				series: hasEncoded
					? [{ name: t('sections.stats.client'), values: fps }, { name: t('sections.stats.server'), values: encoded }]
					: [{ name: '', values: fps }],
				max: axisTop(framerate, fps, encoded),
			},
			mbps: { series: [{ name: '', values: mbps }], max: axisTop(1, mbps) },
			rtt: { series: [{ name: '', values: rtt }], max: axisTop(20, rtt) },
		};
		// The history array is appended to in place; its length is what changes.
		// eslint-disable-next-line react-hooks/exhaustive-deps
	}, [history, historyLength, framerate]);

	const info = snapshot ? snapshot.info : null;
	const client = snapshot ? snapshot.client : null;
	const latest = snapshot ? snapshot.latest : null;
	const rows = streamRows(info, client, latest, {
		hardware: t('sections.stats.hardware'),
		software: t('sections.stats.software'),
		unknown: t('sections.stats.tooltipMemoryNA'),
		throttled: t('sections.stats.throttled'),
	});
	const number = (key: string): number => (latest && typeof latest[key] === 'number' ? (latest[key] as number) : 0);

	const toggle = (
		<Tooltip>
			<TooltipTrigger asChild>
				<Button
					variant="ghost"
					size="sm"
					className="h-7 w-7 p-0 min-w-0 pointer-events-auto"
					onClick={() => setIsDetailedView((detailed) => !detailed)}
				>
					{isDetailedView ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
				</Button>
			</TooltipTrigger>
			<TooltipContent side="bottom">
				<p>{isDetailedView ? t('stats.compactView') : t('stats.detailedView')}</p>
			</TooltipContent>
		</Tooltip>
	);

	if (!isDetailedView) {
		return (
			<div className="flex items-center gap-3 rounded-lg border bg-card px-3 py-1.5 text-xs shadow-sm backdrop-blur-sm tabular-nums cursor-grab active:cursor-grabbing">
				<div className="flex items-center gap-1 pointer-events-none">
					{rows.map((row) => <StatusMark key={row.key} status={row.status} />)}
				</div>
				{([['fps', 'fps', graphs.fps], ['mbps', 'Mbps', graphs.mbps], ['rtt_ms', 'ms', graphs.rtt]] as const).map(
					([key, unit, graph]) => (
						<div key={key} className="flex items-center gap-1.5 pointer-events-none">
							<span className="whitespace-nowrap text-muted-foreground">
								<b className="text-sm text-foreground">{number(key)}</b> {unit}
							</span>
							<div className="w-16">
								<Graph label={unit} unit={unit} series={graph.series.slice(0, 1)} max={graph.max} bare />
							</div>
						</div>
					),
				)}
				{toggle}
			</div>
		);
	}

	const tiles = streamTiles(latest, client ? client.transport : 'websockets');
	const meters = streamMeters(latest);
	const meterLabels: Record<string, string> = {
		cpu: t('sections.stats.cpuLabel'),
		mem: t('sections.stats.sysMemLabel'),
		gpu: t('sections.stats.gpuLabel'),
		gpumem: t('sections.stats.gpuMemLabel'),
	};
	const copy = async () => {
		try {
			await navigator.clipboard.writeText(streamReport(info, client, latest));
			setCopied(true);
			setTimeout(() => setCopied(false), 1500);
		} catch (e) {
			console.warn('Could not copy the stats:', e);
		}
	};

	return (
		<div className="flex w-80 flex-col gap-2.5 rounded-lg border bg-background/95 p-3 text-xs shadow-lg backdrop-blur-sm tabular-nums cursor-grab active:cursor-grabbing">
			<div className="flex items-center justify-between">
				<h3 className="text-sm font-semibold text-card-foreground pointer-events-none">{t('stats.monitorTitle')}</h3>
				<div className="flex items-center gap-1">
					<Tooltip>
						<TooltipTrigger asChild>
							<Button variant="ghost" size="sm" className="h-7 w-7 p-0 min-w-0 pointer-events-auto"
								onClick={copy} aria-label={t('sections.stats.copyLabel')}>
								{copied ? <Check className="h-3 w-3 text-[var(--stat-good)]" /> : <Copy className="h-3 w-3" />}
							</Button>
						</TooltipTrigger>
						<TooltipContent side="bottom">
							<p>{t('sections.stats.copyLabel')}</p>
						</TooltipContent>
					</Tooltip>
					{toggle}
				</div>
			</div>

			<div className="flex flex-col gap-1.5 pointer-events-none">
				{rows.map((row) => (
					<div key={row.key} className="grid grid-cols-[14px_76px_1fr] items-start gap-1.5">
						<StatusMark status={row.status} />
						<span className="text-[11px] uppercase leading-4 tracking-wide text-muted-foreground">
							{t(`sections.stats.${row.key}Label`)}
						</span>
						<span className="flex min-w-0 flex-col [overflow-wrap:anywhere]">
							<span className="font-semibold">{row.value || t('sections.stats.tooltipMemoryNA')}</span>
							{row.detail && <span className="text-muted-foreground">{row.detail}</span>}
							{row.reason && (
								<span className="mt-0.5 border-l-2 border-[var(--stat-warn)] pl-1.5 text-muted-foreground">{row.reason}</span>
							)}
						</span>
					</div>
				))}
			</div>

			<Graph label={t('sections.stats.fpsLabel')} unit="fps" {...graphs.fps} />
			<Graph label={t('sections.stats.bandwidthLabel')} unit="Mbps" {...graphs.mbps} />
			<Graph label={t('sections.stats.latencyLabel')} unit="ms" {...graphs.rtt} />

			{(
				<div className="grid grid-cols-2 gap-1.5 pointer-events-none">
					{tiles.map((tile) => (
						<div key={tile.key} className="flex flex-col rounded-md border bg-muted/40 px-1.5 py-1">
							<b className="text-[13px]">{tile.value}</b>
							<span className="text-[10.5px] text-muted-foreground">{tile.label}</span>
						</div>
					))}
				</div>
			)}

			{meters.length > 0 && (
				<div className="flex flex-col gap-1">
					{meters.map((meter) => (
						<div key={meter.key} title={meter.detail || undefined}
							className="grid grid-cols-[76px_1fr_2.5rem] items-center gap-2">
							<span className="text-[11px] uppercase tracking-wide text-muted-foreground">{meterLabels[meter.key]}</span>
							{meter.bar && (
								<span className="h-1.5 overflow-hidden rounded-full bg-muted">
									<span className="block h-full rounded-full bg-primary transition-[width] duration-500"
										style={{ width: `${meter.percent}%` }} />
								</span>
							)}
							<span className={meter.bar ? "text-right text-muted-foreground"
								: "col-start-2 col-end-[-1] text-muted-foreground"}>{meter.text}</span>
						</div>
					))}
				</div>
			)}

			{latest && (latest.mic || latest.webcam) && (
				<div className="flex flex-col gap-1 text-muted-foreground pointer-events-none">
					{latest.mic && <div className="flex items-center gap-1.5"><Mic className="h-3.5 w-3.5" aria-label="mic" />{latest.mic}</div>}
					{latest.webcam && <div className="flex items-center gap-1.5"><Video className="h-3.5 w-3.5" aria-label="webcam" />{latest.webcam}</div>}
				</div>
			)}
		</div>
	);
}

export default SystemMonitoring;
