//! The Sans-I/O TURN client.
//!
//! Using a relay takes three steps: Allocate to obtain a public address, CreatePermission for
//! each peer you intend to exchange data with, and then Send/Data indications (or a bound channel)
//! to move bytes. Each step is a STUN transaction, so every [`Event`] carries the
//! transaction id of the request it answers.
//!
//! The three address kinds are easy to confuse: [`RelayedAddr`] is what peers send to,
//! [`ReflexiveAddr`] is how the server sees this client, and [`PeerAddr`] is the far end.
#[cfg(test)]
mod client_test;

/// Channel bindings, which replace the 36-byte Data indication header with a 4-byte one.
pub mod binding;
/// Per-peer send permissions, which a relay requires before it will forward to an address.
pub mod permission;
mod proto;
/// A live allocation on the server, and sending or receiving through it.
pub mod relay;
/// Outstanding request tracking, with the RFC's retransmission schedule.
pub mod transaction;

use bytes::BytesMut;
use crypto::RTCCryptoProvider;
use log::{debug, trace};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use stun::attributes::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use stun::xoraddr::*;

use binding::*;
use transaction::*;

use crate::client::relay::{Relay, RelayState};
use crate::proto::PROTO_UDP;
use crate::proto::chandata::*;
use crate::proto::channum::ChannelNumber;
use crate::proto::data::*;
use crate::proto::lifetime::Lifetime;
use crate::proto::peeraddr::*;
use crate::proto::relayaddr::RelayedAddress;
use crate::proto::reqtrans::RequestedTransport;
use shared::error::{Error, Result};
use shared::util::lookup_host;
use shared::{TransportContext, TransportMessage, TransportProtocol};
use stun::error_code::ErrorCodeAttribute;
use stun::fingerprint::FINGERPRINT;

const DEFAULT_RTO_IN_MS: u64 = 200;
const MAX_DATA_BUFFER_SIZE: usize = u16::MAX as usize; // message size limit for Chromium
const MAX_READ_QUEUE_SIZE: usize = 1024;

/// The public address the TURN server allocated on this client's behalf.
///
/// Peers send here; the server forwards to the client.
pub type RelayedAddr = SocketAddr;
/// The client's own address as seen by the server — its server-reflexive address.
pub type ReflexiveAddr = SocketAddr;
/// The address of a remote peer the client exchanges data with through the relay.
pub type PeerAddr = SocketAddr;

#[derive(Debug)]
/// What the client produces in response to inbound datagrams and elapsed time.
///
/// Every variant carries the [`TransactionId`] of the request it answers, so a caller can
/// match responses to the requests it issued.
#[non_exhaustive]
pub enum Event {
    /// A request exhausted its retransmissions without a response.
    TransactionTimeout(TransactionId),

    /// A STUN Binding succeeded, reporting this client's server-reflexive address.
    BindingResponse(TransactionId, ReflexiveAddr),
    /// A STUN Binding failed.
    BindingError(TransactionId, Error),

    /// An Allocate succeeded; the relayed address is now usable.
    AllocateResponse(TransactionId, RelayedAddr),
    /// An Allocate failed — commonly authentication, or the server being out of ports.
    AllocateError(TransactionId, Error),

    /// A CreatePermission succeeded; the relay will now forward to and from this peer.
    CreatePermissionResponse(TransactionId, PeerAddr),
    /// A CreatePermission failed.
    CreatePermissionError(TransactionId, Error),

    /// Data arrived from a peer through the relay.
    ///
    /// The channel number is `Some` when it came as ChannelData and `None` when it came as a
    /// Data indication.
    DataIndicationOrChannelData(Option<ChannelNumber>, PeerAddr, BytesMut),
}

enum AllocateState {
    Attempting,
    Requesting(TextAttribute),
}

//              interval [msec]
// 0: 0 ms      +500
// 1: 500 ms	+1000
// 2: 1500 ms   +2000
// 3: 3500 ms   +4000
// 4: 7500 ms   +8000
// 5: 15500 ms  +16000
// 6: 31500 ms  +32000
// -: 63500 ms  failed

