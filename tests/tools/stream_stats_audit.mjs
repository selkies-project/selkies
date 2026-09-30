/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a dashboard is told of the session, and when. The collector asks the
// server for the moving figures only while a dashboard has its stats open, never
// for a viewer, and again on a fresh connection; the history it publishes starts
// empty at each opening and grows a sample at a time, and every change is
// announced once. A row warns where the session fell short of what it asked for,
// never for a choice, for a server with no GPU, for a client with no hardware
// decoder, or for the path the network gave it, and the decoder is called
// software only on evidence, hardware where the engine has one and nothing said
// software.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

let announced = 0;
globalThis.window = { dispatchEvent: () => { announced++; } };
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

const { StreamStats, HISTORY_MAX, FIRST_SAMPLE_MS, webcodecsDecoder, webrtcDecoder } =
  await import('../../addons/selkies-web-core/lib/stream-stats.js');
const { streamRows, streamTiles, streamMeters, streamReport, seriesOf, graphPath, GRAPH_POINTS } =
  await import('../../addons/selkies-web-core/lib/stream-stats-view.js');
const { createPresentMeter, watchVideo, DELAY_SAMPLE_MS } = await import('../../addons/selkies-web-core/lib/present-meter.js');

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
check('the round trip the latency graph plots comes straight from the server', first.rtt_ms === 8,
  String(first.rtt_ms));
check('what arrived since the opening is counted as received', first.received_mb === 0.13, String(first.received_mb));
stats.noteBytes(875000);
stats.clientSample({ fps: 58 });
check('and the received count keeps growing from the opening', window.stream_stats.latest.received_mb === 1,
  String(window.stream_stats.latest.received_mb));
await settle();
announced = 0;
stats.setClient({ codec: 'h264', resolution: '1920x1080' });
stats.clientSample({ fps: 60 });
await settle();
check('changes made together are announced once', announced === 1, String(announced));
stats.setClient({ codec: 'h264', resolution: '1920x1080' });
await settle();
check('and a description that did not change is not announced', announced === 1, String(announced));
check('the first sample follows the opening within a second', FIRST_SAMPLE_MS > 0 && FIRST_SAMPLE_MS < 1000);
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

const words = { hardware: 'hardware', software: 'software', unknown: 'n/a', throttled: 'held back',
  hardware_available: 'engine has one', software_preferred: 'left after a failure' };
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
  && rowOf(rows, 'capture').status === 'neutral' && rowOf(rows, 'encoder').detail === 'No GPU exposed to the server'
  && rows.every((row) => row.key === 'decoder' || row.reason === ''));
check('and its report carries no reason for it', !streamReport(noGpu, client, null).includes('libcuda'));

const hardware = { ...info, encoder: 'NVENC', hardware: true, gpu: 'NVIDIA GeForce RTX 3060', driver: 'nvidia',
  encode_node: '/dev/dri/renderD128', zero_copy_available: true };
rows = streamRows(hardware, client, null, words);
check('a hardware encoder behind a readback where the server offered zero-copy warns on the capture, with why',
  rowOf(rows, 'encoder').status === 'good' && rowOf(rows, 'capture').status === 'warn'
  && rowOf(rows, 'capture').reason === info.capture_reason);
rows = streamRows({ ...hardware, zero_copy_available: false,
  capture_reason: 'NvFBC: the X server offers no NV-GLX; DRI3: the server has no DRI3' }, client, null, words);
check('a readback on a server offering no zero-copy path is how the host is built, and marks nothing',
  rowOf(rows, 'capture').status === 'neutral' && rowOf(rows, 'capture').reason === '');
check('the encoder row names the GPU and its node', rowOf(rows, 'encoder').detail.includes('RTX 3060')
  && rowOf(rows, 'encoder').detail.includes('renderD128'));

rows = streamRows({ ...hardware, zero_copy: true, capture: 'DRI3', capture_reason: '' }, client, null, words);
check('zero-copy is good', rowOf(rows, 'capture').status === 'good' && rowOf(rows, 'capture').value.includes('Zero-copy'));

rows = streamRows({ ...info, backend: 'wayland', capture: 'readback', renderer: 'pixman', renderer_reason: 'no render node' },
  client, null, words);
