use crate::chunk::chunk_payload_data::{
    ChunkPayloadData, MessageId, MessageReliability, PayloadProtocolIdentifier,
};
use shared::error::{Error, Result};

use std::collections::VecDeque;
use std::time::Instant;

pub(crate) type PendingBaseQueue = VecDeque<ChunkPayloadData>;

/// Ownership and policy remain available after the first fragment is sent or
/// the API stream closes. Later fragments cannot replace this message snapshot.
#[derive(Debug, Copy, Clone)]
struct MessageSnapshot {
    id: Option<MessageId>,
    stream_identifier: u16,
    stream_sequence_number: u16,
    stream_generation: u64,
    unordered: bool,
    payload_type: PayloadProtocolIdentifier,
    created_at: Option<Instant>,
    reliability: MessageReliability,
}

impl MessageSnapshot {
    fn from_chunk(c: &ChunkPayloadData) -> Self {
        Self {
            id: c.message_id,
            stream_identifier: c.stream_identifier,
            stream_sequence_number: c.stream_sequence_number,
            stream_generation: c.stream_generation,
            unordered: c.unordered,
            payload_type: c.payload_type,
            created_at: c.created_at,
            reliability: c.reliability,
        }
    }

    fn matches(&self, c: &ChunkPayloadData) -> bool {
        self.id == c.message_id
            && self.stream_identifier == c.stream_identifier
            && self.stream_sequence_number == c.stream_sequence_number
            && self.stream_generation == c.stream_generation
            && self.unordered == c.unordered
    }

    fn apply(&self, c: &mut ChunkPayloadData) {
        c.payload_type = self.payload_type;
        c.created_at = self.created_at;
        c.reliability = self.reliability;
    }
}

/// One message owns its unsent tail. Whole messages stay inline; fragmented
/// messages reuse the packetization buffer.
#[derive(Debug)]
enum QueuedMessage {
    Whole(ChunkPayloadData),
    Fragmented {
        snapshot: MessageSnapshot,
        fragments: PendingBaseQueue,
        next_fragment: usize,
        bytes: usize,
    },
}

impl QueuedMessage {
    fn new(mut chunks: Vec<ChunkPayloadData>) -> Option<Self> {
        let first = chunks.first()?;
        if chunks.len() == 1 && first.beginning_fragment && first.ending_fragment {
            return chunks.pop().map(Self::Whole);
        }
        let snapshot = MessageSnapshot::from_chunk(first);
        let mut bytes = 0;
        for c in &mut chunks {
            snapshot.apply(c);
            bytes += c.user_data.len();
        }
        Some(Self::Fragmented {
            snapshot,
            bytes,
            fragments: VecDeque::from(chunks),
            next_fragment: 0,
        })
    }

    fn peek(&self) -> Option<&ChunkPayloadData> {
        match self {
            Self::Whole(c) => Some(c),
            Self::Fragmented { fragments, .. } => fragments.front(),
        }
    }

    fn snapshot(&self) -> MessageSnapshot {
        match self {
            Self::Whole(c) => MessageSnapshot::from_chunk(c),
            Self::Fragmented { snapshot, .. } => *snapshot,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Whole(_) => 1,
            Self::Fragmented { fragments, .. } => fragments.len(),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Whole(c) => c.user_data.len(),
            Self::Fragmented { bytes, .. } => *bytes,
        }
    }

    /// Check the complete remaining tail before changing ownership or counters.
    fn validate_tail(&self) -> Result<(usize, usize)> {
        let (snapshot, fragments, next_fragment, bytes) = match self {
            Self::Whole(c) => return Ok((1, c.user_data.len())),
            Self::Fragmented {
                snapshot,
                fragments,
                next_fragment,
                bytes,
            } => (snapshot, fragments, next_fragment, bytes),
        };
        if !fragments.back().is_some_and(|c| c.ending_fragment) {
            return Err(Error::OtherSctpErr(
                "pending message has no ending fragment".into(),
            ));
        }
        let mut actual_bytes = 0;
        for (i, c) in fragments.iter().enumerate() {
            if !snapshot.matches(c)
                || c.beginning_fragment != (*next_fragment == 0 && i == 0)
                || c.ending_fragment != (i + 1 == fragments.len())
            {
                return Err(Error::OtherSctpErr(
                    "invalid pending message fragments".into(),
                ));
            }
            actual_bytes += c.user_data.len();
        }
        if actual_bytes != *bytes {
            return Err(Error::OtherSctpErr(
                "invalid pending message accounting".into(),
            ));
        }
        Ok((fragments.len(), *bytes))
    }

