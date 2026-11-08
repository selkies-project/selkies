/// The named elliptic curves and key generation over them.
pub mod named_curve;

// https://www.iana.org/assignments/tls-parameters/tls-parameters.xhtml#tls-parameters-10
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
/// How an elliptic curve is identified in a key exchange — by name, or explicitly.
#[non_exhaustive]
pub enum EllipticCurveType {
    /// `NAMED_CURVE` (`0x03`).
    NamedCurve = 0x03,
    /// A curve type this crate does not implement.
    Unsupported,
}

impl From<u8> for EllipticCurveType {
    fn from(val: u8) -> Self {
        match val {
            0x03 => EllipticCurveType::NamedCurve,
            _ => EllipticCurveType::Unsupported,
        }
    }
}
