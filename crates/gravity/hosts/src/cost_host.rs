//! The host's cost profile at server start (`cost_profile::anchor`): the postmaster reads this host's anchors from its
//! cache file, else measures them (tens of ms) and keeps them there, and installs the profile before any backend exists,
//! so every backend prices with it. `PGRUST_COST_PROFILE` picks it (`anchor::word`): `reference` keeps the reference
//! machine's prices and cache facts (the test rigs and the A/B kits, so a pin or an arm never depends on the host or on its
//! noise), `host` the host's; unset is `host` (a debug build: `reference`, its units run unoptimized).
//!
//! The cache lives outside the data directory, one file a host key (`anchor::host_key`: the CPU, its caches and the
//! anchor set's version): `PGRUST_COST_CACHE_DIR`, else `$XDG_CACHE_HOME/pgrust/cost-profile`, else
//! `$HOME/.cache/pgrust/cost-profile`. A file is written under a temporary name and renamed, so two servers starting at
//! once on one host each read whole anchors. With none of those directories writable (a service account without a home)
//! the server measures at each start; the log says so once, and nothing here fails the start.

use cost_profile::anchor::{self, Anchor, Measured, Profile, Source, Word};

/// The takes of each unit; its least counts (a loaded host reads as an idle one).
const TAKES: usize = 5;

/// Install this process's profile (the postmaster, at init) and log it. Idempotent.
pub fn install() {
    if anchor::installed() {
        return;
    }
    let p = profile();
    log(&p.describe());
    if matches!(p.source, Source::Measured { kept: None, .. }) {
        log("cost profile: no writable cache directory (PGRUST_COST_CACHE_DIR, XDG_CACHE_HOME or HOME), so the anchors are measured at each start");
    }
    anchor::install(p);
}

fn log(msg: &str) {
    let _ = elog::ereport(types_error::LOG).errmsg(msg.to_string()).finish(types_error::ErrorLocation::new("gravity", 0, "cost_host")); // alloc: Cold
}

fn profile() -> Profile {
    if anchor::word_unknown() {
        log("cost profile: PGRUST_COST_PROFILE is neither reference nor host; the default applies");
    }
    if let Word::Reference(why) = anchor::word() {
        return Profile::reference(why);
    }
    let (m, l2_read, l1d) = cost_profile::machine::sysfs();
    let key = anchor::host_key(std::env::consts::ARCH, &cpu_id(), l1d.unwrap_or(0), if l2_read { m.l2 } else { 0 }, m.smt);
    let path = cache_dir().map(|d| format!("{d}/{key}.tsv")); // alloc: Cold
    if let Some(Ok(b)) = path.as_deref().map(fd::sync::read_whole_file) {
        if let Some(measured) = std::str::from_utf8(&b).ok().and_then(|t| anchor::parse_cache(t, &key)) {
            return Profile { key, ratios: measured.ratios(), measured: Some(measured), source: Source::Cached(path.unwrap_or_default()) };
        }
    }
    let t0 = pg_clock::mono_ns();
    let measured = measure();
    let micros = pg_clock::mono_ns().saturating_sub(t0) / 1000;
    let kept = path.filter(|p| keep(p, &anchor::cache_text(&key, &measured)));
    Profile { key, ratios: measured.ratios(), measured: Some(measured), source: Source::Measured { micros, kept } }
}

/// The cache directory: an absolute `PGRUST_COST_CACHE_DIR`, else under an absolute `XDG_CACHE_HOME` or `HOME`.
fn cache_dir() -> Option<String> {
    let abs = |v: Option<String>| v.map(|s| s.trim_end_matches('/').to_string()).filter(|s| s.starts_with('/')); // alloc: Cold
    abs(gravity_driver::env::text("PGRUST_COST_CACHE_DIR"))
        .or_else(|| abs(gravity_driver::env::text("XDG_CACHE_HOME")).map(|d| format!("{d}/pgrust/cost-profile"))) // alloc: Cold
        .or_else(|| abs(gravity_driver::env::text("HOME")).map(|d| format!("{d}/.cache/pgrust/cost-profile"))) // alloc: Cold
}

/// `text` written to `path` under a temporary name and renamed; false when any step fails.
fn keep(path: &str, text: &str) -> bool {
    let Some((dir, _)) = path.rsplit_once('/') else { return false };
    if fd::copydir::pg_mkdir_p(dir).is_err() {
        return false;
    }
    let tmp = format!("{path}.{}.tmp", init_small::globals::process_id()); // alloc: Cold
    if fd::sync::write_whole_file(&tmp, text.as_bytes(), false).is_err() || fd::sync::pg_rename(&tmp, path) != 0 {
        let _ = fd::sync::pg_unlink(&tmp);
        return false;
    }
    true
}

/// The CPU's id: aarch64's MIDR (implementer, part, revision), x86's vendor / family / model (sysfs `modalias`);
/// "unknown" where neither reads.
fn cpu_id() -> String {
    let read = |p: &str| fd::sync::read_whole_file(p).ok().and_then(|b| String::from_utf8(b).ok()); // alloc: Cold
    if let Some(midr) = read("/sys/devices/system/cpu/cpu0/regs/identification/midr_el1") {
        return midr.trim().to_string(); // alloc: Cold
    }
    // "cpu:type:x86,ven0002fam0019mod0001:feature:,..."
    if let Some(alias) = read("/sys/devices/system/cpu/modalias") {
        if let Some(id) = alias.split(':').find(|f| f.contains("ven") && f.contains("fam")) {
            return id.to_string(); // alloc: Cold
        }
    }
    "unknown".to_string() // alloc: Cold
}

/// Each unit's least ns an item over `TAKES` takes, in this thread's CPU time, after an untimed take that warms its data.
fn measure() -> Measured {
    let mut d = anchor::Data::new();
    let mut ns = [f64::INFINITY; 6];
    for a in Anchor::ALL {
        std::hint::black_box(d.run(a));
        for _ in 0..TAKES {
            let t0 = pg_clock::posix::thread_cpu_ns();
            let (n, v) = d.run(a);
            let t = pg_clock::posix::thread_cpu_ns().saturating_sub(t0);
            std::hint::black_box(v);
            ns[a as usize] = ns[a as usize].min(t as f64 / n as f64);
        }
    }
    Measured(ns)
}
