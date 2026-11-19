use bytes::BytesMut;
use log::trace;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::ops::Add;
use std::time::{Duration, Instant};

use stun::message::*;

use crate::client::{Event, RelayedAddr};
use shared::{TransportContext, TransportMessage, TransportProtocol};
use stun::textattrs::TextAttribute;

const MAX_RTX_INTERVAL_IN_MS: u64 = 1600;
const MAX_RTX_COUNT: u16 = 7; // total 7 requests (Rc)
/// Ti (RFC 8489 §6.2.2): over a reliable transport a request is sent once, and
/// this is how long the client waits for its answer.
const RELIABLE_TRANSACTION_TIMEOUT: Duration = Duration::from_millis(39_500);

pub(crate) enum TransactionType {
    BindingRequest,
    AllocateAttempt,
    AllocateRequest(TextAttribute),
    CreatePermissionRequest(RelayedAddr, Option<SocketAddr>),
    RefreshRequest(RelayedAddr),
    ChannelBindRequest(RelayedAddr, SocketAddr),
}

// TransactionConfig is a set of config params used by NewTransaction
pub(crate) struct TransactionConfig {
    /// The instant the transaction starts, from which its first retransmit deadline is computed.
    pub(crate) now: Instant,
    pub(crate) transaction_id: TransactionId,
    pub(crate) transaction_type: TransactionType,
    pub(crate) raw: BytesMut,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) transport_protocol: TransportProtocol,
    pub(crate) interval: u64,
}

// Transaction represents a transaction
pub(crate) struct Transaction {
    pub(crate) transaction_id: TransactionId,
    pub(crate) transaction_type: TransactionType,
    pub(crate) raw: BytesMut,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) transport_protocol: TransportProtocol,
    pub(crate) n_rtx: u16,
    pub(crate) interval: u64,
    pub(crate) timeout: Instant,
    pub(crate) transmits: VecDeque<TransportMessage<BytesMut>>,
}

impl Transaction {
    // NewTransaction creates a new instance of Transaction
    pub(crate) fn new(config: TransactionConfig) -> Self {
        Self {
            transaction_id: config.transaction_id,
            transaction_type: config.transaction_type,
            raw: config.raw,
            local_addr: config.local_addr,
            peer_addr: config.peer_addr,
            transport_protocol: config.transport_protocol,
            n_rtx: 0,
            interval: config.interval,
            timeout: if is_reliable(config.transport_protocol) {
                config.now.add(RELIABLE_TRANSACTION_TIMEOUT)
            } else {
                config.now.add(Duration::from_millis(config.interval))
            },
            transmits: VecDeque::new(),
        }
    }

    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        if self.retries() < MAX_RTX_COUNT {
            Some(self.timeout)
        } else {
            None
        }
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        if self.retries() < MAX_RTX_COUNT && self.timeout <= now {
            if is_reliable(self.transport_protocol) {
                // One deadline and no retransmission: TCP already retransmits, and
                // repeating the request over it only sends the server duplicates.
                // Passing Ti exhausts the transaction, which reports the timeout.
                self.n_rtx = MAX_RTX_COUNT;
                return;
            }
            self.n_rtx += 1;
            self.interval *= 2;
            if self.interval > MAX_RTX_INTERVAL_IN_MS {
                self.interval = MAX_RTX_INTERVAL_IN_MS;
            }

            self.on_rtx_timeout(now);

            self.timeout = now.add(Duration::from_millis(self.interval));
        }
    }

    pub(crate) fn poll_transmit(&mut self) -> Option<TransportMessage<BytesMut>> {
        self.transmits.pop_front()
    }

    fn on_rtx_timeout(&mut self, now: Instant) {
        if self.n_rtx == MAX_RTX_COUNT {
            return;
        }

        trace!(
            "retransmitting transaction {:?} to {} (n_rtx={})",
            self.transaction_id, self.peer_addr, self.n_rtx
        );

        self.transmits.push_back(TransportMessage {
            now,
            transport: TransportContext {
                local_addr: self.local_addr,
                peer_addr: self.peer_addr,
                transport_protocol: self.transport_protocol,
                ecn: None,
            },
            message: self.raw.clone(),
        });
    }

    // retries returns the number of retransmission it has made
    pub(crate) fn retries(&self) -> u16 {
        self.n_rtx
    }
}

/// Whether the transport delivers reliably on its own, so a request must not be
/// retransmitted over it. Anything that is not UDP is a stream.
fn is_reliable(transport_protocol: TransportProtocol) -> bool {
    transport_protocol != TransportProtocol::UDP
}

// TransactionMap is a thread-safe transaction map
#[derive(Default)]
pub(crate) struct TransactionMap {
    tr_map: HashMap<TransactionId, Transaction>,
    transmits: VecDeque<TransportMessage<BytesMut>>,
    events: VecDeque<Event>,
}

impl TransactionMap {
    // NewTransactionMap create a new instance of the transaction map
    pub(crate) fn new() -> TransactionMap {
        TransactionMap {
            tr_map: HashMap::new(),
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub(crate) fn poll_timout(&self) -> Option<Instant> {
        let mut eto = None;
        for tr in self.tr_map.values() {
            if let Some(to) = tr.poll_timeout()
                && (eto.is_none() || to < *eto.as_ref().unwrap())
            {
                eto = Some(to);
            }
        }
        eto
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        let mut keys = vec![];
        for (key, tr) in self.tr_map.iter_mut() {
            tr.handle_timeout(now);
            if tr.retries() >= MAX_RTX_COUNT {
                keys.push(*key);
            }
        }

        for key in keys {
            self.tr_map.remove(&key);
            self.events.push_back(Event::TransactionTimeout(key));
        }
    }

    pub(crate) fn poll_transmit(&mut self) -> Option<TransportMessage<BytesMut>> {
        for tr in self.tr_map.values_mut() {
            while let Some(transmit) = tr.poll_transmit() {
                self.transmits.push_back(transmit);
            }
        }
        self.transmits.pop_front()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // Insert inserts a trasaction to the map
    pub(crate) fn insert(&mut self, tid: TransactionId, tr: Transaction) -> bool {
        self.tr_map.insert(tid, tr);
        true
    }

    // Find looks up a transaction by its key
    pub(crate) fn find(&self, tid: &TransactionId) -> Option<&Transaction> {
        self.tr_map.get(tid)
    }

    pub(crate) fn get(&mut self, tid: &TransactionId) -> Option<&mut Transaction> {
        self.tr_map.get_mut(tid)
    }

    // Delete deletes a transaction by its key
    pub(crate) fn delete(&mut self, tid: &TransactionId) -> Option<Transaction> {
        self.tr_map.remove(tid)
    }

    // close_and_delete_all closes and deletes all transactions
    pub(crate) fn delete_all(&mut self) {
        self.tr_map.clear();
    }

    // Size returns the length of the transaction map
    pub(crate) fn size(&self) -> usize {
        self.tr_map.len()
    }
}
