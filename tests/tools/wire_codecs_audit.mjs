/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The codec strings the page reads off a key frame: an AV1 sequence header read
// whole, every operating point and its own bytes as they stand.

import { parseAv1CodecFromObus } from '../../addons/selkies-web-core/lib/wire-codecs.js';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [wire-codecs] ${label}  ${detail}`);
}

/** Bits written most significant first, into whole bytes. */
function bits() {
  const out = [];
  let acc = 0;
  let n = 0;
  const put = (value, width) => {
    for (let i = width - 1; i >= 0; i--) {
      acc = (acc << 1) | (Math.floor(value / 2 ** i) & 1);
      if (++n === 8) {
        out.push(acc);
        acc = 0;
        n = 0;
      }
    }
  };
  const bytes = () => Uint8Array.from(n ? [...out, acc << (8 - n)] : out);
  return { put, bytes };
}

/**
 * An OBU_SEQUENCE_HEADER for a 1920x1080 stream with the given operating
 * points (`[level, tier]`), timing info whose tick is `tick`, and `depth` bits.
 */
function sequenceHeader({ points, tick = 0, depth = 8, profile = 0 }) {
  const b = bits();
  b.put(profile, 3);
  b.put(0, 1); // still_picture
  b.put(0, 1); // reduced_still_picture_header
  b.put(tick ? 1 : 0, 1); // timing_info_present_flag
  if (tick) {
    b.put(tick, 32); // num_units_in_display_tick
    b.put(60000, 32); // time_scale
    b.put(0, 1); // equal_picture_interval
    b.put(0, 1); // decoder_model_info_present_flag
  }
  b.put(0, 1); // initial_display_delay_present_flag
  b.put(points.length - 1, 5);
  for (const [level, tier] of points) {
    b.put(0, 12); // operating_point_idc
    b.put(level, 5);
    if (level > 7) b.put(tier, 1);
  }
  b.put(10, 4); // frame_width_bits_minus_1
  b.put(10, 4); // frame_height_bits_minus_1
  b.put(1919, 11);
  b.put(1079, 11);
  b.put(0, 1); // frame_id_numbers_present_flag
  b.put(0, 3); // 128x128 superblocks, filter intra, intra edge filter
  b.put(0, 4); // interintra and masked compound, warped motion, dual filter
  b.put(1, 1); // enable_order_hint
  b.put(0, 2); // jnt_comp, ref_frame_mvs
  b.put(1, 1); // seq_choose_screen_content_tools
  b.put(1, 1); // seq_choose_integer_mv
  b.put(6, 3); // order_hint_bits_minus_1
  b.put(0, 3); // superres, cdef, restoration
  b.put(depth > 8 ? 1 : 0, 1); // high_bitdepth
  if (profile === 2 && depth > 8) b.put(depth === 12 ? 1 : 0, 1);
  b.put(0, 8); // the rest of the color config, read by nobody here
  const payload = b.bytes();
  // obu_type 1 with obu_has_size_field, then the leb128 size.
  return Uint8Array.from([0x0a, payload.length, ...payload]);
}

const one = parseAv1CodecFromObus(sequenceHeader({ points: [[8, 0]] }));
check('one operating point reads its level, main tier and 8 bits', one === 'av01.0.08M.08', one);
const two = parseAv1CodecFromObus(sequenceHeader({ points: [[8, 0], [5, 0]] }));
check('the first of two operating points names the level and tier, and the depth after both is read right',
  two === 'av01.0.08M.08', two);
const twoHigh = parseAv1CodecFromObus(sequenceHeader({ points: [[13, 1], [5, 0]], depth: 10 }));
check('likewise a high-tier first point over a 10-bit stream', twoHigh === 'av01.0.13H.10', twoHigh);
const many = parseAv1CodecFromObus(sequenceHeader({ points: Array.from({ length: 24 }, (_, i) => [9 + (i % 3), 0]) }));
check('a header longer than 64 bytes, its points many, is read to its color config', many === 'av01.0.09M.08', many);
// A display tick of 192 lays the bytes 00 00 03 down whole in the timing info.
const ticked = sequenceHeader({ points: [[8, 0]], tick: 192 });
check('that header does carry the bytes 00 00 03',
  [...ticked].some((v, i, a) => i > 1 && a[i - 2] === 0 && a[i - 1] === 0 && v === 3));
const tick = parseAv1CodecFromObus(ticked);
check('and they are AV1 data, not H.264 emulation prevention, so they stay', tick === 'av01.0.08M.08', tick);

process.exit(failed ? 1 : 0);
