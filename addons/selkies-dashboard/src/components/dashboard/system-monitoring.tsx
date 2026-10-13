/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Button } from "@/components/ui/button";
import { SectionAccordion, SectionItem } from "@/components/dashboard/section-accordion";
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
import { STATS_EVENT, type StreamClient, type StreamInfo, type StreamSample } from "../../../../selkies-web-core/lib/stream-stats.js";
import {
	graphPath,
	seriesOf,
	streamMeters,
	streamReport,
	streamRows,
	streamTiles,
} from "../../../../selkies-web-core/lib/stream-stats-view.js";

/**
 * The stats overlay: the stream's figures as half-circle gauges, what the
 * stream runs on, the graphs that grow while it stays open, the figures
 * under them, and the host's meters, as a compact strip or in full. The
 * strip reads at a glance, at the toolbar's height — the stream's three
 * figures against their own scales, then after a divider every meter of the
 * host's load as shares. The full view keeps the same gauges,
 * every meter among them, above an accordion of the pipeline rows, the
 * histories, and the figures, one open at a time, with the uplinks under it.
 *
 * Everything drawn comes from the core's `window.stream_info`,
 * `window.stream_client`, and `window.stream_stats`
 * (`selkies-web-core/lib/stream-stats.js`), read each time the core announces
 * a change (`STATS_EVENT`) while the overlay is mounted and the tab is
 * visible; what a row says and when it warns
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
		 * leaves the old peer's teardown to the reload the switch makes.
		 */
		__selkiesModeSwitching?: boolean;
	}
}

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
}

const SERIES_STROKES = ['var(--stat-series-1)', 'var(--stat-series-2)'];

/** One graph: its name, the value under the pointer or else the newest, and its series. */
function Graph({ label, unit, series, max }: GraphProps) {
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
			<svg
				className="block h-11 w-full rounded-md bg-muted/50 pointer-events-auto"
				viewBox={`0 0 ${GRAPH_WIDTH} ${GRAPH_HEIGHT}`}
				preserveAspectRatio="none"
				onPointerMove={onMove}
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
			<span className="pointer-events-none absolute bottom-7 right-1 rounded-sm bg-muted px-1 text-[11px] text-muted-foreground">
				{`${Math.round(max)} ${unit}`}
			</span>
		</div>
	);
}

interface GaugeProps {
	/** The gauge's short name, drawn under the base line. */
	label: string;
	/** The figure drawn in the bowl. */
	value: string;
	/** The share of the half circle the arc fills, 0-100. */
	percent: number;
	/** Draws the arc in the warn color; set by the figure that fell short. */
	warn?: boolean;
	/** A hover note carrying what the arc alone omits, the memory amounts. */
	title?: string;
	/** Draws the gauge at the size of the toolbar's controls, for the strip. */
	compact?: boolean;
}

/** One gauge of a row: its identity beside what it draws. */
interface GaugeSpec extends GaugeProps {
	/** Tells the gauges apart in their row. */
	key: string;
}

/**
 * One half-circle gauge: the arc fills to `percent` of the half circle, the
 * figure sits in the bowl, and the name sits under the base line. A gauge
 * reads a figure against its scale in one glance, where a graph has to be
 * read; the scales themselves belong to the caller.
 * @param props The gauge's name, figure, fill, and warn state.
 * @returns The gauge element.
 */
function Gauge({ label, value, percent, warn, title, compact }: GaugeProps) {
	const fill = Math.max(0, Math.min(100, percent));
	const angle = ((180 + fill * 1.8) * Math.PI) / 180;
	const endX = (20 + 16 * Math.cos(angle)).toFixed(2);
	const endY = (20 + 16 * Math.sin(angle)).toFixed(2);
	return (
		<div title={title} className={`flex flex-col items-center ${compact ? 'w-9' : 'w-12'}`}>
			<div className={`relative ${compact ? 'w-9' : 'w-12'}`}>
				<svg viewBox="0 0 40 22" className={`block w-full ${compact ? 'h-4' : 'h-5.5'}`} role="img" aria-label={`${label}: ${value}`}>
					<path d="M 4 20 A 16 16 0 0 1 36 20" fill="none" stroke="var(--muted)" strokeWidth={4} strokeLinecap="round" />
					{fill > 0.5 && (
						<path d={`M 4 20 A 16 16 0 0 1 ${endX} ${endY}`} fill="none"
							stroke={warn ? 'var(--stat-warn)' : 'var(--primary)'} strokeWidth={4} strokeLinecap="round" />
					)}
				</svg>
				<b className={`absolute inset-x-0 bottom-0 text-center leading-none text-foreground ${compact ? 'text-[7px]' : 'text-[10px]'}`}>{value}</b>
			</div>
			<span className={`mt-0.5 whitespace-nowrap uppercase tracking-wide text-muted-foreground ${compact ? 'text-[7px] leading-none' : 'text-[7px]'}`}>{label}</span>
		</div>
	);
}

