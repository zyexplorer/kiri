//! HTTPS for container names: `https://myapp.kiri.local`. Kiri keeps its own
//! certificate authority in `~/.kiri/tls`, made once, whose name constraints
//! let it vouch for `*.kiri.local` and `*.kiri.localhost` only, so even its
//! key could not impersonate any other site. The router (domains.rs) asks
//! for a certificate per name as browsers connect (SNI), signed here on
//! first use. macOS trusts the authority once the user says so in Settings
//! → Domains (`security add-trusted-cert`, which asks for the password).

use std::collections::HashMap;
use std::io;
use std::ops::Not;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// The authority's name, as Keychain Access shows it.
pub const CA_NAME: &str = "Kiri Local Certificate Authority";
const SUFFIXES: [&str; 2] = ["kiri.local", "kiri.localhost"];

pub fn dir() -> Option<PathBuf> {
    super::state_dir().ok().map(|d| d.join("tls"))
}

#[cfg(target_os = "macos")]
pub fn ca_path() -> Option<PathBuf> {
    dir().map(|d| d.join("ca.pem"))
}

/// The authority's fields. They are rebuilt the same way on every start, so
/// with the saved key they sign exactly as the saved certificate does.
fn ca_params() -> Result<CertificateParams, rcgen::Error> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.distinguished_name.push(DnType::CommonName, CA_NAME);
    p.distinguished_name.push(DnType::OrganizationName, "Kiri");
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: SUFFIXES
            .iter()
            .map(|s| GeneralSubtree::DnsName((*s).to_owned()))
            .collect(),
        // Exclude every IP address: with only the DNS subtrees permitted, an
        // IP SAN would otherwise be unconstrained, so a leaked CA key could
        // vouch for https://<any IP>. Kiri only ever issues for its own names.
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    p.not_before = rcgen::date_time_ymd(2026, 1, 1);
    p.not_after = rcgen::date_time_ymd(2046, 1, 1);
    Ok(p)
}

fn err(e: rcgen::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// The authority: loaded from `~/.kiri/tls`, or made there the first time.
fn authority() -> io::Result<(Issuer<'static, KeyPair>, CertificateDer<'static>)> {
    let dir = dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let (cert_path, key_path) = (dir.join("ca.pem"), dir.join("ca-key.pem"));
    // The authority's format version: 2 added the exclusion of IP addresses
    // from its name constraints. An older authority is replaced (the user
    // trusts the new one once), or that fix would never reach an install
    // that already had one.
    const CA_VERSION: &str = "2";
    let version_path = dir.join("ca.version");
    if key_path.is_file()
        && std::fs::read_to_string(&version_path)
            .is_ok_and(|v| v.trim() == CA_VERSION)
            .not()
    {
        let _ = std::fs::remove_file(&key_path);
        let _ = std::fs::remove_file(&cert_path);
    }
    let key = match std::fs::read_to_string(&key_path) {
        Ok(pem) if cert_path.is_file() => KeyPair::from_pem(&pem).map_err(err)?,
        _ => {
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            let key = KeyPair::generate().map_err(err)?;
            let cert = ca_params().map_err(err)?.self_signed(&key).map_err(err)?;
            let tmp = key_path.with_extension("tmp");
            std::fs::write(&tmp, key.serialize_pem())?;
            std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            std::fs::rename(&tmp, &key_path)?;
            std::fs::write(&cert_path, cert.pem())?;
            std::fs::write(&version_path, CA_VERSION)?;
            key
        }
    };
    let pem = std::fs::read_to_string(&cert_path)?;
    let der = pem_to_der(&pem).ok_or_else(|| io::Error::other("ca.pem is not a certificate"))?;
    Ok((Issuer::new(ca_params().map_err(err)?, key), der))
}

fn pem_to_der(pem: &str) -> Option<CertificateDer<'static>> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    kiri_image::b64::decode(&body).map(CertificateDer::from)
}

/// Days since 1970 as a (year, month, day), for certificate dates.
fn civil(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    ((yoe + era * 400 + i64::from(m <= 2)) as i32, m, d)
}

/// Whether `host` is a name this authority may vouch for.
pub fn ours(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    SUFFIXES.iter().any(|s| h.ends_with(&format!(".{s}")))
        && h.split('.').all(|l| !l.is_empty() && l.len() <= 63)
}