check('a Wayland session rendering in software that asked for hardware warns', rowOf(rows, 'capture').status === 'warn'
  && rowOf(rows, 'capture').reason === 'No render node');

rows = streamRows(hardware, { ...client, transport: 'webrtc', path: 'relay udp' }, null, words);
check('a relayed WebRTC path marks nothing', rowOf(rows, 'connection').status === 'neutral'
  && rowOf(rows, 'connection').value === 'WebRTC \u00b7 Relay UDP', rowOf(rows, 'connection').value);
rows = streamRows(hardware, { ...client, transport: 'webrtc', path: 'host tcp' }, null, words);
check('nor does one over TCP', rowOf(rows, 'connection').status === 'neutral');
rows = streamRows(hardware, { ...client, transport: 'webrtc', path: 'host udp' }, null, words);
check('a direct UDP one is good', rowOf(rows, 'connection').status === 'good');
rows = streamRows(hardware, client, { throttled: true }, words);
check('a server holding frames back warns on the connection', rowOf(rows, 'connection').status === 'warn'
  && rowOf(rows, 'connection').reason === 'held back');

const fallback = webcodecsDecoder({ forcedSoftware: true, hardwareSupported: true, format: 'NV12' });
check('a software preference after a fallback is software that fell short', fallback.decoder === 'software'
  && fallback.hardware_expected && fallback.decoder_reason === 'software_preferred');
const noHardware = webcodecsDecoder({ forcedSoftware: false, hardwareSupported: false, format: undefined });
check('no hardware decoder for the stream is software that fell short of nothing', noHardware.decoder === 'software'
  && !noHardware.hardware_expected && noHardware.decoder_reason === '');
rows = streamRows(null, { ...client, ...noHardware }, null, words);
check('and its row marks nothing', rowOf(rows, 'decoder').status === 'neutral' && rowOf(rows, 'decoder').reason === '');
const hinted = webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: 'I420' });
rows = streamRows(null, { ...client, ...hinted }, null, words);
check('planar frames mark nothing even where the engine accepts a hardware preference, a hint to some',
  hinted.decoder === 'software' && rowOf(rows, 'decoder').status === 'neutral');
rows = streamRows(null, { ...client, ...fallback }, null, words);
check('the fallback warns, with why, in the dashboard\'s words', rowOf(rows, 'decoder').status === 'warn'
  && rowOf(rows, 'decoder').reason === 'left after a failure');
check('and the report a user pastes words it in English',
  streamReport(null, { ...client, ...fallback }, null).includes('Software preferred after a decoder fallback'));
const unused = webrtcDecoder({ trackFormat: 'I420', capable: true });
rows = streamRows(null, { ...client, transport: 'webrtc', ...unused }, null, words);
check('a WebRTC stream decoding in software where the engine has an efficient decoder for it warns, with why',
  unused.decoder === 'software' && rowOf(rows, 'decoder').status === 'warn'
  && rowOf(rows, 'decoder').reason === 'engine has one');
check('NV12 and opaque frames are hardware', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: 'NV12' }).decoder === 'hardware'
  && webcodecsDecoder({ forcedSoftware: false, hardwareSupported: null, format: null }).decoder === 'hardware');
check('planar frames are software', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: 'I420' }).decoder === 'software');
check('no frame seen is unknown', webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: undefined }).decoder === 'unknown');
check('frames of one format whatever decodes them leave it to the engine\'s capability',
  webcodecsDecoder({ format: 'BGRX' }).decoder_evidence === 'BGRX frames'
  && webcodecsDecoder({ format: 'BGRX' }).decoder === 'unknown'
  && webcodecsDecoder({ format: 'BGRX', capable: false }).decoder === 'software'
  && webcodecsDecoder({ format: 'BGRX', capable: true }).decoder === 'hardware'
  && rowOf(streamRows(null, { ...client, ...webcodecsDecoder({ format: 'BGRX', capable: true }) }, null, words),
    'decoder').status === 'good');
// One rule on both transports: software falls short where the engine says it
// decodes the stream efficiently, whichever rung named the decoder.
const efficient = webcodecsDecoder({ forcedSoftware: false, hardwareSupported: true, format: 'I420', capable: true });
rows = streamRows(null, { ...client, ...efficient }, null, words);
check('WebSockets planar frames where the engine decodes the stream efficiently warn, with why, as WebRTC\'s do',
  efficient.decoder === 'software' && rowOf(rows, 'decoder').status === 'warn'
  && rowOf(rows, 'decoder').reason === 'engine has one');
