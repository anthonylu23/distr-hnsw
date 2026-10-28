//! Portal HTTPS certificates from `tailscaled` (DESIGN §3;
//! docs/m2-implementation-plan.md, decision 7).
//!
//! The portal asks LocalAPI for this node's certificate and serves it
//! through a rustls certificate resolver, so a renewed certificate takes
//! effect on the next handshake without a restart. A renewal starts when a
//! third of the certificate's lifetime remains and retries with backoff.
//! The resolver hands out only a certificate that is valid now: with none,
//! every handshake fails, and the portal never falls back to plaintext.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rustls::{
    crypto::ring::{default_provider, sign::any_supported_type},
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    ServerConfig,
};
use thiserror::Error;

use crate::tailscale::{LocalApi, LocalApiError};

#[derive(Debug, Error)]
pub enum CertError {
    #[error(transparent)]
    LocalApi(#[from] LocalApiError),
    #[error("certificate pair has no private key: {0}")]
    MissingKey(String),
    #[error("certificate pair has no certificate")]
    MissingCertificate,
    #[error("unsupported or mismatched private key: {0}")]
    Key(rustls::Error),
    #[error("certificate is malformed: {0}")]
    Malformed(&'static str),
    #[error("certificate is not valid now (valid {not_before} to {not_after}, now {now})")]
    NotValidNow {
        not_before: i64,
        not_after: i64,
        now: i64,
    },
}

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// A certificate chain with its key and the leaf's validity window.
#[derive(Debug)]
pub struct Certificate {
    key: Arc<CertifiedKey>,
    not_before: i64,
    not_after: i64,
    fingerprint: [u8; 32],
}

impl Certificate {
    /// Parse LocalAPI's `type=pair` answer: a PEM private key and the PEM
    /// certificate chain, leaf first.
    pub fn from_pem_pair(pem: &[u8]) -> Result<Self, CertError> {
        let key = PrivateKeyDer::from_pem_slice(pem)
            .map_err(|error| CertError::MissingKey(error.to_string()))?;
        let chain = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| CertError::Malformed("PEM certificate block"))?;
        let leaf = chain.first().ok_or(CertError::MissingCertificate)?;
        let (not_before, not_after) = validity(leaf)?;
        if not_after <= not_before {
            return Err(CertError::Malformed("validity ends before it starts"));
        }
        let fingerprint = *blake3::hash(leaf.as_ref()).as_bytes();
        let signing_key = any_supported_type(&key).map_err(CertError::Key)?;
        let key = CertifiedKey::new(chain, signing_key);
        key.keys_match().map_err(CertError::Key)?;
        Ok(Self {
            key: Arc::new(key),
            not_before,
            not_after,
            fingerprint,
        })
    }

    pub fn not_before(&self) -> i64 {
        self.not_before
    }

    pub fn not_after(&self) -> i64 {
        self.not_after
    }

    /// When a third of the lifetime remains.
    pub fn renew_at(&self) -> i64 {
        self.not_before + (self.not_after - self.not_before) * 2 / 3
    }

    pub fn fingerprint_hex(&self) -> String {
        hex::encode(self.fingerprint)
    }

    pub fn valid_at(&self, now: i64) -> bool {
        self.not_before <= now && now < self.not_after
    }
}

/// The certificate the portal serves; read on every handshake.
#[derive(Debug, Default)]
pub struct CertificateStore {
    current: RwLock<Option<Arc<Certificate>>>,
}

impl CertificateStore {
    pub fn install(&self, certificate: Certificate) {
        *self
            .current
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::new(certificate));
    }

    pub fn current(&self) -> Option<Arc<Certificate>> {
        self.current
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// The certificate, only while it is valid.
    pub fn serving(&self, now: i64) -> Option<Arc<Certificate>> {
        self.current()
            .filter(|certificate| certificate.valid_at(now))
    }
}

impl ResolvesServerCert for CertificateStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.serving(now_seconds())
            .map(|certificate| certificate.key.clone())
    }
}

/// TLS 1.2 and 1.3 with the ring provider and HTTP/1.1 only.
pub fn server_config(store: Arc<CertificateStore>) -> Arc<ServerConfig> {
    let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions")
        .with_no_client_auth()
        .with_cert_resolver(store);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Fetch and check this node's certificate for `domain`.
pub async fn fetch(api: &LocalApi, domain: &str) -> Result<Certificate, CertError> {
    let certificate = Certificate::from_pem_pair(&api.cert_pair(domain).await?)?;
    let now = now_seconds();
    if !certificate.valid_at(now) {
        return Err(CertError::NotValidNow {
            not_before: certificate.not_before,
            not_after: certificate.not_after,
            now,
        });
    }
    Ok(certificate)
}

/// How certificate upkeep retries and refreshes.
#[derive(Clone, Copy, Debug)]
pub struct RenewalPolicy {
    /// First retry delay after a failed or unrenewed fetch; doubles up to
    /// `retry_max`.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Longest wait between fetches while the certificate is fresh, so a
    /// certificate `tailscaled` renewed on its own is picked up.
    pub refresh: Duration,
}

impl Default for RenewalPolicy {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(30),
            retry_max: Duration::from_secs(3600),
            refresh: Duration::from_secs(6 * 3600),
        }
    }
}