/// ClientConfig is a bag of config parameters for Client.
pub struct ClientConfig {
    /// The STUN server to use for Binding requests, as `host:port`. May be empty.
    pub stun_serv_addr: String, // STUN server address (e.g. "stun.abc.com:3478")
    /// The TURN server to allocate from, as `host:port`.
    pub turn_serv_addr: String, // TURN server address (e.g. "turn.abc.com:3478")
    /// The local address the client sends from.
    pub local_addr: SocketAddr,
    /// Whether to reach the server over UDP or TCP.
    pub transport_protocol: TransportProtocol,
    /// The long-term credential username for the TURN server.
    pub username: String,
    /// The long-term credential password.
    pub password: String,
    /// The authentication realm, used in the `MESSAGE-INTEGRITY` computation.
    pub realm: String,
    /// An optional `SOFTWARE` attribute value, sent for diagnostics.
    pub software: String,
    /// The initial retransmission timeout in milliseconds; each retry doubles it.
    pub rto_in_ms: u64,
    /// Optional upper bound for the interval between allocation Refresh requests.
    ///
    /// By default, allocations are refreshed at half the lifetime advertised by the server.
    /// A shorter cap can also keep the client-to-server NAT mapping active when an allocation
    /// waits without carrying application traffic. Values below one second are rounded up.
    pub allocation_refresh_interval_cap: Option<Duration>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            stun_serv_addr: "".to_string(),
            turn_serv_addr: "".to_string(),
            local_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
            transport_protocol: Default::default(),
            username: "".to_string(),
            password: "".to_string(),
            realm: "".to_string(),
            software: "".to_string(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        }
    }
}

/// Client is a STUN client
pub struct Client {
    crypto_provider: Arc<dyn RTCCryptoProvider>,
    stun_serv_addr: Option<SocketAddr>,
    turn_serv_addr: Option<SocketAddr>,
    local_addr: SocketAddr,
    transport_protocol: TransportProtocol,
    username: Username,
    password: String,
    realm: Realm,
    software: Software,
    tr_map: TransactionMap,
    binding_mgr: BindingManager,
    rto_in_ms: u64,
    allocation_refresh_interval_cap: Option<Duration>,

    relays: HashMap<RelayedAddr, RelayState>,
    transmits: VecDeque<TransportMessage<BytesMut>>,
    events: VecDeque<Event>,
}

impl Client {
    /// Returns a new TURN client.
    ///
    /// The crypto provider is supplied by the caller; this crate never resolves a default.
    pub fn new(config: ClientConfig, crypto_provider: Arc<dyn RTCCryptoProvider>) -> Result<Self> {
        let stun_serv_addr = if config.stun_serv_addr.is_empty() {
            None
        } else {
            Some(lookup_host(
                config.local_addr.is_ipv4(),
                config.stun_serv_addr.as_str(),
            )?)
        };

        let turn_serv_addr = if config.turn_serv_addr.is_empty() {
            None
        } else {
            Some(lookup_host(
                config.local_addr.is_ipv4(),
                config.turn_serv_addr.as_str(),
            )?)
        };

        Ok(Client {
            crypto_provider: crypto_provider.clone(),
            stun_serv_addr,
            turn_serv_addr,
            local_addr: config.local_addr,
            transport_protocol: config.transport_protocol,
            username: Username::new(ATTR_USERNAME, config.username),
            password: config.password,
            realm: Realm::new(ATTR_REALM, config.realm),
            software: Software::new(ATTR_SOFTWARE, config.software),
            tr_map: TransactionMap::new(),
            binding_mgr: BindingManager::new(),
            rto_in_ms: if config.rto_in_ms != 0 {
                config.rto_in_ms
            } else {
                DEFAULT_RTO_IN_MS
            },
            allocation_refresh_interval_cap: config.allocation_refresh_interval_cap,
            relays: HashMap::new(),
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        })
    }