/// Certificates for container names, made on first use and kept while the
/// daemon runs.
pub struct Certs {
    ca: Mutex<Option<(Arc<Issuer<'static, KeyPair>>, CertificateDer<'static>)>>,
    /// Per name: the certificate and the day (since the epoch) it expires.
    issued: Mutex<HashMap<String, (Arc<CertifiedKey>, i64)>>,
}

/// Days since the epoch, the unit certificate dates are made in.
fn today() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64
}

impl std::fmt::Debug for Certs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Certs")
    }
}

impl Certs {
    pub fn new() -> Certs {
        Certs {
            ca: Mutex::new(None),
            issued: Mutex::new(HashMap::new()),
        }
    }

    /// A certificate for `host` and the day it expires.
    fn issue(&self, host: &str) -> io::Result<(Arc<CertifiedKey>, i64)> {
        let mut ca = self.ca.lock().unwrap_or_else(|e| e.into_inner());
        if ca.is_none() {
            let (issuer, der) = authority()?;
            *ca = Some((Arc::new(issuer), der));
        }
        let (issuer, ca_der) = ca.as_ref().expect("loaded above");
        let mut p = CertificateParams::new(vec![host.to_owned()]).map_err(err)?;
        p.distinguished_name.push(DnType::CommonName, host);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.use_authority_key_identifier_extension = true;
        // macOS refuses server certificates valid for more than 825 days.
        let today = today();
        let not_after = today + 390;
        let ymd = |d: i64| {
            let (y, m, dd) = civil(d);
            rcgen::date_time_ymd(y, m, dd)
        };
        p.not_before = ymd(today - 1);
        p.not_after = ymd(not_after);
        let key = KeyPair::generate().map_err(err)?;
        let cert = p.signed_by(&key, issuer).map_err(err)?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signer = rustls::crypto::ring::sign::any_supported_type(&der)
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok((
            Arc::new(CertifiedKey::new(
                vec![cert.der().clone(), ca_der.clone()],
                signer,
            )),
            not_after,
        ))
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = hello.server_name()?.to_ascii_lowercase();
        if !ours(&host) {
            return None;
        }
        // Reissue within 30 days of expiry: a daemon that runs for a year
        // would otherwise keep serving a certificate browsers reject.
        if let Some((c, not_after)) = self
            .issued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&host)
        {
            if today() + 30 < *not_after {
                return Some(c.clone());
            }
        }
        let (cert, not_after) = self.issue(&host).ok()?;
        let mut issued = self.issued.lock().unwrap_or_else(|e| e.into_inner());
        // Bound the cache: a client can ask for unlimited distinct SNI names.
        if issued.len() >= 512 {
            issued.clear();
        }
        issued.insert(host, (cert.clone(), not_after));
        Some(cert)
    }
}

/// The router's TLS settings: HTTP/1.1 only, since it relays bytes as they are.
pub fn server_config() -> io::Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::other(e.to_string()))?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Certs::new()));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// The last answer to `trusted()`, and when.
static TRUST: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Whether macOS trusts Kiri's authority for this user, asked at most every
/// few seconds. It is this authority's own certificate that must be trusted:
/// a replaced authority (see `CA_VERSION`) has the same name as the old one,
/// which stays trusted until removed, so matching by name would say yes while
/// browsers refuse the new certificates.
pub fn trusted() -> bool {
    let mut c = TRUST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, v)) = *c {
        if at.elapsed() < Duration::from_secs(5) {
            return v;
        }
    }
    let v = current_ca_trusted();
    *c = Some((Instant::now(), v));
    v
}

#[cfg(target_os = "macos")]
fn current_ca_trusted() -> bool {
    let Some(der) = ca_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|p| pem_to_der(&p))
    else {
        return false;
    };
    sec::with_cert(&der, |cert| {
        let mut settings: sec::Ref = std::ptr::null();
        // SAFETY: cert is a live SecCertificate; a returned array is ours to release.
        unsafe {
            let status =
                sec::SecTrustSettingsCopyTrustSettings(cert, sec::DOMAIN_USER, &mut settings);
            if !settings.is_null() {
                sec::CFRelease(settings);
            }
            status
        }
    }) == 0
}