    fn into_fragments(self) -> Vec<ChunkPayloadData> {
        match self {
            Self::Whole(c) => vec![c],
            Self::Fragmented { fragments, .. } => Vec::from(fragments),
        }
    }
}

/// A reset is control state, with no DATA payload or reliability policy.
#[derive(Debug)]
pub(crate) struct ResetMarker {
    pub(crate) stream_identifier: u16,
}

#[derive(Debug)]
enum PendingEntry {
    Message(QueuedMessage),
    Reset(ResetMarker),
}

#[derive(Debug, Default)]
pub(crate) struct PendingQueue {
    unordered_queue: VecDeque<PendingEntry>,
    ordered_queue: VecDeque<PendingEntry>,
    queue_len: usize,
    n_bytes: usize,
    selected: bool,
    unordered_is_selected: bool,
}

impl PendingQueue {
    pub(crate) fn new() -> Self {
        PendingQueue::default()
    }

    /// Enqueue one complete packetized message. An empty packetization is a
    /// no-op, matching an empty application write.
    pub(crate) fn push_message(&mut self, chunks: Vec<ChunkPayloadData>) {
        let Some(message) = QueuedMessage::new(chunks) else {
            return;
        };
        self.n_bytes += message.bytes();
        self.queue_len += message.len();
        let queue = if message.snapshot().unordered {
            &mut self.unordered_queue
        } else {
            &mut self.ordered_queue
        };
        queue.push_back(PendingEntry::Message(message));
    }

    pub(crate) fn push_reset(&mut self, reset: ResetMarker) {
        self.ordered_queue.push_back(PendingEntry::Reset(reset));
        self.queue_len += 1;
    }

    fn front_queue(&self) -> &VecDeque<PendingEntry> {
        if self.selected {
            if self.unordered_is_selected {
                &self.unordered_queue
            } else {
                &self.ordered_queue
            }
        } else if !self.unordered_queue.is_empty() {
            &self.unordered_queue
        } else {
            &self.ordered_queue
        }
    }

    pub(crate) fn peek(&self) -> Option<&ChunkPayloadData> {
        match self.front_queue().front()? {
            PendingEntry::Message(message) => message.peek(),
            PendingEntry::Reset(_) => None,
        }
    }

    /// Preserve the existing ordered/unordered scheduling and reset position.
    pub(crate) fn pop_ready_reset(&mut self) -> Option<ResetMarker> {
        if self.selected || !self.unordered_queue.is_empty() {
            return None;
        }
        let PendingEntry::Reset(_) = self.ordered_queue.front()? else {
            return None;
        };
        let queue_len = self.queue_len.checked_sub(1)?;
        let PendingEntry::Reset(reset) = self.ordered_queue.pop_front()? else {
            return None;
        };
        self.queue_len = queue_len;
        Some(reset)
    }

    /// Remove the rest of this message once. A repeated call cannot consume
    /// the next message, even when it has the same stream and policy metadata.
    pub(crate) fn drain_message(&mut self, id: MessageId) -> Result<Vec<ChunkPayloadData>> {
        let Some(PendingEntry::Message(message)) = self.front_queue().front() else {
            return Ok(vec![]);
        };
        let snapshot = message.snapshot();
        if snapshot.id != Some(id) {
            return Ok(vec![]);
        }
        let (count, bytes) = message.validate_tail()?;
        if count > self.queue_len || bytes > self.n_bytes {
            return Err(Error::OtherSctpErr(
                "invalid pending message accounting".into(),
            ));
        }
        let queue = if snapshot.unordered {
            &mut self.unordered_queue
        } else {
            &mut self.ordered_queue
        };
        let Some(PendingEntry::Message(message)) = queue.pop_front() else {
            return Err(Error::OtherSctpErr("missing pending message".into()));
        };
        self.queue_len -= count;
        self.n_bytes -= bytes;
        self.selected = false;
        Ok(message.into_fragments())
    }