    // handle_inbound handles data received.
    // This method handles incoming packet demultiplex it by the source address
    // and the types of the message.
    // This return Ok(handled or not) and if there was an error.
    // Caller should check if the packet was handled by this client or not.
    // If not handled, it is assumed that the packet is application data.
    // If an error is returned, the caller should discard the packet regardless.
    fn handle_inbound(&mut self, now: Instant, data: &[u8], from: SocketAddr) -> Result<()> {
        // +-------------------+-------------------------------+
        // |   Return Values   |                               |
        // +-------------------+       Meaning / Action        |
        // | handled |  error  |                               |
        // |=========+=========+===============================+
        // |  false  |   nil   | Handle the packet as app data |
        // |---------+---------+-------------------------------+
        // |  true   |   nil   |        Nothing to do          |
        // |---------+---------+-------------------------------+
        // |  false  |  error  |     (shouldn't happen)        |
        // |---------+---------+-------------------------------+
        // |  true   |  error  | Error occurred while handling |
        // +---------+---------+-------------------------------+
        // Possible causes of the error:
        //  - Malformed packet (parse error)
        //  - STUN message was a request
        //  - Non-STUN message from the STUN server

        if is_stun_message(data) {
            self.handle_stun_message(now, data)
        } else if ChannelData::is_channel_data(data) {
            self.handle_channel_data(data)
        } else if self.stun_serv_addr.is_some() && &from == self.stun_serv_addr.as_ref().unwrap() {
            // received from STUN server, but it is not a STUN message
            Err(Error::ErrNonStunmessage)
        } else {
            // assume, this is an application data
            trace!("non-STUN/TURN packet, unhandled");
            Ok(())
        }
    }