#[cfg(not(target_os = "macos"))]
fn current_ca_trusted() -> bool {
    false
}

/// Trusting Kiri's authority is the Mac keychain's; Linux's stores aren't wired yet.
#[cfg(not(target_os = "macos"))]
pub fn trust() -> io::Result<()> {
    Err(io::Error::other("HTTPS for containers isn't on Linux yet"))
}

#[cfg(not(target_os = "macos"))]
pub fn untrust() -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
/// Makes the authority (if needed) and asks macOS to trust it for websites.
/// macOS shows its own password prompt. The trust is set in this process,
/// not through `security add-trusted-cert`, so the prompt names Kiri.
pub fn trust() -> io::Result<()> {
    let (_, der) = authority()?;
    let ca = ca_path().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let keychain = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Keychains/login.keychain-db");
    // Into the login keychain, so Keychain Access shows it; this asks nothing,
    // and fails harmlessly when it is already there.
    let _ = std::process::Command::new("/usr/bin/security")
        .args(["add-certificates", "-k"])
        .arg(&keychain)
        .arg(&ca)
        .output();
    let status = trust_for_websites(&der);
    forget_trust();
    trust_status(status)
}

#[cfg(target_os = "macos")]
/// The Security framework calls for this user's trust settings.
mod sec {
    use std::ffi::c_void;
    pub type Ref = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFTypeDictionaryKeyCallBacks: u8;
        pub static kCFTypeDictionaryValueCallBacks: u8;
        pub static kCFTypeArrayCallBacks: u8;
        pub fn CFDataCreate(alloc: Ref, bytes: *const u8, len: isize) -> Ref;
        pub fn CFStringCreateWithCString(
            alloc: Ref,
            s: *const std::ffi::c_char,
            encoding: u32,
        ) -> Ref;
        pub fn CFNumberCreate(alloc: Ref, kind: isize, value: *const c_void) -> Ref;
        pub fn CFDictionaryCreate(
            alloc: Ref,
            keys: *const Ref,
            values: *const Ref,
            n: isize,
            kcb: *const u8,
            vcb: *const u8,
        ) -> Ref;
        pub fn CFArrayCreate(alloc: Ref, values: *const Ref, n: isize, cb: *const u8) -> Ref;
        pub fn CFRelease(r: Ref);
    }
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        pub fn SecCertificateCreateWithData(alloc: Ref, data: Ref) -> Ref;
        pub fn SecPolicyCreateSSL(server: u8, hostname: Ref) -> Ref;
        pub fn SecTrustSettingsSetTrustSettings(cert: Ref, domain: u32, settings: Ref) -> i32;
        pub fn SecTrustSettingsRemoveTrustSettings(cert: Ref, domain: u32) -> i32;
        pub fn SecTrustSettingsCopyTrustSettings(cert: Ref, domain: u32, settings: *mut Ref)
        -> i32;
    }
    pub const DOMAIN_USER: u32 = 0;

    /// Runs `f` with a SecCertificate made from `der`; -1 if it is not one.
    pub fn with_cert(der: &[u8], f: impl FnOnce(Ref) -> i32) -> i32 {
        // SAFETY: each object is checked for NULL and released once.
        unsafe {
            let data = CFDataCreate(std::ptr::null(), der.as_ptr(), der.len() as isize);
            if data.is_null() {
                return -1;
            }
            let cert = SecCertificateCreateWithData(std::ptr::null(), data);
            CFRelease(data);
            if cert.is_null() {
                return -1;
            }
            let status = f(cert);
            CFRelease(cert);
            status
        }
    }
}

