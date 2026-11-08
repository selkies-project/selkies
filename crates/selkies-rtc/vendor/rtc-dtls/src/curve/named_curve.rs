use crypto::{ActiveKeyExchange, KeyExchangeAlgorithm, RTCCrypto};
use shared::error::*;

// https://www.iana.org/assignments/tls-parameters/tls-parameters.xml#tls-parameters-8
#[repr(u16)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
/// The named elliptic curves this crate can perform ECDHE over.
#[non_exhaustive]
pub enum NamedCurve {
    /// `UNSUPPORTED` (`0x0000`).
    Unsupported = 0x0000,
    /// `P256` (`0x0017`).
    P256 = 0x0017,
    /// `P384` (`0x0018`).
    P384 = 0x0018,
    /// `X25519` (`0x001d`).
    X25519 = 0x001d,
}

impl From<u16> for NamedCurve {
    fn from(val: u16) -> Self {
        match val {
            0x0017 => NamedCurve::P256,
            0x0018 => NamedCurve::P384,
            0x001d => NamedCurve::X25519,
            _ => NamedCurve::Unsupported,
        }
    }
}

/// An ephemeral ECDHE key pair, with the curve it belongs to.
pub struct NamedCurveKeypair {
    pub(crate) curve: NamedCurve,
    pub(crate) public_key: Vec<u8>,
    active: Option<Box<dyn ActiveKeyExchange>>,
}

impl NamedCurveKeypair {
    pub(crate) fn complete(&mut self, peer_public_key: &[u8]) -> Result<Vec<u8>> {
        let active = self
            .active
            .take()
            .ok_or(Error::ErrNamedCurveAndPrivateKeyMismatch)?;
        active
            .complete(peer_public_key)
            .map(|secret| secret.into_bytes())
            .map_err(|error| Error::Crypto(error.to_string()))
    }
}

impl NamedCurve {
    pub(crate) const fn crypto_algorithm(self) -> Result<KeyExchangeAlgorithm> {
        match self {
            Self::P256 => Ok(KeyExchangeAlgorithm::P256),
            Self::P384 => Ok(KeyExchangeAlgorithm::P384),
            Self::X25519 => Ok(KeyExchangeAlgorithm::X25519),
            Self::Unsupported => Err(Error::ErrInvalidNamedCurve),
        }
    }

    pub(crate) fn generate_keypair_with_crypto(
        self,
        crypto: &dyn RTCCrypto,
    ) -> Result<NamedCurveKeypair> {
        let active = crypto
            .start_key_exchange(self.crypto_algorithm()?)
            .map_err(|error| Error::Crypto(error.to_string()))?;
        let public_key = active.public_key().to_vec();
        Ok(NamedCurveKeypair {
            curve: self,
            public_key,
            active: Some(active),
        })
    }
}
