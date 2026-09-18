/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The color space every decoder is told to assume. No engine reads it out of an
// H.264 bitstream -- Chromium and Firefox both report BT.709 limited for a
// stream whose VUI says full range, and a full range session rendered on that
// assumption is wrong by up to eight levels a channel -- so the range the
// session converted at travels here instead, and a decoder is told it whatever
// the codec. VP8 is the exception: its bitstream carries one color bit that can
// only say BT.601, Firefox reads it and ignores this, so VP8 is held there.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { decoderColorSpace } from '../../addons/selkies-web-core/lib/wire-codecs.js';

let failed = 0;

function check(label, ok, detail = '') {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [color-range] ${label}  ${detail}`);
}

const limited = decoderColorSpace('avc1.42001e');
check('a session is told a space even where the codec could declare one',
  limited && limited.matrix === 'bt709' && limited.fullRange === false, JSON.stringify(limited));

const full = decoderColorSpace('avc1.f4001e', true);
check('a full range session is told full range',
  full.fullRange === true && full.matrix === 'bt709', JSON.stringify(full));

check('the primaries and transfer are the desktop\'s either way',
  ['bt709'].includes(limited.primaries) && limited.transfer === 'bt709'
  && full.primaries === 'bt709' && full.transfer === 'bt709');

const vp8 = decoderColorSpace('vp8', true);
check('VP8 stays on the matrix its bitstream can name, at limited range',
  vp8.matrix === 'smpte170m' && vp8.fullRange === false, JSON.stringify(vp8));

for (const codec of ['vp9', 'vp09.00.10.08']) {
  const vp9 = decoderColorSpace(codec);
  check(`${codec} is told BT.709, which its frame header does not settle`,
    vp9.matrix === 'bt709' && vp9.fullRange === false, JSON.stringify(vp9));
}

for (const codec of ['av01.0.04M.08', 'hev1.1.6.L93.B0']) {
  const other = decoderColorSpace(codec, true);
  check(`${codec} carries the session's range too`,
    other.matrix === 'bt709' && other.fullRange === true, JSON.stringify(other));
}

check('an absent range is limited, which is what every session but a 4:4:4 one converts at',
  decoderColorSpace('avc1.42001e', undefined).fullRange === false);

process.exit(failed ? 1 : 0);
