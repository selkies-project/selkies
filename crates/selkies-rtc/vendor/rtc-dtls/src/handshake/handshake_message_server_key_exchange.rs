#[cfg(test)]
mod handshake_message_server_key_exchange_test;

use super::*;
use crate::curve::named_curve::*;
use crate::curve::*;
use crate::signature_hash_algorithm::*;

use byteorder::{BigEndian, WriteBytesExt};
use std::io::{Read, Write};

// Structure supports ECDH and PSK
#[derive(Clone, Debug, PartialEq, Eq)]
/// The server's half of the key agreement, signed so the client can authenticate it.
pub struct HandshakeMessageServerKeyExchange {
    pub(crate) identity_hint: Vec<u8>,

    pub(crate) elliptic_curve_type: EllipticCurveType,
    pub(crate) named_curve: NamedCurve,
    pub(crate) public_key: Vec<u8>,
    pub(crate) algorithm: SignatureHashAlgorithm,
    pub(crate) signature: Vec<u8>,
}

impl HandshakeMessageServerKeyExchange {
    /// The handshake type that identifies this message on the wire.
    pub fn handshake_type(&self) -> HandshakeType {
        HandshakeType::ServerKeyExchange
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        if !self.identity_hint.is_empty() {
            2 + self.identity_hint.len()
        } else {
            1 + 2 + 1 + self.public_key.len() + 2 + 2 + self.signature.len()
        }
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        if !self.identity_hint.is_empty() {
            writer.write_u16::<BigEndian>(self.identity_hint.len() as u16)?;
            writer.write_all(&self.identity_hint)?;
            return Ok(writer.flush()?);
        }

        writer.write_u8(self.elliptic_curve_type as u8)?;
        writer.write_u16::<BigEndian>(self.named_curve as u16)?;

        writer.write_u8(self.public_key.len() as u8)?;
        writer.write_all(&self.public_key)?;

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
        let mut data = vec![];
        reader.read_to_end(&mut data)?;

        if data.len() < 2 {
            return Err(Error::ErrBufferTooSmall);
        }

        // If parsed as PSK return early and only populate PSK Identity Hint
        let psk_length = ((data[0] as u16) << 8) | data[1] as u16;
        if data.len() == psk_length as usize + 2 {
            return Ok(HandshakeMessageServerKeyExchange {
                identity_hint: data[2..].to_vec(),

                elliptic_curve_type: EllipticCurveType::Unsupported,
                named_curve: NamedCurve::Unsupported,
                public_key: vec![],
                algorithm: SignatureHashAlgorithm {
                    hash: HashAlgorithm::Unsupported,
                    signature: SignatureAlgorithm::Unsupported,
                },
                signature: vec![],
            });
        }

        let elliptic_curve_type = data[0].into();
        if data[1..].len() < 2 {
            return Err(Error::ErrBufferTooSmall);
        }

        let named_curve = (((data[1] as u16) << 8) | data[2] as u16).into();
        if data.len() < 4 {
            return Err(Error::ErrBufferTooSmall);
        }

        let public_key_length = data[3] as usize;
        let mut offset = 4 + public_key_length;
        if data.len() < offset {
            return Err(Error::ErrBufferTooSmall);
        }
        let public_key = data[4..offset].to_vec();
        if data.len() <= offset {
            return Err(Error::ErrBufferTooSmall);
        }

        let hash_algorithm = data[offset].into();
        offset += 1;
        if data.len() <= offset {
            return Err(Error::ErrBufferTooSmall);
        }

        let signature_algorithm = data[offset].into();
        offset += 1;
        if data.len() < offset + 2 {
            return Err(Error::ErrBufferTooSmall);
        }

        let signature_length = (((data[offset] as u16) << 8) | data[offset + 1] as u16) as usize;
        offset += 2;
        if data.len() < offset + signature_length {
            return Err(Error::ErrBufferTooSmall);
        }
        let signature = data[offset..offset + signature_length].to_vec();

        Ok(HandshakeMessageServerKeyExchange {
            identity_hint: vec![],

            elliptic_curve_type,
            named_curve,
            public_key,
            algorithm: SignatureHashAlgorithm {
                hash: hash_algorithm,
                signature: signature_algorithm,
            },
            signature,
        })
    }
}
