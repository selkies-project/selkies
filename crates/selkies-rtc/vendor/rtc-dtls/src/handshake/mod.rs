//! DTLS handshake messages.
//!
//! Each message type has its own module; [`HandshakeMessage`](crate::handshake::HandshakeMessage) is the parsed union of them and
//! [`Handshake`](crate::handshake::Handshake) pairs one with its [`HandshakeHeader`](crate::handshake::handshake_header::HandshakeHeader).
//!
//! Two things distinguish this from a TLS handshake. Messages carry a sequence number and
//! fragment offsets, because a handshake message may be larger than a datagram and must be
//! reassembled. And the server may answer a ClientHello with a
//! [`HelloVerifyRequest`](crate::handshake::handshake_message_hello_verify_request) carrying a cookie, which the
//! client echoes — a cheap defence against using the handshake for amplification.
//!
//! [`handshake_cache`](crate::handshake::handshake_cache) retains the messages so the hash in `Finished` can be computed over
//! exactly what both sides saw.
/// Buffers handshake messages so their hash can be computed for `Finished` verification.
pub mod handshake_cache;
/// The header prefixing every handshake message, including fragment offsets.
pub mod handshake_header;
/// Certificate: the sender's certificate chain.
pub mod handshake_message_certificate;
/// CertificateRequest: the server asks the client to authenticate.
pub mod handshake_message_certificate_request;
/// CertificateVerify: proves possession of the certificate's private key.
pub mod handshake_message_certificate_verify;
/// ClientHello: opens the handshake with the client's offers.
pub mod handshake_message_client_hello;
/// ClientKeyExchange: the client's half of the key agreement.
pub mod handshake_message_client_key_exchange;
/// Finished: a hash over the handshake, proving both sides saw the same messages.
pub mod handshake_message_finished;
/// HelloVerifyRequest: DTLS's cookie exchange, which resists amplification attacks.
pub mod handshake_message_hello_verify_request;
/// ServerHello: the server's chosen parameters.
pub mod handshake_message_server_hello;
/// ServerHelloDone: the server has finished its first flight.
pub mod handshake_message_server_hello_done;
/// ServerKeyExchange: the server's half of the key agreement.
pub mod handshake_message_server_key_exchange;
/// The 32-byte random each side contributes to key derivation.
pub mod handshake_random;

#[cfg(test)]
mod handshake_test;

use std::fmt;
use std::io::{Read, Write};

use super::content::*;
use shared::error::*;

use handshake_header::*;
use handshake_message_certificate::*;
use handshake_message_certificate_request::*;
use handshake_message_certificate_verify::*;
use handshake_message_client_hello::*;
use handshake_message_client_key_exchange::*;
use handshake_message_finished::*;
use handshake_message_hello_verify_request::*;
use handshake_message_server_hello::*;
use handshake_message_server_hello_done::*;
use handshake_message_server_key_exchange::*;

/// ## Specifications
///
/// * [RFC 5246 §7.4]
///
/// [RFC 5246 §7.4]: https://tools.ietf.org/html/rfc5246#section-7.4
#[derive(Default, Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HandshakeType {
    /// `HELLO_REQUEST` (`0`).
    HelloRequest = 0,
    /// `CLIENT_HELLO` (`1`).
    ClientHello = 1,
    /// `SERVER_HELLO` (`2`).
    ServerHello = 2,
    /// `HELLO_VERIFY_REQUEST` (`3`).
    HelloVerifyRequest = 3,
    /// `CERTIFICATE` (`11`).
    Certificate = 11,
    /// `SERVER_KEY_EXCHANGE` (`12`).
    ServerKeyExchange = 12,
    /// `CERTIFICATE_REQUEST` (`13`).
    CertificateRequest = 13,
    /// `SERVER_HELLO_DONE` (`14`).
    ServerHelloDone = 14,
    /// `CERTIFICATE_VERIFY` (`15`).
    CertificateVerify = 15,
    /// `CLIENT_KEY_EXCHANGE` (`16`).
    ClientKeyExchange = 16,
    /// `FINISHED` (`20`).
    Finished = 20,
    #[default]
    /// A handshake type this crate does not recognise.
    Invalid,
}

