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
 * @module
 */

/** Codec names by wire id. */
export const WIRE_CODECS: Record<number, string> = { 1: 'h264', 2: 'vp8', 3: 'vp9', 4: 'av1', 5: 'h265' };

/** Encoder wire values and the codec each streams. */
export const ENCODER_CODECS: Record<string, string> = {
  jpeg: 'jpeg',
  h264enc: 'h264',
  h265enc: 'h265',
  vp8enc: 'vp8',
  vp9enc: 'vp9',
  av1enc: 'av1',
};

/**
 * @param typeByte The second byte of a video frame's wire header.
 * @returns The codec name; an id no codec has reads as H.264.
 */
export const wireCodecName = (typeByte: number): string => WIRE_CODECS[typeByte >> 4] || 'h264';

/**
 * @param typeByte The second byte of a video frame's wire header.
 * @returns Whether the frame is a decode entry point.
 */
export const wireFrameIsKey = (typeByte: number): boolean => (typeByte & 0x0f) === 0x01;

/**
 * @param encoder An encoder wire value.
 * @returns The codec it streams; an unknown encoder reads as H.264.
 */
export const codecOfEncoder = (encoder: string): string => ENCODER_CODECS[encoder] || 'h264';

/**
 * @param codec A codec name.
 * @returns Whether the codec can carry 4:4:4 chroma.
 */
export const codecCarriesFullColor = (codec: string): boolean => codec === 'h264' || codec === 'h265' || codec === 'vp9';

/**
 * Whether a codec has a 10-bit profile the server's encoders emit (H.264 High 10
 * and High 4:4:4 Predictive, H.265 Main 10 and Main 4:4:4 10, VP9 profiles 2
 * and 3, AV1).
 * @param codec The codec name.
 * @returns
 */
export const codecCarriesTenBit = (codec: string): boolean => codec === 'h264' || codec === 'h265' || codec === 'vp9' || codec === 'av1';

/**
 * The NAL units of an Annex-B buffer, each without its start code.
 * @param bytes
 * @returns
 */
