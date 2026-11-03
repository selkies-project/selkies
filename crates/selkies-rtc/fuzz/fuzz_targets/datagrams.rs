/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The framing a target reads a run of datagrams from: each behind its big-endian 16-bit
//! length, the last running to the input's end where its length claims more than is left.

/// The datagrams of `data`, in order.
pub fn split(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (head, rest) = data.split_first_chunk::<2>()?;
        let (one, tail) = rest.split_at((u16::from_be_bytes(*head) as usize).min(rest.len()));
        data = tail;
        Some(one)
    })
}