    fn handle_stun_message(&mut self, now: Instant, data: &[u8]) -> Result<()> {
        let mut msg = Message::new();
        msg.raw = data.to_vec();
        msg.decode()?;

        if msg.typ.class == CLASS_REQUEST {
            return Err(Error::Other(format!(
                "{:?} : {}",
                Error::ErrUnexpectedStunrequestMessage,
                msg
            )));
        }

        if msg.typ.class == CLASS_INDICATION {
            if msg.typ.method == METHOD_DATA {
                let mut peer_addr = PeerAddress::default();
                peer_addr.get_from(&msg)?;
                let from = SocketAddr::new(peer_addr.ip, peer_addr.port);

                let mut data = Data::default();
                data.get_from(&msg)?;

                debug!("data indication received from {}", from);

                self.events.push_back(Event::DataIndicationOrChannelData(
                    None,
                    from,
                    BytesMut::from(&data.0[..]),
                ))
            }

            return Ok(());
        }

        // This is a STUN response message (transactional)
        // The type is either:
        // - stun.ClassSuccessResponse
        // - stun.ClassErrorResponse

        if self.tr_map.find(&msg.transaction_id).is_none() {
            // silently discard
            debug!("no transaction for {}", msg);
            return Ok(());
        }

        if let Some(tr) = self.tr_map.delete(&msg.transaction_id) {
            match msg.typ.method {
                METHOD_BINDING => {
                    if msg.typ.class == CLASS_ERROR_RESPONSE {
                        let mut code = ErrorCodeAttribute::default();
                        let err = if code.get_from(&msg).is_err() {
                            Error::Other(format!("{}", msg.typ))
                        } else {
                            Error::Other(format!("{} (error {})", msg.typ, code))
                        };
                        self.events
                            .push_back(Event::BindingError(tr.transaction_id, err));
                    } else {
                        let mut refl_addr = XorMappedAddress::default();
                        match refl_addr.get_from(&msg) {
                            Ok(_) => {
                                self.events.push_back(Event::BindingResponse(
                                    tr.transaction_id,
                                    ReflexiveAddr::new(refl_addr.ip, refl_addr.port),
                                ));
                            }
                            Err(err) => {
                                self.events
                                    .push_back(Event::BindingError(tr.transaction_id, err));
                            }
                        }
                    }
                }
                METHOD_ALLOCATE => {
                    self.handle_allocate_response(now, msg, tr.transaction_type)?;
                }
                METHOD_CREATE_PERMISSION => {
                    if let TransactionType::CreatePermissionRequest(relayed_addr, peer_addr) =
                        tr.transaction_type
                    {
                        let mut relay = Relay {
                            relayed_addr,
                            client: self,
                        };
                        relay.handle_create_permission_response(msg, peer_addr)?;
                    }
                }
                METHOD_REFRESH => {
                    if let TransactionType::RefreshRequest(relayed_addr) = tr.transaction_type {
                        let mut relay = Relay {
                            relayed_addr,
                            client: self,
                        };
                        relay.handle_refresh_allocation_response(now, msg)?;
                    }
                }
                METHOD_CHANNEL_BIND => {
                    if let TransactionType::ChannelBindRequest(relayed_addr, bind_addr) =
                        tr.transaction_type
                    {
                        let mut relay = Relay {
                            relayed_addr,
                            client: self,
                        };
                        relay.handle_channel_bind_response(now, msg, bind_addr)?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_channel_data(&mut self, data: &[u8]) -> Result<()> {
        let mut ch_data = ChannelData {
            raw: data.to_vec(),
            ..Default::default()
        };
        ch_data.decode()?;

        let addr = self
            .find_addr_by_channel_number(ch_data.number.0)
            .ok_or(Error::ErrChannelBindNotFound)?;

        trace!(
            "channel data received from {} (ch={})",
            addr, ch_data.number.0
        );

        self.events.push_back(Event::DataIndicationOrChannelData(
            Some(ch_data.number),
            addr,
            BytesMut::from(&ch_data.data[..]),
        ));

        Ok(())
    }

    /// Borrows the allocation for `relayed_addr` so data can be sent or permissions created.
    ///
    /// # Errors
    ///
    /// Fails if this client has no allocation for that address — it was never allocated, or has
    /// already been closed.
    pub fn relay(&mut self, relayed_addr: SocketAddr) -> Result<Relay<'_>> {
        if !self.relays.contains_key(&relayed_addr) {
            Err(Error::ErrStreamNotExisted)
        } else {
            Ok(Relay {
                relayed_addr,
                client: self,
            })
        }
    }

    /// Whether this client is waiting on a response for transaction `id`.
    ///
    /// A STUN response belongs to whoever sent the matching request — [RFC 5389 §7.3.3] matches
    /// them by transaction ID, and nothing else identifies the owner. That is only a question
    /// worth asking when several STUN users share one socket, which is exactly the situation a
    /// host is in when it points a STUN gatherer and this client at the same server address: both
    /// see every response, and the four-tuple is identical for both, so addresses cannot say whose
    /// a response is.
    ///
    /// Such a host calls this to route a response, rather than guessing from the peer address and
    /// handing it to a client that will silently discard it — [`handle_read`](Self::handle_read)
    /// drops a response with no matching transaction, as it must.
    ///
    /// Applies to responses only. TURN ChannelData carries no transaction ID at all, and
    /// indications are not transaction-matched; both are routed by address.
    ///
    /// [RFC 5389 §7.3.3]: https://www.rfc-editor.org/rfc/rfc5389#section-7.3.3
    #[must_use]
    pub fn has_transaction(&self, id: &TransactionId) -> bool {
        self.tr_map.find(id).is_some()
    }

    /// send_binding_request_to sends a new STUN request to the given transport address
    /// return key to find out corresponding Event either BindingResponse or BindingRequestTimeout
    pub fn send_binding_request_to(
        &mut self,
        now: Instant,
        to: SocketAddr,
    ) -> Result<TransactionId> {
        let msg = {
            let attrs: Vec<Box<dyn Setter + '_>> = if !self.software.text.is_empty() {
                vec![
                    Box::new(TransactionId::new()),
                    Box::new(BINDING_REQUEST),
                    Box::new(self.software.clone()),
                ]
            } else {
                vec![Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)]
            };

            let mut msg = Message::new();
            msg.build(&attrs)?;
            msg
        };

        debug!("client.SendBindingRequestTo call PerformTransaction 1");
        Ok(self.perform_transaction(now, &msg, to, TransactionType::BindingRequest))
    }

    /// send_binding_request sends a new STUN request to the STUN server
    /// return key to find out corresponding Event either BindingResponse or BindingRequestTimeout
    pub fn send_binding_request(&mut self, now: Instant) -> Result<TransactionId> {
        if let Some(stun_serv_addr) = &self.stun_serv_addr {
            self.send_binding_request_to(now, *stun_serv_addr)
        } else {
            Err(Error::ErrStunserverAddressNotSet)
        }
    }

    // find_addr_by_channel_number returns a peer address associated with the
    // channel number on this UDPConn
    fn find_addr_by_channel_number(&self, ch_num: u16) -> Option<SocketAddr> {
        self.binding_mgr.find_by_number(ch_num).map(|b| b.addr)
    }

    // stun_server_addr return the STUN server address
    fn stun_server_addr(&self) -> Option<SocketAddr> {
        self.stun_serv_addr
    }

    /* https://datatracker.ietf.org/doc/html/rfc8656#section-20
    TURN                                 TURN          Peer         Peer
    client                               server         A            B
      |                                    |            |            |
      |--- Allocate request -------------->|            |            |
      |    Transaction-Id=0xA56250D3F17ABE679422DE85    |            |
      |    SOFTWARE="Example client, version 1.03"      |            |
      |    LIFETIME=3600 (1 hour)          |            |            |
      |    REQUESTED-TRANSPORT=17 (UDP)    |            |            |
      |    DONT-FRAGMENT                   |            |            |
      |                                    |            |            |
      |<-- Allocate error response --------|            |            |
      |    Transaction-Id=0xA56250D3F17ABE679422DE85    |            |
      |    SOFTWARE="Example server, version 1.17"      |            |
      |    ERROR-CODE=401 (Unauthorized)   |            |            |
      |    REALM="example.com"             |            |            |
      |    NONCE="obMatJos2gAAAadl7W7PeDU4hKE72jda"     |            |
      |    PASSWORD-ALGORITHMS=MD5 and SHA256           |            |
      |                                    |            |            |
      |--- Allocate request -------------->|            |            |
      |    Transaction-Id=0xC271E932AD7446A32C234492    |            |
      |    SOFTWARE="Example client 1.03"  |            |            |
      |    LIFETIME=3600 (1 hour)          |            |            |
      |    REQUESTED-TRANSPORT=17 (UDP)    |            |            |
      |    DONT-FRAGMENT                   |            |            |
      |    USERNAME="George"               |            |            |
      |    REALM="example.com"             |            |            |
      |    NONCE="obMatJos2gAAAadl7W7PeDU4hKE72jda"     |            |
      |    PASSWORD-ALGORITHMS=MD5 and SHA256           |            |
      |    PASSWORD-ALGORITHM=SHA256       |            |            |
      |    MESSAGE-INTEGRITY=...           |            |            |
      |    MESSAGE-INTEGRITY-SHA256=...    |            |            |
      |                                    |            |            |
      |<-- Allocate success response ------|            |            |
      |    Transaction-Id=0xC271E932AD7446A32C234492    |            |
      |    SOFTWARE="Example server, version 1.17"      |            |
      |    LIFETIME=1200 (20 minutes)      |            |            |
      |    XOR-RELAYED-ADDRESS=192.0.2.15:50000         |            |
      |    XOR-MAPPED-ADDRESS=192.0.2.1:7000            |            |
      |    MESSAGE-INTEGRITY-SHA256=...    |            |            |
    */
    /// Replaces the long-term credential used to sign subsequent requests, **keeping any
    /// existing allocation**.
    ///
    /// A TURN allocation is a property of the 5-tuple, not of the credential that created
    /// it: [RFC 5766 §6.2] identifies an allocation by 5-tuple, and a server's
    /// `Refresh` handling looks it up the same way. So when credentials are rotated on the
    /// same server there is no need to give up the allocation and re-`Allocate` — which
    /// would in fact be rejected with **437 (Allocation Mismatch)**, since the server still
    /// holds the previous allocation for that 5-tuple. Re-signing the existing allocation is
    /// both correct and seamless: permissions and channel bindings survive.
    ///
    /// The realm is *not* re-negotiated. It was learned from the server's 401 during the
    /// first `Allocate`, and a credential rotation keeps the same server, so it still
    /// applies. Follow this with [`Self::refresh_allocations`] so the server sees the new
    /// credential before the allocation would otherwise expire.
    ///
    /// [RFC 5766 §6.2]: https://datatracker.ietf.org/doc/html/rfc5766#section-6.2
    pub fn update_credentials(&mut self, username: String, password: String) -> Result<()> {
        let username = Username::new(ATTR_USERNAME, username);
        let long_term_integrity_key = MessageIntegrity::long_term_integrity_key(
            username.text.clone(),
            self.realm.text.clone(),
            password.clone(),
            self.crypto_provider.crypto(),
        )?;
        self.username = username;
        self.password = password;

        // Each allocation carries the integrity it will sign its own Refresh /
        // CreatePermission / ChannelBind with, so they have to be re-signed too — otherwise
        // the next refresh would still present the retired credential.
        for relay in self.relays.values_mut() {
            relay.long_term_integrity_key = long_term_integrity_key.clone();
        }

        Ok(())
    }

    /// Refreshes every live allocation, re-signing each with the current credential.
    ///
    /// Each allocation is refreshed with its own current lifetime, so this extends rather
    /// than changes it. Intended to follow [`update_credentials`](Self::update_credentials).
    pub fn refresh_allocations(&mut self, now: Instant) -> Result<()> {
        let relays: Vec<(RelayedAddr, Duration)> = self
            .relays
            .iter()
            .map(|(addr, relay)| (*addr, relay.lifetime))
            .collect();

        for (relayed_addr, lifetime) in relays {
            self.relay(relayed_addr)?
                .refresh_allocation(now, lifetime)?;
        }

        Ok(())
    }

    /// Allocate sends a TURN allocation request to the given transport address
    pub fn allocate(&mut self, now: Instant) -> Result<TransactionId> {
        let mut msg = Message::new();
        msg.build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
            Box::new(RequestedTransport {
                // Always a UDP relay, however the server is reached: REQUESTED-TRANSPORT
                // names what the allocation relays, not the client-to-server transport
                // (RFC 8656 §7.1). TCP allocations (RFC 6062) are not implemented here.
                protocol: PROTO_UDP,
            }),
            Box::new(FINGERPRINT),
        ])?;

        debug!("client.Allocate call PerformTransaction 1");
        let mut tid = self.perform_transaction(
            now,
            &msg,
            self.turn_server_addr()?,
            TransactionType::AllocateAttempt,
        );
        tid.0[TRANSACTION_ID_SIZE - 1] = tid.0[TRANSACTION_ID_SIZE - 1].wrapping_add(1);
        Ok(tid)
    }

