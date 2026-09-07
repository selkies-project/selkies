/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the video wire says about its codec, and the WebCodecs codec string a
 * stream's own key frame declares.
 *
 * A `0x04` frame's type byte carries the frame kind in its low nibble (`1` = a
 * decode entry point) and the codec id in its high nibble. The codec string a
 * `VideoDecoder` is configured with is read from the key frame's parameter sets
 * wherever the codec has them (the H.264 SPS, the H.265 SPS, the AV1 sequence
 * header), so it always matches the bitstream; VP8 has one string, and VP9's
 * level, which its bitstream never carries, is derived from the geometry.
 *
 * Every function here is also injected into the video worker by `toString()`,
 * so they reference nothing but each other and stay free of module state.
 */

/** Codec names by wire id. */
export const WIRE_CODECS = { 1: 'h264', 2: 'vp8', 3: 'vp9', 4: 'av1', 5: 'h265' };

/** Encoder wire values and the codec each streams. */
export const ENCODER_CODECS = {
  jpeg: 'jpeg',
  h264enc: 'h264',
  'h264enc-striped': 'h264',
  h265enc: 'h265',
  vp8enc: 'vp8',
  vp9enc: 'vp9',
  av1enc: 'av1',
};

/**
 * @param {number} typeByte The second byte of a video frame's wire header.
 * @returns {string} The codec name; an id no codec has reads as H.264.
 */
export const wireCodecName = (typeByte) => WIRE_CODECS[typeByte >> 4] || 'h264';

/**
 * @param {number} typeByte The second byte of a video frame's wire header.
 * @returns {boolean} Whether the frame is a decode entry point.
 */
export const wireFrameIsKey = (typeByte) => (typeByte & 0x0f) === 0x01;

/**
 * @param {string} encoder An encoder wire value.
 * @returns {string} The codec it streams; an unknown encoder reads as H.264.
 */
export const codecOfEncoder = (encoder) => ENCODER_CODECS[encoder] || 'h264';

/**
 * @param {string} codec A codec name.
 * @returns {boolean} Whether the codec can carry 4:4:4 chroma.
 */
export const codecCarriesFullColor = (codec) => codec === 'h264' || codec === 'h265';

/**
 * The NAL units of an Annex-B buffer, each without its start code.
 * @param {Uint8Array} bytes
 * @returns {Uint8Array[]}
 */
export const annexbNals = (bytes) => {
  const nals = [];
  const n = bytes.length;
  let start = -1;
  for (let i = 0; i + 2 < n; i++) {
    if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) {
      if (start >= 0) {
        let end = i;
        while (end > start && bytes[end - 1] === 0) end--;
        if (end > start) nals.push(bytes.subarray(start, end));
      }
      start = i + 3;
      i += 2;
    }
  }
  if (start >= 0 && start < n) nals.push(bytes.subarray(start, n));
  return nals;
};

/**
 * A bit reader over an RBSP: `bytes` from `offset` with emulation prevention
 * bytes removed, `limit` bytes at most.
 * @param {Uint8Array} bytes
 * @param {number} offset
 * @param {number} limit
 * @returns {{u: (n: number) => number, skip: (n: number) => void}}
 */
export const rbspReader = (bytes, offset, limit) => {
  const data = [];
  let zeros = 0;
  for (let i = offset; i < bytes.length && data.length < limit; i++) {
    const b = bytes[i];
    if (zeros >= 2 && b === 3) { zeros = 0; continue; }
    zeros = b === 0 ? zeros + 1 : 0;
    data.push(b);
  }
  let pos = 0;
  const u = (n) => {
    let v = 0;
    for (let i = 0; i < n; i++) {
      const byte = data[pos >> 3];
      const bit = byte === undefined ? 0 : (byte >> (7 - (pos & 7))) & 1;
      v = v * 2 + bit;
      pos++;
    }
    return v;
  };
  return { u, skip: (n) => { pos += n; } };
};

/**
 * `avc1.PPCCLL` from the first SPS of an H.264 Annex-B key frame.
 * @param {Uint8Array} bytes
 * @returns {string|null} `null` when no SPS is found.
 */
export const parseAvcCodecFromAnnexB = (bytes) => {
  if (!bytes || bytes.length < 5) return null;
  const hex2 = (n) => n.toString(16).toUpperCase().padStart(2, '0');
  for (const nal of annexbNals(bytes)) {
    if ((nal[0] & 0x80) === 0 && (nal[0] & 0x1f) === 7) {
      // profile_idc, constraint flags and level_idc are the first three RBSP
      // bytes and, with profile_idc always >= 66, never need emulation prevention.
      if (nal.length < 4) return null;
      return `avc1.${hex2(nal[1])}${hex2(nal[2])}${hex2(nal[3])}`;
    }
  }
  return null;
};

