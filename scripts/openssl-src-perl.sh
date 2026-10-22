#!/bin/sh
# openssl-src-perl.sh: the OPENSSL_SRC_PERL hook (.cargo/config.toml [env]; D4503).
#
# openssl-src (the `vendored` OpenSSL build behind openssl-sys) runs
#     "$OPENSSL_SRC_PERL" ./Configure <its own fixed options>
# and has no other way to take Configure options. For that one call this wrapper appends the
# OpenSSL modules a PostgreSQL server never uses; for any other call it is plain perl.
#
# Removed: QUIC; SM2 (+ its 512 KiB precomputed table) and SM4; the post-quantum SIGNATURE schemes ML-DSA,
# SLH-DSA, LMS (used only with post-quantum certificates); ARIA; the legacy-provider ciphers Blowfish, CAST5,
# RC2, RC4 (unusable here anyway: pgrust never loads OpenSSL's legacy provider; pgcrypto's Blowfish / CAST5 /
# DES are Rust crates); engines.
# Kept on purpose: ML-KEM (OpenSSL >= 3.5 offers the X25519MLKEM768 TLS 1.3 key share by default);
# DES / 3DES (encrypted PEM private keys); RIPEMD-160 (rust-openssl binds EVP_ripemd160 unconditionally);
# SM3, MD4, MDC2, Whirlpool: pgcrypto's digest() / hmac() look any digest up by name (EVP_get_digestbyname,
# as C pgcrypto does), so SM3 hashes and MD4 / MDC2 / Whirlpool fail with "Cipher cannot be initialized"
# (legacy provider); removing them would change those results to "No such hash algorithm".
#
# Never silently untrimmed: no perl -> hard error; the Configure call prints what it appended to
# stderr (target/<profile>/build/openssl-sys-*/stderr). Tripwire: be_secure_openssl's
# src/openssl_trim.rs tests fail a Linux build whose vendored OpenSSL lacks this trim.
set -eu
TRIM="no-quic no-sm2 no-sm2-precomp no-sm4 no-ml-dsa no-slh-dsa no-lms no-aria no-bf no-cast no-rc2 no-rc4 no-engine"
perl_bin=${PERL:-perl}
case "$perl_bin" in *openssl-src-perl.sh) perl_bin=perl ;; esac
if ! command -v "$perl_bin" >/dev/null 2>&1; then
    echo "openssl-src-perl.sh: no perl ('$perl_bin') found; refusing to configure OpenSSL untrimmed (D4503)" >&2
    exit 127
fi
if [ "${1:-}" = "./Configure" ]; then
    # shellcheck disable=SC2086 # TRIM is a word list on purpose
    set -- "$@" $TRIM
    echo "openssl-src-perl.sh: pgrust OpenSSL trim (D4503) appended to Configure: $TRIM" >&2
fi
exec "$perl_bin" "$@"