    pub(crate) fn pop(
        &mut self,
        beginning_fragment: bool,
        unordered: bool,
    ) -> Option<ChunkPayloadData> {
        let unordered = if self.selected {
            self.unordered_is_selected
        } else if beginning_fragment {
            unordered
        } else {
            return None;
        };
        let queue = if unordered {
            &mut self.unordered_queue
        } else {
            &mut self.ordered_queue
        };
        let PendingEntry::Message(message) = queue.front_mut()? else {
            return None;
        };
        let first = message.peek()?;
        let n_bytes = self.n_bytes.checked_sub(first.user_data.len())?;
        let queue_len = self.queue_len.checked_sub(1)?;
        let c = match message {
            QueuedMessage::Whole(_) => {
                let PendingEntry::Message(QueuedMessage::Whole(c)) = queue.pop_front()? else {
                    return None;
                };
                c
            }
            QueuedMessage::Fragmented {
                snapshot,
                fragments,
                next_fragment,
                bytes,
            } => {
                let first = fragments.front()?;
                if !snapshot.matches(first)
                    || first.beginning_fragment != (*next_fragment == 0)
                    || first.ending_fragment != (fragments.len() == 1)
                {
                    return None;
                }
                let remaining_bytes = bytes.checked_sub(first.user_data.len())?;
                let next = next_fragment.checked_add(1)?;
                let c = fragments.pop_front()?;
                *next_fragment = next;
                *bytes = remaining_bytes;
                if c.ending_fragment {
                    queue.pop_front();
                }
                c
            }
        };
        self.selected = !c.ending_fragment;
        self.unordered_is_selected = unordered;
        self.n_bytes = n_bytes;
        self.queue_len = queue_len;
        Some(c)
    }

