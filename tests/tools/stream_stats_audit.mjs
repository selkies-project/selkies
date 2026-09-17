/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a dashboard is told of the session, and when. The collector asks the
// server for the moving figures only while a dashboard has its stats open, never
// for a viewer, and again on a fresh connection; the history it publishes starts
// empty at each opening and grows a second at a time. A row warns where the
// session fell short of what it asked for, never for a choice and never for a
// server with no GPU, and the decoder is called hardware or software only on
// evidence.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

globalThis.window = {};

const { StreamStats, HISTORY_MAX, webcodecsDecoder, webrtcDecoder } =
  await import('../../addons/selkies-web-core/lib/stream-stats.js');
const { streamRows, streamTiles, streamMeters, streamReport, seriesOf, graphPath, GRAPH_POINTS } =
  await import('../../addons/selkies-web-core/lib/stream-stats-view.js');

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [stream-stats] ${label}  ${detail}`);
}

const sent = [];
let viewer = false;
let connected = true;
const opened = [];
const stats = new StreamStats({
  transport: 'websockets',
  send: (m) => { if (!connected) throw new Error('not connected'); sent.push(m); },
  isViewer: () => viewer,
  onOpenChange: (open) => opened.push(open),
});

check('nothing is asked of the server before a dashboard opens its stats', sent.length === 0);
stats.clientSample({ fps: 60 });
check('and nothing is sampled', window.stream_stats.history.length === 0);

stats.setOpen(true);
check('opening asks the server for the figures', sent.join() === '_stats,1', sent.join());
check('and tells the core to sample', opened.join() === 'true');
stats.setOpen(true);
check('opening twice asks once', sent.length === 1);

stats.serverSample({ cpu_percent: 12, encoded_fps: 59, rtt_ms: 8 });
stats.noteBytes(125000);
stats.clientSample({ fps: 58 });
const first = window.stream_stats.latest;
check('a sample merges the server and the page', first.cpu_percent === 12 && first.fps === 58 && first.encoded_fps === 59,
  JSON.stringify(first));
check('bandwidth is what arrived', first.mbps > 0, String(first.mbps));
for (let i = 0; i < HISTORY_MAX + 5; i++) stats.clientSample({ fps: i });
check('the history grows to its cap and no further', window.stream_stats.history.length === HISTORY_MAX);

stats.disconnected();
stats.subscribe();
check('a fresh connection is asked again', sent.join() === '_stats,1,_stats,1', sent.join());

stats.setOpen(false);
check('shutting tells the server to stop', sent[sent.length - 1] === '_stats,0');
check('and empties the history', window.stream_stats.history.length === 0 && window.stream_stats.latest === null);

viewer = true;
const before = sent.length;
stats.setOpen(true);
check('a viewer never subscribes', sent.length === before);
stats.setOpen(false);
viewer = false;

connected = false;
stats.setOpen(true);
connected = true;
stats.subscribe();
check('a subscription that could not be sent goes out once connected', sent[sent.length - 1] === '_stats,1');

const info = {
  backend: 'x11', capture: 'XShm', zero_copy: false, capture_reason: 'DRI3: no DRI3', encoder: 'x264',
  hardware: false, gpu_present: true, hardware_expected: true, encoder_reason: 'NVENC H264 did not open', codec: 'h264',
  fullcolor: false, striped: false, gpu: '', driver: '', encode_node: '',
};
stats.setInfo(info);
check('the description is published', window.stream_info === info);

const words = { hardware: 'hardware', software: 'software', unknown: 'n/a', throttled: 'held back' };
const client = { transport: 'websockets', decoder: 'hardware', decoder_evidence: 'NV12 frames', codec: 'h264', resolution: '1920x1080', path: '' };
const rowOf = (rows, key) => rows.find((row) => row.key === key);

let rows = streamRows(info, client, null, words);
check('software on a session that asked for hardware warns with the reason',
  rowOf(rows, 'encoder').status === 'warn' && rowOf(rows, 'encoder').reason === info.encoder_reason);
check('its readback is not a second warning', rowOf(rows, 'capture').status === 'neutral');

rows = streamRows({ ...info, hardware_expected: false }, client, null, words);
check('software somebody chose does not warn', rowOf(rows, 'encoder').status === 'neutral'
  && rowOf(rows, 'encoder').reason === '');

const noGpu = {
  ...info, backend: 'wayland', capture: 'readback', capture_reason: 'no GPU renderer', renderer: 'pixman',
  renderer_reason: 'no render node', gpu_present: false, hardware_expected: false,
  encoder_reason: 'NVENC H264 did not open: Could not load CUDA library (libcuda.so.1)',
};
rows = streamRows(noGpu, client, null, words);
check('a server with no GPU warns on nothing and says so', rowOf(rows, 'encoder').status === 'neutral'
  && rowOf(rows, 'capture').status === 'neutral' && rowOf(rows, 'encoder').detail === 'no GPU exposed to the server'
  && rows.every((row) => row.key === 'decoder' || row.reason === ''));
check('and its report carries no reason for it', !streamReport(noGpu, client, null).includes('libcuda'));

const hardware = { ...info, encoder: 'NVENC', hardware: true, gpu: 'NVIDIA GeForce RTX 3060', driver: 'nvidia', encode_node: '/dev/dri/renderD128' };
rows = streamRows(hardware, client, null, words);
check('a hardware encoder behind a readback warns on the capture, with why',
  rowOf(rows, 'encoder').status === 'good' && rowOf(rows, 'capture').status === 'warn'
  && rowOf(rows, 'capture').reason === info.capture_reason);
check('the encoder row names the GPU and its node', rowOf(rows, 'encoder').detail.includes('RTX 3060')
  && rowOf(rows, 'encoder').detail.includes('renderD128'));

rows = streamRows({ ...hardware, zero_copy: true, capture: 'DRI3', capture_reason: '' }, client, null, words);
check('zero-copy is good', rowOf(rows, 'capture').status === 'good' && rowOf(rows, 'capture').value.includes('zero-copy'));

rows = streamRows({ ...info, backend: 'wayland', capture: 'readback', renderer: 'pixman', renderer_reason: 'no render node' },
  client, null, words);
check('a Wayland session rendering in software that asked for hardware warns', rowOf(rows, 'capture').status === 'warn'
  && rowOf(rows, 'capture').reason === 'no render node');

rows = streamRows(hardware, { ...client, transport: 'webrtc', path: 'relay udp relay' }, null, words);
check('a relayed WebRTC path warns', rowOf(rows, 'connection').status === 'warn');
rows = streamRows(hardware, { ...client, transport: 'webrtc', path: 'host udp' }, null, words);
check('a direct one is good', rowOf(rows, 'connection').status === 'good');
rows = streamRows(hardware, client, { throttled: true }, words);
check('a server holding frames back warns on the connection', rowOf(rows, 'connection').status === 'warn'
  && rowOf(rows, 'connection').reason === 'held back');

check('a software preference is software', webcodecsDecoder({ forcedSoftware: true, hardwareSupported: true, format: 'NV12' }).decoder === 'software');
check('no hardware decoder for the stream is software', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: false, format: undefined }).decoder === 'software');
check('NV12 and opaque frames are hardware', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: 'NV12' }).decoder === 'hardware'
  && webcodecsDecoder({ forcedSoftware: false, hardwareSupported: null, format: null }).decoder === 'hardware');
check('planar frames are software', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: 'I420' }).decoder === 'software');
check('no frame seen is unknown', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: undefined }).decoder === 'unknown');
check('WebRTC takes the engine at its word', webrtcDecoder({ implementation: 'ExternalDecoder', powerEfficient: true }).decoder === 'hardware'
  && webrtcDecoder({ implementation: 'FFmpeg' }).decoder === 'software');
check('and claims nothing where the engine withholds the decoder',
  webrtcDecoder({ implementation: 'unknown', capable: true }).decoder === 'unknown');

const latest = { encode_ms: 1.2, decode_ms: 2, encoded_kbps: 900, audio_dropped: 0, cpu_percent: 20, mem_used: 2 ** 30, mem_total: 2 ** 32, fps: 60 };
const tiles = streamTiles(latest, 'websockets');
check('the tiles are a fixed set that repeats no graph', tiles.map((tile) => tile.key).join() === 'encode_ms,pipeline_ms,decode_ms,audio_buffer_ms');
check('a figure not measured this second reads as a dash', tiles[0].value === '1.2 ms' && tiles[1].value === '\u2013');
check('the same set stands before any sample', streamTiles(null, 'websockets').length === 4
  && streamTiles(null, 'websockets').every((tile) => tile.value === '\u2013'));
check('WebRTC adds what it measures of the link', streamTiles(null, 'webrtc').some((tile) => tile.key === 'packet_loss_percent'));
check('a memory meter reads as a share and keeps the amounts for the hover',
  streamMeters(latest)[1].text === '25%' && streamMeters(latest)[1].detail === '1.0 GiB / 4.0 GiB' && streamMeters(latest)[0].detail === '');
check('the GPU meters are left out without a GPU reading', streamMeters(latest).map((m) => m.key).join() === 'cpu,mem');
check('and shown with one', streamMeters({ ...latest, gpu_percent: 5, gpu_mem_used: 1, gpu_mem_total: 2 }).length === 4);

const long = Array.from({ length: 500 }, (_, i) => ({ fps: i === 250 ? 144 : 60 }));
const series = seriesOf(long, 'fps');
check('a long history is bucketed down and keeps its peak', series.length === GRAPH_POINTS && Math.max(...series) === 144);
const path = graphPath([0, 30, 60], 240, 44, 60);
check('a graph grows from the left edge', path.xs[0] === 0 && path.xs[2] < 240 && path.ys[2] < path.ys[0], JSON.stringify(path.xs));

process.exit(failed ? 1 : 0);
