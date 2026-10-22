// initdb drives native processes and the unix filesystem (termios, getaddrinfo,
// PermissionsExt, locale categories) that wasm32-wasip1 does not offer; the wasm
// build (wasm/wasm-build.sh) gets a stub entry point so the workspace member
// still compiles there (wasm/wasm-crate-ledger.md is ratchet-only), the way
// C's initdb would be absent from a wasm port.
#[cfg(not(target_family = "wasm"))]
mod app;

#[cfg(not(target_family = "wasm"))]
fn main() {
    app::main()
}

#[cfg(target_family = "wasm")]
fn main() {
    eprintln!("initdb: not available on wasm32-wasip1");
    std::process::exit(1);
}