/**
 * `hev1.P.C.TL.CC` from the first SPS of an H.265 Annex-B key frame: the
 * general profile, its compatibility flags as the reversed-bit hexadecimal the
 * codecs registration specifies, the tier and level, and the constraint bytes
 * with trailing zero bytes dropped.
 * @param {Uint8Array} bytes
 * @returns {string|null} `null` when no SPS is found.
 */
export const parseHevcCodecFromAnnexB = (bytes) => {
  if (!bytes || bytes.length < 5) return null;
  for (const nal of annexbNals(bytes)) {
    if (((nal[0] >> 1) & 0x3f) !== 33 || nal.length < 16) continue;
    const r = rbspReader(nal, 2, 32);
    r.skip(4 + 3 + 1);
    const profileSpace = r.u(2);
    const tier = r.u(1);
    const profile = r.u(5);
    let compat = 0;
    for (let j = 0; j < 32; j++) compat |= r.u(1) << j;
    const constraints = [];
    for (let j = 0; j < 6; j++) constraints.push(r.u(8));
    const level = r.u(8);
    while (constraints.length > 1 && constraints[constraints.length - 1] === 0) constraints.pop();
    const space = ['', 'A', 'B', 'C'][profileSpace];
    const flags = (compat >>> 0).toString(16).toUpperCase();
    const bytesHex = constraints.map((b) => b.toString(16).toUpperCase()).join('.');
    return `hev1.${space}${profile}.${flags}.${tier ? 'H' : 'L'}${level}.${bytesHex}`;
  }
  return null;
};

/**
 * `av01.P.LLT.08` from the sequence header of an AV1 temporal unit: the profile,
 * the first operating point's level and tier. The encoders here emit 8-bit
 * 4:2:0, which the fixed bit-depth field states.
 * @param {Uint8Array} bytes
 * @returns {string|null} `null` when no sequence header is found.
 */
export const parseAv1CodecFromObus = (bytes) => {
  if (!bytes || bytes.length < 2) return null;
  let pos = 0;
  while (pos < bytes.length) {
    const header = bytes[pos];
    const obuType = (header >> 3) & 0x0f;
    const hasExtension = (header & 0x04) !== 0;
    const hasSize = (header & 0x02) !== 0;
    let i = pos + 1 + (hasExtension ? 1 : 0);
    let size = bytes.length - i;
    if (hasSize) {
      size = 0;
      let shift = 0;
      for (;;) {
        if (i >= bytes.length || shift > 28) return null;
        const b = bytes[i++];
        size += (b & 0x7f) * (2 ** shift);
        if ((b & 0x80) === 0) break;
        shift += 7;
      }
    }
    if (obuType === 1) {
      const r = rbspReader(bytes, i, Math.min(size, 64));
      const profile = r.u(3);
      r.skip(1);
      const reduced = r.u(1);
      let level = 0;
      let tier = 0;
      if (reduced) {
        level = r.u(5);
      } else {
        const timingInfo = r.u(1);
        let decoderModel = 0;
        let bufferDelayBits = 0;
        if (timingInfo) {
          r.skip(64);
          if (r.u(1)) {
            let leading = 0;
            while (r.u(1) === 0 && leading < 32) leading++;
            r.skip(leading);
          }
          decoderModel = r.u(1);
          if (decoderModel) {
            bufferDelayBits = r.u(5) + 1;
            r.skip(32 + 5 + 5);
          }
        }
        const displayDelay = r.u(1);
        r.skip(5);
        r.skip(12);
        level = r.u(5);
        tier = level > 7 ? r.u(1) : 0;
        if (decoderModel && r.u(1)) r.skip(bufferDelayBits * 2 + 1);
        if (displayDelay && r.u(1)) r.skip(4);
      }
      return `av01.${profile}.${String(level).padStart(2, '0')}${tier ? 'H' : 'M'}.08`;
    }
    pos = i + size;
  }
  return null;
};

/**
 * The VP9 profile a frame's uncompressed header declares.
 * @param {Uint8Array} bytes
 * @returns {number}
 */
export const parseVp9Profile = (bytes) => {
  if (!bytes || bytes.length < 1) return 0;
  const b = bytes[0];
  return ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
};

