/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the dashboard draws of `lib/stream-stats.ts`'s contract, kept free of
 * any framework so the two cannot read the same session differently: the four
 * status rows and what makes one a warning, the figures under the graphs, the
 * host meters, the geometry of a graph, and the plain-text report a user pastes
 * into an issue.
 *
 * A row warns where the session fell short of what it asked for, never for a
 * choice and never for a server with no GPU, which is an ordinary deployment:
 * software encoding somebody selected, or on a host exposed no GPU, is neutral,
 * and software encoding on a session that asked for hardware where there is a
 * GPU is a warning and carries pixelflux's reason. A readback in front of a
 * hardware encoder is read the same way: it warns where the display server
 * offered the encoder a zero-copy path (`zero_copy_available`) and the capture
 * took a copy anyway, and is neutral on a server that offers none, such as an
 * Xvfb with neither DRI3 nor NvFBC. The client's side reads the
 * same way: software decoding warns only where a hardware decoder was there to
 * take the stream, since a client without one is as ordinary as a server
 * without a GPU, and a path through a relay or over TCP is how the network
 * connects this client, which marks nothing either way, while a direct UDP
 * path is good.
 * Every value opens with a capital, and names the system reports (`nvidia`,
 * `renderD128`, a decoder's own) are kept as reported. Technical values (`NVENC`,
 * `Zero-copy`, `renderD128`) are not translated; the words a dashboard does
 * translate arrive in `words`, and a figure's label is the dashboard's to word
 * by the figure's key, the English here being the report's and the fallback.
 *
 * @module
 */

import type { StreamClient, StreamInfo, StreamSample } from './stream-stats.ts';

/** One status row. */
export interface StreamRow {
  key: 'encoder' | 'capture' | 'decoder' | 'connection';
  status: 'good' | 'warn' | 'neutral';
  value: string;
  detail: string;
  reason: string;
}

/** The translated words the rows use. */
export interface StreamWords {
  hardware: string;
  software: string;
  unknown: string;
  throttled: string;
  hardware_available: string;
  software_preferred: string;
}

const CODEC_NAMES: Record<string, string> = { h264: 'H.264', h265: 'H.265', hevc: 'H.265', av1: 'AV1', vp8: 'VP8', vp9: 'VP9', jpeg: 'JPEG' };

/** @param codec @returns */
const codecName = (codec: string): string => CODEC_NAMES[String(codec || '').toLowerCase()] || String(codec || '').toUpperCase();
/** @param path @returns */
const nodeName = (path: string): string => String(path || '').split('/').pop() || '';
/** @param parts @returns */
const joined = (parts: Array<string | undefined | null | false>): string => parts.filter(Boolean).join(' · ');
 /** @param text Prose the server or an engine phrased. @returns */
const sentence = (text: string | undefined): string => (text ? text[0].toUpperCase() + text.slice(1) : '');

/**
 * @param info
 * @returns
 */
function encoderRow(info: StreamInfo | null): StreamRow {
  if (!info) return { key: 'encoder', status: 'neutral', value: '', detail: '', reason: '' };
  const video = info.codec !== 'jpeg';
  const fell = !info.hardware && info.hardware_expected;
  return {
    key: 'encoder',
    status: info.hardware ? 'good' : fell ? 'warn' : 'neutral',
    value: joined([info.encoder, `${codecName(info.codec)}${video ? (info.fullcolor ? ' 4:4:4' : ' 4:2:0') : ''}${video && info.bit_depth === 10 ? ' 10-bit' : ''}`,
      info.striped ? 'Striped' : undefined]),
    detail: info.hardware ? joined([info.gpu, nodeName(info.encode_node), info.driver])
      : info.gpu_present === false ? 'No GPU exposed to the server' : '',
    reason: fell ? sentence(info.encoder_reason) : '',
  };
}

/**
 * @param info
 * @returns
 */
function captureRow(info: StreamInfo | null): StreamRow {
  if (!info) return { key: 'capture', status: 'neutral', value: '', detail: '', reason: '' };
  const wayland = info.backend === 'wayland';
  const software = wayland && info.renderer === 'pixman';
  const path = info.zero_copy ? `Zero-copy${wayland ? '' : ` (${info.capture})`}` : `Readback${wayland ? '' : ` (${info.capture})`}`;
  const readbackToGpu = !info.zero_copy && info.hardware && info.zero_copy_available;
  const renderedInSoftware = software && info.hardware_expected;
  return {
    key: 'capture',
    status: info.zero_copy ? 'good' : (readbackToGpu || renderedInSoftware) ? 'warn' : 'neutral',
    value: joined([wayland ? 'Wayland' : 'X11', path]),
    detail: !wayland ? '' : software ? 'Pixman (software rendering)'
      : joined([`GL on ${nodeName(info.render_node || '')}`, info.render_gpu]),
    reason: sentence(renderedInSoftware ? info.renderer_reason : readbackToGpu ? info.capture_reason : ''),
  };
}

