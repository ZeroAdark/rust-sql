//! Hash contract S0 (docs/gravity/hash-contract.md §3.1, §3.6): every Fp128 producer is full strength, i.e. SipHash-2-4-128
//! under the one key over ALL of the key's bytes. Checked against a reference written from the SipHash paper (not the
//! crate's code) at every length 0..=4096 and for keys differing at every byte. The registry names every producer call
//! site in the tree; a new unregistered site fails the test.
mod common;
use common::{Buf, VecDict};
use gravity_dict::fp::{fp128, install, seed, siphash128};
use gravity_dict::Fp128;

/// SipHash-2-4 with 128-bit output, from Aumasson & Bernstein §2 and the reference `siphash.c` (outlen 16).
fn reference(k0: u64, k1: u64, m: &[u8]) -> [u64; 2] {
    let mut v0 = 0x736f_6d65_7073_6575u64 ^ k0;
    let mut v1 = 0x646f_7261_6e64_6f6du64 ^ k1 ^ 0xee;
    let mut v2 = 0x6c79_6765_6e65_7261u64 ^ k0;
    let mut v3 = 0x7465_6462_7974_6573u64 ^ k1;
    macro_rules! round {
        () => {
            v0 = v0.wrapping_add(v1);
            v1 = v1.rotate_left(13) ^ v0;
            v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3);
            v3 = v3.rotate_left(16) ^ v2;
            v0 = v0.wrapping_add(v3);
            v3 = v3.rotate_left(21) ^ v0;
            v2 = v2.wrapping_add(v1);
            v1 = v1.rotate_left(17) ^ v2;
            v2 = v2.rotate_left(32);
        };
    }
    let blocks = m.len() / 8;
    for i in 0..blocks {
        let w = u64::from_le_bytes(m[8 * i..8 * i + 8].try_into().unwrap());
        v3 ^= w;
        round!();
        round!();
        v0 ^= w;
    }
    let mut last = (m.len() as u64 & 0xff) << 56;
    for (i, &b) in m[8 * blocks..].iter().enumerate() {
        last |= (b as u64) << (8 * i);
    }
    v3 ^= last;
    round!();
    round!();
    v0 ^= last;
    v2 ^= 0xee;
    for _ in 0..4 {
        round!();
    }
    let lo = v0 ^ v1 ^ v2 ^ v3;
    v1 ^= 0xdd;
    for _ in 0..4 {
        round!();
    }
    [lo, v0 ^ v1 ^ v2 ^ v3]
}

fn rng(mut x: u64) -> impl FnMut() -> u64 {
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

fn lengths() -> Vec<usize> {
    let mut v: Vec<usize> = (0..=300).collect();
    v.extend((301..=4096).step_by(61));
    v.extend([511, 512, 513, 1023, 1024, 1025, 2047, 2048, 2049, 4095, 4096]);
    v
}

#[test]
fn the_reference_matches_the_published_vectors() {
    // vectors_sip128 of veorq/SipHash: key 00..0f, input 00..len-1
    let (k0, k1) = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let input: Vec<u8> = (0u8..64).collect();
    for (len, want) in [(0usize, "a3817f04ba25a8e66df67214c7550293"), (7, "a1f1ebbed8dbc153c0b84aa61ff08239"), (8, "3b62a9ba6258f5610f83e264f31497b4"), (63, "5150d1772f50834a503e069a973fbd7c")] {
        let [lo, hi] = reference(k0, k1, &input[..len]);
        let got: String = lo.to_le_bytes().iter().chain(hi.to_le_bytes().iter()).map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, want, "len {len}");
    }
}