/**
 * The lowest VP9 level whose luma sample rate and picture size admit a stream,
 * as the two digits of the codec string.
 * @param {number} width
 * @param {number} height
 * @param {number} fps
 * @returns {string}
 */
export const vp9Level = (width, height, fps) => {
  const size = width * height;
  const rate = size * (fps > 0 ? fps : 60);
  const levels = [
    ['10', 829440, 36864], ['11', 2764800, 73728], ['20', 4608000, 122880],
    ['21', 9216000, 245760], ['30', 20736000, 552960], ['31', 36864000, 983040],
    ['40', 83558400, 2228224], ['41', 160432128, 2228224], ['50', 311951360, 8912896],
    ['51', 588251136, 8912896], ['52', 1176502272, 8912896], ['60', 1176502272, 35651584],
    ['61', 2353004544, 35651584], ['62', 4706009088, 35651584],
  ];
  for (const [level, maxRate, maxSize] of levels) {
    if (rate <= maxRate && size <= maxSize) return level;
  }
  return '62';
};

/**
 * The lowest AV1 level admitting a stream, as `seq_level_idx`; used only when a
 * key frame's sequence header is unreadable. MaxPicSize is its own Annex A limit,
 * far below the product of the axis maxima, since no level admits a picture that
 * is both its widest and its tallest.
 * @param {number} width
 * @param {number} height
 * @param {number} fps
 * @returns {number}
 */
export const av1LevelIdx = (width, height, fps) => {
  const size = width * height;
  const rate = size * (fps > 0 ? fps : 60);
  const levels = [
    [8, 2359296, 6144, 3456, 70778880], [9, 2359296, 6144, 3456, 141557760],
    [12, 8912896, 8192, 4352, 267386880], [13, 8912896, 8192, 4352, 534773760],
    [14, 8912896, 8192, 4352, 1069547520], [15, 8912896, 8192, 4352, 1069547520],
    [16, 35651584, 16384, 8704, 1069547520], [17, 35651584, 16384, 8704, 2139095040],
    [18, 35651584, 16384, 8704, 4278190080], [19, 35651584, 16384, 8704, 4278190080],
  ];
  for (const [idx, maxSize, maxW, maxH, maxRate] of levels) {
    if (size <= maxSize && width <= maxW && height <= maxH && rate <= maxRate) return idx;
  }
  return 19;
};

/**
 * Pre-stream guess of the H.264 codec string. Decoder creation re-derives the
 * exact codec from the first key frame's SPS, and outside Chromium only a
 * conservative baseline is guessed because Safari rejects a stream whose real
 * profile or level exceeds the configured one.
 * @param {number} width
 * @param {number} height
 * @param {boolean} is444 Whether the stream is 4:4:4 full-color.
 * @param {number} fps
 * @param {boolean} chromium Whether the engine takes a High profile guess.
 * @returns {string}
 */
export const guessAvcCodec = (width, height, is444, fps, chromium) => {
  if (!chromium) return 'avc1.42E01E';
  const effFps = (typeof fps === 'number' && fps > 0) ? fps : 60;
  const pixelsPerSecond = width * height * effFps;
  // The encoders' emitted profile_idc: High (0x64) for 4:2:0, High 4:4:4 (0xF4) for 4:4:4.
  const profile = is444 ? 'F400' : '6400';
  // Floored at level 5.2 (0x34), the encoders' emitted level, so the first
  // key frame does not trigger a level-only reconfigure.
  let level;
  if (pixelsPerSecond <= 3840 * 2160 * 60) level = '34';
  else if (pixelsPerSecond <= 7680 * 4320 * 30) level = '3C';
  else if (pixelsPerSecond <= 7680 * 4320 * 60) level = '3D';
  else level = '3E';
  return `avc1.${profile}${level}`;
};

/**
 * The WebCodecs codec string of a stream, from its key frame where the codec
 * declares it and from the geometry otherwise.
 * @param {string} codec The wire codec name.
 * @param {Uint8Array|null} keyframe The key frame's payload, or `null` for a delta.
 * @param {number} width
 * @param {number} height
 * @param {number} fps
 * @param {boolean} is444
 * @param {boolean} chromium
 * @returns {string}
 */
