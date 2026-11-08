#[cfg(test)]
mod handshake_message_certificate_verify_test;

use super::*;
use crate::signature_hash_algorithm::*;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
/// A signature over the handshake so far, proving possession of the certificate's private key.
pub struct HandshakeMessageCertificateVerify {
    pub(crate) algorithm: SignatureHashAlgorithm,
    pub(crate) signature: Vec<u8>,
}

const HANDSHAKE_MESSAGE_CERTIFICATE_VERIFY_MIN_LENGTH: usize = 4;

impl HandshakeMessageCertificateVerify {
    /// The handshake type that identifies this message on the wire.
    pub fn handshake_type(&self) -> HandshakeType {
        HandshakeType::CertificateVerify
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        1 + 1 + 2 + self.signature.len()
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_u8(self.algorithm.hash as u8)?;
        writer.write_u8(self.algorithm.signature as u8)?;
        writer.write_u16::<BigEndian>(self.signature.len() as u16)?;
        writer.write_all(&self.signature)?;

        Ok(writer.flush()?)
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let hash_algorithm = reader.read_u8()?.into();
        let signature_algorithm = reader.read_u8()?.into();
        let signature_length = reader.read_u16::<BigEndian>()? as usize;
        let mut signature = vec![0; signature_length];
        reader.read_exact(&mut signature)?;

        Ok(HandshakeMessageCertificateVerify {
            algorithm: SignatureHashAlgorithm {
                hash: hash_algorithm,
                signature: signature_algorithm,
            },
            signature,
        })
    }
}
