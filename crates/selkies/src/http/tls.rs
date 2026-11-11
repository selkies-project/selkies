/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! HTTPS: the certificate the listener serves, minted when none is configured, and swapped
//! in place when the files change.
//!
//! Browsers gate the clipboard, gamepads, pointer lock, and the camera on a secure context,
//! so a session that is not on localhost needs TLS before those work at all. Turning HTTPS on
//! is otherwise a two-step job (make a certificate, then point at it), so [`build`] uses the
//! configured `https_cert`/`https_key` pair when both files exist and otherwise
//! [`make_self_signed_cert`]: a P-256 pair for `localhost`, the host's name, and the loopback
//! addresses under both spellings (a published container port is what most deployments type,
//! and a certificate naming only `localhost` fails hostname verification at 127.0.0.1),
//! written to the configured path when its directory is writable and to the user's state
//! directory otherwise, and reused on the next start so the exception a browser was told to
//! make survives it. A pair from before the address names were issued is replaced rather than
//! carried forward.
//!
//! The listener never restarts for a new certificate: rustls asks [`CertResolver`] for the
//! key on every handshake, and [`watch_and_reload`] polls the files' mtimes every
//! `cert_reload_interval` seconds, loads the new pair on a blocking thread before anything
//! changes, and swaps it in, so a bad certificate never takes the server offline and a good
//! one serves the next handshake with no window in which nothing listens.

use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{RootCertStore, ServerConfig};

/// The TLS settings.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub enable_https: bool,
    /// The configured certificate path (`/etc/ssl/certs/ssl-cert-snakeoil.pem` by default).
    pub cert: String,
    /// The configured key path; empty means the key sits in the certificate file.
    pub key: String,
    /// Seconds between mtime polls; 0 disables reloading.
    pub reload_interval: i64,
}

impl Default for TlsConfig {
    fn default() -> TlsConfig {
        TlsConfig {
            enable_https: false,
            cert: "/etc/ssl/certs/ssl-cert-snakeoil.pem".into(),
            key: "/etc/ssl/private/ssl-cert-snakeoil.key".into(),
            reload_interval: 30,
        }
    }
}

/// Why a certificate could not be served.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Rustls(#[from] rustls::Error),
    #[error("{0}")]
    Pem(#[from] pem::Error),
    #[error("{0}")]
    Generate(#[from] rcgen::Error),
    #[error("{0}")]
    Other(String),
}

/// The crypto provider every context here is built with.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// The absolute path of a setting's file, when it names an existing regular file.
fn existing(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    Path::new(path).is_file().then(|| absolute(path))
}

/// The configured certificate and key, each none when unset or missing on disk.
pub fn configured_certs(cfg: &TlsConfig) -> (Option<PathBuf>, Option<PathBuf>) {
    (existing(&cfg.cert), existing(&cfg.key))
}

/// Where a generated pair may live, most preferred first: the configured path, so an
/// operator who pointed at one gets the pair there, then the state directory, which catches a
/// user install (the default `ssl-cert-snakeoil` path means anyone not running as root).
pub fn self_signed_candidates(cfg: &TlsConfig) -> Vec<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    if !cfg.cert.is_empty() && !cfg.key.is_empty() {
        candidates.push((absolute(&cfg.cert), absolute(&cfg.key)));
    }
    let state = state_dir().join("selkies");
    candidates.push((state.join("selkies.pem"), state.join("selkies.key")));
    candidates
}

/// `$XDG_STATE_HOME`, else `~/.local/state`.
fn state_dir() -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));
            home.join(".local/state")
        }
    }
}

/// A path made absolute against the working directory, without touching the filesystem.
fn absolute(path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// The names a generated certificate is issued for: loopback under every spelling, since
/// that is what a published container port is reached by, plus the host's own name. Its
/// routable address is deliberately absent: the pair outlives the address a container is
/// given on each run, and one naming last week's is worse than one claiming none.
pub fn self_signed_names() -> (Vec<String>, Vec<IpAddr>) {
    let mut dns = vec!["localhost".to_string()];
    if let Ok(hostname) = nix::unistd::gethostname() {
        let hostname = hostname.to_string_lossy().into_owned();
        if !hostname.is_empty() && hostname != "localhost" {
            dns.push(hostname);
        }
    }
    (
        dns,
        vec![
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]),
        ],
    )
}

