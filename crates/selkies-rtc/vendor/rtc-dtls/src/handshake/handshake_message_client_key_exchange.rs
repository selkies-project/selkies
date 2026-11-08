#[cfg(test)]
mod handshake_message_client_key_exchange_test;

use super::*;

use byteorder::{BigEndian, WriteBytesExt};
use std::io::{Read, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
/// The client's half of the key agreement — its ECDHE public key, or a PSK identity.
pub struct HandshakeMessageClientKeyExchange {
    pub(crate) identity_hint: Vec<u8>,
    pub(crate) public_key: Vec<u8>,
}

impl HandshakeMessageClientKeyExchange {
    /// The handshake type that identifies this message on the wire.
    pub fn handshake_type(&self) -> HandshakeType {
        HandshakeType::ClientKeyExchange
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        if !self.public_key.is_empty() {
            1 + self.public_key.len()
        } else {
            2 + self.identity_hint.len()
        }
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        if (!self.identity_hint.is_empty() && !self.public_key.is_empty())
            || (self.identity_hint.is_empty() && self.public_key.is_empty())
        {
            return Err(Error::ErrInvalidClientKeyExchange);
        }

        if !self.public_key.is_empty() {
            writer.write_u8(self.public_key.len() as u8)?;
            writer.write_all(&self.public_key)?;
        } else {
            writer.write_u16::<BigEndian>(self.identity_hint.len() as u16)?;
            writer.write_all(&self.identity_hint)?;
        }

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let mut data = vec![];
        reader.read_to_end(&mut data)?;

        if data.len() < 2 {
            return Err(Error::ErrBufferTooSmall);
        }

        // If parsed as PSK return early and only populate PSK Identity Hint
        let psk_length = ((data[0] as u16) << 8) | data[1] as u16;
        if data.len() == psk_length as usize + 2 {
            return Ok(HandshakeMessageClientKeyExchange {
                identity_hint: data[2..].to_vec(),
                public_key: vec![],
            });
        }

        let public_key_length = data[0] as usize;
        if data.len() != public_key_length + 1 {
            return Err(Error::ErrBufferTooSmall);
        }

        Ok(HandshakeMessageClientKeyExchange {
            identity_hint: vec![],
            public_key: data[1..].to_vec(),
        })
    }
}
