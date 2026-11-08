use super::*;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

// msg_len for Handshake messages assumes an extra 12 bytes for
// sequence, Fragment and version information
pub(crate) const HANDSHAKE_HEADER_LENGTH: usize = 12;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
/// The header on every handshake message: type, length, message sequence, and the fragment
/// offset and length that let one message span datagrams.
pub struct HandshakeHeader {
    pub(crate) handshake_type: HandshakeType,
    pub(crate) length: u32, // uint24 in spec
    pub(crate) message_sequence: u16,
    pub(crate) fragment_offset: u32, // uint24 in spec
    pub(crate) fragment_length: u32, // uint24 in spec
}

impl HandshakeHeader {
    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        1 + 3 + 2 + 3 + 3
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_u8(self.handshake_type as u8)?;
        writer.write_u24::<BigEndian>(self.length)?;
        writer.write_u16::<BigEndian>(self.message_sequence)?;
        writer.write_u24::<BigEndian>(self.fragment_offset)?;
        writer.write_u24::<BigEndian>(self.fragment_length)?;

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let handshake_type = reader.read_u8()?.into();
        let length = reader.read_u24::<BigEndian>()?;
        let message_sequence = reader.read_u16::<BigEndian>()?;
        let fragment_offset = reader.read_u24::<BigEndian>()?;
        let fragment_length = reader.read_u24::<BigEndian>()?;

        Ok(HandshakeHeader {
            handshake_type,
            length,
            message_sequence,
            fragment_offset,
            fragment_length,
        })
    }
}
