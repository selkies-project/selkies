/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright (c) Jeremy Lainé.
 *   All rights reserved.
 *
 *   Redistribution and use in source and binary forms, with or without
 *   modification, are permitted provided that the following conditions are met:
 *
 *       * Redistributions of source code must retain the above copyright notice,
 *       this list of conditions and the following disclaimer.
 *       * Redistributions in binary form must reproduce the above copyright notice,
 *       this list of conditions and the following disclaimer in the documentation
 *       and/or other materials provided with the distribution.
 *       * Neither the name of aiortc nor the names of its contributors may
 *       be used to endorse or promote products derived from this software without
 *       specific prior written permission.
 *
 *   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
 *   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 *   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 *   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 *   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 *   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 *   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 *   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 *   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 *   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

//! Router associating the RTP and RTCP packets of one bundled transport with its streams
//! (draft-ietf-mmusic-sdp-bundle-negotiation). `K` names a stream however the owner does.
//!
//! An unknown SSRC is learned from its payload type where exactly one receiver takes that
//! type, as a browser's first packets may precede anything that announces it; the table is
//! bounded, since an SSRC is the peer's to invent.

use std::collections::HashMap;
use std::hash::Hash;

use super::packet::RtpPacket;
use super::rtcp::{RTCP_PSFB_APP, RTCP_RTPFB_TWCC, RtcpPacket, unpack_remb_fci};

/// SSRCs learned at most, per transport.
const LEARNED_SSRCS_MAX: usize = 256;

/// Who a packet goes to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recipients<K> {
    pub receivers: Vec<K>,
    pub senders: Vec<K>,
}

impl<K: PartialEq> Recipients<K> {
    fn receiver(&mut self, key: Option<K>) {
        if let Some(key) = key.filter(|k| !self.receivers.contains(k)) {
            self.receivers.push(key);
        }
    }

    fn sender(&mut self, key: Option<K>) {
        if let Some(key) = key.filter(|k| !self.senders.contains(k)) {
            self.senders.push(key);
        }
    }
}

pub struct RtpRouter<K> {
    senders: HashMap<u32, (K, bool)>,
    ssrc_table: HashMap<u32, K>,
    payload_type_table: HashMap<u8, Vec<K>>,
}

impl<K: Copy + Eq + Hash> Default for RtpRouter<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash> RtpRouter<K> {
    pub fn new() -> Self {
        Self {
            senders: HashMap::new(),
            ssrc_table: HashMap::new(),
            payload_type_table: HashMap::new(),
        }
    }

    /// A receiver and the SSRCs and payload types it takes.
    pub fn register_receiver(&mut self, key: K, ssrcs: &[u32], payload_types: &[u8]) {
        for ssrc in ssrcs {
            self.ssrc_table.insert(*ssrc, key);
        }
        for pt in payload_types {
            let receivers = self.payload_type_table.entry(*pt).or_default();
            if !receivers.contains(&key) {
                receivers.push(key);
            }
        }
    }

    pub fn register_sender(&mut self, key: K, ssrc: u32, video: bool) {
        self.senders.insert(ssrc, (key, video));
    }

    pub fn unregister_receiver(&mut self, key: K) {
        self.ssrc_table.retain(|_, k| *k != key);
        for receivers in self.payload_type_table.values_mut() {
            receivers.retain(|k| *k != key);
        }
    }

    pub fn unregister_sender(&mut self, key: K) {
        self.senders.retain(|_, (k, _)| *k != key);
    }

    fn sender_of(&self, ssrc: u32) -> Option<K> {
        self.senders.get(&ssrc).map(|(k, _)| *k)
    }

    /// The receivers and senders an RTCP packet concerns.
    pub fn route_rtcp(&self, packet: &RtcpPacket) -> Recipients<K> {
        let mut to = Recipients {
            receivers: Vec::new(),
            senders: Vec::new(),
        };
        match packet {
            RtcpPacket::Sr { ssrc, reports, .. } => {
                to.receiver(self.ssrc_table.get(ssrc).copied());
                for report in reports {
                    to.sender(self.sender_of(report.ssrc));
                }
            }
            RtcpPacket::Rr { reports, .. } => {
                for report in reports {
                    to.sender(self.sender_of(report.ssrc));
                }
            }
            RtcpPacket::Bye { sources } => {
                for source in sources {
                    to.receiver(self.ssrc_table.get(source).copied());
                }
            }
            RtcpPacket::Rtpfb(fb) => {
                // Transport-cc feedback is association-wide (media_ssrc 0), so keying by
                // media_ssrc alone discards it entirely. It goes to one sender, video first:
                // that is where congestion control input matters.
                if fb.fmt == RTCP_RTPFB_TWCC {
                    let video = self.senders.values().find(|(_, video)| *video);
                    to.sender(video.or(self.senders.values().next()).map(|(k, _)| *k));
                }
                to.sender(self.sender_of(fb.media_ssrc));
            }
            RtcpPacket::Psfb(fb) => {
                to.sender(self.sender_of(fb.media_ssrc));
                // A REMB's media_ssrc is 0: the SSRCs are in its FCI.
                if fb.fmt == RTCP_PSFB_APP
                    && let Ok((_, ssrcs)) = unpack_remb_fci(&fb.fci)
                {
                    for ssrc in ssrcs {
                        to.sender(self.sender_of(ssrc));
                    }
                }
            }
            // A reference time names no media source: every sender answers it.
            RtcpPacket::Xr(xr) if xr.rrtr.is_some() => {
                for (key, _) in self.senders.values() {
                    to.sender(Some(*key));
                }
            }
            _ => {}
        }
        to
    }