#[cfg(target_os = "macos")]
/// A trusted root for SSL only, for this user: what
/// `add-trusted-cert -r trustRoot -p ssl` sets.
fn trust_for_websites(der: &[u8]) -> i32 {
    use sec::*;
    const UTF8: u32 = 0x0800_0100;
    const SINT32: isize = 3;
    const RESULT_TRUST_ROOT: i32 = 1;
    with_cert(der, |cert| {
        // SAFETY: as in with_cert; the dictionary and array retain what
        // they hold, so everything made here is released here.
        unsafe {
            let policy = SecPolicyCreateSSL(1, std::ptr::null());
            let k_policy = CFStringCreateWithCString(
                std::ptr::null(),
                c"kSecTrustSettingsPolicy".as_ptr(),
                UTF8,
            );
            let k_result = CFStringCreateWithCString(
                std::ptr::null(),
                c"kSecTrustSettingsResult".as_ptr(),
                UTF8,
            );
            let result = CFNumberCreate(
                std::ptr::null(),
                SINT32,
                (&RESULT_TRUST_ROOT as *const i32).cast(),
            );
            let made = [policy, k_policy, k_result, result];
            let status = if made.iter().any(|r| r.is_null()) {
                -1
            } else {
                let (keys, values) = ([k_policy, k_result], [policy, result]);
                let dict = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    2,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                let array = CFArrayCreate(std::ptr::null(), &dict, 1, &kCFTypeArrayCallBacks);
                let status = SecTrustSettingsSetTrustSettings(cert, DOMAIN_USER, array);
                CFRelease(array);
                CFRelease(dict);
                status
            };
            for r in made.into_iter().filter(|r| !r.is_null()) {
                CFRelease(r);
            }
            status
        }
    })
}

#[cfg(target_os = "macos")]
/// An OSStatus from the trust calls, as the window expects it.
fn trust_status(status: i32) -> io::Result<()> {
    match status {
        0 => Ok(()),
        // errAuthorizationCanceled, userCanceledErr
        -60006 | -128 => Err(io::Error::other("canceled")),
        // errSecItemNotFound: there was no trust to take back.
        -25300 => Ok(()),
        n => Err(io::Error::other(format!(
            "macOS refused the trust setting (OSStatus {n})"
        ))),
    }
}

#[cfg(target_os = "macos")]
/// Takes the trust back and removes the authority from the keychain.
pub fn untrust() -> io::Result<()> {
    let ca = ca_path().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let der = std::fs::read_to_string(&ca)
        .ok()
        .and_then(|p| pem_to_der(&p));
    // SAFETY: cert is a live SecCertificate for the call.
    let status = der.map_or(0, |der| {
        sec::with_cert(&der, |cert| unsafe {
            sec::SecTrustSettingsRemoveTrustSettings(cert, sec::DOMAIN_USER)
        })
    });
    if matches!(status, 0 | -25300) {
        let _ = std::process::Command::new("/usr/bin/security")
            .args(["delete-certificate", "-c", CA_NAME])
            .output();
    }
    forget_trust();
    trust_status(status)
}

#[cfg(target_os = "macos")]
/// The next `trusted()` asks `security` again.
fn forget_trust() {
    *TRUST.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    /// A certificate macOS has no trust settings for reads as untrusted,
    /// whatever it is called: the check is by certificate, not by name.
    #[test]
    #[cfg(target_os = "macos")]
    fn untrusted_certificate_is_not_trusted() {
        let mut p = super::ca_params().unwrap();
        p.distinguished_name
            .push(rcgen::DnType::CommonName, super::CA_NAME);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = p.self_signed(&key).unwrap();
        let status = super::sec::with_cert(cert.der(), |c| {
            let mut settings: super::sec::Ref = std::ptr::null();
            // SAFETY: c is a live SecCertificate; nothing is returned to release.
            unsafe {
                super::sec::SecTrustSettingsCopyTrustSettings(
                    c,
                    super::sec::DOMAIN_USER,
                    &mut settings,
                )
            }
        });
        assert_ne!(status, 0);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn trust_rejects_garbage_without_asking() {
        assert_eq!(super::trust_for_websites(b"not a certificate"), -1);
        assert_eq!(super::sec::with_cert(b"", |_| 0), -1);
    }

    #[test]
    fn dates() {
        assert_eq!(super::civil(0), (1970, 1, 1));
        assert_eq!(super::civil(20_750), (2026, 10, 24));
        assert_eq!(super::civil(11_016), (2000, 2, 29));
    }

    #[test]
    fn only_kiri_names() {
        assert!(super::ours("myapp.kiri.local"));
        assert!(super::ours("web.shop.kiri.localhost"));
        assert!(!super::ours("kiri.local"));
        assert!(!super::ours("example.com"));
        assert!(!super::ours("evil.kiri.local.example.com"));
    }
}
