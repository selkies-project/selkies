/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What both dashboards draw of `lib/stream-stats.js`'s contract, kept free of
 * any framework so the two cannot read the same session differently: the four
 * status rows and what makes one a warning, the figures under the graphs, the
 * host meters, the geometry of a graph, and the plain-text report a user pastes
 * into an issue.
 *
 * A row warns where the session fell short of what it asked for, never for a
 * choice and never for a server with no GPU, which is an ordinary deployment:
 * software encoding somebody selected, or on a host exposed no GPU, is neutral,
 * and software encoding on a session that asked for hardware where there is a
 * GPU is a warning and carries pixelflux's reason.
 * Every value opens with a capital, and names the system reports (`nvidia`,
 * `renderD128`, a decoder's own) are kept as reported. Technical values (`NVENC`,
 * `Zero-copy`, `renderD128`) are not translated; the words a dashboard does
 * translate arrive in `words`.
 *
 * @module
 */

/**
 * @typedef {import('./stream-stats.js').StreamInfo} StreamInfo
 * @typedef {import('./stream-stats.js').StreamClient} StreamClient
 * @typedef {import('./stream-stats.js').StreamSample} StreamSample
 */

/**
 * @typedef {Object} StreamRow One status row.
 * @property {'encoder'|'capture'|'decoder'|'connection'} key
 * @property {'good'|'warn'|'neutral'} status
 * @property {string} value The headline.
 * @property {string} detail The quieter second line, may be empty.
 * @property {string} reason Why the row warns, empty unless it does.
 */

/**
 * @typedef {Object} StreamWords The translated words the rows use.
 * @property {string} hardware
 * @property {string} software
 * @property {string} unknown
 * @property {string} throttled Said when the server holds frames back.
 */

const CODEC_NAMES = { h264: 'H.264', h265: 'H.265', hevc: 'H.265', av1: 'AV1', vp8: 'VP8', vp9: 'VP9', jpeg: 'JPEG' };

/** @param {string} codec @returns {string} */
const codecName = (codec) => CODEC_NAMES[String(codec || '').toLowerCase()] || String(codec || '').toUpperCase();
/** @param {string} path @returns {string} */
const nodeName = (path) => String(path || '').split('/').pop() || '';
/** @param {Array<string|undefined|null|false>} parts @returns {string} */
const joined = (parts) => parts.filter(Boolean).join(' · ');
/** @param {string|undefined} text Prose the server or an engine phrased. @returns {string} */
const sentence = (text) => (text ? text[0].toUpperCase() + text.slice(1) : '');

/**
 * @param {StreamInfo|null} info
 * @returns {StreamRow}
 */
function encoderRow(info) {
  if (!info) return { key: 'encoder', status: 'neutral', value: '', detail: '', reason: '' };
  const video = info.codec !== 'jpeg';
  const fell = !info.hardware && info.hardware_expected;
  return {
    key: 'encoder',
    status: info.hardware ? 'good' : fell ? 'warn' : 'neutral',
    value: joined([info.encoder, `${codecName(info.codec)}${video ? (info.fullcolor ? ' 4:4:4' : ' 4:2:0') : ''}`,
      info.striped && 'Striped']),
    detail: info.hardware ? joined([info.gpu, nodeName(info.encode_node), info.driver])
      : info.gpu_present === false ? 'No GPU exposed to the server' : '',
    reason: fell ? sentence(info.encoder_reason) : '',
  };
}

/**
 * @param {StreamInfo|null} info
 * @returns {StreamRow}
 */
function captureRow(info) {
  if (!info) return { key: 'capture', status: 'neutral', value: '', detail: '', reason: '' };
  const wayland = info.backend === 'wayland';
  const software = wayland && info.renderer === 'pixman';
  const path = info.zero_copy ? `Zero-copy${wayland ? '' : ` (${info.capture})`}` : `Readback${wayland ? '' : ` (${info.capture})`}`;
  const readbackToGpu = !info.zero_copy && info.hardware;
  const renderedInSoftware = software && info.hardware_expected;
  return {
    key: 'capture',
    status: info.zero_copy ? 'good' : (readbackToGpu || renderedInSoftware) ? 'warn' : 'neutral',
    value: joined([wayland ? 'Wayland' : 'X11', path]),
    detail: !wayland ? '' : software ? 'Pixman (software rendering)'
      : joined([`GL on ${nodeName(info.render_node)}`, info.render_gpu]),
    reason: sentence(renderedInSoftware ? info.renderer_reason : readbackToGpu ? info.capture_reason : ''),
  };
}