    /// The receiver an RTP packet goes to, learning an unknown SSRC from its payload type.
    pub fn route_rtp(&mut self, packet: &RtpPacket) -> Option<K> {
        let by_pt = self.payload_type_table.get(&packet.payload_type);
        match self.ssrc_table.get(&packet.ssrc) {
            Some(key) => by_pt.filter(|pts| pts.contains(key)).map(|_| *key),
            None => match by_pt.map(Vec::as_slice) {
                Some([only]) if self.ssrc_table.len() < LEARNED_SSRCS_MAX => {
                    let only = *only;
                    self.ssrc_table.insert(packet.ssrc, only);
                    Some(only)
                }
                _ => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::rtcp::{PsfbPacket, RTCP_PSFB_PLI, RtpfbPacket, XrPacket};
    use super::*;

    #[test]
    fn a_reference_time_reaches_every_sender() {
        let mut router = RtpRouter::new();
        router.register_sender(1, 100, true);
        router.register_sender(2, 200, false);
        let mut to = router
            .route_rtcp(&RtcpPacket::Xr(XrPacket {
                ssrc: 1,
                rrtr: Some(5),
                dlrr: vec![],
            }))
            .senders;
        to.sort_unstable();
        assert_eq!(to, vec![1, 2]);
        let none = router.route_rtcp(&RtcpPacket::Xr(XrPacket {
            ssrc: 1,
            rrtr: None,
            dlrr: vec![(100, 1, 2)],
        }));
        assert!(none.senders.is_empty());
    }

    #[test]
    fn transport_cc_goes_to_the_video_sender_and_feedback_by_ssrc() {
        let mut router = RtpRouter::new();
        router.register_sender(2, 200, false);
        router.register_sender(1, 100, true);
        let twcc = RtcpPacket::Rtpfb(RtpfbPacket::twcc(9, 0, vec![]));
        assert_eq!(router.route_rtcp(&twcc).senders, vec![1]);
        let pli = RtcpPacket::Psfb(PsfbPacket {
            fmt: RTCP_PSFB_PLI,
            ssrc: 9,
            media_ssrc: 200,
            fci: vec![],
        });
        assert_eq!(router.route_rtcp(&pli).senders, vec![2]);
        let remb = RtcpPacket::Psfb(PsfbPacket {
            fmt: RTCP_PSFB_APP,
            ssrc: 9,
            media_ssrc: 0,
            fci: super::super::rtcp::pack_remb_fci(1000, &[100]),
        });
        assert_eq!(router.route_rtcp(&remb).senders, vec![1]);
    }

    #[test]
    fn an_unknown_ssrc_is_learned_from_a_payload_type_one_receiver_takes() {
        let mut router = RtpRouter::new();
        router.register_receiver(1, &[], &[96, 97]);
        router.register_receiver(2, &[50], &[97, 98]);
        let packet = |pt, ssrc| RtpPacket::new(pt, 0, 0, ssrc);
        assert_eq!(router.route_rtp(&packet(96, 7)), Some(1));
        assert_eq!(router.route_rtp(&packet(96, 7)), Some(1), "learned");
        assert_eq!(router.route_rtp(&packet(97, 8)), None, "two take it");
        assert_eq!(router.route_rtp(&packet(98, 50)), Some(2));
        assert_eq!(
            router.route_rtp(&packet(96, 50)),
            None,
            "a known SSRC under another's type"
        );
        assert_eq!(router.route_rtp(&packet(55, 9)), None);
        for ssrc in 1000..2000 {
            router.route_rtp(&packet(96, ssrc));
        }
        assert!(
            router.ssrc_table.len() <= LEARNED_SSRCS_MAX,
            "the peer's SSRCs are bounded"
        );
    }
}