/// The PEM certificates and the private key of a pair; the key is read from the certificate
/// file when no key path is given.
fn read_pair(
    cert: &Path,
    key: Option<&Path>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let cert_pem = fs::read(cert)?;
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(&cert_pem).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        return Err(TlsError::Other(format!(
            "no certificate in {}",
            cert.display()
        )));
    }
    let key_pem = match key {
        Some(key) => fs::read(key)?,
        None => cert_pem,
    };
    let key = match PrivateKeyDer::from_pem_slice(&key_pem) {
        Err(pem::Error::NoItemsFound) => {
            return Err(TlsError::Other("no private key found".to_string()));
        }
        key => key?,
    };
    Ok((certs, key))
}

/// The pair as rustls serves it, the key checked against the certificate.
pub fn load_certified_key(cert: &Path, key: Option<&Path>) -> Result<CertifiedKey, TlsError> {
    let (certs, key) = read_pair(cert, key)?;
    Ok(CertifiedKey::from_der(certs, key, &provider())?)
}

/// Whether `cert`, trusted as its own anchor, verifies for `name` right now: it has not
/// expired, its signature holds, and its names include `name`.
pub fn cert_valid_for(cert: &CertificateDer<'static>, name: ServerName<'static>) -> bool {
    let mut roots = RootCertStore::empty();
    if roots.add(cert.clone()).is_err() {
        return false;
    }
    let Ok(verifier) =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build()
    else {
        return false;
    };
    verifier
        .verify_server_cert(cert, &[], &name, &[], UnixTime::now())
        .is_ok()
}

/// Whether an existing generated pair can be served again as it is: it loads, has not
/// expired, and names loopback by address. Anything unreadable counts as absent, so a
/// truncated file is replaced.
pub fn self_signed_pair_usable(cert: &Path, key: &Path) -> bool {
    if !(cert.is_file() && key.is_file()) {
        return false;
    }
    let Ok((certs, _)) = read_pair(cert, Some(key)) else {
        return false;
    };
    let Some(leaf) = certs.first() else {
        return false;
    };
    cert_valid_for(
        leaf,
        ServerName::IpAddress(IpAddr::from([127, 0, 0, 1]).into()),
    ) && load_certified_key(cert, Some(key)).is_ok()
}

/// A self-signed certificate and key, writing one where none is usable (the module docstring
/// says where and why). Returns the pair's paths.
pub fn make_self_signed_cert(cfg: &TlsConfig) -> Result<(PathBuf, PathBuf), TlsError> {
    let candidates = self_signed_candidates(cfg);
    for (cert, key) in &candidates {
        if self_signed_pair_usable(cert, key) {
            log::info!(target: "server", "HTTPS is running on the self-signed certificate already at {}.", cert.display());
            return Ok((cert.clone(), key.clone()));
        }
    }
    let (dns, addresses) = self_signed_names();
    let key_pair = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    let mut sans = Vec::new();
    for name in &dns {
        sans.push(rcgen::SanType::DnsName(name.as_str().try_into()?));
    }
    sans.extend(addresses.iter().map(|ip| rcgen::SanType::IpAddress(*ip)));
    params.subject_alt_names = sans;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "localhost");
    params.distinguished_name = dn;
    let now = chrono::Utc::now();
    let (y, m, d) = ymd(now - chrono::Duration::days(1));
    params.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = ymd(now + chrono::Duration::days(3650));
    params.not_after = rcgen::date_time_ymd(y, m, d);
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    let serial: [u8; 16] = rand::random();
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    let cert = params.self_signed(&key_pair)?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let mut errors = Vec::new();
    for (cert_path, key_path) in &candidates {
        if let Err(error) = write_pair(cert_path, key_path, &cert_pem, &key_pem) {
            errors.push(format!("{}: {error}", cert_path.display()));
            continue;
        }
        let names: Vec<String> = dns
            .iter()
            .cloned()
            .chain(addresses.iter().map(|a| a.to_string()))
            .collect();
        log::warn!(target: "server",
            "No certificate at the configured path, so HTTPS runs on a self-signed one written to {}, issued for {} \
             and no other address. Browsers will warn once until it is trusted; a certificate from an authority, or \
             a reverse proxy terminating TLS, avoids that.",
            cert_path.display(), names.join(", "));
        return Ok((cert_path.clone(), key_path.clone()));
    }
    Err(TlsError::Other(format!(
        "could not write a self-signed certificate: {}",
        errors.join("; ")
    )))
}

