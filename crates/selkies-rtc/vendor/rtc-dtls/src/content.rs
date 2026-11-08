use std::io::{Read, Write};

use super::alert::*;
use super::application_data::*;
use super::change_cipher_spec::*;
use super::handshake::*;
use shared::error::*;

/// ## Specifications
///
/// * [RFC 4346 §6.2.1]
///
/// [RFC 4346 §6.2.1]: https://tools.ietf.org/html/rfc4346#section-6.2.1
#[derive(Default, Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ContentType {
    /// `CHANGE_CIPHER_SPEC` (`20`).
    ChangeCipherSpec = 20,
    /// `ALERT` (`21`).
    Alert = 21,
    /// `HANDSHAKE` (`22`).
    Handshake = 22,
    /// `APPLICATION_DATA` (`23`).
    ApplicationData = 23,
    #[default]
    /// A content type this crate does not recognise.
    Invalid,
}

impl From<u8> for ContentType {
    fn from(val: u8) -> Self {
        match val {
            20 => ContentType::ChangeCipherSpec,
            21 => ContentType::Alert,
            22 => ContentType::Handshake,
            23 => ContentType::ApplicationData,
            _ => ContentType::Invalid,
        }
    }
}

#[derive(PartialEq, Debug, Clone)]
/// The parsed body of a DTLS record.
#[non_exhaustive]
pub enum Content {
    /// A ChangeCipherSpec record.
    ChangeCipherSpec(ChangeCipherSpec),
    /// An alert record.
    Alert(Alert),
    /// A handshake record.
    Handshake(Handshake),
    /// An application data record.
    ApplicationData(ApplicationData),
}

impl Content {
    /// The record content type this message is carried in.
    pub fn content_type(&self) -> ContentType {
        match self {
            Content::ChangeCipherSpec(c) => c.content_type(),
            Content::Alert(c) => c.content_type(),
            Content::Handshake(c) => c.content_type(),
            Content::ApplicationData(c) => c.content_type(),
        }
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        match self {
            Content::ChangeCipherSpec(c) => c.size(),
            Content::Alert(c) => c.size(),
            Content::Handshake(c) => c.size(),
            Content::ApplicationData(c) => c.size(),
        }
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        match self {
            Content::ChangeCipherSpec(c) => c.marshal(writer),
            Content::Alert(c) => c.marshal(writer),
            Content::Handshake(c) => c.marshal(writer),
            Content::ApplicationData(c) => c.marshal(writer),
        }
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(content_type: ContentType, reader: &mut R) -> Result<Self> {
        match content_type {
            ContentType::ChangeCipherSpec => Ok(Content::ChangeCipherSpec(
                ChangeCipherSpec::unmarshal(reader)?,
            )),
            ContentType::Alert => Ok(Content::Alert(Alert::unmarshal(reader)?)),
            ContentType::Handshake => Ok(Content::Handshake(Handshake::unmarshal(reader)?)),
            ContentType::ApplicationData => Ok(Content::ApplicationData(
                ApplicationData::unmarshal(reader)?,
            )),
            _ => Err(Error::ErrInvalidContentType),
        }
    }
}
