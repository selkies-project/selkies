#[cfg(test)]
mod handshake_message_server_hello_done_test;

use super::*;

use std::io::{Read, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
/// Marks the end of the server's first flight. Carries no fields.
pub struct HandshakeMessageServerHelloDone;

impl HandshakeMessageServerHelloDone {
    /// The handshake type that identifies this message on the wire.
    pub fn handshake_type(&self) -> HandshakeType {
        HandshakeType::ServerHelloDone
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        0
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, _writer: &mut W) -> Result<()> {
        Ok(())
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(_reader: &mut R) -> Result<Self> {
        Ok(HandshakeMessageServerHelloDone {})
    }
}
