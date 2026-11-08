use shared::error::Result;

use byteorder::{ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// Compression methods. DTLS in WebRTC always negotiates `Null`.
#[non_exhaustive]
pub enum CompressionMethodId {
    /// `NULL` (`0`).
    Null = 0,
    /// A method this crate does not implement.
    Unsupported,
}

impl From<u8> for CompressionMethodId {
    fn from(val: u8) -> Self {
        match val {
            0 => CompressionMethodId::Null,
            _ => CompressionMethodId::Unsupported,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// The compression-methods list offered or selected in a hello message.
pub struct CompressionMethods {
    /// The methods, in preference order.
    pub ids: Vec<CompressionMethodId>,
}

impl CompressionMethods {
    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        1 + self.ids.len()
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_u8(self.ids.len() as u8)?;

        for id in &self.ids {
            writer.write_u8(*id as u8)?;
        }

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let compression_methods_count = reader.read_u8()? as usize;
        let mut ids = vec![];
        for _ in 0..compression_methods_count {
            let id = reader.read_u8()?.into();
            if id != CompressionMethodId::Unsupported {
                ids.push(id);
            }
        }

        Ok(CompressionMethods { ids })
    }
}

/// The default list: null compression only.
pub fn default_compression_methods() -> CompressionMethods {
    CompressionMethods {
        ids: vec![CompressionMethodId::Null],
    }
}
