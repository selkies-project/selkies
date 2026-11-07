//! Fixed-work end-to-end SCTP association benchmark for `perf`/`poop`.
//!
//! Connects two sans-IO endpoints in memory (no sockets, virtual clock) and
//! streams `num_msgs` messages of `msg_len` bytes from client to server over a
//! single reliable ordered stream. Unlike `sctp_micro` (marshal only), this
//! exercises the full steady-state transfer path on both sides: fragmentation,
//! pending/payload queues, congestion control, packet bundling, marshal,
//! endpoint demux, unmarshal, SACK generation/processing, and reassembly.
//!
//! Usage: sctp_e2e [msg_len] [num_msgs]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use rtc_sctp::{
    Association, AssociationHandle, ClientConfig, DatagramEvent, Endpoint, EndpointConfig, Event,
    Payload, PayloadProtocolIdentifier, ServerConfig, TransportConfig,
};
use shared::TransportProtocol;

/// Cap on locally buffered (not yet transmitted/acked) user data, so the
/// pending queue stays realistic instead of absorbing the whole workload.
const SEND_BUF_CAP: usize = 1 << 20;

struct Node {
    endpoint: Endpoint,
    peer_addr: SocketAddr,
    handle: Option<AssociationHandle>,
    assoc: Option<Association>,
    timeout: Option<Instant>,
    connected: bool,
}

impl Node {
    fn new(endpoint: Endpoint, peer_addr: SocketAddr) -> Self {
        Self {
            endpoint,
            peer_addr,
            handle: None,
            assoc: None,
            timeout: None,
            connected: false,
        }
    }

    /// Deliver queued inbound datagrams, fire due timers, drain events and
    /// transmits. Returns true if any packet was consumed or produced.
    fn drive(
        &mut self,
        now: Instant,
        inbound: &mut VecDeque<Bytes>,
        outbound: &mut VecDeque<Bytes>,
    ) -> bool {
        let mut did_work = false;

        while let Some(data) = inbound.pop_front() {
            did_work = true;
            if let Some((ch, event)) = self.endpoint.handle(now, self.peer_addr, None, data) {
                match event {
                    DatagramEvent::NewAssociation(assoc) => {
                        self.handle = Some(ch);
                        self.assoc = Some(assoc);
                    }
                    DatagramEvent::AssociationEvent(event) => {
                        if let Some(assoc) = self.assoc.as_mut() {
                            assoc.handle_event(event);
                        }
                    }
                    _ => {}
                }
            }
        }

        let Some(assoc) = self.assoc.as_mut() else {
            return did_work;
        };

        if let Some(t) = self.timeout
            && t <= now
        {
            self.timeout = None;
            assoc.handle_timeout(now);
        }

        while let Some(event) = assoc.poll() {
            if matches!(event, Event::Connected) {
                self.connected = true;
            }
        }

        while let Some(event) = assoc.poll_endpoint_event() {
            if let Some(ch) = self.handle {
                self.endpoint.handle_event(ch, event);
            }
        }

        while let Some(transmit) = assoc.poll_transmit(now) {
            if let Payload::RawEncode(contents) = transmit.message {
                for content in contents {
                    outbound.push_back(content);
                    did_work = true;
                }
            }
        }
        self.timeout = assoc.poll_timeout();

        did_work
    }
}

fn min_opt<T: Ord>(x: Option<T>, y: Option<T>) -> Option<T> {
    match (x, y) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let msg_len: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1200);
    let num_msgs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(50_000);

    let client_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let server_addr: SocketAddr = "127.0.0.1:5001".parse().unwrap();

    let endpoint_config = Arc::new(EndpointConfig::default());
    let mut client = Node::new(
        Endpoint::new(
            client_addr,
            TransportProtocol::UDP,
            endpoint_config.clone(),
            None,
        ),
        server_addr,
    );
    let mut server = Node::new(
        Endpoint::new(
            server_addr,
            TransportProtocol::UDP,
            endpoint_config,
            Some(Arc::new(ServerConfig::new(TransportConfig::default()))),
        ),
        client_addr,
    );

    let (ch, assoc) = client
        .endpoint
        .connect(
            Instant::now(),
            ClientConfig::new(TransportConfig::default()),
            server_addr,
        )
        .unwrap();
    client.handle = Some(ch);
    client.assoc = Some(assoc);

    // client -> server and server -> client "wires" (zero latency, no loss).
    let mut c2s: VecDeque<Bytes> = VecDeque::new();
    let mut s2c: VecDeque<Bytes> = VecDeque::new();

    let mut now = Instant::now();
    let start = now;

    // Drive the handshake.
    while !(client.connected && server.connected) {
        let worked = client.drive(now, &mut s2c, &mut c2s) | server.drive(now, &mut c2s, &mut s2c);
        if !worked && c2s.is_empty() && s2c.is_empty() {
            match min_opt(client.timeout, server.timeout) {
                Some(t) => now = now.max(t),
                None => panic!("handshake deadlocked"),
            }
        }
    }

    client
        .assoc
        .as_mut()
        .unwrap()
        .open_stream(0, PayloadProtocolIdentifier::Binary)
        .unwrap();

    let msg = Bytes::from(vec![0x5au8; msg_len]);
    let total_bytes = msg_len as u64 * num_msgs;
    let mut sent: u64 = 0;
    let mut received: u64 = 0;
    let mut accepted = false;
    let mut read_buf = vec![0u8; msg_len.max(1)];

    while received < total_bytes {
        // Writer: top up the send buffer.
        let mut wrote = false;
        {
            let assoc = client.assoc.as_mut().unwrap();
            let mut stream = assoc.stream(0).unwrap();
            while sent < num_msgs && stream.buffered_amount().unwrap() < SEND_BUF_CAP {
                stream
                    .write_sctp(now, &msg, PayloadProtocolIdentifier::Binary)
                    .unwrap();
                sent += 1;
                wrote = true;
            }
        }

        let worked = client.drive(now, &mut s2c, &mut c2s) | server.drive(now, &mut c2s, &mut s2c);

        // Reader: drain everything that has been reassembled.
        let mut read = false;
        {
            let assoc = server.assoc.as_mut().unwrap();
            if !accepted {
                accepted = assoc.accept_stream().is_some();
            }
            if accepted {
                let mut stream = assoc.stream(0).unwrap();
                while let Some(chunks) = stream.read_sctp().unwrap_or(None) {
                    let n = chunks.read(&mut read_buf).unwrap();
                    std::hint::black_box(&read_buf[..n]);
                    received += n as u64;
                    read = true;
                }
            }
        }

        if !(worked || wrote || read) && c2s.is_empty() && s2c.is_empty() {
            match min_opt(client.timeout, server.timeout) {
                Some(t) if t > now => now = t,
                // Timer already due (or none): nudge virtual time forward so
                // handle_timeout fires rather than spinning forever.
                _ => panic!("transfer deadlocked at sent={sent} received={received}"),
            }
        }
    }

    let virt = now.duration_since(start);
    eprintln!(
        "transferred {total_bytes} bytes in {num_msgs} msgs of {msg_len}B (virtual time: {virt:?})"
    );
}
