//! D4503 tripwire: a Linux build whose vendored OpenSSL lost pgrust's trim must not pass unnoticed.
//!
//! The trim is appended to OpenSSL's Configure by scripts/openssl-src-perl.sh, which openssl-src runs through
//! OPENSSL_SRC_PERL (.cargo/config.toml [env]). It is lost silently when a caller exports its own
//! OPENSSL_SRC_PERL or bypasses the repo's cargo config, so this test fails then. It checks two things:
//! - what OpenSSL was configured with: the OPENSSL_NO_* macros of the linked build's configuration.h (build.rs);
//! - what the linked library actually provides: fetching SM4 / ARIA must fail (they live in the default provider
//!   when present), while AES-GCM / SHA-256 / SM3 must work, and the digests pgcrypto's digest() / hmac() can name
//!   (EVP_get_digestbyname) must still resolve.
//! Skipped for system-OpenSSL builds (DEP_OPENSSL_VENDORED != 1) and off Linux.

/// Disabled by scripts/openssl-src-perl.sh (and openssl-src's `no-dso` feature, set in this crate's Cargo.toml).
const TRIMMED: &[&str] = &[
    "OPENSSL_NO_QUIC",
    "OPENSSL_NO_SM2",
    "OPENSSL_NO_SM2_PRECOMP",
    "OPENSSL_NO_SM4",
    "OPENSSL_NO_ML_DSA",
    "OPENSSL_NO_SLH_DSA",
    "OPENSSL_NO_LMS",
    "OPENSSL_NO_ARIA",
    "OPENSSL_NO_BF",
    "OPENSSL_NO_CAST",
    "OPENSSL_NO_RC2",
    "OPENSSL_NO_RC4",
    "OPENSSL_NO_ENGINE",
    "OPENSSL_NO_DSO",
];

/// Kept on purpose: ML-KEM (OpenSSL >= 3.5's default X25519MLKEM768 TLS 1.3 key share), DES (encrypted PEM keys),
/// RIPEMD-160 (rust-openssl binds EVP_ripemd160 unconditionally), TLS itself, and every digest pgcrypto's digest() /
/// hmac() can name: SM3 hashes, MD4 / MDC2 / Whirlpool answer "Cipher cannot be initialized" (legacy provider not
/// loaded); without them both would answer "No such hash algorithm".
const KEPT: &[&str] = &[
    "OPENSSL_NO_ML_KEM",
    "OPENSSL_NO_DES",
    "OPENSSL_NO_RMD160",
    "OPENSSL_NO_TLS1_3",
    "OPENSSL_NO_SM3",
    "OPENSSL_NO_MD4",
    "OPENSSL_NO_MDC2",
    "OPENSSL_NO_WHIRLPOOL",
];

fn applies() -> bool {
    cfg!(target_os = "linux") && env!("PGRUST_OPENSSL_VENDORED") == "1"
}

#[test]
fn trim_applied_to_vendored_openssl() {
    if !applies() {
        eprintln!("skipped: not a vendored Linux OpenSSL build");
        return;
    }
    let disabled: Vec<&str> = env!("PGRUST_OPENSSL_DISABLED").split(',').filter(|s| !s.is_empty()).collect();
    let missing: Vec<&&str> = TRIMMED.iter().filter(|m| !disabled.contains(m)).collect();
    assert!(
        missing.is_empty(),
        "the vendored OpenSSL was built WITHOUT pgrust's trim (D4503): {missing:?} not disabled. Was OPENSSL_SRC_PERL \
         overridden, or the repo's .cargo/config.toml [env] bypassed? Rebuild openssl-sys (cargo clean -p openssl-sys)."
    );
    let dropped: Vec<&&str> = KEPT.iter().filter(|m| disabled.contains(m)).collect();
    assert!(dropped.is_empty(), "the vendored OpenSSL disables modules pgrust keeps (D4503): {dropped:?}");
}

#[cfg(pgrust_ossl3)]
#[test]
fn trimmed_algorithms_absent_from_the_linked_library() {
    if !applies() {
        eprintln!("skipped: not a vendored Linux OpenSSL build");
        return;
    }
    use openssl::cipher::Cipher;
    use openssl::md::Md;
    assert!(Cipher::fetch(None, "AES-128-GCM", None).is_ok(), "AES-128-GCM must be available");
    assert!(Md::fetch(None, "SHA256", None).is_ok(), "SHA-256 must be available");
    for name in ["SM4-CBC", "ARIA-128-CBC"] {
        assert!(Cipher::fetch(None, name, None).is_err(), "{name} is still in the linked OpenSSL (D4503 trim lost)");
    }
    assert!(Md::fetch(None, "SM3", None).is_ok(), "SM3 must be available (pgcrypto digest(.., 'sm3'))");
    // pgcrypto hashing.rs px_find_digest: a NULL here turns "Cipher cannot be initialized" into "No such hash algorithm"
    for name in ["sm3", "md4", "mdc2", "whirlpool", "ripemd160"] {
        let c = std::ffi::CString::new(name).unwrap();
        // SAFETY: c is NUL-terminated; the result is a static table entry OpenSSL owns (only compared with NULL).
        let md = unsafe { openssl_sys::EVP_get_digestbyname(c.as_ptr()) };
        assert!(!md.is_null(), "EVP_get_digestbyname({name}) is NULL: pgcrypto's digest()/hmac() result would change");
    }
}