/// The calendar date of `when`, as rcgen's `date_time_ymd` takes it.
fn ymd(when: chrono::DateTime<chrono::Utc>) -> (i32, u8, u8) {
    use chrono::Datelike;
    (when.year(), when.month() as u8, when.day() as u8)
}

/// Write the certificate, then the key readable by its owner alone; the mode is set after the
/// write too, since creating leaves an existing file's mode alone.
fn write_pair(
    cert_path: &Path,
    key_path: &Path,
    cert_pem: &str,
    key_pem: &str,
) -> std::io::Result<()> {
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(cert_path, cert_pem)?;
    let mut key_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(key_path)?;
    key_file.write_all(key_pem.as_bytes())?;
    fs::set_permissions(
        key_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    Ok(())
}

/// The certificate rustls serves, swapped whole on a reload.
#[derive(Debug)]
pub struct CertResolver {
    current: RwLock<Arc<CertifiedKey>>,
}

impl CertResolver {
    /// Serve `key` until `swap` replaces it.
    pub fn new(key: CertifiedKey) -> CertResolver {
        CertResolver {
            current: RwLock::new(Arc::new(key)),
        }
    }

    /// Serve `key` from the next handshake on.
    pub fn swap(&self, key: CertifiedKey) {
        *self.current.write() = Arc::new(key);
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().clone())
    }
}

/// The TLS state of a running server: the rustls configuration the listener accepts with and
/// the resolver the watcher swaps.
pub struct Tls {
    pub server_config: Arc<ServerConfig>,
    pub resolver: Arc<CertResolver>,
    /// The pair being served.
    pub cert: PathBuf,
    pub key: Option<PathBuf>,
}

/// The pair `cfg` resolves to: the configured one when it exists, else a self-signed one.
fn resolve_pair(cfg: &TlsConfig) -> Result<(PathBuf, Option<PathBuf>), TlsError> {
    let (cert, key) = configured_certs(cfg);
    match cert {
        Some(cert) => Ok((cert, key)),
        None => {
            let (cert, key) = make_self_signed_cert(cfg)?;
            Ok((cert, Some(key)))
        }
    }
}

/// The server's TLS state, or none when HTTPS is disabled.
///
/// TLS 1.2 is the floor; clients are not asked for certificates;
/// `http/1.1` is the one protocol offered, since the WebSocket upgrade rides HTTP/1.1.
pub fn build(cfg: &TlsConfig) -> Result<Option<Tls>, TlsError> {
    if !cfg.enable_https {
        return Ok(None);
    }
    let (cert, key) = resolve_pair(cfg)?;
    log::debug!(
        target: "server",
        "Creating TLS context with certificate={} ({})",
        cert.display(),
        if key.is_some() { "its key in a separate file" } else { "its key in the same file" }
    );
    let certified = load_certified_key(&cert, key.as_deref()).inspect_err(|_| {
        log::error!(target: "server",
            "Certificate or private key file not found or incorrect. To use a self-signed certificate, install the \
             package \"ssl-cert\" and add the group \"ssl-cert\" to your user in Debian-based distributions, or \
             generate a new certificate with root using: openssl req -x509 -newkey rsa:4096 -keyout \
             /etc/ssl/private/ssl-cert-snakeoil.key -out /etc/ssl/certs/ssl-cert-snakeoil.pem -days 3650 -nodes \
             -subj \"/CN=localhost\"");
    })?;
    let resolver = Arc::new(CertResolver::new(certified));
    let mut server_config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Some(Tls {
        server_config: Arc::new(server_config),
        resolver,
        cert,
        key,
    }))
}

/// The most recent modification time of the pair being served, in seconds; falls back to the
/// generated pair so the watcher is not blind to it when the configured path holds nothing,
/// and 0 when there is none.
pub fn cert_mtime(cfg: &TlsConfig) -> f64 {
    let (mut cert, mut key) = configured_certs(cfg);
    if cert.is_none() {
        for (candidate_cert, candidate_key) in self_signed_candidates(cfg) {
            if candidate_cert.is_file() {
                cert = Some(candidate_cert);
                key = Some(candidate_key);
                break;
            }
        }
    }
    let Some(cert) = cert else { return 0.0 };
    let mtime = |path: &Path| -> f64 {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    };
    let key_mtime = key.as_deref().map(mtime).unwrap_or(0.0);
    mtime(&cert).max(key_mtime)
}

