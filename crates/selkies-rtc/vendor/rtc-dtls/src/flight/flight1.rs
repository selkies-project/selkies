use super::flight3::*;
use super::*;
use crate::compression_methods::*;
use crate::config::*;
use crate::conn::*;
use crate::content::*;
use crate::extension::extension_server_name::*;
use crate::extension::extension_supported_elliptic_curves::*;
use crate::extension::extension_supported_point_formats::*;
use crate::extension::extension_supported_signature_algorithms::*;
use crate::extension::extension_use_extended_master_secret::*;
use crate::extension::extension_use_srtp::*;
use crate::extension::*;
use crate::handshake::handshake_message_client_hello::*;
use crate::handshake::*;
use crate::record_layer::record_layer_header::*;
use crate::record_layer::*;
use shared::error::Error;

use crate::extension::renegotiation_info::ExtensionRenegotiationInfo;
use std::fmt;

#[derive(Debug, PartialEq)]
pub(crate) struct Flight1;

impl fmt::Display for Flight1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Flight 1")
    }
}

impl Flight for Flight1 {
    fn parse(
        &self,
        //tx: &mut mpsc::Sender<mpsc::Sender<()>>,
        state: &mut State,
        cache: &HandshakeCache,
        cfg: &HandshakeConfig,
    ) -> Result<Box<dyn Flight>, (Option<Alert>, Option<Error>)> {
        // HelloVerifyRequest can be skipped by the server,
        // so allow ServerHello during flight1 also
        let (seq, msgs) = match cache.full_pull_map(
            state.handshake_recv_sequence,
            &[
                HandshakeCachePullRule {
                    typ: HandshakeType::HelloVerifyRequest,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: true,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::ServerHello,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: true,
                },
            ],
        ) {
            // No valid message received. Keep reading
            Ok((seq, msgs)) => (seq, msgs),
            Err(_) => return Err((None, None)),
        };

        if msgs.contains_key(&HandshakeType::ServerHello) {
            // Flight1 and flight2 were skipped.
            // Parse as flight3.
            let flight3 = Flight3 {};
            return flight3.parse(/*tx,*/ state, cache, cfg);
        }

        if let Some(message) = msgs.get(&HandshakeType::HelloVerifyRequest) {
            // DTLS 1.2 clients must not assume that the server will use the protocol version
            // specified in HelloVerifyRequest message. RFC 6347 Section 4.2.1
            let h = match message {
                HandshakeMessage::HelloVerifyRequest(h) => h,
                _ => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        None,
                    ));
                }
            };

            if h.version != PROTOCOL_VERSION1_0 && h.version != PROTOCOL_VERSION1_2 {
                return Err((
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::ProtocolVersion,
                    }),
                    Some(Error::ErrUnsupportedProtocolVersion),
                ));
            }

            state.cookie.clone_from(&h.cookie);
            state.handshake_recv_sequence = seq;
            Ok(Box::new(Flight3 {}))
        } else {
            Err((
                Some(Alert {
                    alert_level: AlertLevel::Fatal,
                    alert_description: AlertDescription::InternalError,
                }),
                None,
            ))
        }
    }

    fn generate(
        &self,
        state: &mut State,
        _cache: &HandshakeCache,
        cfg: &HandshakeConfig,
    ) -> Result<Vec<Packet>, (Option<Alert>, Option<Error>)> {
        state.local_epoch = 0;
        state.remote_epoch = 0;

        state.named_curve = DEFAULT_NAMED_CURVE;
        state.cookie = vec![];
        if let Err(error) = state.local_random.populate(cfg.provider().random()) {
            return Err((None, Some(error)));
        }

        let mut extensions = vec![
            Extension::SupportedSignatureAlgorithms(ExtensionSupportedSignatureAlgorithms {
                signature_hash_algorithms: cfg.local_signature_schemes.clone(),
            }),
            Extension::RenegotiationInfo(ExtensionRenegotiationInfo {
                renegotiated_connection: 0,
            }),
        ];

        if cfg.local_psk_callback.is_none() {
            extensions.extend_from_slice(&[
                Extension::SupportedEllipticCurves(ExtensionSupportedEllipticCurves {
                    elliptic_curves: cfg.local_named_curves.clone(),
                }),
                Extension::SupportedPointFormats(ExtensionSupportedPointFormats {
                    point_formats: vec![ELLIPTIC_CURVE_POINT_FORMAT_UNCOMPRESSED],
                }),
            ]);
        }

        if !cfg.local_srtp_protection_profiles.is_empty() {
            extensions.push(Extension::UseSrtp(ExtensionUseSrtp {
                protection_profiles: cfg.local_srtp_protection_profiles.clone(),
            }));
        }

        if cfg.extended_master_secret == ExtendedMasterSecretType::Request
            || cfg.extended_master_secret == ExtendedMasterSecretType::Require
        {
            extensions.push(Extension::UseExtendedMasterSecret(
                ExtensionUseExtendedMasterSecret { supported: true },
            ));
        }

        if !cfg.server_name.is_empty() {
            extensions.push(Extension::ServerName(ExtensionServerName {
                server_name: cfg.server_name.clone(),
            }));
        }

        Ok(vec![Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                0,
                Content::Handshake(Handshake::new(HandshakeMessage::ClientHello(
                    HandshakeMessageClientHello {
                        version: PROTOCOL_VERSION1_2,
                        random: state.local_random.clone(),
                        cookie: state.cookie.clone(),

                        cipher_suites: cfg.local_cipher_suites.clone(),
                        compression_methods: default_compression_methods(),
                        extensions,
                    },
                ))),
            ),
            should_encrypt: false,
            reset_local_sequence_number: false,
        }])
    }
}