/**
 * @param {StreamClient|null} client
 * @param {StreamWords} words
 * @returns {StreamRow}
 */
function decoderRow(client, words) {
  const decoder = client ? client.decoder : 'unknown';
  return {
    key: 'decoder',
    status: decoder === 'hardware' ? 'good' : decoder === 'software' ? 'warn' : 'neutral',
    value: words[decoder] || words.unknown,
    detail: client
      ? joined([client.decoder_evidence, codecName(client.codec), client.resolution,
        client.decode_path, client.sink])
      : '',
    reason: '',
  };
}

/**
 * @param {StreamClient|null} client
 * @param {StreamSample|null} latest
 * @param {StreamWords} words
 * @returns {StreamRow}
 */
function connectionRow(client, latest, words) {
  const webrtc = !!client && client.transport === 'webrtc';
  const path = (client && client.path) || '';
  const indirect = /relay|tcp/.test(path);
  const throttled = !!(latest && latest.throttled);
  return {
    key: 'connection',
    status: throttled || indirect ? 'warn' : webrtc && path ? 'good' : 'neutral',
    value: joined([webrtc ? 'WebRTC' : 'WebSockets', sentence(path.replace(/\b(udp|tcp|tls)\b/g, (p) => p.toUpperCase()))]),
    detail: '',
    reason: throttled ? words.throttled : '',
  };
}

/**
 * The four status rows, in display order.
 * @param {StreamInfo|null} info
 * @param {StreamClient|null} client
 * @param {StreamSample|null} latest
 * @param {StreamWords} words
 * @returns {StreamRow[]}
 */
export function streamRows(info, client, latest, words) {
  return [encoderRow(info), captureRow(info), decoderRow(client, words), connectionRow(client, latest, words)];
}

/** The figures under the graphs by sample key: the label and the unit. */
const TILES = {
  encode_ms: ['Encode', 'ms'],
  pipeline_ms: ['Capture to encoded', 'ms'],
  decode_ms: ['Decode', 'ms'],
  jitter_buffer_ms: ['Jitter buffer', 'ms'],
  audio_buffer_ms: ['Audio buffer', 'ms'],
  packet_loss_percent: ['Packet loss', '%'],
  frames_dropped: ['Frames dropped', ''],
  freezes: ['Freezes', ''],
  nacks: ['NACKs', ''],
  keyframe_requests: ['Key frame requests', ''],
};

/** The figures each transport shows, in order. */
const TRANSPORT_TILES = {
  websockets: ['encode_ms', 'pipeline_ms', 'decode_ms', 'audio_buffer_ms'],
  webrtc: ['encode_ms', 'pipeline_ms', 'decode_ms', 'jitter_buffer_ms', 'audio_buffer_ms',
    'packet_loss_percent',
    'frames_dropped', 'freezes', 'nacks', 'keyframe_requests'],
};

/**
 * The figures under the graphs: one fixed set per transport, so the layout
 * holds still. A figure with nothing measured this second, an encode time on
 * an idle screen, reads as a dash rather than leaving.
 * @param {StreamSample|null} latest
 * @param {'websockets'|'webrtc'} transport
 * @returns {Array<{key: string, label: string, value: string}>}
 */
export function streamTiles(latest, transport) {
  return (TRANSPORT_TILES[transport] || TRANSPORT_TILES.websockets).map((key) => {
    const [label, unit] = TILES[key];
    const measured = latest && typeof latest[key] === 'number';
    return { key, label, value: measured ? `${latest[key]}${unit === '%' ? '%' : unit ? ` ${unit}` : ''}` : '\u2013' };
  });
}

/** @param {number} bytes @returns {string} */
const gib = (bytes) => `${(bytes / 1073741824).toFixed(1)} GiB`;