const same = [];
for (const format of ['I420', 'I444', 'NV12', null, undefined]) {
  for (const capable of [true, false, null]) {
    const ws = webcodecsDecoder({ forcedSoftware: false, hardwareSupported: null, format, capable });
    const wr = webrtcDecoder({ trackFormat: format, capable });
    if (ws.decoder !== wr.decoder || ws.hardware_expected !== wr.hardware_expected
        || ws.decoder_reason !== wr.decoder_reason) same.push(`${format}/${capable}`);
  }
}
check('the same frames and engine answer give both transports the same verdict', same.length === 0, same.join(', '));
check('WebRTC takes the engine at its word', webrtcDecoder({ implementation: 'ExternalDecoder', powerEfficient: true }).decoder === 'hardware'
  && webrtcDecoder({ implementation: 'FFmpeg' }).decoder === 'software');
check('and where the engine withholds the decoder its capability is the word',
  webrtcDecoder({ implementation: 'unknown', capable: true }).decoder === 'hardware'
  && webrtcDecoder({ implementation: 'unknown', capable: null }).decoder === 'unknown');
check('a frame read from the track is the decoder\'s own',
  webrtcDecoder({ trackFormat: 'NV12', capable: true }).decoder === 'hardware'
  && webrtcDecoder({ trackFormat: null }).decoder === 'hardware'
  && webrtcDecoder({ trackFormat: 'I420', capable: false }).decoder === 'software');
check('the element\'s picture proves software alone, a GPU copy of it proving nothing',
  webrtcDecoder({ elementFormat: 'I420' }).decoder === 'software'
  && webrtcDecoder({ elementFormat: 'NV12' }).decoder === 'unknown'
  && webrtcDecoder({ elementFormat: null, capable: true }).decoder === 'hardware');

// What presents the picture is troubleshooting data in its own right: two engines
// decoding the same stream can differ only in the sink they allowed.
const presented = streamRows(null, {
  transport: 'websockets', decoder: 'hardware', decoder_evidence: 'NV12 frames', codec: 'h264',
  resolution: '1920x1080', path: '', sink: 'VideoTrackGenerator in the video worker',
  decode_path: '',
}, null, { hardware: 'hardware', software: 'software', unknown: 'unknown', throttled: '' })[2];
check('the decoder row names the sink that was taken',
  presented.detail.includes('VideoTrackGenerator in the video worker'), presented.detail);
const jpeg = streamRows(null, {
  transport: 'websockets', decoder: 'unknown', decoder_evidence: '', codec: 'jpeg',
  resolution: '1920x1080', path: '', sink: '2D canvas on the page',
  decode_path: 'ImageDecoder in the video worker',
}, null, { hardware: 'hardware', software: 'software', unknown: 'unknown', throttled: '' })[2];
check('and how a JPEG stripe was decoded, with where it ran',
  jpeg.detail.includes('ImageDecoder in the video worker'), jpeg.detail);

const latest = { encode_ms: 1.2, decode_ms: 2, encoded_kbps: 900, audio_dropped: 0, cpu_percent: 20, mem_used: 2 ** 30, mem_total: 2 ** 32, fps: 60 };
const tiles = streamTiles(latest, 'websockets');
check('the tiles are a fixed set that repeats no graph', tiles.map((tile) => tile.key).join()
  === 'encode_ms,pipeline_ms,decode_ms,present_ms,audio_buffer_ms,received_mb,lost_frames,frames_not_shown,keyframe_requests');
check('the round trip is not also a tile, the latency graph being where it is read',
  !tiles.some((tile) => tile.key === 'rtt_ms'));
check('a figure not measured this second reads as a dash', tiles[0].value === '1.2 ms' && tiles[1].value === '\u2013');
check('the same set stands before any sample', streamTiles(null, 'websockets').length === 9
  && streamTiles(null, 'websockets').every((tile) => tile.value === '\u2013'));