/// Poll the pair's mtimes and swap a changed pair into the resolver.
///
/// The new key is loaded before anything changes so a bad certificate never takes the server
/// offline, and the recorded mtime advances only once the swap happened, so a failed reload is
/// retried on the next poll. Both the stat and the load run off the runtime's workers: they
/// read files that may sit on a network mount, while the session is streaming.
pub async fn watch_and_reload(tls: Arc<Tls>, cfg: TlsConfig) {
    if cfg.reload_interval <= 0 {
        log::debug!(target: "server", "Automatic certificate reloading is disabled (interval=0)");
        return;
    }
    let interval = Duration::from_secs(cfg.reload_interval as u64);
    let stat_cfg = cfg.clone();
    let mut last_mtime = tokio::task::spawn_blocking(move || cert_mtime(&stat_cfg))
        .await
        .unwrap_or(0.0);
    log::debug!(target: "server", "Certificate reload watcher started (interval={}s, initial mtime={last_mtime:.0})",
                cfg.reload_interval);
    loop {
        tokio::time::sleep(interval).await;
        let stat_cfg = cfg.clone();
        let new_mtime = match tokio::task::spawn_blocking(move || cert_mtime(&stat_cfg)).await {
            Ok(mtime) => mtime,
            Err(_) => return,
        };
        if new_mtime <= last_mtime {
            continue;
        }
        log::info!(target: "server", "Certificate change detected (mtime {last_mtime:.0} -> {new_mtime:.0}), reloading…");
        let load_cfg = cfg.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let (cert, key) = resolve_pair(&load_cfg)?;
            load_certified_key(&cert, key.as_deref()).map(|k| (k, cert))
        })
        .await;
        match loaded {
            Ok(Ok((key, cert))) => {
                tls.resolver.swap(key);
                last_mtime = new_mtime;
                log::info!(target: "server", "Serving the reloaded certificate from {}", cert.display());
            }
            Ok(Err(exc)) => {
                log::error!(target: "server", "Failed to create new SSL context, keeping old certificate: {exc}");
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::tempfile_dir;
    use std::os::unix::fs::PermissionsExt;

    /// The settings of a server pointed at `cert`/`key` with HTTPS on.
    fn cfg(cert: &Path, key: &Path) -> TlsConfig {
        TlsConfig {
            enable_https: true,
            cert: cert.to_string_lossy().into_owned(),
            key: key.to_string_lossy().into_owned(),
            reload_interval: 30,
        }
    }

    fn leaf(cert: &Path) -> CertificateDer<'static> {
        let pem = fs::read(cert).unwrap();
        CertificateDer::from_pem_slice(&pem).unwrap()
    }

    /// Whether this process writes into `directory` whatever its mode says (CAP_DAC_OVERRIDE).
    fn writes_through_mode(directory: &Path) -> bool {
        let probe = directory.join(".probe");
        match fs::write(&probe, b"") {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    /// A pair naming `localhost` alone, as an install from before the address SANs carried.
    fn write_localhost_only_pair(cert: &Path, key: &Path) {
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        let c = params.self_signed(&key_pair).unwrap();
        fs::write(cert, c.pem()).unwrap();
        fs::write(key, key_pair.serialize_pem()).unwrap();
    }

    #[test]
    fn a_pair_loads_from_separate_files_and_from_one_combined_file() {
        let work = tempfile_dir("selkies-pem-");
        let (cert, key) = (work.join("s.pem"), work.join("s.key"));
        write_localhost_only_pair(&cert, &key);
        load_certified_key(&cert, Some(&key)).expect("the pair loads from its two files");

        let combined = work.join("both.pem");
        let mut both = fs::read(&cert).unwrap();
        both.extend(fs::read(&key).unwrap());
        fs::write(&combined, both).unwrap();
        load_certified_key(&combined, None).expect("the key is read from the certificate file");
    }

    #[test]
    fn a_certificate_without_a_key_is_refused_by_name() {
        let work = tempfile_dir("selkies-pem-");
        let (cert, key) = (work.join("s.pem"), work.join("s.key"));
        write_localhost_only_pair(&cert, &key);
        let err = load_certified_key(&cert, None).expect_err("no key, no pair");
        assert_eq!(err.to_string(), "no private key found");
        let err = load_certified_key(&key, Some(&key)).expect_err("no certificate, no pair");
        assert!(err.to_string().starts_with("no certificate in "), "{err}");
    }

    /// One TLS handshake against `config` on an ephemeral port, verified by `verifier` and
    /// for `name`; the server's answer to `hi`, or the client's error.
    async fn handshake(
        config: Arc<ServerConfig>,
        roots: Option<CertificateDer<'static>>,
        name: ServerName<'static>,
    ) -> Result<Vec<u8>, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = acceptor.accept(stream).await {
                let mut buf = [0u8; 16];
                let _ = tls.read(&mut buf).await;
                let _ = tls.write_all(b"ok").await;
                let _ = tls.shutdown().await;
            }
        });
        let client_config = match roots {
            Some(cert) => {
                let mut store = RootCertStore::empty();
                store.add(cert).unwrap();
                rustls::ClientConfig::builder_with_provider(provider())
                    .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
                    .unwrap()
                    .with_root_certificates(store)
                    .with_no_client_auth()
            }
            None => rustls::ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth(),
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let result = async {
            let mut tls = connector
                .connect(name, stream)
                .await
                .map_err(|e| e.to_string())?;
            tls.write_all(b"hi").await.map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            tls.read_to_end(&mut out).await.map_err(|e| e.to_string())?;
            Ok(out)
        }
        .await;
        let _ = server.await;
        result
    }

    /// A client verifier that trusts anything, for the handshake that only checks the pair works.
    #[derive(Debug)]
    struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// The whole path a user takes: `Server::start` mints the pair, serves HTTPS on it, tells
    /// the page it arrived secure, and a rewritten pair answers the next handshake with no
    /// listener restart.
    #[tokio::test]
    async fn server_serves_https_and_reloads_in_place() {
        use crate::http::supervisor::FixedSupervisor;
        use crate::http::{AppState, HttpConfig, Server, build};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let work = tempfile_dir("selkies-https-server-");
        let (cert, key) = (work.join("s.pem"), work.join("s.key"));
        let mut config = HttpConfig {
            tls: cfg(&cert, &key),
            ..HttpConfig::default()
        };
        config.tls.reload_interval = 1;
        config.listen.addr = "127.0.0.1".into();
        config.listen.port = 0;
        let state = AppState::new(config, FixedSupervisor::new(None, &[]));
        let server = Server::start(&state, build(state.clone(), Vec::new()))
            .await
            .expect("server");
        assert!(
            server.endpoint().starts_with("https://127.0.0.1:"),
            "{}",
            server.endpoint()
        );
        let addr = server.local_addrs()[0];
        if !addr.ip().is_loopback() {
            panic!("the server bound {addr}, not loopback");
        }

        let peer_cert = || async move {
            let client = rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut tls = tokio_rustls::TlsConnector::from(Arc::new(client))
                .connect(ServerName::try_from("localhost").unwrap(), stream)
                .await
                .unwrap();
            tls.write_all(b"GET /api/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut out = Vec::new();
            let _ = tls.read_to_end(&mut out).await;
            assert!(
                out.starts_with(b"HTTP/1.1 200"),
                "{:?}",
                String::from_utf8_lossy(&out)
            );
            tls.get_ref().1.peer_certificates().unwrap()[0]
                .clone()
                .into_owned()
        };
        let first = peer_cert().await;
        assert_eq!(
            first,
            leaf(&cert),
            "the pair minted at the configured path is what is served"
        );

        tokio::time::sleep(Duration::from_millis(1100)).await;
        fs::remove_file(&cert).unwrap();
        fs::remove_file(&key).unwrap();
        make_self_signed_cert(&cfg(&cert, &key)).unwrap();
        let future = std::time::SystemTime::now() + Duration::from_secs(5);
        for path in [&cert, &key] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(future)
                .unwrap();
        }
        let mut reloaded = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if peer_cert().await == leaf(&cert) {
                reloaded = true;
                break;
            }
        }
        assert!(
            reloaded,
            "the next handshake after a poll carries the new certificate"
        );
        server.stop().await;
        let _ = fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn https_comes_up_self_signed() {
        if crate::http::testing::rerun_alone("http::tls::tests::https_comes_up_self_signed") {
            return;
        }
        let work = tempfile_dir("selkies-https-");
        let state = work.join("state");
        // SAFETY: under rerun_alone no other test runs in this process, and nothing here has
        // started a thread that reads the environment.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

        let configured = work.join("configured");
        fs::create_dir_all(&configured).unwrap();
        let (cert, key) = (configured.join("s.pem"), configured.join("s.key"));
        let (made_cert, made_key) = make_self_signed_cert(&cfg(&cert, &key)).unwrap();
        assert_eq!(
            (made_cert, made_key),
            (cert.clone(), key.clone()),
            "a writable configured path is where the pair lands"
        );
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600,
            "the private key is readable by its owner alone"
        );

        let blocked = work.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
        let bound_by_mode = !writes_through_mode(&blocked);
        let blocked_cfg = cfg(&blocked.join("s.pem"), &blocked.join("s.key"));
        let (fallback_cert, fallback_key) = make_self_signed_cert(&blocked_cfg).unwrap();
        if bound_by_mode {
            assert!(
                fallback_cert.starts_with(&state),
                "an unwritable one falls back to the state directory"
            );
        } else {
            eprintln!(
                "SKIP an unwritable one falls back to the state directory: this process writes through the mode"
            );
        }

        let body = fs::read(&cert).unwrap();
        let tls = build(&cfg(&cert, &key))
            .unwrap()
            .expect("a context is built from the pair already on disk");
        assert_eq!(
            fs::read(&cert).unwrap(),
            body,
            "that pair is reused rather than replaced"
        );

        let answer = handshake(
            tls.server_config.clone(),
            None,
            ServerName::try_from("localhost").unwrap(),
        )
        .await;
        assert_eq!(
            answer,
            Ok(b"ok".to_vec()),
            "the certificate completes a TLS handshake"
        );

        let der = leaf(&cert);
        assert!(
            cert_valid_for(&der, ServerName::try_from("localhost").unwrap()),
            "the certificate names localhost"
        );
        assert!(
            cert_valid_for(
                &der,
                ServerName::IpAddress(IpAddr::from([127, 0, 0, 1]).into())
            ) && cert_valid_for(
                &der,
                ServerName::IpAddress(IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]).into())
            ),
            "the certificate names the loopback addresses it is reached by"
        );
        let verified = handshake(
            tls.server_config.clone(),
            Some(der.clone()),
            ServerName::IpAddress(IpAddr::from([127, 0, 0, 1]).into()),
        )
        .await;
        assert_eq!(
            verified,
            Ok(b"ok".to_vec()),
            "it verifies for 127.0.0.1, not only for the name localhost"
        );

        let fallback_body = fs::read(&fallback_cert).unwrap();
        let again = make_self_signed_cert(&blocked_cfg).unwrap();
        assert!(
            again == (fallback_cert.clone(), fallback_key.clone())
                && fs::read(&fallback_cert).unwrap() == fallback_body,
            "a second start reuses the fallback pair rather than minting one"
        );
        assert!(
            cert_mtime(&blocked_cfg) > 0.0,
            "the reload watcher sees the pair actually being served"
        );

        let stale = work.join("stale");
        fs::create_dir_all(&stale).unwrap();
        let (stale_cert, stale_key) = (stale.join("s.pem"), stale.join("s.key"));
        write_localhost_only_pair(&stale_cert, &stale_key);
        assert!(
            !self_signed_pair_usable(&stale_cert, &stale_key),
            "a localhost-only pair from an earlier install is replaced"
        );
        // SAFETY: as above.
        unsafe { std::env::set_var("XDG_STATE_HOME", work.join("state-upgrade")) };
        make_self_signed_cert(&cfg(&stale_cert, &stale_key)).unwrap();
        assert!(
            cert_valid_for(
                &leaf(&stale_cert),
                ServerName::IpAddress(IpAddr::from([127, 0, 0, 1]).into())
            ),
            "and what replaces it names the loopback addresses"
        );

        let mut off = cfg(&cert, &key);
        off.enable_https = false;
        assert!(
            build(&off).unwrap().is_none(),
            "disabled HTTPS builds no context and writes nothing"
        );

        // The watcher swaps a rewritten pair in without the listener noticing.
        let mut reload_cfg = cfg(&cert, &key);
        reload_cfg.reload_interval = 1;
        let tls = Arc::new(tls);
        let watcher = tokio::spawn(watch_and_reload(tls.clone(), reload_cfg));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let before = tls.resolver.current.read().clone();
        fs::remove_file(&cert).unwrap();
        fs::remove_file(&key).unwrap();
        make_self_signed_cert(&cfg(&cert, &key)).unwrap();
        let future = std::time::SystemTime::now() + Duration::from_secs(5);
        for path in [&cert, &key] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(future)
                .unwrap();
        }
        let mut swapped = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !Arc::ptr_eq(&before, &tls.resolver.current.read()) {
                swapped = true;
                break;
            }
        }
        watcher.abort();
        assert!(swapped, "a changed pair is served after the next poll");
        let _ = fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&work);
    }
}
