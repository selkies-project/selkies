#[cfg(test)]
mod extension_supported_elliptic_curves_test;

use super::*;
use crate::curve::named_curve::*;

const EXTENSION_SUPPORTED_GROUPS_HEADER_SIZE: usize = 6;

/// ## Specifications
///
/// * [RFC 8422 §5.1.1]
///
/// [RFC 8422 §5.1.1]: https://tools.ietf.org/html/rfc8422#section-5.1.1
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionSupportedEllipticCurves {
    /// The curves the sender accepts, in preference order.
    pub elliptic_curves: Vec<NamedCurve>,
}

impl ExtensionSupportedEllipticCurves {
    /// The extension type this value is carried under.
    pub fn extension_value(&self) -> ExtensionValue {
        ExtensionValue::SupportedEllipticCurves
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        2 + 2 + self.elliptic_curves.len() * 2
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_u16::<BigEndian>(2 + 2 * self.elliptic_curves.len() as u16)?;
        writer.write_u16::<BigEndian>(2 * self.elliptic_curves.len() as u16)?;
        for v in &self.elliptic_curves {
            writer.write_u16::<BigEndian>(*v as u16)?;
        }

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let _ = reader.read_u16::<BigEndian>()?;

        let group_count = reader.read_u16::<BigEndian>()? as usize / 2;
        let mut elliptic_curves = vec![];
        for _ in 0..group_count {
            let elliptic_curve = reader.read_u16::<BigEndian>()?.into();
            elliptic_curves.push(elliptic_curve);
        }

        Ok(ExtensionSupportedEllipticCurves { elliptic_curves })
    }
}