check('both transports show how long a frame took to the screen and how many never got there',
  ['websockets', 'webrtc'].every((transport) => ['present_ms', 'frames_not_shown']
    .every((key) => streamTiles(null, transport).some((tile) => tile.key === key))));
check('WebRTC adds what it measures of the link', streamTiles(null, 'webrtc').some((tile) => tile.key === 'packet_loss_percent'));
check('a memory meter reads as its amounts and carries no bar, the share going to the hover',
  streamMeters(latest)[1].text === '1.0 GiB / 4.0 GiB' && streamMeters(latest)[1].detail === '25%'
  && streamMeters(latest)[1].bar === false);
check('a utilization keeps its bar and reads as a share',
  streamMeters(latest)[0].text === '20%' && streamMeters(latest)[0].detail === '' && streamMeters(latest)[0].bar === true);
check('the GPU meters are left out without a GPU reading', streamMeters(latest).map((m) => m.key).join() === 'cpu,mem');
check('and shown with one, the utilizations grouped above the memories',
  streamMeters({ ...latest, gpu_percent: 5, gpu_mem_used: 1, gpu_mem_total: 2 })
    .map((meter) => meter.key).join() === 'cpu,gpu,mem,gpumem');
check('and the bars are the utilizations, so they do not alternate with the figures',
  streamMeters({ ...latest, gpu_percent: 5, gpu_mem_used: 1, gpu_mem_total: 2 })
    .map((meter) => meter.bar).join() === 'true,true,false,false');

const long = Array.from({ length: 500 }, (_, i) => ({ fps: i === 250 ? 144 : 60 }));
const series = seriesOf(long, 'fps');
check('a long history is bucketed down and keeps its peak', series.length === GRAPH_POINTS && Math.max(...series) === 144);
const path = graphPath([0, 30, 60], 240, 44, 60);
check('a graph grows from the left edge', path.xs[0] === 0 && path.xs[2] < 240 && path.ys[2] < path.ys[0], JSON.stringify(path.xs));

// The server's figures after an opening: the first reach the sample on screen,
// the later ones ride the next sample, so a dashboard reads once a sample.
const opening = new StreamStats({ transport: 'webrtc', send: () => {}, isViewer: () => false });
opening.setOpen(true);
opening.clientSample({ fps: 30 });
check('a sample taken before the server said anything carries none of its figures',
  !window.stream_stats.latest.server && window.stream_stats.latest.cpu_percent === undefined);
opening.serverSample({ cpu_percent: 40, encoded_fps: 30 });
check('the server\'s first figures after an opening reach the sample on screen at once',
  window.stream_stats.latest.cpu_percent === 40 && window.stream_stats.history.length === 1);
opening.serverSample({ cpu_percent: 50 });
check('and later ones wait for the next sample, which is the one read a dashboard pays for',
  window.stream_stats.latest.cpu_percent === 40);
opening.clientSample({ fps: 30 });
check('which carries them', window.stream_stats.latest.cpu_percent === 50 && window.stream_stats.latest.server);

// What reaches the screen: a canvas draw lands at its thread's next animation
// frame, and every draw ahead of the last one before that frame was replaced
// unseen; a <video> reports its own frames and drops.
const frames = [];
const meter = createPresentMeter((land) => frames.push(land));
const epoch = () => performance.timeOrigin + performance.now();
const arrived = epoch() - 5;
meter.drawn(arrived - 10);
meter.drawn(arrived);
check('draws wait for the next animation frame, asked for once', frames.length === 1
  && meter.take().presented === 0, String(frames.length));
frames.shift()();
let figures = meter.take();
check('which hands on the newest draw and counts the one before it as replaced', figures.presented === 1
  && figures.superseded === 1 && figures.delays === 1 && figures.delaySum >= 5 && figures.delaySum < 1000,
  JSON.stringify(figures));
check('a take starts the next count afresh', JSON.stringify(meter.take())
  === JSON.stringify({ presented: 0, delaySum: 0, delays: 0, superseded: 0, refused: 0 }));
meter.drawn(NaN);
meter.land();
figures = meter.take();
check('a draw landed by its own frame counts at once, and one of unknown arrival carries no delay',
  figures.presented === 1 && figures.delays === 0, JSON.stringify(figures));
