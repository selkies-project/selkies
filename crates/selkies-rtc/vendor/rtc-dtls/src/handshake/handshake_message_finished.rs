#[cfg(test)]
mod handshake_message_finished_test;

use super::*;

use std::io::{Read, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
/// A hash over every preceding handshake message, which both sides compare to detect tampering.
pub struct HandshakeMessageFinished {
    pub(crate) verify_data: Vec<u8>,
}

impl HandshakeMessageFinished {
    /// The handshake type that identifies this message on the wire.
    pub fn handshake_type(&self) -> HandshakeType {
        HandshakeType::Finished
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        self.verify_data.len()
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.verify_data)?;

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let mut verify_data: Vec<u8> = vec![];
        reader.read_to_end(&mut verify_data)?;

        Ok(HandshakeMessageFinished { verify_data })
    }
}