impl fmt::Display for HandshakeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HandshakeType::HelloRequest => write!(f, "HelloRequest"),
            HandshakeType::ClientHello => write!(f, "ClientHello"),
            HandshakeType::ServerHello => write!(f, "ServerHello"),
            HandshakeType::HelloVerifyRequest => write!(f, "HelloVerifyRequest"),
            HandshakeType::Certificate => write!(f, "Certificate"),
            HandshakeType::ServerKeyExchange => write!(f, "ServerKeyExchange"),
            HandshakeType::CertificateRequest => write!(f, "CertificateRequest"),
            HandshakeType::ServerHelloDone => write!(f, "ServerHelloDone"),
            HandshakeType::CertificateVerify => write!(f, "CertificateVerify"),
            HandshakeType::ClientKeyExchange => write!(f, "ClientKeyExchange"),
            HandshakeType::Finished => write!(f, "Finished"),
            HandshakeType::Invalid => write!(f, "Invalid"),
        }
    }
}

impl From<u8> for HandshakeType {
    fn from(val: u8) -> Self {
        match val {
            0 => HandshakeType::HelloRequest,
            1 => HandshakeType::ClientHello,
            2 => HandshakeType::ServerHello,
            3 => HandshakeType::HelloVerifyRequest,
            11 => HandshakeType::Certificate,
            12 => HandshakeType::ServerKeyExchange,
            13 => HandshakeType::CertificateRequest,
            14 => HandshakeType::ServerHelloDone,
            15 => HandshakeType::CertificateVerify,
            16 => HandshakeType::ClientKeyExchange,
            20 => HandshakeType::Finished,
            _ => HandshakeType::Invalid,
        }
    }
}

#[derive(PartialEq, Debug, Clone)]
/// A parsed handshake message.
#[non_exhaustive]
pub enum HandshakeMessage {
    //HelloRequest(errNotImplemented),
    /// ClientHello, which opens the handshake.
    ClientHello(HandshakeMessageClientHello),
    /// ServerHello, carrying the server's chosen parameters.
    ServerHello(HandshakeMessageServerHello),
    /// HelloVerifyRequest, DTLS's cookie challenge.
    HelloVerifyRequest(HandshakeMessageHelloVerifyRequest),
    /// Certificate, carrying a certificate chain.
    Certificate(HandshakeMessageCertificate),
    /// ServerKeyExchange, the server's key-agreement share.
    ServerKeyExchange(HandshakeMessageServerKeyExchange),
    /// CertificateRequest, asking the client to authenticate.
    CertificateRequest(HandshakeMessageCertificateRequest),
    /// ServerHelloDone, ending the server's first flight.
    ServerHelloDone(HandshakeMessageServerHelloDone),
    /// CertificateVerify, proving possession of the certificate key.
    CertificateVerify(HandshakeMessageCertificateVerify),
    /// ClientKeyExchange, the client's key-agreement share.
    ClientKeyExchange(HandshakeMessageClientKeyExchange),
    /// Finished, a hash over the handshake that both sides verify.
    Finished(HandshakeMessageFinished),
}

impl HandshakeMessage {
    /// The handshake type that identifies this message on the wire.
    pub fn handshake_type(&self) -> HandshakeType {
        match self {
            HandshakeMessage::ClientHello(msg) => msg.handshake_type(),
            HandshakeMessage::ServerHello(msg) => msg.handshake_type(),
            HandshakeMessage::HelloVerifyRequest(msg) => msg.handshake_type(),
            HandshakeMessage::Certificate(msg) => msg.handshake_type(),
            HandshakeMessage::ServerKeyExchange(msg) => msg.handshake_type(),
            HandshakeMessage::CertificateRequest(msg) => msg.handshake_type(),
            HandshakeMessage::ServerHelloDone(msg) => msg.handshake_type(),
            HandshakeMessage::CertificateVerify(msg) => msg.handshake_type(),
            HandshakeMessage::ClientKeyExchange(msg) => msg.handshake_type(),
            HandshakeMessage::Finished(msg) => msg.handshake_type(),
        }
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        match self {
            HandshakeMessage::ClientHello(msg) => msg.size(),
            HandshakeMessage::ServerHello(msg) => msg.size(),
            HandshakeMessage::HelloVerifyRequest(msg) => msg.size(),
            HandshakeMessage::Certificate(msg) => msg.size(),
            HandshakeMessage::ServerKeyExchange(msg) => msg.size(),
            HandshakeMessage::CertificateRequest(msg) => msg.size(),
            HandshakeMessage::ServerHelloDone(msg) => msg.size(),
            HandshakeMessage::CertificateVerify(msg) => msg.size(),
            HandshakeMessage::ClientKeyExchange(msg) => msg.size(),
            HandshakeMessage::Finished(msg) => msg.size(),
        }
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        match self {
            HandshakeMessage::ClientHello(msg) => msg.marshal(writer)?,
            HandshakeMessage::ServerHello(msg) => msg.marshal(writer)?,
            HandshakeMessage::HelloVerifyRequest(msg) => msg.marshal(writer)?,
            HandshakeMessage::Certificate(msg) => msg.marshal(writer)?,
            HandshakeMessage::ServerKeyExchange(msg) => msg.marshal(writer)?,
            HandshakeMessage::CertificateRequest(msg) => msg.marshal(writer)?,
            HandshakeMessage::ServerHelloDone(msg) => msg.marshal(writer)?,
            HandshakeMessage::CertificateVerify(msg) => msg.marshal(writer)?,
            HandshakeMessage::ClientKeyExchange(msg) => msg.marshal(writer)?,
            HandshakeMessage::Finished(msg) => msg.marshal(writer)?,
        }

        Ok(())
    }
}

