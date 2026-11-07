use crate::chunk::chunk_payload_data::{ChunkPayloadData, MessageId, MessageReliability};
use crate::chunk::chunk_selective_ack::GapAckBlock;
use crate::util::*;

use rustc_hash::FxHashMap;
use std::collections::{VecDeque, hash_map::Entry};

/// A fragmented message may have only one retained DATA chunk. Store that TSN
/// inline until another fragment is queued.
#[derive(Debug)]
enum MessageTsns {
    One(u32),
    Fragmented(VecDeque<u32>),
}

impl MessageTsns {
    fn insert(&mut self, tsn: u32) {
        match self {
            Self::One(first) => {
                let pair = if sna32lt(tsn, *first) {
                    [tsn, *first]
                } else {
                    [*first, tsn]
                };
                *self = Self::Fragmented(VecDeque::from(pair));
            }
            Self::Fragmented(tsns) => {
                if tsns.back().is_none_or(|last| sna32lt(*last, tsn)) {
                    tsns.push_back(tsn);
                } else {
                    let index = tsns.partition_point(|&item| sna32lt(item, tsn));
                    tsns.insert(index, tsn);
                }
            }
        }
    }

    /// Cumulative acknowledgment removes only a prefix of a message's TSNs.
    /// Returns None without mutation if the expected prefix is missing, or
    /// Some(true) when the whole index entry can be removed.
    fn pop(&mut self, tsn: u32) -> Option<bool> {
        match self {
            Self::One(first) if *first == tsn => Some(true),
            Self::Fragmented(tsns) if tsns.front() == Some(&tsn) => {
                tsns.pop_front();
                Some(tsns.is_empty())
            }
            _ => None,
        }
    }

    fn to_vec(&self) -> Vec<u32> {
        match self {
            Self::One(tsn) => vec![*tsn],
            Self::Fragmented(tsns) => tsns.iter().copied().collect(),
        }
    }
}

/// The most duplicate TSNs one SACK names.
const MAX_REPORTED_DUPLICATES: usize = 256;

#[derive(Default, Debug)]
pub(crate) struct PayloadQueue {
    // length: usize,
    /// Keyed by TSN; per-chunk lookups on both the send (in-flight) and
    /// receive paths. TSNs are window-bounded, so the faster non-SipHash
    /// hasher is safe.
    chunk_map: FxHashMap<u32, ChunkPayloadData>,
    /// TSNs in serial-number order. A `VecDeque` so that `pop` — which almost
    /// always removes the front, once per acked/received chunk — is O(1)
    /// instead of shifting the whole in-flight window left (`Vec::remove(0)`
    /// showed up as ~9% of the end-to-end transfer profile as memmove).
    pub(crate) sorted: VecDeque<u32>,
    dup_tsn: Vec<u32>,
    n_bytes: usize,
    /// Sent fragments of messages eligible for abandonment. A whole message
    /// uses its candidate TSN directly; only fragmented messages need a group.
    message_tsns: FxHashMap<MessageId, MessageTsns>,
    #[cfg(test)]
    pub(crate) track_lookups: bool,
    #[cfg(test)]
    pub(crate) lookups: std::cell::Cell<usize>,
}

impl PayloadQueue {
    pub(crate) fn new() -> Self {
        PayloadQueue::default()
    }

    /// Insert `tsn` into `sorted`, keeping SCTP serial-number order. Binary-search
    /// the insertion point instead of re-sorting the whole vector on every push:
    /// re-sorting made a burst of N chunks O(N^2 log N). In-order arrivals — the
    /// common case, since TSNs are assigned/received sequentially — land at the
    /// end in O(1) amortized.
    fn insert_sorted(&mut self, tsn: u32) {
        let idx = self.sorted.partition_point(|&x| sna32lt(x, tsn));
        self.sorted.insert(idx, tsn);
    }

    pub(crate) fn can_push(&self, p: &ChunkPayloadData, cumulative_tsn: u32) -> bool {
        // A duplicate unless ahead: exactly 2^31 away is neither.
        !self.chunk_map.contains_key(&p.tsn) && sna32gt(p.tsn, cumulative_tsn)
    }

