use super::*;
use mdns::{MDNS_PORT, MdnsEvent};

impl sansio::Protocol<TaggedBytesMut, (), ()> for Agent {
    type Rout = TaggedBytesMut;
    type Wout = TaggedBytesMut;
    type Eout = TaggedEvent;
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedBytesMut) -> Result<()> {
        // demuxing mDNS packet from STUN packet whether peer addr port is MDNS_PORT
        if msg.transport.peer_addr.port() == MDNS_PORT {
            let remote_candidates = if let Some(mdns_conn) = &mut self.mdns {
                mdns_conn.handle_read(msg)?;

                let mut remote_candidates = vec![];
                // After mdns handle_read, check any query result
                while let Some(event) = mdns_conn.poll_event() {
                    match event {
                        MdnsEvent::QueryAnswered(id, addr) => {
                            if let Some(mut c) = self.mdns_queries.remove(&id)
                                && c.set_ip(&addr).is_ok()
                            {
                                debug!(
                                    "mDNS query id {} answered Candidate {} with resolved_addr {} is added into remote candidates",
                                    id,
                                    c,
                                    c.addr()
                                );
                                remote_candidates.push(c);
                            }
                        }
                        MdnsEvent::QueryTimeout(id) => {
                            if let Some(c) = self.mdns_queries.remove(&id) {
                                error!("mDNS Query {} timed out for {}", id, c.address());
                            }
                        }
                        _ => {}
                    }
                }
                remote_candidates
            } else {
                vec![]
            };

            self.trigger_request_connectivity_check(remote_candidates);

            Ok(())
        } else if let Some(local_index) =
            self.find_local_candidate(msg.transport.local_addr, msg.transport.transport_protocol)
        {
            self.handle_inbound_candidate_msg(local_index, msg)
        } else {
            warn!(
                "[{}]: Discarded message, not a valid local candidate from {:?}:{}",
                self.get_name(),
                msg.transport.transport_protocol,
                msg.transport.local_addr,
            );
            Err(Error::ErrUnhandledStunpacket)
        }
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        None
    }

    fn handle_write(&mut self, _msg: ()) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        if let Some(mdns_conn) = &mut self.mdns {
            while let Some(msg) = mdns_conn.poll_write() {
                self.write_outs.push_back(msg);
            }
        }

        self.write_outs.pop_front()
    }

    fn handle_event(&mut self, _evt: ()) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn poll_event(&mut self) -> Option<Self::Eout> {
        self.event_outs.pop_front()
    }

    fn handle_timeout(&mut self, now: Self::Time) -> std::result::Result<(), Self::Error> {
        let remote_candidates = if let Some(mdns_conn) = &mut self.mdns {
            let _ = mdns_conn.handle_timeout(now);

            let mut remote_candidates = vec![];
            // After mdns handle_timeout, check any query result
            while let Some(event) = mdns_conn.poll_event() {
                match event {
                    MdnsEvent::QueryAnswered(id, addr) => {
                        if let Some(mut c) = self.mdns_queries.remove(&id)
                            && c.set_ip(&addr).is_ok()
                        {
                            debug!(
                                "mDNS query id {} answered Candidate {} with resolved_addr {} is added into remote candidates",
                                id,
                                c,
                                c.addr()
                            );
                            remote_candidates.push(c);
                        }
                    }
                    MdnsEvent::QueryTimeout(id) => {
                        if let Some(c) = self.mdns_queries.remove(&id) {
                            error!("mDNS Query {} timed out for {}", id, c.address());
                        }
                    }
                    _ => {}
                }
            }
            remote_candidates
        } else {
            vec![]
        };
        self.trigger_request_connectivity_check(remote_candidates);

        if self.ufrag_pwd.remote_credentials.is_some()
            && (self.force_candidate_contact
                || self.last_checking_time + self.get_timeout_interval() <= now)
        {
            self.contact(now);
        }
        Ok(())
    }

    fn poll_timeout(&mut self) -> Option<Self::Time> {
        let mdns_timeout = if let Some(mdns_conn) = &mut self.mdns {
            mdns_conn.poll_timeout()
        } else {
            None
        };

        let ice_timeout = if self.ufrag_pwd.remote_credentials.is_some()
            && !matches!(
                self.connection_state,
                ConnectionState::Failed | ConnectionState::Closed
            ) {
            if self.force_candidate_contact {
                // A connectivity check was requested; ask the driver to call
                // `handle_timeout` as soon as possible. `last_checking_time` is in the
                // past, so the resulting deadline is immediate.
                Some(self.last_checking_time)
            } else {
                Some(self.last_checking_time + self.get_timeout_interval())
            }
        } else {
            None
        };

        // This treats the two options as a collection and picks the minimum
        [mdns_timeout, ice_timeout].into_iter().flatten().min()
    }

    fn close(&mut self) -> std::result::Result<(), Self::Error> {
        self.set_selected_pair(None, None);
        self.delete_all_candidates(false);
        self.update_connection_state(None, ConnectionState::Closed);
        if let Some(mdns_conn) = &mut self.mdns {
            mdns_conn.close()?;
        }
        Ok(())
    }
}
