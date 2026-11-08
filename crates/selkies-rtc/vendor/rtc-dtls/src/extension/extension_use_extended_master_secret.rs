#[cfg(test)]
mod extension_use_extended_master_secret_test;

use super::*;

const EXTENSION_USE_EXTENDED_MASTER_SECRET_HEADER_SIZE: usize = 4;

/// ## Specifications
///
/// * [RFC 8422]
///
/// [RFC 8422]: https://tools.ietf.org/html/rfc8422
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionUseExtendedMasterSecret {
    pub(crate) supported: bool,
}

impl ExtensionUseExtendedMasterSecret {
    /// The extension type this value is carried under.
    pub fn extension_value(&self) -> ExtensionValue {
        ExtensionValue::UseExtendedMasterSecret
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        2
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        // length
        writer.write_u16::<BigEndian>(0)?;

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let _ = reader.read_u16::<BigEndian>()?;

        Ok(ExtensionUseExtendedMasterSecret { supported: true })
    }
}