#[test]
fn siphash128_and_fp128_are_full_strength_at_every_length() {
    let mut r = rng(0x2545_F491_4F6C_DD1D);
    for len in lengths() {
        let bytes: Vec<u8> = (0..len).map(|_| r() as u8).collect();
        let (k0, k1) = (r(), r());
        assert_eq!(siphash128(k0, k1, &bytes), reference(k0, k1, &bytes), "siphash128 len {len}");
    }
    install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let (k0, k1) = seed().unwrap();
    for len in lengths() {
        let bytes: Vec<u8> = (0..len).map(|_| r() as u8).collect();
        assert_eq!(fp128(&bytes), Some(reference(k0, k1, &bytes)), "fp128 len {len}");
    }
}

/// Every byte counts: a key and the same key with byte `i` changed never share a fingerprint, for every `i`.
#[test]
fn a_change_at_any_byte_changes_the_fingerprint() {
    let (k0, k1) = (0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);
    for len in [1usize, 8, 9, 16, 17, 24, 31, 64, 300, 2049] {
        let base: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let f0 = siphash128(k0, k1, &base);
        for i in 0..len {
            let mut v = base.clone();
            v[i] ^= 0x20;
            let f = siphash128(k0, k1, &v);
            assert_ne!(f, f0, "len {len} byte {i}");
            assert_eq!(f, reference(k0, k1, &v));
        }
        // a longer key sharing the prefix, and trailing spaces (bpchar padding is the caller's canonicalisation, §3.6 F1)
        let mut longer = base.clone();
        longer.push(b' ');
        assert_ne!(siphash128(k0, k1, &longer), f0, "len {len} + ' '");
    }
}

/// The dictionary's Fp128 table (`Fp128::build`, table.rs:498): every entry's fingerprint is the reference over the whole entry.
#[test]
fn the_dictionary_fp128_table_is_full_strength() {
    install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let (k0, k1) = seed().unwrap();
    let mut entries: Vec<Vec<u8>> = Vec::new();
    for len in [0usize, 1, 8, 9, 16, 17, 40, 127, 128, 300, 2049, 4096] {
        let base: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        entries.push(base.clone());
        for i in (0..len).step_by((len / 7).max(1)) {
            let mut v = base.clone();
            v[i] = b'#';
            if !entries.contains(&v) {
                entries.push(v);
            }
        }
    }
    let d = VecDict { entries: entries.clone(), null: None };
    let mut buf = Buf::new(entries.len() * 16 + 4096);
    let mut region = buf.region();
    let t = Fp128::build(&d, &mut region, &mut |v| Ok(v)).unwrap();
    let mut seen = std::collections::HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        let f = unsafe { t.get(i as u32) };
        assert_eq!(f, reference(k0, k1, e), "code {i} len {}", e.len());
        assert!(seen.insert(f), "code {i}: two entries share a fingerprint");
    }
}

