#[cfg(test)]
mod change_cipher_spec_test;

use byteorder::{ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use super::content::*;
use shared::error::*;

// The change cipher spec protocol exists to signal transitions in
// ciphering strategies.  The protocol consists of a single message,
// which is encrypted and compressed under the current (not the pending)
// connection state.  The message consists of a single byte of value 1.
/// ## Specifications
///
/// * [RFC 5246 §7.1]
///
/// [RFC 5246 §7.1]: https://tools.ietf.org/html/rfc5246#section-7.1
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangeCipherSpec;

impl ChangeCipherSpec {
    /// The record content type this message is carried in.
    pub fn content_type(&self) -> ContentType {
        ContentType::ChangeCipherSpec
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        1
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_u8(0x01)?;

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let data = reader.read_u8()?;
        if data != 0x01 {
            return Err(Error::ErrInvalidCipherSpec);
        }

        Ok(ChangeCipherSpec {})
    }
}