    fn abandonment_id(p: &ChunkPayloadData) -> Option<MessageId> {
        if p.beginning_fragment && p.ending_fragment {
            return None;
        }
        match p.reliability {
            MessageReliability::Reliable => None,
            _ => p.message_id,
        }
    }

    pub(crate) fn push_no_check(&mut self, p: ChunkPayloadData) {
        if let Some(id) = Self::abandonment_id(&p) {
            self.message_tsns
                .entry(id)
                .and_modify(|tsns| tsns.insert(p.tsn))
                .or_insert(MessageTsns::One(p.tsn));
        }
        self.n_bytes += p.user_data.len();
        self.insert_sorted(p.tsn);
        self.chunk_map.insert(p.tsn, p);
        //self.length += 1;
    }

    /// push pushes a payload data. If the payload data is already in our queue or
    /// older than our cumulative_tsn marker, it will be recored as duplications,
    /// which can later be retrieved using popDuplicates.
    pub(crate) fn push(&mut self, p: ChunkPayloadData, cumulative_tsn: u32) -> bool {
        let ok = self.chunk_map.contains_key(&p.tsn);
        if ok || !sna32gt(p.tsn, cumulative_tsn) {
            // Found the packet, log in dups
            self.note_duplicate(p.tsn);
            return false;
        }

        self.push_no_check(p);

        true
    }

    /// Drops every queued chunk at or below `tsn` in serial-number order, for a FORWARD TSN:
    /// each chunk held is visited once, never each TSN the jump spans.
    pub(crate) fn prune_through(&mut self, tsn: u32) {
        while let Some(&front) = self.sorted.front() {
            if sna32gt(front, tsn) {
                break;
            }
            if self.pop(front).is_none() {
                self.sorted.pop_front();
                if let Some(c) = self.chunk_map.remove(&front) {
                    self.n_bytes -= c.user_data.len();
                }
            }
        }
    }

    /// pop pops only if the oldest chunk's TSN matches the given TSN.
    pub(crate) fn pop(&mut self, tsn: u32) -> Option<ChunkPayloadData> {
        if self.sorted.front() != Some(&tsn) {
            return None;
        }
        let Entry::Occupied(entry) = self.chunk_map.entry(tsn) else {
            return None;
        };
        // Validate the message index before removing DATA or updating accounting.
        if let Some(id) = Self::abandonment_id(entry.get()) {
            let tsns = self.message_tsns.get_mut(&id)?;
            if tsns.pop(tsn)? {
                self.message_tsns.remove(&id);
            }
        }
        let c = entry.remove();
        self.sorted.pop_front();
        self.n_bytes -= c.user_data.len();
        Some(c)
    }

    pub(crate) fn message_tsns(&self, id: MessageId) -> Vec<u32> {
        self.message_tsns
            .get(&id)
            .map_or_else(Vec::new, MessageTsns::to_vec)
    }

    /// get returns reference to chunkPayloadData with the given TSN value.
    pub(crate) fn get(&self, tsn: u32) -> Option<&ChunkPayloadData> {
        #[cfg(test)]
        if self.track_lookups {
            self.lookups.set(self.lookups.get() + 1);
        }
        self.chunk_map.get(&tsn)
    }
    pub(crate) fn get_mut(&mut self, tsn: u32) -> Option<&mut ChunkPayloadData> {
        #[cfg(test)]
        if self.track_lookups {
            self.lookups.set(self.lookups.get() + 1);
        }
        self.chunk_map.get_mut(&tsn)
    }

    /// Note a TSN received again, for the next SACK; a peer sending its TSNs over and over
    /// grows this no further than a SACK reports.
    pub(crate) fn note_duplicate(&mut self, tsn: u32) {
        if self.dup_tsn.len() < MAX_REPORTED_DUPLICATES {
            self.dup_tsn.push(tsn);
        }
    }