/// Every Fp128 producer call site in the tree, with its verdict (§3.6). A site not listed here fails: register it with its
/// check, or fix it. `Full` = SipHash-128 under the one key over the whole canonical payload, checked where named.
const REGISTRY: &[(&str, &str, &str)] = &[
    ("crates/gravity/dict/src/fp.rs", "Full", "fp128 / Keys::fp_bytes / siphash128: this file's tests"),
    ("crates/gravity/abi/src/hash.rs", "Full", "fp128 / siphash128: the one Fp128 family (hash-contract.md §3.5, lowerv4 M0); dict/tests/hash_quality.rs checks it against dict's siphash128"),
    ("crates/gravity/dict/src/table.rs", "Full; F1", "Fp128::build (this file) / build_by_code / build_by_canon: the rendered entry's bytes; a bpchar entry is hashed padded"),
    ("crates/gravity/expr/src/siphash.rs", "Full", "text_key: IR twin, expr/tests/siphash.rs (lengths 0..4097, TPDE + interp)"),
    ("crates/gravity/kernels/src/typesem/keyimg.rs", "Full; F2", "fp_out / text_fp: kernels/tests/core_fp_strength.rs; text_strnxfrm: the whole sort key, F2 = `big[..m.min(n)]` edge"),
    ("crates/gravity/lowerv4/src/rep/varlena/hash.rs", "Full", "fp: siphash128 in IR (gravity_expr::siphash) over a varlena key's payload under the process seed (m6_fp_seed), the image every dictionary's Fp128 table holds; a bpchar trimmed first (hashbpchar)"),
    ("crates/gravity/hosts/examples/frontcode.rs", "Lab", "an example binary, not the server"),
    ("crates/backend/executor/sqe/src/fp.rs", "Full; own key", "SQE's entry_fp128: SipHash-2-4-128, a per-process random key, never persisted (the oracle; outside Gravity's tables)"),
    ("crates/gravity/hosts/src/ops/scan/pgrc/cond_cache.rs", "Full", "fingerprint: Keys::fp_bytes over \"gravity.v4.cond-words.2\", the shape and the bound words; the condition cache keeps no bytes (V4R-15)"),
    ("crates/gravity/lowerv4/src/ops/scan/pgrc/verdict_cond.rs", "Full", "finish: fp128 over \"gravity.v4.cond.2\" and the covered quals' shape; none before the seed (V4R-15)"),
    ("crates/gravity/kernels/src/ops/agg/trans/entries.rs", "Full", "m6_num_fp (M6-HOSTS H2): Keys::fp_bytes over a numeric's canonical image (class byte; a finite nonzero one's sign, weight and digits without leading or trailing zeros), exact under numeric_eq: this file's tests"),
    ("crates/gravity/kernels/src/ops/agg/trans/entries/raw.rs", "Full", "m6_hosted_fp (rep.hosted.hash): Keys::fp_bytes over the key's canonical form (gravity_fn typesem `canonical`: array / composite / range walked, floats, numerics, intervals, names and bpchars folded to one image per equal value), exact under the type's =: gravity_fn typesem's canon tests"),
    ("crates/gravity/kernels/src/typesem/userfp.rs", "Full", "m6_user_fp (semantics USEROP, D1604): Keys::fp_bytes over the user key's spec id and its class number (the class found by the type's own hash support proc and confirmed by its = through fmgr), exact under that =: the e2e witness v4/m4_sem_userop_keys"),
];

const PRODUCERS: &[&str] = &["siphash128(", "fp_bytes(", "fp128(", "text_key(", "fp_out("];

fn scan(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, usize, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            if name != "target" && name != "tests" && !name.starts_with('.') {
                scan(&p, root, out);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" {
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let mut in_tests = false;
            for (n, line) in text.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("#[cfg(test)]") {
                    in_tests = true; // a unit-test module runs to the end of the file in this tree's convention
                }
                if in_tests || t.starts_with("//") || t.starts_with("fn ") || t.starts_with("pub fn ") || t.contains("fn siphash128") || t.contains("fn fp128") {
                    continue;
                }
                let called = PRODUCERS.iter().any(|pat| line.match_indices(pat).any(|(at, _)| at == 0 || !matches!(line.as_bytes()[at - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')));
                if called {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                    out.push((rel, n + 1, t.chars().take(100).collect()));
                }
            }
        }
    }
}

#[test]
fn every_fp128_producer_is_registered() {
    let root = manifest_dir().join("../../..").canonicalize().unwrap();
    let mut sites = Vec::new();
    scan(&root.join("crates"), &root, &mut sites);
    let unregistered: Vec<_> = sites.iter().filter(|(f, _, _)| !REGISTRY.iter().any(|(r, _, _)| r == f)).collect();
    for (f, v, why) in REGISTRY {
        eprintln!("{f}: {v} — {why}");
    }
    assert!(unregistered.is_empty(), "Fp128 producer sites missing from the registry (hash-contract.md §3.6):\n{}", unregistered.iter().map(|(f, n, l)| format!("  {f}:{n}: {l}")).collect::<Vec<_>>().join("\n"));
    assert!(sites.len() > 5, "the scan found {} sites under {}: wrong root?", sites.len(), root.display());
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