frames.shift()();
check('and the frame asked for before it then finds nothing to count', meter.take().presented === 0);
meter.drawn(arrived);
meter.reset();
frames.shift()();
check('a reset forgets draws still waiting for their frame', meter.take().presented === 0);
const eager = createPresentMeter(null);
eager.drawn(arrived);
eager.drawn(arrived);
eager.refused();
figures = eager.take();
check('a thread without animation frames counts each draw as it is made', figures.presented === 2
  && figures.superseded === 0 && figures.refused === 1, JSON.stringify(figures));

const callbacks = [];
let cancelled = 0;
let quality = { totalVideoFrames: 10, droppedVideoFrames: 3 };
const video = {
  requestVideoFrameCallback: (cb) => { callbacks.push(cb); return callbacks.length; },
  cancelVideoFrameCallback: () => { cancelled++; },
  getVideoPlaybackQuality: () => quality,
};
const followed = createPresentMeter(null);
const watch = watchVideo(video, followed, (frame) => frame.presentationTime - frame.receiveTime);
callbacks.shift()(0, { presentedFrames: 10, presentationTime: 100, receiveTime: 90 });
callbacks.shift()(0, { presentedFrames: 13, presentationTime: 150, receiveTime: 146 });
figures = followed.take();
check('a <video> reports each frame\'s delay as it hands it on', figures.delays === 2 && figures.delaySum === 14
  && figures.presented === 0, JSON.stringify(figures));
quality = { totalVideoFrames: 130, droppedVideoFrames: 63 };
let read = watch.read();
check('what it showed is what it took less what a newer frame replaced, as Chromium counts both',
  read.shown === 60 && read.dropped === 60, JSON.stringify(read));
quality = { totalVideoFrames: 5, droppedVideoFrames: 1 };
read = watch.read();
check('a new source restarting the counters is read as a restart', read.shown === 4 && read.dropped === 1,
  JSON.stringify(read));
const liveOnly = { ...video, getVideoPlaybackQuality: () => ({ totalVideoFrames: 0, droppedVideoFrames: 0 }) };
const firefoxWatch = watchVideo(liveOnly, followed, () => NaN);
callbacks.shift();
callbacks.shift()(0, { presentedFrames: 40, presentationTime: 0 });
callbacks.shift()(0, { presentedFrames: 42, presentationTime: 0 });
check('an engine that keeps no playback quality for a live stream counts the frames the callback reports',
  firefoxWatch.read().shown === 3);
watch.stop();
check('stopping cancels the callback it had asked for', cancelled === 1 && watch.reported());
check('an engine without the callback never reports a frame',
  !watchVideo({}, followed, () => NaN).reported());
const sampled = [];
let counts = { totalVideoFrames: 100, droppedVideoFrames: 0 };
const counting = {
  requestVideoFrameCallback: (cb) => { sampled.push(cb); return sampled.length; },
  cancelVideoFrameCallback: () => {},
  getVideoPlaybackQuality: () => counts,
};
const spaced = createPresentMeter(null);
const countingWatch = watchVideo(counting, spaced, (frame) => frame.presentationTime - frame.receiveTime);
const spacing = () => new Promise((resolve) => setTimeout(resolve, DELAY_SAMPLE_MS + 20));
counts = { totalVideoFrames: 101, droppedVideoFrames: 0 };
sampled.shift()(0, { presentedFrames: 101, presentationTime: 100, receiveTime: 95 });
check('once the playback quality counts the frames, the next callback waits out the sample spacing',
  sampled.length === 0, String(sampled.length));
await spacing();
check('and is asked for after it', sampled.length === 1, String(sampled.length));
sampled.shift()(0, { presentedFrames: 107, presentationTime: 200, receiveTime: 193 });
figures = spaced.take();
check('the sampled frames carry their delays', figures.delays === 2 && figures.delaySum === 12,
  JSON.stringify(figures));
counts = { totalVideoFrames: 160, droppedVideoFrames: 2 };
read = countingWatch.read();
check('while what it showed stays the playback quality\'s count', read.shown === 58 && read.dropped === 2,
  JSON.stringify(read));
countingWatch.stop();
await spacing();
check('stopping drops a sample still waiting out the spacing', sampled.length === 0, String(sampled.length));

process.exit(failed ? 1 : 0);