export const annexbNals = (bytes: Uint8Array): Uint8Array[] => {
  const nals: Uint8Array[] = [];
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

/** A bit reader over an RBSP. */
export interface RbspReader {
  u: (n: number) => number;
  skip: (n: number) => void;
}

/**
 * A bit reader over an RBSP: `bytes` from `offset` with emulation prevention
 * bytes removed, `limit` bytes at most; `raw` reads them as they stand, for
 * AV1, which has none.
 * @param bytes
 * @param offset
 * @param limit
 * @param raw
 * @returns
 */
export const rbspReader = (bytes: Uint8Array, offset: number, limit: number, raw = false): RbspReader => {
  const data: number[] = [];
  let zeros = 0;
  for (let i = offset; i < bytes.length && data.length < limit; i++) {
    const b = bytes[i];
    if (!raw && zeros >= 2 && b === 3) { zeros = 0; continue; }
    zeros = b === 0 ? zeros + 1 : 0;
    data.push(b);
  }
  let pos = 0;
  const u = (n: number): number => {
    let v = 0;
    for (let i = 0; i < n; i++) {
      const byte = data[pos >> 3];
      const bit = byte === undefined ? 0 : (byte >> (7 - (pos & 7))) & 1;
      v = v * 2 + bit;
      pos++;
    }
    return v;
  };
  return { u, skip: (n: number): void => { pos += n; } };
};

/**
 * `avc1.PPCCLL` from the first SPS of an H.264 Annex-B key frame.
 * @param bytes
 * @returns `null` when no SPS is found.
 */
export const parseAvcCodecFromAnnexB = (bytes: Uint8Array): string | null => {
  if (!bytes || bytes.length < 5) return null;
  const hex2 = (n: number): string => n.toString(16).toUpperCase().padStart(2, '0');
  for (const nal of annexbNals(bytes)) {
    if ((nal[0] & 0x80) === 0 && (nal[0] & 0x1f) === 7) {
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
 * @param bytes
 * @returns `null` when no SPS is found.
 */
export const parseHevcCodecFromAnnexB = (bytes: Uint8Array): string | null => {
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
    const constraints: number[] = [];
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
 * `av01.P.LLT.DD` from the sequence header of an AV1 temporal unit: the profile,
 * the first operating point's level and tier, and the bit depth its color
 * configuration declares. Every operating point is read past, and the header
 * as it stands, since AV1 has no emulation prevention.
 * @param bytes
 * @returns `null` when no sequence header is found.
 */
export const parseAv1CodecFromObus = (bytes: Uint8Array): string | null => {
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
      const r = rbspReader(bytes, i, size, true);
      const profile = r.u(3);
      r.skip(1);
      const reduced = r.u(1);
      let level: number | undefined;
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
        const points = r.u(5) + 1;
        for (let op = 0; op < points; op++) {
          r.skip(12);
          const opLevel = r.u(5);
          const opTier = opLevel > 7 ? r.u(1) : 0;
          if (op === 0) {
            level = opLevel;
            tier = opTier;
          }
          if (decoderModel && r.u(1)) r.skip(bufferDelayBits * 2 + 1);
          if (displayDelay && r.u(1)) r.skip(4);
        }
      }
      const widthBits = r.u(4) + 1;
      const heightBits = r.u(4) + 1;
      r.skip(widthBits + heightBits);
      if (!reduced) {
        if (r.u(1)) r.skip(4 + 3);
        r.skip(3 + 4);
        const orderHint = r.u(1);
        if (orderHint) r.skip(2);
        const screenContent = r.u(1) ? 2 : r.u(1);
        if (screenContent > 0 && !r.u(1)) r.skip(1);
        if (orderHint) r.skip(3);
      } else {
        r.skip(3);
      }
      r.skip(3);
      const depth = r.u(1) ? ((profile === 2 && r.u(1)) ? 12 : 10) : 8;
      return `av01.${profile}.${String(level).padStart(2, '0')}${tier ? 'H' : 'M'}.${String(depth).padStart(2, '0')}`;
    }
    pos = i + size;
  }
  return null;
};

/**
 * The VP9 profile a frame's uncompressed header declares.
 * @param bytes
 * @returns
 */
export const parseVp9Profile = (bytes: Uint8Array): number => {
  if (!bytes || bytes.length < 1) return 0;
  const b = bytes[0];
  return ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
};

/**
 * The bit depth a VP9 key frame's uncompressed header declares: 8 for profiles 0
 * and 1, 10 or 12 by the flag that follows the sync code for profiles 2 and 3.
 * @param bytes
 * @returns
 */
export const parseVp9BitDepth = (bytes: Uint8Array): number => {
  const profile = parseVp9Profile(bytes);
  if (profile < 2 || !bytes || bytes.length < 5) return 8;
  const r = rbspReader(bytes, 0, 8);
  r.skip(profile === 3 ? 5 : 4);
  if (r.u(1) || r.u(1)) return 10;
  r.skip(2 + 24);
  return r.u(1) ? 12 : 10;
};

/**
 * The lowest VP9 level whose luma sample rate and picture size admit a stream,
 * as the two digits of the codec string.
 * @param width
 * @param height
 * @param fps
 * @returns
 */
export const vp9Level = (width: number, height: number, fps: number): string => {
  const size = width * height;
  const rate = size * (fps > 0 ? fps : 60);
  const levels: Array<[string, number, number]> = [
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
 * @param width
 * @param height
 * @param fps
 * @returns
 */
export const av1LevelIdx = (width: number, height: number, fps: number): number => {
  const size = width * height;
  const rate = size * (fps > 0 ? fps : 60);
  const levels: Array<[number, number, number, number, number]> = [
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
 * @param width
 * @param height
 * @param is444 Whether the stream is 4:4:4 full-color.
 * @param fps
 * @param chromium Whether the engine takes a High profile guess.
 * @returns
 */
/**
 * The `level_idc`, as a two-hex-digit string, an H.264 stream of this geometry declares, floored
 * across the common frame rates like the encoder's own ladder so a rate change moves nothing.
 * Mirrors `h264_level` in pixelflux `codec.rs`.
 * @param width
 * @param height
 * @param fps
 * @returns
 */
export const h264LevelIdc = (width: number, height: number, fps: number): string => {
  const mbs = Math.ceil(width / 16) * Math.ceil(height / 16);
  const mbps = mbs * Math.max(fps > 0 ? fps : 60, 60);
  const levels: Array<[number, number, number]> = [
    [0x29, 8192, 245760], [0x2A, 8704, 522240], [0x32, 22080, 589824], [0x33, 36864, 983040],
    [0x34, 36864, 2073600], [0x3C, 139264, 4177920], [0x3D, 139264, 8355840], [0x3E, 139264, 16711680],
  ];
  for (const [idc, maxFs, maxMbps] of levels) {
    if (mbs <= maxFs && mbps <= maxMbps) return idc.toString(16).toUpperCase().padStart(2, '0');
  }
  return '3E';
};

/**
 * @param width
 * @param height
 * @param is444 Whether the stream is 4:4:4 full-color.
 * @param fps
 * @param chromium Whether the engine takes a High profile guess.
 * @returns
 */
export const guessAvcCodec = (width: number, height: number, is444: boolean, fps: number, chromium: boolean): string => {
  if (!chromium) return 'avc1.42E01E';
  const profile = is444 ? 'F400' : '6400';
  return `avc1.${profile}${h264LevelIdc(width, height, fps)}`;
};

/**
 * The WebCodecs codec string of a stream, from its key frame where the codec
 * declares it and from the geometry otherwise.
 * @param codec The wire codec name.
 * @param keyframe The key frame's payload, or `null` for a delta.
 * @param width
 * @param height
 * @param fps
 * @param is444
 * @param chromium
 * @returns
 */
export const codecStringFor = (codec: string, keyframe: Uint8Array | null, width: number, height: number, fps: number, is444: boolean, chromium: boolean): string => {
  switch (codec) {
    case 'h265':
      return (keyframe && parseHevcCodecFromAnnexB(keyframe)) ||
        (is444 ? 'hev1.4.10.L153.9E.8' : 'hev1.1.6.L153.B0');
    case 'vp8':
      return 'vp8';
    case 'vp9':
      return `vp09.0${keyframe ? parseVp9Profile(keyframe) : 0}.${vp9Level(width, height, fps)}.${
        String(keyframe ? parseVp9BitDepth(keyframe) : 8).padStart(2, '0')}`;
    case 'av1':
      return (keyframe && parseAv1CodecFromObus(keyframe)) ||
        `av01.0.${String(av1LevelIdx(width, height, fps)).padStart(2, '0')}M.08`;
    default:
      return (keyframe && parseAvcCodecFromAnnexB(keyframe)) ||
        guessAvcCodec(width, height, is444, fps, chromium);
  }
};

/**
 * The color space a decoder is told to assume, which is every session's: no
 * engine reads it out of an H.264 bitstream. Chromium and Firefox both report
 * `bt709` at limited range for a stream whose VUI says otherwise, so a full
 * range session rendered without this hint is wrong by up to eight levels a
 * channel, and both honor the hint exactly. VP8 is held to BT.601 whatever the
 * session converted with, the one value its bitstream can name, because Firefox
 * reads that bit and ignores this; VP9 is told BT.709 because Chromium's
 * decoder does not read the matrix from the frame header.
 * @param codec The wire codec name or WebCodecs codec string.
 * @param fullRange Whether the session converted at full range.
 * @returns
 */
export const decoderColorSpace = (codec: string, fullRange = false): VideoColorSpaceInit => {
  const vp8 = codec === 'vp8';
  const matrix = vp8 ? 'smpte170m' : 'bt709';
  return { primaries: 'bt709', transfer: 'bt709', matrix, fullRange: !vp8 && !!fullRange };
};

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
 * @param bytes
 * @returns
 */
export const avcDescription = (bytes: Uint8Array): Uint8Array | null => {
  let sps: Uint8Array | null = null;
  let pps: Uint8Array | null = null;
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
 * @param bytes
 * @returns
 */
export const annexbToAvcc = (bytes: Uint8Array): Uint8Array => {
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
 * @param a
 * @param b
 * @returns Whether both are absent or byte-for-byte equal.
 */
export const sameBytes = (a: Uint8Array | null, b: Uint8Array | null): boolean => {
  if (!a || !b) return !a && !b;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
};

export const PROBE_CODEC_STRINGS: Record<string, string> = {
  h264: 'avc1.42E01E',
  h265: 'hev1.1.6.L93.B0',
  vp8: 'vp8',
  vp9: 'vp09.00.31.08',
  av1: 'av01.0.05M.08',
};

/**
 * One 10-bit key frame of each format, 320x240 of flat gray, for asking a
 * decoder for a picture rather than for its word: a codec name is its 4:2:0,
 * the name with `444` after it its 4:4:4. An engine can accept a 10-bit
 * configuration and then fail the frame, which only decoding one shows.
 */
export const TEN_BIT_SAMPLES: Record<string, string> = {
  h264: 'AAAAAWduAA2mzZQUH7ARAAADAAEAAAMAPA8UKZYAAAABaOvjyyLAAAABZYiEADf//vbw/gNCUJcRzeidMx+/Fbi6NDe9zgACrU8Qb7gsE2lPYBBAAAGfEd0SuCykEBAvSus=',
  h264444: 'AAAAAWf0AA2Q2bKCg/YCIAAAAwAgAAAHgeKFMsAAAAABaOvjxEhEAAABZYiEADf//vaH+A0Lun/9P+C6zp85f52wAAq0QAbCNhAp8AAAAwAAAwAAAwACNw==',
  h265: 'AAAAAUABDAH//wIgAAADAJAAAAMAAAMAPJWYCQAAAAFCAQECIAAAAwCQAAADAAADADygCggPE2WVmkkyvAWgIAAAAwAgAAADA8EAAAABRAHBcrQiQAAAASgBrwhg+Snjmyr/oamsIyBbC7MwAAADAAADAAADAAADAAADAPSA',
  h265444: 'AAAAAUABDAH//wQIAAADAJwIAAADAAA8lZgJAAAAAUIBAQQIAAADAJwIAAADAAA8kAFBAeJssrNJJleAtAQAAAMABAAAAwB4IAAAAAFEAcFyhgxCJAAAASgBrwhg+Snjmyr/oamsIyBbC7MwAAADAAADAAADAAADAAADAPSA',
  vp9: 'kkmDQgAJ+Ad7ABwSDgxJAAAYYAAAZzaRO7i1N12+muU5wAA=',
  vp9444: 'sSTBoQAAn4B3sAHBIODEkAABgGAAAGc2kOnpLRNlZkoaQGA=',
  av1: 'EgAKCwAAAAQ8/7xq+cBAMhcQAJEAhBAggIAAAgF1u6aQC9g5Dyf14A==',
};

/** The 4:4:4 configurations, at the profiles the encoders emit (VP9 profile 1). */
export const PROBE_FULLCOLOR_STRINGS: Record<string, string> = {
  h264: 'avc1.F4001E',
  h265: 'hev1.4.10.L93.9E.8',
  vp9: 'vp09.01.10.08.03',
};