// The handshake protocol is responsible for selecting a cipher spec and
// generating a master secret, which together comprise the primary
// cryptographic parameters associated with a secure session.  The
// handshake protocol can also optionally authenticate parties who have
// certificates signed by a trusted certificate authority.
// https://tools.ietf.org/html/rfc5246#section-7.3
#[derive(PartialEq, Debug, Clone)]
/// A handshake record: its header plus the message it carries.
pub struct Handshake {
    pub(crate) handshake_header: HandshakeHeader,
    pub(crate) handshake_message: HandshakeMessage,
}

impl Handshake {
    /// Wraps a message in a handshake record, filling in its header.
    pub fn new(handshake_message: HandshakeMessage) -> Self {
        Handshake {
            handshake_header: HandshakeHeader {
                handshake_type: handshake_message.handshake_type(),
                length: handshake_message.size() as u32,
                message_sequence: 0,
                fragment_offset: 0,
                fragment_length: handshake_message.size() as u32,
            },
            handshake_message,
        }
    }

    /// The record content type this message is carried in.
    pub fn content_type(&self) -> ContentType {
        ContentType::Handshake
    }

    /// The encoded size of this message in bytes.
    pub fn size(&self) -> usize {
        self.handshake_header.size() + self.handshake_message.size()
    }

    /// Encodes this message to `writer`.
    ///
    /// # Errors
    ///
    /// Fails on a write error, or if a field exceeds the length its wire format allows.
    pub fn marshal<W: Write>(&self, writer: &mut W) -> Result<()> {
        self.handshake_header.marshal(writer)?;
        self.handshake_message.marshal(writer)?;
        Ok(())
    }

    /// Decodes one of these messages from `reader`.
    ///
    /// # Errors
    ///
    /// Fails if `reader` is truncated or its contents are not a valid encoding.
    pub fn unmarshal<R: Read>(reader: &mut R) -> Result<Self> {
        let handshake_header = HandshakeHeader::unmarshal(reader)?;

        let handshake_message = match handshake_header.handshake_type {
            HandshakeType::ClientHello => {
                HandshakeMessage::ClientHello(HandshakeMessageClientHello::unmarshal(reader)?)
            }
            HandshakeType::ServerHello => {
                HandshakeMessage::ServerHello(HandshakeMessageServerHello::unmarshal(reader)?)
            }
            HandshakeType::HelloVerifyRequest => HandshakeMessage::HelloVerifyRequest(
                HandshakeMessageHelloVerifyRequest::unmarshal(reader)?,
            ),
            HandshakeType::Certificate => {
                HandshakeMessage::Certificate(HandshakeMessageCertificate::unmarshal(reader)?)
            }
            HandshakeType::ServerKeyExchange => HandshakeMessage::ServerKeyExchange(
                HandshakeMessageServerKeyExchange::unmarshal(reader)?,
            ),
            HandshakeType::CertificateRequest => HandshakeMessage::CertificateRequest(
                HandshakeMessageCertificateRequest::unmarshal(reader)?,
            ),
            HandshakeType::ServerHelloDone => HandshakeMessage::ServerHelloDone(
                HandshakeMessageServerHelloDone::unmarshal(reader)?,
            ),
            HandshakeType::CertificateVerify => HandshakeMessage::CertificateVerify(
                HandshakeMessageCertificateVerify::unmarshal(reader)?,
            ),
            HandshakeType::ClientKeyExchange => HandshakeMessage::ClientKeyExchange(
                HandshakeMessageClientKeyExchange::unmarshal(reader)?,
            ),
            HandshakeType::Finished => {
                HandshakeMessage::Finished(HandshakeMessageFinished::unmarshal(reader)?)
            }
            _ => return Err(Error::ErrNotImplemented),
        };

        Ok(Handshake {
            handshake_header,
            handshake_message,
        })
    }
}