/// Fetch the first certificate, retrying with the policy's backoff until
/// `wait` has passed.
pub async fn obtain(
    api: &LocalApi,
    domain: &str,
    policy: RenewalPolicy,
    wait: Duration,
) -> Result<Certificate, CertError> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut retry = policy.retry_min;
    loop {
        let error = match fetch(api, domain).await {
            Ok(certificate) => return Ok(certificate),
            Err(error) => error,
        };
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(error);
        }
        eprintln!("warning: no certificate for {domain} yet: {error}; retrying");
        tokio::time::sleep(retry.min(deadline - now)).await;
        retry = (retry * 2).min(policy.retry_max);
    }
}

/// Keep `store` supplied with a valid certificate. Runs until cancelled.
/// A fetched certificate (always valid now) replaces the current one if it
/// is a different certificate that expires no earlier, or if the current
/// one is no longer valid. A failed fetch leaves the current one in place;
/// once that expires, handshakes fail until a fetch succeeds.
pub async fn maintain(
    store: Arc<CertificateStore>,
    api: LocalApi,
    domain: String,
    policy: RenewalPolicy,
) {
    let mut retry = policy.retry_min;
    loop {
        let now = now_seconds();
        let current = store.current();
        let due = current
            .as_ref()
            .is_none_or(|certificate| certificate.renew_at() <= now);
        let wait = match &current {
            Some(certificate) if !due => {
                Duration::from_secs((certificate.renew_at() - now) as u64).min(policy.refresh)
            }
            _ => retry,
        };
        tokio::time::sleep(wait).await;
        match fetch(&api, &domain).await {
            Ok(fetched) if replaces(&fetched, store.current().as_deref(), now_seconds()) => {
                eprintln!(
                    "installed certificate for {domain} valid until {} (sha {})",
                    fetched.not_after,
                    &fetched.fingerprint_hex()[..16]
                );
                store.install(fetched);
                retry = policy.retry_min;
            }
            Ok(_) if due => {
                eprintln!(
                    "warning: tailscaled has not renewed the certificate for {domain} yet; \
                     retrying in {:?}",
                    retry
                );
                retry = (retry * 2).min(policy.retry_max);
            }
            Ok(_) => {}
            Err(error) => {
                let expired = store.serving(now_seconds()).is_none();
                eprintln!(
                    "{}: certificate fetch for {domain} failed: {error}{}",
                    if expired { "error" } else { "warning" },
                    if expired {
                        "; no valid certificate, so every TLS handshake is refused"
                    } else {
                        ""
                    }
                );
                if due || expired {
                    retry = (retry * 2).min(policy.retry_max);
                }
            }
        }
    }
}

fn replaces(fetched: &Certificate, current: Option<&Certificate>, now: i64) -> bool {
    current.is_none_or(|current| {
        !current.valid_at(now)
            || (fetched.fingerprint != current.fingerprint
                && fetched.not_after >= current.not_after)
    })
}

/// The leaf's `notBefore` and `notAfter` as Unix seconds, read from the
/// DER `TBSCertificate` (RFC 5280 §4.1).
fn validity(certificate: &CertificateDer<'_>) -> Result<(i64, i64), CertError> {
    let (_, certificate, _) = der_expect(certificate.as_ref(), 0x30, "certificate")?;
    let (_, tbs, _) = der_expect(certificate, 0x30, "tbsCertificate")?;
    let mut rest = tbs;
    let (tag, _, after) = der_element(rest)?;
    if tag == 0xa0 {
        rest = after; // explicit version
    }
    let (_, _, rest) = der_expect(rest, 0x02, "serialNumber")?;
    let (_, _, rest) = der_expect(rest, 0x30, "signature")?;
    let (_, _, rest) = der_expect(rest, 0x30, "issuer")?;
    let (_, validity, _) = der_expect(rest, 0x30, "validity")?;
    let (tag, not_before, rest) = der_element(validity)?;
    let not_before = der_time(tag, not_before)?;
    let (tag, not_after, _) = der_element(rest)?;
    let not_after = der_time(tag, not_after)?;
    Ok((not_before, not_after))
}

fn der_element(input: &[u8]) -> Result<(u8, &[u8], &[u8]), CertError> {
    let malformed = CertError::Malformed("DER element");
    let (&tag, rest) = input
        .split_first()
        .ok_or(CertError::Malformed("DER element"))?;
    let (&first, rest) = rest
        .split_first()
        .ok_or(CertError::Malformed("DER length"))?;
    let (length, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || rest.len() < count {
            return Err(malformed);
        }
        let length = rest[..count]
            .iter()
            .fold(0_usize, |length, &byte| (length << 8) | byte as usize);
        (length, &rest[count..])
    };
    if rest.len() < length {
        return Err(CertError::Malformed("DER length exceeds input"));
    }
    Ok((tag, &rest[..length], &rest[length..]))
}