    /// popDuplicates returns an array of TSN values that were found duplicate.
    pub(crate) fn pop_duplicates(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.dup_tsn)
    }

    pub(crate) fn get_gap_ack_blocks(&self, cumulative_tsn: u32) -> Vec<GapAckBlock> {
        if self.chunk_map.is_empty() {
            return vec![];
        }

        let mut b = GapAckBlock::default();
        let mut gap_ack_blocks = vec![];
        for (i, tsn) in self.sorted.iter().enumerate() {
            // Modular, so the blocks stay ascending and merged across the TSN wrap.
            let diff = tsn.wrapping_sub(cumulative_tsn) as u16;

            if i == 0 {
                b.start = diff;
                b.end = b.start;
            } else if u32::from(b.end) + 1 == u32::from(diff) {
                b.end += 1;
            } else {
                gap_ack_blocks.push(b);

                b.start = diff;
                b.end = diff;
            }
        }

        gap_ack_blocks.push(b);

        gap_ack_blocks
    }

    pub(crate) fn get_gap_ack_blocks_string(&self, cumulative_tsn: u32) -> String {
        let mut s = format!("cumTSN={}", cumulative_tsn);
        for b in self.get_gap_ack_blocks(cumulative_tsn) {
            s += format!(",{}-{}", b.start, b.end).as_str();
        }
        s
    }

    pub(crate) fn acknowledge(&mut self, tsn: u32) -> bool {
        self.chunk_map
            .get_mut(&tsn)
            .is_some_and(ChunkPayloadData::acknowledge)
    }

    /// Release each payload byte once, preserving its acknowledgment state.
    pub(crate) fn release_payload(&mut self, tsn: u32) -> usize {
        if let Some(c) = self.chunk_map.get_mut(&tsn) {
            let n = c.user_data.len();
            self.n_bytes -= n;
            c.user_data.clear();
            n
        } else {
            0
        }
    }

    pub(crate) fn get_last_tsn_received(&self) -> Option<&u32> {
        self.sorted.back()
    }

    pub(crate) fn mark_all_to_retrasmit(&mut self) {
        for c in self.chunk_map.values_mut() {
            if !c.is_outstanding() {
                continue;
            }
            c.retransmit = true;
        }
    }

    pub(crate) fn get_num_bytes(&self) -> usize {
        self.n_bytes
    }

    pub(crate) fn len(&self) -> usize {
        //assert_eq!(self.chunk_map.len(), self.length);
        self.chunk_map.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn inconsistent_message_index_preserves_data_and_accounting() {
        let id = MessageId::new(0).unwrap();
        for invalid_index in [
            None,
            Some(MessageTsns::One(101)),
            Some(MessageTsns::Fragmented(VecDeque::from([101, 100]))),
        ] {
            let mut queue = PayloadQueue::new();
            for tsn in [100, 101] {
                queue.push_no_check(ChunkPayloadData {
                    tsn,
                    message_id: Some(id),
                    reliability: MessageReliability::Rexmit { max_retransmits: 1 },
                    beginning_fragment: tsn == 100,
                    ending_fragment: tsn == 101,
                    user_data: Bytes::from_static(b"data"),
                    ..Default::default()
                });
            }
            if let Some(index) = invalid_index {
                queue.message_tsns.insert(id, index);
            } else {
                queue.message_tsns.remove(&id);
            }
            let before = queue.message_tsns(id);
            assert!(queue.pop(100).is_none());
            assert_eq!(VecDeque::from([100, 101]), queue.sorted);
            assert_eq!(2, queue.len());
            assert_eq!(8, queue.get_num_bytes());
            for tsn in [100, 101] {
                assert_eq!(
                    Bytes::from_static(b"data"),
                    queue.get(tsn).unwrap().user_data
                );
            }
            assert_eq!(before, queue.message_tsns(id));

            // Repairing only the index must leave both DATA chunks removable.
            queue
                .message_tsns
                .insert(id, MessageTsns::Fragmented(VecDeque::from([100, 101])));
            assert!(queue.pop(100).is_some());
            assert!(queue.pop(101).is_some());
            assert!(queue.is_empty());
            assert!(queue.message_tsns(id).is_empty());
            assert_eq!(0, queue.get_num_bytes());
        }
    }
}