    fn handle_allocate_response(
        &mut self,
        now: Instant,
        response: Message,
        allocate_state: TransactionType,
    ) -> Result<()> {
        match allocate_state {
            TransactionType::AllocateAttempt => {
                // Anonymous allocate failed, trying to authenticate.
                let nonce = match Nonce::get_from_as(&response, ATTR_NONCE) {
                    Ok(nonce) => nonce,
                    Err(err) => {
                        self.events
                            .push_back(Event::AllocateError(response.transaction_id, err));
                        return Ok(());
                    }
                };
                self.realm = match Realm::get_from_as(&response, ATTR_REALM) {
                    Ok(realm) => realm,
                    Err(err) => {
                        self.events
                            .push_back(Event::AllocateError(response.transaction_id, err));
                        return Ok(());
                    }
                };

                let integrity = MessageIntegrity::new_long_term_integrity_with_provider(
                    self.username.text.clone(),
                    self.realm.text.clone(),
                    self.password.clone(),
                    self.crypto_provider.crypto(),
                )?;

                let mut msg = Message::new();

                // make it same as allocate() return value so that client can retrieve it
                // from Event::AllocateResponse
                let mut tid = response.transaction_id;
                tid.0[TRANSACTION_ID_SIZE - 1] = tid.0[TRANSACTION_ID_SIZE - 1].wrapping_add(1);

                // Trying to authorize.
                msg.build(&[
                    Box::new(tid),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                    Box::new(RequestedTransport {
                        // Always a UDP relay, however the server is reached: REQUESTED-TRANSPORT
                        // names what the allocation relays, not the client-to-server transport
                        // (RFC 8656 §7.1). TCP allocations (RFC 6062) are not implemented here.
                        protocol: PROTO_UDP,
                    }),
                    Box::new(self.username.clone()),
                    Box::new(self.realm.clone()),
                    Box::new(nonce.clone()),
                    Box::new(integrity),
                    Box::new(FINGERPRINT),
                ])?;

                debug!("client.Allocate call PerformTransaction 2");
                self.perform_transaction(
                    now,
                    &msg,
                    self.turn_server_addr()?,
                    TransactionType::AllocateRequest(nonce),
                );
            }
            TransactionType::AllocateRequest(nonce) => {
                if response.typ.class == CLASS_ERROR_RESPONSE {
                    let mut code = ErrorCodeAttribute::default();
                    let err = if code.get_from(&response).is_err() {
                        Error::Other(format!("{}", response.typ))
                    } else {
                        Error::Other(format!("{} (error {})", response.typ, code))
                    };
                    self.events
                        .push_back(Event::AllocateError(response.transaction_id, err));
                    return Ok(());
                }

                // Getting relayed addresses from response.
                let mut relayed = RelayedAddress::default();
                relayed.get_from(&response)?;
                let relayed_addr = RelayedAddr::new(relayed.ip, relayed.port);

                // Getting lifetime from response
                let mut lifetime = Lifetime::default();
                lifetime.get_from(&response)?;

                // A zero lifetime here is a protocol violation, not a degenerate allocation.
                // RFC 5766 §6.2 has the server take `min(client proposed, server maximum)` and
                // fall back to the *default* lifetime (600 s) whenever that computation does
                // not exceed it — so the value returned by a successful Allocate is never
                // below the default, and certainly never zero. Zero is meaningful only on the
                // Refresh path (§7), where it means "allocation deleted".
                //
                // Accepting it would build a `RelayState` whose `refresh_alloc_timer` is
                // `now.add(0)` — expired the instant it is created — for an allocation that is
                // already gone. That relay then reports an expired refresh deadline forever,
                // which is what a caller polling deadlines hot-loops on. See
                // [webrtc#862](https://github.com/webrtc-rs/webrtc/issues/862).
                if lifetime.0.is_zero() {
                    self.events.push_back(Event::AllocateError(
                        response.transaction_id,
                        Error::Other(
                            "Allocate success response carried LIFETIME=0; RFC 5766 §6.2 \
                             requires at least the default lifetime"
                                .to_owned(),
                        ),
                    ));
                    return Ok(());
                }

                self.relays.insert(
                    relayed_addr,
                    RelayState::new(
                        now,
                        relayed_addr,
                        MessageIntegrity::long_term_integrity_key(
                            self.username.text.clone(),
                            self.realm.text.clone(),
                            self.password.clone(),
                            self.crypto_provider.crypto(),
                        )?,
                        nonce,
                        lifetime.0,
                        self.allocation_refresh_interval_cap,
                    ),
                );
                self.events.push_back(Event::AllocateResponse(
                    response.transaction_id,
                    relayed_addr,
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// turn_server_addr return the TURN server address
    fn turn_server_addr(&self) -> Result<SocketAddr> {
        self.turn_serv_addr.ok_or(Error::ErrNilTurnSocket)
    }

    /// username returns username
    fn username(&self) -> Username {
        self.username.clone()
    }

    /// realm return realm
    fn realm(&self) -> Realm {
        self.realm.clone()
    }

    /// WriteTo sends data to the specified destination using the base socket.
    fn write_to(&mut self, now: Instant, data: &[u8], remote: SocketAddr) {
        self.transmits.push_back(TransportMessage {
            now,
            transport: TransportContext {
                local_addr: self.local_addr,
                peer_addr: remote,
                transport_protocol: self.transport_protocol,
                ecn: None,
            },
            message: BytesMut::from(data),
        });
    }

    // PerformTransaction performs STUN transaction
    fn perform_transaction(
        &mut self,
        now: Instant,
        msg: &Message,
        to: SocketAddr,
        transaction_type: TransactionType,
    ) -> TransactionId {
        let tr = Transaction::new(TransactionConfig {
            now,
            transaction_id: msg.transaction_id,
            transaction_type,
            raw: BytesMut::from(&msg.raw[..]),
            local_addr: self.local_addr,
            peer_addr: to,
            transport_protocol: self.transport_protocol,
            interval: self.rto_in_ms,
        });

        trace!(
            "start {} transaction {:?} to {}",
            msg.typ, msg.transaction_id, tr.peer_addr
        );
        self.tr_map.insert(msg.transaction_id, tr);

        self.write_to(now, &msg.raw, to);

        msg.transaction_id
    }
}