fn der_expect<'a>(
    input: &'a [u8],
    expected: u8,
    what: &'static str,
) -> Result<(u8, &'a [u8], &'a [u8]), CertError> {
    let element = der_element(input)?;
    if element.0 != expected {
        return Err(CertError::Malformed(what));
    }
    Ok(element)
}

/// `UTCTime` (`YYMMDDHHMMSSZ`) or `GeneralizedTime` (`YYYYMMDDHHMMSSZ`).
fn der_time(tag: u8, content: &[u8]) -> Result<i64, CertError> {
    let malformed = CertError::Malformed("validity time");
    let text = std::str::from_utf8(content).map_err(|_| CertError::Malformed("validity time"))?;
    let digits = text
        .strip_suffix('Z')
        .ok_or(CertError::Malformed("validity time"))?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed);
    }
    let number = |range: std::ops::Range<usize>| -> i64 { digits[range].parse().unwrap_or(-1) };
    let (year, rest) = match (tag, digits.len()) {
        (0x17, 12) => {
            let year = number(0..2);
            (if year >= 50 { 1900 + year } else { 2000 + year }, 2)
        }
        (0x18, 14) => (number(0..4), 4),
        _ => return Err(malformed),
    };
    let month = number(rest..rest + 2);
    let day = number(rest + 2..rest + 4);
    let hour = number(rest + 4..rest + 6);
    let minute = number(rest + 6..rest + 8);
    let second = number(rest + 8..rest + 10);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(malformed);
    }
    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed pair in LocalAPI's format, and its key PEM.
    fn pair(not_before: i64, not_after: i64) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["node.example.ts.net".into()]).unwrap();
        params.not_before = time::OffsetDateTime::from_unix_timestamp(not_before).unwrap();
        params.not_after = time::OffsetDateTime::from_unix_timestamp(not_after).unwrap();
        let certificate = params.self_signed(&key).unwrap();
        (
            format!("{}{}", key.serialize_pem(), certificate.pem()),
            key.serialize_pem(),
        )
    }

    #[test]
    fn pairs_parse_with_their_validity_and_a_matching_key() {
        let (pem, _) = pair(1_000_000_000, 1_000_090_000);
        let certificate = Certificate::from_pem_pair(pem.as_bytes()).unwrap();
        assert_eq!(certificate.not_before(), 1_000_000_000);
        assert_eq!(certificate.not_after(), 1_000_090_000);
        assert_eq!(certificate.renew_at(), 1_000_060_000);
        assert!(certificate.valid_at(1_000_000_000));
        assert!(!certificate.valid_at(1_000_090_000));
        // Long-lived certificates use GeneralizedTime after 2049.
        let (pem, _) = pair(2_600_000_000, 2_700_000_000);
        assert_eq!(
            Certificate::from_pem_pair(pem.as_bytes())
                .unwrap()
                .not_after(),
            2_700_000_000
        );

        let (pem_a, _) = pair(1_000_000_000, 1_000_090_000);
        let (_, key_b) = pair(1_000_000_000, 1_000_090_000);
        let certificate_a = &pem_a[pem_a.find("-----BEGIN CERTIFICATE").unwrap()..];
        let mismatched = format!("{key_b}{certificate_a}");
        assert!(matches!(
            Certificate::from_pem_pair(mismatched.as_bytes()),
            Err(CertError::Key(_))
        ));
        assert!(matches!(
            Certificate::from_pem_pair(certificate_a.as_bytes()),
            Err(CertError::MissingKey(_))
        ));
        assert!(matches!(
            Certificate::from_pem_pair(key_b.as_bytes()),
            Err(CertError::MissingCertificate)
        ));
    }

    #[test]
    fn a_fetched_certificate_replaces_only_a_newer_or_dead_one() {
        let parse = |not_before, not_after| {
            Certificate::from_pem_pair(pair(not_before, not_after).0.as_bytes()).unwrap()
        };
        let current = parse(100, 400);
        assert!(replaces(&current, None, 200));
        assert!(
            !replaces(&current, Some(&current), 200),
            "the same certificate"
        );
        assert!(replaces(&parse(150, 500), Some(&current), 200), "renewed");
        assert!(
            replaces(&parse(150, 400), Some(&current), 200),
            "reissued, same expiry"
        );
        assert!(
            !replaces(&parse(50, 300), Some(&current), 200),
            "older expiry while valid"
        );
        assert!(
            replaces(&parse(50, 300), Some(&current), 450),
            "the current one expired"
        );
    }

    #[test]
    fn civil_dates_convert_to_unix_seconds() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(der_time(0x17, b"261028120000Z").unwrap(), 1_793_188_800);
        assert_eq!(der_time(0x18, b"20261028120000Z").unwrap(), 1_793_188_800);
        assert_eq!(der_time(0x17, b"700101000000Z").unwrap(), 0);
        assert!(der_time(0x17, b"261328120000Z").is_err());
        assert!(der_time(0x17, b"2610281200Z").is_err());
        assert!(der_time(0x18, b"20261028120000+0100").is_err());
    }
}