/**
 * @param client
 * @param words
 * @returns
 */
function decoderRow(client: StreamClient | null, words: StreamWords): StreamRow {
  const decoder = client ? client.decoder : 'unknown';
  const fell = decoder === 'software' && !!client?.hardware_expected;
  return {
    key: 'decoder',
    status: decoder === 'hardware' ? 'good' : fell ? 'warn' : 'neutral',
    value: (words as unknown as Record<string, string>)[decoder] || words.unknown,
    detail: client
      ? joined([client.decoder_evidence, codecName(client.codec), client.resolution,
        client.decode_path, client.sink])
      : '',
    reason: fell && client ? ((words as unknown as Record<string, string>)[client.decoder_reason] || sentence(client.decoder_reason)) : '',
  };
}

/**
 * @param client
 * @param latest
 * @param words
 * @returns
 */
function connectionRow(client: StreamClient | null, latest: StreamSample | null, words: StreamWords): StreamRow {
  const webrtc = !!client && client.transport === 'webrtc';
  const path = (client && client.path) || '';
  const direct = /^(host|srflx|prflx) udp$/.test(path);
  const throttled = !!(latest && latest.throttled);
  return {
    key: 'connection',
    status: throttled ? 'warn' : webrtc && direct ? 'good' : 'neutral',
    value: joined([webrtc ? 'WebRTC' : 'WebSockets', sentence(path.replace(/\b(udp|tcp|tls)\b/g, (p) => p.toUpperCase()))]),
    detail: '',
    reason: throttled ? words.throttled : '',
  };
}

/**
 * The four status rows, in display order.
 * @param info
 * @param client
 * @param latest
 * @param words
 * @returns
 */
export function streamRows(info: StreamInfo | null, client: StreamClient | null, latest: StreamSample | null, words: StreamWords): StreamRow[] {
  return [encoderRow(info), captureRow(info), decoderRow(client, words), connectionRow(client, latest, words)];
}

/** The figures under the graphs by sample key: the label and the unit. */
const TILES: Record<string, [string, string]> = {
  encode_ms: ['Encode', 'ms'],
  pipeline_ms: ['Capture to encoded', 'ms'],
  send_ms: ['Capture to sent', 'ms'],
  decode_ms: ['Decode', 'ms'],
  present_ms: ['Arrival to screen', 'ms'],
  jitter_buffer_ms: ['Jitter buffer', 'ms'],
  audio_buffer_ms: ['Audio buffer', 'ms'],
  video_mbps: ['Video rate', 'Mbps'],
  peak_mbps: ['Peak, 250 ms', 'Mbps'],
  packet_loss_percent: ['Packet loss', '%'],
  received_mb: ['Received', 'MB'],
  frames_dropped: ['Frames dropped', ''],
  frames_not_shown: ['Frames not shown', ''],
  lost_frames: ['Lost frames', ''],
  freezes: ['Freezes', ''],
  nacks: ['NACKs', ''],
  keyframe_requests: ['Key frame requests', ''],
};

/** The figures each transport shows, in order. */
const TRANSPORT_TILES: Record<string, string[]> = {
  websockets: ['encode_ms', 'pipeline_ms', 'send_ms', 'decode_ms', 'present_ms', 'audio_buffer_ms',
    'video_mbps', 'peak_mbps', 'received_mb', 'lost_frames', 'frames_not_shown', 'keyframe_requests'],
  webrtc: ['encode_ms', 'pipeline_ms', 'send_ms', 'decode_ms', 'jitter_buffer_ms', 'present_ms', 'audio_buffer_ms',
    'video_mbps', 'peak_mbps', 'received_mb', 'packet_loss_percent',
    'frames_dropped', 'frames_not_shown', 'freezes', 'nacks', 'keyframe_requests'],
};

/** The server's least and most of a time over the same second, by the time's key. */
const RANGES: Record<string, [string, string]> = { pipeline_ms: ['pipeline_min_ms', 'pipeline_max_ms'], send_ms: ['send_min_ms', 'send_max_ms'] };

/** How far above its CBR target a stream may run, the policy's bound, before its rate warns. */
export const OVERSHOOT = 1.1;

/**
 * How long a time measured in an earlier second stands in for seconds that
 * measured none, in ms: a still screen encodes nothing, so motion that comes and
 * goes would otherwise blink its times out between frames.
 */
export const HOLD_MS = 5000;

/** One tile figure. */
export interface TileFigure {
  value: string;
  detail: string;
  warn: boolean;
}

