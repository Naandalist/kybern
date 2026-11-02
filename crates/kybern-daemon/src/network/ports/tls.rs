//! The certificate of the Fast (HTTP/2) preview listeners (ADE-116).
//!
//! Browsers speak HTTP/2 only over TLS, so a forward's Fast listener serves `https://localhost:<port>`. The certificate is
//! generated once per install and kept in `<data dir>/network/preview-tls/` (the key file is 0600). It is self-signed for
//! `localhost`, `127.0.0.1` and `::1`, and no trust store ever learns about it: Kybern's own web views accept exactly
//! this certificate, by its SHA-256 fingerprint (or its PEM on Linux), for these listeners, and every other client
//! rejects it. A CA would add nothing, since the web views pin the one leaf.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// The certificate is replaced this long before it expires.
const RENEW_BEFORE_DAYS: i64 = 30;
/// How long a new certificate is valid.
const VALID_DAYS: i64 = 365;

/// The certificate and what serves it.
pub struct PreviewCert {
    pub pem: String,
    /// Lowercase hex of the SHA-256 of the DER.
    pub sha256: String,
    pub config: Arc<rustls::ServerConfig>,
}

/// Loads or creates the certificate on first use.
pub struct CertStore {
    dir: PathBuf,
    loaded: Mutex<Option<Arc<PreviewCert>>>,
}

impl CertStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, loaded: Mutex::new(None) }
    }

    pub fn get(&self) -> Result<Arc<PreviewCert>> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cert) = &*loaded {
            return Ok(cert.clone());
        }
        let cert = Arc::new(load_or_create(&self.dir)?);
        *loaded = Some(cert.clone());
        Ok(cert)
    }
}

fn load_or_create(dir: &Path) -> Result<PreviewCert> {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if let (Ok(cert), Ok(key)) = (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path))
        && let Ok(parsed) = build(&cert, &key)
        && !expiring(&cert)
    {
        return Ok(parsed);
    }
    let (cert, key) = generate()?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_private(&key_path, &key)?;
    std::fs::write(&cert_path, &cert).with_context(|| format!("write {}", cert_path.display()))?;
    tracing::info!(target: "kybern::network::ports", dir = %dir.display(), "created the preview certificate");
    build(&cert, &key)
}

fn generate() -> Result<(String, String)> {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()])?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "Kybern preview (this computer only)");
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(VALID_DAYS);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// The certificate's `notAfter` is within [`RENEW_BEFORE_DAYS`], or it cannot be read.
fn expiring(pem: &str) -> bool {
    let Some(der) = der_of(pem) else { return true };
    match not_after(&der) {
        Some(at) => at - time::OffsetDateTime::now_utc() < time::Duration::days(RENEW_BEFORE_DAYS),
        None => true,
    }
}

fn der_of(pem: &str) -> Option<Vec<u8>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls::pki_types::pem::PemObject::from_pem_reader(&mut reader).ok().map(|der: rustls::pki_types::CertificateDer<'static>| der.to_vec())
}

/// `notAfter` of a DER certificate: the second time of the validity sequence. A small DER walk, enough for the
/// certificates this module writes (UTCTime or GeneralizedTime).
fn not_after(der: &[u8]) -> Option<time::OffsetDateTime> {
    fn header(data: &[u8]) -> Option<(u8, usize, usize)> {
        let tag = *data.first()?;
        let first = *data.get(1)? as usize;
        if first < 0x80 {
            return Some((tag, 2, first));
        }
        let count = first & 0x7f;
        let mut length = 0usize;
        for byte in data.get(2..2 + count)? {
            length = (length << 8) | *byte as usize;
        }
        Some((tag, 2 + count, length))
    }
    fn inner(data: &[u8]) -> Option<&[u8]> {
        let (_, head, length) = header(data)?;
        data.get(head..head + length)
    }
    fn skip(data: &[u8]) -> Option<&[u8]> {
        let (_, head, length) = header(data)?;
        data.get(head + length..)
    }
    let tbs = inner(inner(der)?)?;
    // version [0] (optional), serial, signature algorithm, issuer, validity
    let mut rest = tbs;
    if *rest.first()? == 0xa0 {
        rest = skip(rest)?;
    }
    rest = skip(skip(skip(rest)?)?)?;
    let validity = inner(rest)?;
    let after = skip(validity)?;
    let (tag, head, length) = header(after)?;
    let text = std::str::from_utf8(after.get(head..head + length)?).ok()?;
    let digits = text.trim_end_matches('Z');
    let full = match tag {
        0x17 if digits.len() == 12 => {
            let year: i32 = digits[..2].parse().ok()?;
            format!("{}{}", if year >= 50 { 1900 + year } else { 2000 + year }, &digits[2..])
        }
        0x18 if digits.len() == 14 => digits.to_string(),
        _ => return None,
    };
    let number = |range: std::ops::Range<usize>| full.get(range)?.parse::<u32>().ok();
    let date =
        time::Date::from_calendar_date(number(0..4)? as i32, time::Month::try_from(number(4..6)? as u8).ok()?, number(6..8)? as u8).ok()?;
    let clock = time::Time::from_hms(number(8..10)? as u8, number(10..12)? as u8, number(12..14)? as u8).ok()?;
    Some(time::PrimitiveDateTime::new(date, clock).assume_utc())
}

fn build(cert_pem: &str, key_pem: &str) -> Result<PreviewCert> {
    use rustls::pki_types::pem::PemObject;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes()).context("read the preview certificate")?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("read the preview key")?;
    let sha256 = Sha256::digest(der.as_ref()).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![der], key)
        .context("use the preview certificate")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(PreviewCert { pem: cert_pem.to_string(), sha256, config: Arc::new(config) })
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("write {}", path.display()))?;
        file.write_all(text.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_certificate_is_created_once_reused_and_renewed_before_it_expires() {
        let dir = tempfile_dir();
        let store = CertStore::new(dir.clone());
        let first = store.get().unwrap();
        assert_eq!(first.sha256.len(), 64);
        assert!(first.pem.contains("BEGIN CERTIFICATE"));
        assert_eq!(first.config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("key.pem")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // A new store (a restart) reads the same certificate.
        assert_eq!(CertStore::new(dir.clone()).get().unwrap().sha256, first.sha256);
        let after = not_after(&der_of(&first.pem).unwrap()).unwrap();
        let days = (after - time::OffsetDateTime::now_utc()).whole_days();
        assert!((VALID_DAYS - 2..=VALID_DAYS).contains(&days), "{days}");
        assert!(!expiring(&first.pem));
        // An unreadable file is replaced.
        std::fs::write(dir.join("cert.pem"), "nonsense").unwrap();
        assert_ne!(CertStore::new(dir.clone()).get().unwrap().sha256, first.sha256);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kybern-preview-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
