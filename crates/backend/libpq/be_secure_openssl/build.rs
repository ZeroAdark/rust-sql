// D4503 tripwire input. openssl-sys (`links = "openssl"`) hands its direct dependents DEP_OPENSSL_INCLUDE,
// DEP_OPENSSL_VENDORED and DEP_OPENSSL_VERSION_NUMBER. Read the OPENSSL_NO_* macros the linked OpenSSL was
// configured with (include/openssl/configuration.h) and pass them to the crate's tests, which check them against
// the trim scripts/openssl-src-perl.sh appends to the vendored build (src/openssl_trim.rs).
fn main() {
    println!("cargo:rerun-if-env-changed=DEP_OPENSSL_INCLUDE");
    println!("cargo:rerun-if-env-changed=DEP_OPENSSL_VENDORED");
    println!("cargo:rerun-if-env-changed=DEP_OPENSSL_VERSION_NUMBER");
    println!("cargo:rustc-check-cfg=cfg(pgrust_ossl3)");

    let vendored = std::env::var("DEP_OPENSSL_VENDORED").as_deref() == Ok("1");
    let mut disabled: Vec<String> = Vec::new();
    if let Some(inc) = std::env::var_os("DEP_OPENSSL_INCLUDE") {
        for dir in std::env::split_paths(&inc) {
            let conf = dir.join("openssl").join("configuration.h");
            println!("cargo:rerun-if-changed={}", conf.display());
            let Ok(text) = std::fs::read_to_string(&conf) else { continue };
            for line in text.lines() {
                // "#  define OPENSSL_NO_QUIC" (any spacing after the '#')
                let rest = line.trim_start().trim_start_matches('#').trim_start();
                if let Some(name) = rest.strip_prefix("define ") {
                    let name = name.split_whitespace().next().unwrap_or("");
                    if name.starts_with("OPENSSL_NO_") && !disabled.iter().any(|d| d == name) {
                        disabled.push(name.to_string());
                    }
                }
            }
        }
    }
    let ossl3 = std::env::var("DEP_OPENSSL_VERSION_NUMBER")
        .ok()
        .and_then(|v| u64::from_str_radix(&v, 16).ok())
        .is_some_and(|v| v >= 0x3_00_00_00_0);
    if ossl3 {
        println!("cargo:rustc-cfg=pgrust_ossl3");
    }
    println!("cargo:rustc-env=PGRUST_OPENSSL_VENDORED={}", if vendored { "1" } else { "0" });
    println!("cargo:rustc-env=PGRUST_OPENSSL_DISABLED={}", disabled.join(","));
}