export const codecStringFor = (codec, keyframe, width, height, fps, is444, chromium) => {
  switch (codec) {
    case 'h265':
      return (keyframe && parseHevcCodecFromAnnexB(keyframe)) ||
        (is444 ? 'hev1.4.10.L153.9E.8' : 'hev1.1.6.L153.B0');
    case 'vp8':
      return 'vp8';
    case 'vp9':
      return `vp09.0${keyframe ? parseVp9Profile(keyframe) : 0}.${vp9Level(width, height, fps)}.08`;
    case 'av1':
      return (keyframe && parseAv1CodecFromObus(keyframe)) ||
        `av01.0.${String(av1LevelIdx(width, height, fps)).padStart(2, '0')}M.08`;
    default:
      return (keyframe && parseAvcCodecFromAnnexB(keyframe)) ||
        guessAvcCodec(width, height, is444, fps, chromium);
  }
};

/**
 * The colour space a decoder is told to assume where the bitstream does not
 * settle it: VP8 carries no colour signalling, and Chromium's VP9 decoder does
 * not read the matrix from the frame header. The server converts both with the
 * BT.601 matrix every WebRTC receiver assumes, and the WebCodecs decoder is told
 * the same; the other codecs declare their matrix and are left to it.
 * @param {string} codec The wire codec name or WebCodecs codec string.
 * @returns {VideoColorSpaceInit|undefined}
 */
export const decoderColorSpace = (codec) =>
  (codec === 'vp8' || codec === 'vp9' || codec.startsWith('vp09'))
    ? { primaries: 'bt709', transfer: 'bt709', matrix: 'smpte170m', fullRange: false } : undefined;

/**
 * Representative decoder configurations, one per codec at the profile and
 * level the encoders emit for a 720p stream, for asking an engine what it can
 * decode before any stream exists.
 */
/**
 * A 16x16 Baseline key frame in Annex B form, for asking a decoder whether it
 * takes H.264 without an `avcC` description.
 */
export const H264_ANNEXB_SAMPLE = 'AAAAAWdCwArd7ARAAAADAEAAAA8DxIngAAAAAWjOD8gAAAABZYiEOiYoDg==';

/**
 * The `avcC` record of an Annex B key frame's SPS and PPS, for a decoder that
 * takes H.264 only with a description; null when either is missing.
 * @param {Uint8Array} bytes
 * @returns {Uint8Array|null}
 */
export const avcDescription = (bytes) => {
  let sps = null;
  let pps = null;
  for (const nal of annexbNals(bytes)) {
    const type = nal[0] & 0x1f;
    if (type === 7 && !sps) sps = nal;
    else if (type === 8 && !pps) pps = nal;
  }
  if (!sps || !pps) return null;
  const out = new Uint8Array(11 + sps.length + pps.length);
  out.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps.length >> 8, sps.length & 0xff], 0);
  out.set(sps, 8);
  out.set([1, pps.length >> 8, pps.length & 0xff], 8 + sps.length);
  out.set(pps, 11 + sps.length);
  return out;
};

/**
 * An Annex B access unit as length-prefixed NAL units, the framing an `avcC`
 * description implies, without its parameter sets and delimiters.
 * @param {Uint8Array} bytes
 * @returns {Uint8Array}
 */
export const annexbToAvcc = (bytes) => {
  const nals = annexbNals(bytes).filter((nal) => {
    const type = nal[0] & 0x1f;
    return type !== 7 && type !== 8 && type !== 9;
  });
  let size = 0;
  for (const nal of nals) size += 4 + nal.length;
  const out = new Uint8Array(size);
  let pos = 0;
  for (const nal of nals) {
    out[pos] = nal.length >>> 24;
    out[pos + 1] = (nal.length >>> 16) & 0xff;
    out[pos + 2] = (nal.length >>> 8) & 0xff;
    out[pos + 3] = nal.length & 0xff;
    out.set(nal, pos + 4);
    pos += 4 + nal.length;
  }
  return out;
};

/**
 * @param {Uint8Array|null} a
 * @param {Uint8Array|null} b
 * @returns {boolean} Whether both are absent or byte-for-byte equal.
 */
export const sameBytes = (a, b) => {
  if (!a || !b) return !a && !b;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
};

export const PROBE_CODEC_STRINGS = {
  h264: 'avc1.42E01E',
  h265: 'hev1.1.6.L93.B0',
  vp8: 'vp8',
  vp9: 'vp09.00.31.08',
  av1: 'av01.0.05M.08',
};

/** The 4:4:4 configurations, at the profiles the encoders emit. */
export const PROBE_FULLCOLOR_STRINGS = {
  h264: 'avc1.F4001E',
  h265: 'hev1.4.10.L93.9E.8',
};