/**
 * What a figure reads: its value with the unit, and for a time the server
 * measured its range, as `least–most`, and whether it warns: the video rate
 * when it runs past `OVERSHOOT` of its CBR target, which it is shown against.
 * A time with nothing measured this second reads the last one measured within
 * `HOLD_MS` (`history`), else `idle` where the stream moved no frame, else a
 * dash.
 * @param latest
 * @param key
 * @param history
 * @returns
 */
function figure(latest: StreamSample | null, key: string, history: StreamSample[] | null = null): TileFigure {
  const [, unit] = TILES[key] || ['', ''];
  const at = (sample: StreamSample | null, k: string): number | null =>
    (sample && typeof sample[k] === 'number' ? (sample[k] as number) : null);
  let shown: StreamSample | null = latest;
  if (at(latest, key) === null && unit === 'ms' && latest && history) {
    shown = null;
    for (let i = history.length - 1; i >= 0 && (latest.t || 0) - (history[i].t || 0) <= HOLD_MS; i--) {
      if (at(history[i], key) !== null) {
        shown = history[i];
        break;
      }
    }
    if (shown === null) {
      const idle = latest.encoded_fps === 0 || latest.fps === 0;
      return { value: idle ? 'idle' : '\u2013', detail: '', warn: false };
    }
  }
  const of = (k: string): number | null => at(shown, k);
  const value = of(key);
  if (value === null) return { value: '\u2013', detail: '', warn: false };
  const range = RANGES[key] ? RANGES[key].map(of) : null;
  const target = key === 'video_mbps' ? of('target_mbps') : null;
  return {
    value: `${value}${target ? ` / ${target}` : ''}${unit === '%' ? '%' : unit ? ` ${unit}` : ''}`,
    detail: range && range[0] !== null && range[1] !== null ? `${range[0]}\u2013${range[1]}` : '',
    warn: !!target && value > OVERSHOOT * target,
  };
}

/** One tile. */
export interface StreamTile extends TileFigure {
  key: string;
  label: string;
}

/**
 * The figures under the graphs: one fixed set per transport, so the layout
 * holds still. A time is the last second's, its range under it where the
 * server measured one, and the video rate the last ten seconds' against the
 * CBR target with its fullest quarter second beside it; everything else counts
 * from the opening: the data received, the packet loss, and each drop and
 * repair. A figure with nothing measured this second keeps its place: a time
 * reads its last measurement for a while (`figure`), and the rest a dash.
 * @param latest
 * @param transport
 * @param history What the time figures fall back on.
 * @returns
 */
export function streamTiles(latest: StreamSample | null, transport: 'websockets' | 'webrtc', history: StreamSample[] | null = null): StreamTile[] {
  return (TRANSPORT_TILES[transport] || TRANSPORT_TILES.websockets).map((key) => (
    { key, label: TILES[key][0], ...figure(latest, key, history) }));
}

/** One strip entry. */
export interface StripEntry extends TileFigure {
  key: string;
}

/**
 * The strip a dashboard lays over the stream: how fast frames reach the
 * screen, the video rate against its target, and how long a frame takes from
 * its capture to the wire, with the round trip over WebSockets, where the
 * server measures one. Each names the tile, or graph, whose label captions it.
 * @param latest
 * @param transport
 * @returns
 */
export function streamStrip(latest: StreamSample | null, transport: 'websockets' | 'webrtc'): StripEntry[] {
  const fps = latest && typeof latest.fps === 'number' ? `${latest.fps} fps` : '\u2013';
  const keys = transport === 'webrtc' ? ['video_mbps', 'send_ms'] : ['video_mbps', 'send_ms', 'rtt_ms'];
  return [{ key: 'fps', value: fps, warn: false, detail: '' }, ...keys.map((key) => {
    if (key !== 'rtt_ms') return { key, ...figure(latest, key) };
    const rtt = latest && typeof latest.rtt_ms === 'number' ? `${latest.rtt_ms} ms` : '\u2013';
    return { key, value: rtt, warn: false, detail: '' };
  })];
}

/** @param bytes @returns */
const gib = (bytes: number): string => `${(bytes / 1073741824).toFixed(1)} GiB`;

/** One host meter. */
export interface StreamMeter {
  key: 'cpu' | 'mem' | 'gpu' | 'gpumem';
  percent: number;
  text: string;
  detail: string;
  bar: boolean;
}

/**
 * The host meters: the utilizations first and the memories under them, so the
 * bars read as one block rather than alternating with the figures. A utilization
 * carries a bar, which is what a share of a whole reads well as; a memory pair
 * carries its amounts as the figure and no bar, because how much of how much is
 * what an operator needs and a bar leaves it to a hover no touch screen has. The
 * GPU rows are left out where the server reads no GPU.
 * @param latest
 * @returns
 */