/**
 * The host meters: the utilizations first and the memories under them, so the
 * bars read as one block rather than alternating with the figures. A utilization
 * carries a bar, which is what a share of a whole reads well as; a memory pair
 * carries its amounts as the figure and no bar, because how much of how much is
 * what an operator needs and a bar leaves it to a hover no touch screen has. The
 * GPU rows are left out where the server reads no GPU.
 * @param {StreamSample|null} latest
 * @returns {Array<{key: ('cpu'|'mem'|'gpu'|'gpumem'), percent: number, text: string, detail: string, bar: boolean}>}
 */
export function streamMeters(latest) {
  if (!latest || typeof latest.cpu_percent !== 'number') return [];
  const share = (used, total) => (total > 0 ? Math.min(100, (100 * used) / total) : 0);
  const used = (key, percent) => ({ key, percent, text: `${Math.round(percent)}%`, detail: '', bar: true });
  const amounts = (key, gotten, total) => ({
    key,
    percent: share(gotten, total),
    text: `${gib(gotten)} / ${gib(total)}`,
    detail: `${Math.round(share(gotten, total))}%`,
    bar: false,
  });
  const meters = [used('cpu', Math.min(100, latest.cpu_percent))];
  if (typeof latest.gpu_percent === 'number') {
    meters.push(used('gpu', Math.min(100, latest.gpu_percent)));
  }
  meters.push(amounts('mem', latest.mem_used, latest.mem_total));
  if (typeof latest.gpu_percent === 'number' && latest.gpu_mem_total > 0) {
    meters.push(amounts('gpumem', latest.gpu_mem_used, latest.gpu_mem_total));
  }
  return meters;
}

/** Points a graph draws at most; a longer history is bucketed down to it by maximum. */
export const GRAPH_POINTS = 120;

/**
 * One series of a history, bucketed down to `GRAPH_POINTS` so a graph that has
 * grown for ten minutes costs what a short one does. A bucket keeps its
 * maximum, since a graph of a rate is read for its peaks.
 * @param {StreamSample[]} history
 * @param {string} key
 * @returns {number[]}
 */
export function seriesOf(history, key) {
  const values = history.map((sample) => (typeof sample[key] === 'number' ? sample[key] : 0));
  if (values.length <= GRAPH_POINTS) return values;
  const size = values.length / GRAPH_POINTS;
  return Array.from({ length: GRAPH_POINTS }, (_, i) =>
    Math.max(...values.slice(Math.floor(i * size), Math.max(Math.floor(i * size) + 1, Math.floor((i + 1) * size)))));
}

/**
 * A graph's SVG geometry. The history grows from the left edge until it fills
 * the width, the way it reads while the stats stay open.
 * @param {number[]} values
 * @param {number} width
 * @param {number} height
 * @param {number} max The value at the top edge.
 * @returns {{line: string, area: string, xs: number[], ys: number[]}} Path data
 *     for the stroke and the fill under it, and each point's coordinates.
 */
export function graphPath(values, width, height, max) {
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
 * @param {StreamInfo|null} info
 * @param {StreamClient|null} client
 * @param {StreamSample|null} latest
 * @returns {string}
 */
export function streamReport(info, client, latest) {
  const words = { hardware: 'Hardware', software: 'Software', unknown: 'Unknown', throttled: 'The server is holding frames back' };
  const rows = streamRows(info, client, latest, words);
  const lines = rows.map((row) => `${row.key}: ${joined([row.value, row.detail])}${row.status === 'warn' ? ' [!]' : ''}`);
  lines.push(...rows.map((row) => row.reason).filter(Boolean).map((reason) => `  ${reason}`));
  if (latest) {
    const shown = ['fps', 'encoded_fps', 'mbps', 'rtt_ms'].filter((key) => typeof latest[key] === 'number')
      .map((key) => `${key} ${latest[key]}`);
    lines.push(joined(shown), ...streamTiles(latest, client ? client.transport : 'websockets').map((tile) => `${tile.label}: ${tile.value}`),
      ...streamMeters(latest).map((meter) => `${meter.key}: ${joined([meter.text, meter.detail])}`));
    if (latest.mic) lines.push(`mic: ${latest.mic}`);
    if (latest.webcam) lines.push(`webcam: ${latest.webcam}`);
  }
  lines.push(navigator.userAgent);
  return lines.filter(Boolean).join('\n');
}
