#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// The certificate types a server may request from a client.
#[non_exhaustive]
pub enum ClientCertificateType {
    /// `RSA_SIGN` (`1`).
    RsaSign = 1,
    /// `ECDSA_SIGN` (`64`).
    EcdsaSign = 64,
    /// A type this crate does not implement.
    Unsupported,
}

impl From<u8> for ClientCertificateType {
    fn from(val: u8) -> Self {
        match val {
            1 => ClientCertificateType::RsaSign,
            64 => ClientCertificateType::EcdsaSign,
            _ => ClientCertificateType::Unsupported,
        }
    }
}
