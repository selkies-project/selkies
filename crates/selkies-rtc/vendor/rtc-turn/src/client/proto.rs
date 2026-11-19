use crate::client::relay::Relay;
use crate::client::{Client, Event};
use shared::TaggedBytesMut;
use shared::error::Error;
use std::net::SocketAddr;
use std::time::Instant;

impl sansio::Protocol<TaggedBytesMut, TaggedBytesMut, ()> for Client {
    type Rout = ();
    type Wout = TaggedBytesMut;
    type Eout = Event;
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedBytesMut) -> Result<(), Self::Error> {
        self.handle_inbound(msg.now, &msg.message[..], msg.transport.peer_addr)
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        None
    }

    fn handle_write(&mut self, _msg: TaggedBytesMut) -> Result<(), Self::Error> {
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        while let Some(transmit) = self.tr_map.poll_transmit() {
            self.transmits.push_back(transmit);
        }
        self.transmits.pop_front()
    }

    fn poll_event(&mut self) -> Option<Self::Eout> {
        while let Some(event) = self.tr_map.poll_event() {
            self.events.push_back(event);
        }
        self.events.pop_front()
    }

    fn handle_timeout(&mut self, now: Instant) -> Result<(), Self::Error> {
        self.tr_map.handle_timeout(now);

        #[allow(clippy::map_clone)]
        let relayed_addrs: Vec<SocketAddr> = self.relays.keys().map(|key| *key).collect();
        for relayed_addr in relayed_addrs {
            let mut relay = Relay {
                relayed_addr,
                client: self,
            };
            relay.handle_timeout(now);
        }

        Ok(())
    }

    fn poll_timeout(&mut self) -> Option<Instant> {
        let mut eto = None;
        if let Some(to) = self.tr_map.poll_timout()
            && (eto.is_none() || to < *eto.as_ref().unwrap())
        {
            eto = Some(to);
        }

        #[allow(clippy::map_clone)]
        let relayed_addrs: Vec<SocketAddr> = self.relays.keys().map(|key| *key).collect();
        for relayed_addr in relayed_addrs {
            let relay = Relay {
                relayed_addr,
                client: self,
            };
            if let Some(to) = relay.poll_timeout()
                && (eto.is_none() || to < *eto.as_ref().unwrap())
            {
                eto = Some(to);
            }
        }

        eto
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        self.tr_map.delete_all();
        Ok(())
    }
}