export function streamMeters(latest: StreamSample | null): StreamMeter[] {
  if (!latest || typeof latest.cpu_percent !== 'number') return [];
  const share = (used: number, total: number): number => (total > 0 ? Math.min(100, (100 * used) / total) : 0);
  const used = (key: 'cpu' | 'gpu', percent: number): StreamMeter => (
    { key, percent, text: `${percent < 10 ? percent.toFixed(1) : Math.round(percent)}%`, detail: '', bar: true });
  const amounts = (key: 'mem' | 'gpumem', gotten: number, total: number): StreamMeter => ({
    key,
    percent: share(gotten, total),
    text: `${gib(gotten)} / ${gib(total)}`,
    detail: `${Math.round(share(gotten, total))}%`,
    bar: false,
  });
  const meters: StreamMeter[] = [used('cpu', Math.min(100, latest.cpu_percent as number))];
  if (typeof latest.gpu_percent === 'number') {
    meters.push(used('gpu', Math.min(100, latest.gpu_percent as number)));
  }
  meters.push(amounts('mem', latest.mem_used as number, latest.mem_total as number));
  if (typeof latest.gpu_percent === 'number' && (latest.gpu_mem_total as number) > 0) {
    meters.push(amounts('gpumem', latest.gpu_mem_used as number, latest.gpu_mem_total as number));
  }
  return meters;
}

/** Points a graph draws at most; a longer history is bucketed down to it by maximum. */
export const GRAPH_POINTS = 120;

/**
 * One series of a history, bucketed down to `GRAPH_POINTS` so a graph that has
 * grown for ten minutes costs what a short one does. A bucket keeps its
 * maximum, since a graph of a rate is read for its peaks.
 * @param history
 * @param key
 * @returns
 */
export function seriesOf(history: StreamSample[], key: string): number[] {
  const values = history.map((sample) => (typeof sample[key] === 'number' ? (sample[key] as number) : 0));
  if (values.length <= GRAPH_POINTS) return values;
  const size = values.length / GRAPH_POINTS;
  return Array.from({ length: GRAPH_POINTS }, (_, i) =>
    Math.max(...values.slice(Math.floor(i * size), Math.max(Math.floor(i * size) + 1, Math.floor((i + 1) * size)))));
}

/** A graph's SVG geometry. */
export interface GraphPath {
  line: string;
  area: string;
  xs: number[];
  ys: number[];
}

/**
 * A graph's SVG geometry. The history grows from the left edge until it fills
 * the width, the way it reads while the stats stay open.
 * @param values
 * @param width
 * @param height
 * @param max The value at the top edge.
 * @returns Path data for the stroke and the fill under it, and each point's coordinates.
 */
export function graphPath(values: number[], width: number, height: number, max: number): GraphPath {
  if (values.length === 0) return { line: '', area: '', xs: [], ys: [] };
  const step = width / Math.max(1, Math.max(values.length, 30) - 1);
  const top = max > 0 ? max : 1;
  const xs = values.map((_, i) => Math.round(i * step * 10) / 10);
  const ys = values.map((v) => Math.round((height - 1 - (Math.min(v, top) / top) * (height - 2)) * 10) / 10);
  const line = xs.map((x, i) => `${i ? 'L' : 'M'}${x},${ys[i]}`).join('');
  return { line, area: `${line}L${xs[xs.length - 1]},${height}L0,${height}Z`, xs, ys };
}

/**
 * The session as plain text, for an issue or a chat.
 * @param info
 * @param client
 * @param latest
 * @returns
 */
export function streamReport(info: StreamInfo | null, client: StreamClient | null, latest: StreamSample | null): string {
  const words: StreamWords = { hardware: 'Hardware', software: 'Software', unknown: 'Unknown', throttled: 'The server is holding frames back',
    hardware_available: 'A hardware decoder is available for this stream',
    software_preferred: 'Software preferred after a decoder fallback' };
  const rows = streamRows(info, client, latest, words);
  const lines = rows.map((row) => `${row.key}: ${joined([row.value, row.detail])}${row.status === 'warn' ? ' [!]' : ''}`);
  lines.push(...rows.map((row) => row.reason).filter(Boolean).map((reason) => `  ${reason}`));
  if (latest) {
    const shown = ['fps', 'encoded_fps', 'mbps', 'rtt_ms'].filter((key) => typeof latest[key] === 'number')
      .map((key) => `${key} ${latest[key]}`);
    lines.push(joined(shown), ...streamTiles(latest, client ? client.transport : 'websockets').map((tile) =>
      `${tile.label}: ${tile.value}${tile.detail ? ` (${tile.detail})` : ''}${tile.warn ? ' [!]' : ''}`),
      ...streamMeters(latest).map((meter) => `${meter.key}: ${joined([meter.text, meter.detail])}`));
    if (latest.mic) lines.push(`mic: ${latest.mic}`);
    if (latest.webcam) lines.push(`webcam: ${latest.webcam}`);
  }
  lines.push(navigator.userAgent);
  return lines.filter(Boolean).join('\n');
}