/**
 * Renders the overlay, reading the core's `window` stream state whenever the
 * core announces a change and telling the core the stats are on screen for as
 * long as it is mounted in a visible tab.
 */
export function SystemMonitoring() {
	const [isDetailedView, setIsDetailedView] = useState(
		() => localStorage.getItem(getPrefixedKey('stats_detailed')) === 'true');
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
		window.addEventListener(STATS_EVENT, read);
		return () => {
			window.removeEventListener(STATS_EVENT, read);
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
		const rtt = seriesOf(samples, 'rtt_ms');
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
		hardware_available: t('sections.stats.hardwareAvailable'),
		software_preferred: t('sections.stats.softwarePreferred'),
	});
	const number = (key: string): number => (latest && typeof latest[key] === 'number' ? (latest[key] as number) : 0);

	const tiles = streamTiles(latest, client ? client.transport : 'websockets', history);
	const meters = streamMeters(latest);

	// The gauges both views share. The fps arc fills to the configured
	// framerate and warns under nine tenths of it — the one figure with a
	// configured value to fall short of; the latency arc fills as the reading
	// leaves its axis, so a full arc is the best reading; the bitrate arc is
	// the reading against the graph's own top, a scale with no good or bad
	// end. The host meters fill as shares of their wholes.
	// The bitrate gauge reads the graph's own newest point, so the two always
	// agree: the graph is bucketed by maximum once the history is long.
	const mbpsValues = graphs.mbps.series[0].values;
	const mbpsNow = mbpsValues.length ? mbpsValues[mbpsValues.length - 1] : 0;
	const streamGauges: GaugeSpec[] = [
		{
			key: 'fps',
			label: 'FPS',
			value: `${Math.round(number('fps'))}`,
			percent: (number('fps') / framerate) * 100,
			warn: number('fps') > 0 && number('fps') < framerate * 0.9,
		},
		{
			key: 'mbps',
			label: 'Mbps',
			value: mbpsNow.toFixed(1),
			percent: (mbpsNow / graphs.mbps.max) * 100,
		},
		{
			key: 'rtt_ms',
			label: 'RTT',
			value: `${Math.round(number('rtt_ms'))}`,
			percent: 100 - (number('rtt_ms') / graphs.rtt.max) * 100,
		},
	];
	const meterGauges: GaugeSpec[] = meters.map((meter) => ({
		key: meter.key,
		label: meter.key === 'gpumem' ? 'VRAM' : meter.key.toUpperCase(),
		value: `${Math.round(meter.percent)}`,
		percent: meter.percent,
		title: meter.text,
	}));

	const toggle = (
		<Tooltip>
			<TooltipTrigger
				render={
					<Button
						variant="ghost"
						size="sm"
						className="h-7 w-7 p-0 min-w-0 pointer-events-auto"
						onClick={() => {
							localStorage.setItem(getPrefixedKey('stats_detailed'), String(!isDetailedView));
							setIsDetailedView(!isDetailedView);
						}}
					/>
				}
			>
				{isDetailedView ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
			</TooltipTrigger>
			<TooltipContent side="bottom">
				<p>{isDetailedView ? t('stats.compactView') : t('stats.detailedView')}</p>
			</TooltipContent>
		</Tooltip>
	);

	const copy = async () => {
		try {
			await navigator.clipboard.writeText(streamReport(info, client, latest));
			setCopied(true);
			setTimeout(() => setCopied(false), 1500);
		} catch (e) {
			console.warn('Could not copy the stats:', e);
		}
	};

	if (!isDetailedView) {
		return (
			<div data-drag-handle className="flex h-[42px] items-center gap-2 rounded-lg border bg-background px-2 text-xs shadow-lg tabular-nums cursor-grab active:cursor-grabbing select-none">
				<div className="flex items-center gap-1 pointer-events-none">
					{streamGauges.map(({ key, ...gauge }) => <Gauge key={key} compact {...gauge} />)}
					{meterGauges.length > 0 && (
						<>
							<div className="mx-1 h-6 w-px bg-border" aria-hidden />
							{meterGauges.map(({ key, ...gauge }) => <Gauge key={key} compact {...gauge} />)}
						</>
					)}
				</div>
				{toggle}
			</div>
		);
	}

	return (
		<div className="flex min-h-0 w-[min(92vw,22rem)] flex-col rounded-lg border bg-background py-2 text-xs shadow-lg tabular-nums">
			{/* The scroll is an inner box with no background, square corners, and rows that skip rendering
			    while scrolled out of view. Chromium composites an opaque scroller (on a high-density screen,
			    any) as a second layer redrawn over the panel with every video frame, and a rounded one through
			    an offscreen pass; Firefox renders the rows a scroller hides with every frame. The panel's
			    padding keeps the box clear of its corners. */}
			<div className="flex min-h-0 flex-col gap-2.5 overflow-y-auto px-3 py-1 [&>*]:[content-visibility:auto] [&>*]:[contain-intrinsic-size:auto_2.75rem]">
				<div data-drag-handle className="flex items-center justify-between cursor-grab active:cursor-grabbing select-none">
					<h3 className="text-sm font-semibold text-card-foreground pointer-events-none">{t('stats.monitorTitle')}</h3>
					<div className="flex items-center gap-1">
						<Tooltip>
							<TooltipTrigger
								render={
									<Button
										variant="ghost"
										size="sm"
										className="h-7 w-7 p-0 min-w-0 pointer-events-auto"
										onClick={copy}
										aria-label={t('sections.stats.copyLabel')}
									/>
								}
							>
								{copied ? <Check className="h-3 w-3 text-[var(--stat-good)]" /> : <Copy className="h-3 w-3" />}
							</TooltipTrigger>
							<TooltipContent side="bottom">
								<p>{t('sections.stats.copyLabel')}</p>
							</TooltipContent>
						</Tooltip>
						{toggle}
					</div>
				</div>

				<div className="flex flex-wrap items-center gap-x-2 gap-y-1 border-b border-border pb-2">
					{[...streamGauges, ...meterGauges].map(({ key, ...gauge }) => (
						<Gauge key={key} {...gauge} />
					))}
				</div>

				<SectionAccordion defaultValue={["pipeline"]}>
					<SectionItem value="pipeline" title={t('stats.pipeline')}>
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
					</SectionItem>

					<SectionItem value="graphs" title={t('stats.graphs')}>
						<div className="flex flex-col gap-2.5">
							<Graph label={t('sections.stats.fpsLabel')} unit="fps" {...graphs.fps} />
							<Graph label={t('sections.stats.bandwidthLabel')} unit="Mbps" {...graphs.mbps} />
							<Graph label={t('sections.stats.latencyLabel')} unit="ms" {...graphs.rtt} />
						</div>
					</SectionItem>

					<SectionItem value="figures" title={t('stats.figures')}>
					<div className="grid grid-cols-3 gap-1.5 pointer-events-none">
						{tiles.map((tile) => (
							<div key={tile.key}
								className={`flex flex-col rounded-md border bg-muted/40 px-1.5 py-1${tile.warn ? ' border-[var(--stat-warn)]' : ' border-transparent'}`}
								title={tile.warn ? t('sections.stats.overshoot') : undefined}>
								<b className="flex items-center gap-1 text-[13px]">{tile.warn && <StatusMark status="warn" />}{tile.value}</b>
								{tile.detail && <small className="text-[7px] tabular-nums text-muted-foreground">{tile.detail}</small>}
								<span className="text-[10.5px] text-muted-foreground">{t(`sections.stats.tiles.${tile.key}`, tile.label)}</span>
							</div>
						))}
					</div>
					</SectionItem>
				</SectionAccordion>

				{latest && (latest.mic || latest.webcam) && (
					<div className="flex flex-col gap-1 text-muted-foreground pointer-events-none">
						{latest.mic && <div className="flex items-center gap-1.5"><Mic className="h-3.5 w-3.5" aria-label="mic" />{latest.mic}</div>}
						{latest.webcam && <div className="flex items-center gap-1.5"><Video className="h-3.5 w-3.5" aria-label="webcam" />{latest.webcam}</div>}
					</div>
				)}
			</div>
		</div>
	);
}

export default SystemMonitoring;
