use super::flight2::*;
use super::*;
use crate::config::*;
use crate::conn::*;
use crate::extension::*;
use crate::handshake::*;
use crate::record_layer::record_layer_header::*;
use crate::*;
use shared::error::Error;

use log::debug;
use std::fmt;

#[derive(Debug, PartialEq)]
pub(crate) struct Flight0;

impl fmt::Display for Flight0 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Flight 0")
    }
}

impl Flight for Flight0 {
    fn parse(
        &self,
        //_tx: &mut mpsc::Sender<mpsc::Sender<()>>,
        state: &mut State,
        cache: &HandshakeCache,
        cfg: &HandshakeConfig,
    ) -> Result<Box<dyn Flight>, (Option<Alert>, Option<Error>)> {
        let (seq, msgs) = match cache.full_pull_map(
            0,
            &[HandshakeCachePullRule {
                typ: HandshakeType::ClientHello,
                epoch: cfg.initial_epoch,
                is_client: true,
                optional: false,
            }],
        ) {
            Ok((seq, msgs)) => (seq, msgs),
            Err(_) => return Err((None, None)),
        };

        state.handshake_recv_sequence = seq;

        if let Some(message) = msgs.get(&HandshakeType::ClientHello) {
            // Validate type
            let client_hello = match message {
                HandshakeMessage::ClientHello(client_hello) => client_hello,
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

            if client_hello.version != PROTOCOL_VERSION1_2 {
                return Err((
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::ProtocolVersion,
                    }),
                    Some(Error::ErrUnsupportedProtocolVersion),
                ));
            }

            state.remote_random = client_hello.random.clone();

            if let Ok(id) =
                find_matching_cipher_suite(&client_hello.cipher_suites, &cfg.local_cipher_suites)
            {
                if let Ok(cipher_suite) = cipher_suite_for_id(id) {
                    debug!(
                        "[handshake:{}] use cipher suite: {}",
                        srv_cli_str(state.is_client),
                        cipher_suite.to_string()
                    );
                    state.cipher_suite = Some(cipher_suite);
                }
            } else {
                return Err((
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::InsufficientSecurity,
                    }),
                    Some(Error::ErrCipherSuiteNoIntersection),
                ));
            }

            for extension in &client_hello.extensions {
                match extension {
                    Extension::SupportedEllipticCurves(e) => {
                        if e.elliptic_curves.is_empty() {
                            return Err((
                                Some(Alert {
                                    alert_level: AlertLevel::Fatal,
                                    alert_description: AlertDescription::InsufficientSecurity,
                                }),
                                Some(Error::ErrNoSupportedEllipticCurves),
                            ));
                        }
                        for curve in e.elliptic_curves.iter() {
                            if cfg.local_named_curves.contains(curve) {
                                state.named_curve = *curve;
                                break;
                            }
                        }
                    }
                    Extension::UseSrtp(e) => {
                        if let Ok(profile) = find_matching_srtp_profile(
                            &e.protection_profiles,
                            &cfg.local_srtp_protection_profiles,
                        ) {
                            state.srtp_protection_profile = profile;
                        } else {
                            return Err((
                                Some(Alert {
                                    alert_level: AlertLevel::Fatal,
                                    alert_description: AlertDescription::InsufficientSecurity,
                                }),
                                Some(Error::ErrServerNoMatchingSrtpProfile),
                            ));
                        }
                    }
                    Extension::UseExtendedMasterSecret(_)
                        if cfg.extended_master_secret != ExtendedMasterSecretType::Disable =>
                    {
                        state.extended_master_secret = true;
                    }
                    Extension::ServerName(e) => {
                        state.server_name.clone_from(&e.server_name); // remote server name
                    }
                    _ => {}
                }
            }

            if cfg.extended_master_secret == ExtendedMasterSecretType::Require
                && !state.extended_master_secret
            {
                return Err((
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::InsufficientSecurity,
                    }),
                    Some(Error::ErrServerRequiredButNoClientEms),
                ));
            }

            if state.local_keypair.is_none() {
                state.local_keypair = match state
                    .named_curve
                    .generate_keypair_with_crypto(cfg.provider().crypto())
                {
                    Ok(local_keypar) => Some(local_keypar),
                    Err(err) => {
                        return Err((
                            Some(Alert {
                                alert_level: AlertLevel::Fatal,
                                alert_description: AlertDescription::IllegalParameter,
                            }),
                            Some(err),
                        ));
                    }
                };
            }

            Ok(Box::new(Flight2 {}))
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
        // Initialize
        state.cookie = vec![0; COOKIE_LENGTH];
        if let Err(error) = cfg.provider().random().fill(state.cookie.as_mut_slice()) {
            return Err((None, Some(Error::Crypto(error.to_string()))));
        }

        state.local_epoch = 0;
        state.remote_epoch = 0;

        state.named_curve = DEFAULT_NAMED_CURVE;
        if cfg.local_psk_callback.is_none() {
            state.named_curve = cfg.local_named_curves[0];
        }
        if let Err(error) = state.local_random.populate(cfg.provider().random()) {
            return Err((None, Some(error)));
        }

        Ok(vec![])
    }
}