    pub(crate) fn get_num_bytes(&self) -> usize {
        self.n_bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.queue_len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    fn fragment(beginning: bool, ending: bool, unordered: bool) -> ChunkPayloadData {
        ChunkPayloadData {
            message_id: MessageId::new(0),
            beginning_fragment: beginning,
            ending_fragment: ending,
            unordered,
            user_data: Bytes::from_static(b"data"),
            ..Default::default()
        }
    }

    #[test]
    fn invalid_message_drain_preserves_fragments_and_accounting() {
        for unordered in [false, true] {
            for next_message in [false, true] {
                let mut queue = PendingQueue::new();
                queue.push_message(vec![
                    fragment(true, false, unordered),
                    fragment(false, false, unordered),
                ]);
                if next_message {
                    // Even a malformed tail must not consume the next owner.
                    queue.push_message(vec![fragment(true, true, unordered)]);
                }
                let id = queue.peek().unwrap().message_id.unwrap();
                let (len, bytes) = (queue.len(), queue.get_num_bytes());
                assert!(queue.drain_message(id).is_err());
                assert_eq!(Some(id), queue.peek().unwrap().message_id);
                assert_eq!(len, queue.len());
                assert_eq!(bytes, queue.get_num_bytes());
                assert!(!queue.selected);
            }
        }
    }

    #[test]
    fn missing_beginning_is_an_error_without_mutation() {
        let mut queue = PendingQueue::new();
        queue.push_message(vec![fragment(false, true, false)]);
        let id = queue.peek().unwrap().message_id.unwrap();
        assert!(queue.drain_message(id).is_err());
        assert_eq!(Some(id), queue.peek().unwrap().message_id);
        assert_eq!(1, queue.len());
        assert_eq!(4, queue.get_num_bytes());
    }

    #[test]
    fn invalid_batch_pop_preserves_fragments_and_accounting() {
        for unordered in [false, true] {
            for early_end in [false, true] {
                let mut queue = PendingQueue::new();
                queue.push_message(vec![
                    fragment(true, early_end, unordered),
                    fragment(!early_end, true, unordered),
                ]);
                let mut later = fragment(true, true, unordered);
                later.message_id = MessageId::new(1);
                queue.push_message(vec![later]);
                if !early_end {
                    // The first fragment is valid, but the tail repeats B.
                    assert!(queue.pop(true, unordered).is_some());
                }
                let (len, bytes) = (queue.len(), queue.get_num_bytes());
                let first = queue.peek().unwrap();
                let id = first.message_id.unwrap();
                assert!(queue.pop(first.beginning_fragment, unordered).is_none());
                assert!(queue.drain_message(id).is_err());
                assert_eq!(Some(id), queue.peek().unwrap().message_id);
                assert_eq!(len, queue.len());
                assert_eq!(bytes, queue.get_num_bytes());
                assert_eq!(!early_end, queue.selected);
            }
        }
    }

    #[test]
    fn draining_selected_tail_preserves_other_queue_and_is_idempotent() -> Result<()> {
        for unordered in [false, true] {
            let mut queue = PendingQueue::new();
            let chunks = vec![
                fragment(true, false, unordered),
                fragment(false, true, unordered),
            ];
            let buffer = chunks.as_ptr();
            queue.push_message(chunks);
            assert!(queue.pop(true, unordered).is_some());
            queue.push_message(vec![]);
            let mut other = fragment(true, true, !unordered);
            other.message_id = MessageId::new(1);
            queue.push_message(vec![other]);
            let id = queue.peek().unwrap().message_id.unwrap();
            let tail = queue.drain_message(id)?;
            assert_eq!(1, tail.len());
            assert_eq!(buffer, tail.as_ptr());
            assert!(queue.drain_message(id)?.is_empty());
            assert!(!queue.selected);
            assert_eq!(1, queue.len());
            assert_eq!(4, queue.get_num_bytes());
            assert_eq!(!unordered, queue.peek().unwrap().unordered);
        }
        Ok(())
    }

    #[test]
    fn message_reuses_fragment_buffer_and_keeps_policy_after_a_sent_prefix() {
        let mut queue = PendingQueue::new();
        let created = Instant::now();
        let deadline = created + Duration::from_millis(100);
        let mut first = fragment(true, false, false);
        first.stream_generation = 7;
        first.created_at = Some(created);
        first.payload_type = PayloadProtocolIdentifier::Binary;
        first.reliability = MessageReliability::Timed { deadline };
        let mut tail = fragment(false, true, false);
        tail.stream_generation = 7;
        tail.reliability = MessageReliability::Rexmit {
            max_retransmits: 99,
        };
        let chunks = vec![first, tail];
        let buffer = chunks.as_ptr();
        let capacity = chunks.capacity();
        queue.push_message(chunks);
        let Some(PendingEntry::Message(QueuedMessage::Fragmented { fragments, .. })) =
            queue.ordered_queue.front()
        else {
            panic!("expected a fragmented message");
        };
        assert_eq!(buffer, fragments.as_slices().0.as_ptr());
        assert_eq!(capacity, fragments.capacity());
        queue.pop(true, false).unwrap();
        // A different unordered message cannot interrupt the remaining tail.
        let mut other = fragment(true, true, true);
        other.message_id = MessageId::new(1);
        queue.push_message(vec![other]);
        assert_eq!(MessageId::new(0), queue.peek().unwrap().message_id);
        assert!(matches!(
            queue.ordered_queue.front(),
            Some(PendingEntry::Message(QueuedMessage::Fragmented {
                next_fragment: 1,
                ..
            }))
        ));
        let tail = queue.pop(false, false).unwrap();
        assert_eq!(Some(created), tail.created_at);
        assert_eq!(Some(deadline), tail.reliability.deadline());
        assert_eq!(PayloadProtocolIdentifier::Binary, tail.payload_type);
        assert_eq!(7, tail.stream_generation);
        assert_eq!(MessageId::new(1), queue.peek().unwrap().message_id);
        queue.pop(true, true).unwrap();
        assert!(queue.is_empty());
        assert_eq!(0, queue.get_num_bytes());
    }

    #[test]
    fn zero_size_data_is_not_a_reset_marker() {
        for id in [None, MessageId::new(1)] {
            let mut queue = PendingQueue::new();
            let mut data = fragment(true, true, false);
            data.message_id = id;
            data.user_data = Bytes::new();
            queue.push_message(vec![data]);
            assert_eq!(1, queue.len());
            assert_eq!(0, queue.get_num_bytes());
            assert!(queue.pop_ready_reset().is_none());
            assert_eq!(id, queue.peek().unwrap().message_id);
            queue.pop(true, false).unwrap();
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn sending_or_abandoning_a_tail_preserves_the_reset_boundary() -> Result<()> {
        for unordered in [false, true] {
            for abandon in [false, true] {
                let mut queue = PendingQueue::new();
                queue.push_message(vec![
                    fragment(true, false, unordered),
                    fragment(false, true, unordered),
                ]);
                queue.push_reset(ResetMarker {
                    stream_identifier: 0,
                });
                let mut later = fragment(true, true, false);
                later.message_id = MessageId::new(1);
                later.stream_generation += 1;
                queue.push_message(vec![later]);

                let id = queue.pop(true, unordered).unwrap().message_id.unwrap();
                assert!(queue.pop_ready_reset().is_none());
                assert_eq!(3, queue.len());
                assert_eq!(8, queue.get_num_bytes());
                if abandon {
                    assert_eq!(1, queue.drain_message(id)?.len());
                    assert!(queue.drain_message(id)?.is_empty());
                } else {
                    assert!(queue.pop(false, unordered).unwrap().ending_fragment);
                }
                // Neither DATA operation can consume a control marker.
                assert!(queue.peek().is_none());
                assert!(queue.pop(true, false).is_none());
                assert_eq!(0, queue.pop_ready_reset().unwrap().stream_identifier);
                assert!(queue.pop_ready_reset().is_none());
                assert_eq!(1, queue.len());
                assert_eq!(4, queue.get_num_bytes());
                assert!(queue.drain_message(id)?.is_empty());
                let later = queue.pop(true, false).unwrap();
                assert_eq!(MessageId::new(1), later.message_id);
                assert_eq!(1, later.stream_generation);
                assert!(queue.is_empty());
                assert_eq!(0, queue.get_num_bytes());
            }
        }
        Ok(())
    }
}
