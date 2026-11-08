/// Server Name Indication (SNI).
pub mod extension_server_name;
/// The curves a client will accept for ECDHE.
pub mod extension_supported_elliptic_curves;
/// The EC point formats a client will accept; WebRTC uses uncompressed.
pub mod extension_supported_point_formats;
/// The signature and hash algorithm pairs a client will accept.
pub mod extension_supported_signature_algorithms;
/// The extended master secret extension ([RFC 7627]), which binds the master secret to the
/// whole handshake.
pub mod extension_use_extended_master_secret;
/// The `use_srtp` extension, which negotiates SRTP protection profiles during the DTLS
/// handshake ([RFC 5764]).
pub mod extension_use_srtp;
/// The renegotiation info extension, sent empty to signal renegotiation is not supported.
pub mod renegotiation_info;

use extension_server_name::*;
use extension_supported_elliptic_curves::*;
use extension_supported_point_formats::*;
use extension_supported_signature_algorithms::*;
use extension_use_extended_master_secret::*;
use extension_use_srtp::*;

use shared::error::*;

use crate::extension::renegotiation_info::ExtensionRenegotiationInfo;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

// https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml
#[derive(Clone, Debug, PartialEq, Eq)]
/// The extension type code points this crate understands.
#[non_exhaustive]
pub enum ExtensionValue {
    /// `SERVER_NAME` (`0`).
    ServerName = 0,
    /// `SUPPORTED_ELLIPTIC_CURVES` (`10`).
    SupportedEllipticCurves = 10,
    /// `SUPPORTED_POINT_FORMATS` (`11`).
    SupportedPointFormats = 11,
    /// `SUPPORTED_SIGNATURE_ALGORITHMS` (`13`).
    SupportedSignatureAlgorithms = 13,
    /// `USE_SRTP` (`14`).
    UseSrtp = 14,
    /// `USE_EXTENDED_MASTER_SECRET` (`23`).
    UseExtendedMasterSecret = 23,
    /// `RENEGOTIATION_INFO` (`65281`).
    RenegotiationInfo = 65281,
    /// An extension this crate does not implement, which is ignored.
    Unsupported,
}

impl From<u16> for ExtensionValue {
    fn from(val: u16) -> Self {
        match val {
            0 => ExtensionValue::ServerName,
            10 => ExtensionValue::SupportedEllipticCurves,
            11 => ExtensionValue::SupportedPointFormats,
            13 => ExtensionValue::SupportedSignatureAlgorithms,
            14 => ExtensionValue::UseSrtp,
            23 => ExtensionValue::UseExtendedMasterSecret,
            65281 => ExtensionValue::RenegotiationInfo,
            _ => ExtensionValue::Unsupported,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
/// A parsed hello extension.
#[non_exhaustive]
pub enum Extension {
    /// Server Name Indication.
    ServerName(ExtensionServerName),
    /// The curves the sender accepts for ECDHE.
    SupportedEllipticCurves(ExtensionSupportedEllipticCurves),
    /// The EC point formats the sender accepts.
    SupportedPointFormats(ExtensionSupportedPointFormats),
    /// The signature and hash pairs the sender accepts.
    SupportedSignatureAlgorithms(ExtensionSupportedSignatureAlgorithms),
    /// The SRTP protection profiles offered or selected.
    UseSrtp(ExtensionUseSrtp),
    /// The extended master secret extension.
    UseExtendedMasterSecret(ExtensionUseExtendedMasterSecret),
    /// The renegotiation info extension.
    RenegotiationInfo(ExtensionRenegotiationInfo),
}

impl Extension {
    /// The extension type this value is carried under.
    pub fn extension_value(&self) -> ExtensionValue {
        match self {
            Extension::ServerName(ext) => ext.extension_value(),
            Extension::SupportedEllipticCurves(ext) => ext.extension_value(),
            Extension::SupportedPointFormats(ext) => ext.extension_value(),
            Extension::SupportedSignatureAlgorithms(ext) => ext.extension_value(),
            Extension::UseSrtp(ext) => ext.extension_value(),
            Extension::UseExtendedMasterSecret(ext) => ext.extension_value(),
            Extension::RenegotiationInfo(ext) => ext.extension_value(),
        }
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        let mut len = 2;

        len += match self {
            Extension::ServerName(ext) => ext.size(),
            Extension::SupportedEllipticCurves(ext) => ext.size(),
            Extension::SupportedPointFormats(ext) => ext.size(),
            Extension::SupportedSignatureAlgorithms(ext) => ext.size(),
            Extension::UseSrtp(ext) => ext.size(),
            Extension::UseExtendedMasterSecret(ext) => ext.size(),
            Extension::RenegotiationInfo(ext) => ext.size(),
        };

        len
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_u16::<BigEndian>(self.extension_value() as u16)?;
        match self {
            Extension::ServerName(ext) => ext.marshal(writer),
            Extension::SupportedEllipticCurves(ext) => ext.marshal(writer),
            Extension::SupportedPointFormats(ext) => ext.marshal(writer),
            Extension::SupportedSignatureAlgorithms(ext) => ext.marshal(writer),
            Extension::UseSrtp(ext) => ext.marshal(writer),
            Extension::UseExtendedMasterSecret(ext) => ext.marshal(writer),
            Extension::RenegotiationInfo(ext) => ext.marshal(writer),
        }
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let extension_value: ExtensionValue = reader.read_u16::<BigEndian>()?.into();
        match extension_value {
            ExtensionValue::ServerName => Ok(Extension::ServerName(
                ExtensionServerName::unmarshal(reader)?,
            )),
            ExtensionValue::SupportedEllipticCurves => Ok(Extension::SupportedEllipticCurves(
                ExtensionSupportedEllipticCurves::unmarshal(reader)?,
            )),
            ExtensionValue::SupportedPointFormats => Ok(Extension::SupportedPointFormats(
                ExtensionSupportedPointFormats::unmarshal(reader)?,
            )),
            ExtensionValue::SupportedSignatureAlgorithms => {
                Ok(Extension::SupportedSignatureAlgorithms(
                    ExtensionSupportedSignatureAlgorithms::unmarshal(reader)?,
                ))
            }
            ExtensionValue::UseSrtp => Ok(Extension::UseSrtp(ExtensionUseSrtp::unmarshal(reader)?)),
            ExtensionValue::UseExtendedMasterSecret => Ok(Extension::UseExtendedMasterSecret(
                ExtensionUseExtendedMasterSecret::unmarshal(reader)?,
            )),
            ExtensionValue::RenegotiationInfo => Ok(Extension::RenegotiationInfo(
                ExtensionRenegotiationInfo::unmarshal(reader)?,
            )),
            _ => Err(Error::ErrInvalidExtensionType),
        }
    }
}
