//! The exec crate's knobs (docs/gravity/breakers.md "Knobs"): one file per feature, each owning its GUC / env rows and their readers; this table is the census a rig checks every literal against.


pub mod diag;
pub mod dump;
pub mod feedback_file;
pub mod grant;
pub mod lowerer;
pub mod perfmap;
pub mod room;
pub mod seam;
pub mod tier0;
pub mod width;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A process environment variable, read once (`PGRUST_GRAVITY_*`): a measurement lever, never product surface.
    Env,
    /// A placeholder custom option (`SET` / postgresql.conf, `pgrust.gravity_*`), read through `custom_guc_u64` / `custom_guc_text` where used.
    Custom,
    /// A GUC registered in `guc_tables` (SHOW-visible, typed): the row here names it; its definition stays in guc_tables.
    Builtin,
}

pub struct Knob {
    pub name: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    pub doc: &'static str,
}

/// Every feature file's rows, in file order.
pub const TABLES: &[&[Knob]] = &[diag::KNOBS, dump::KNOBS, grant::KNOBS, lowerer::KNOBS, perfmap::KNOBS, room::KNOBS, seam::KNOBS, tier0::KNOBS, width::KNOBS];

pub fn table() -> impl Iterator<Item = &'static Knob> {
    TABLES.iter().flat_map(|t| t.iter())
}

pub fn find(name: &str) -> Option<&'static Knob> {
    table().find(|k| k.name == name)
}

/// A placeholder custom option as a number (`gravity_hosts::inputs::custom_guc_u64`), `None` when unset or unparsable.
pub fn custom_u64(name: &str) -> Option<u64> {
    gravity_hosts::inputs::custom_guc_u64(name)
}

/// An env switch's `1`-ness: set, non-empty and not `0`.
pub fn env_on(name: &str) -> bool {
    env(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// An env knob as a number (`PGRUST_GRAVITY_MAX_CONCURRENT`), `None` when unset or unparsable.
pub fn env_u64(name: &str) -> Option<u64> {
    env(name)?.to_str()?.trim().parse().ok()
}

/// The one raw read behind the `PGRUST_GRAVITY_*` env knobs (callers cache it or read it once).
fn env(name: &str) -> Option<std::ffi::OsString> {
    gravity_driver::env::os(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
    /// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
    fn manifest_dir() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
    }

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    fn literals(text: &str, prefix: &str, tail: impl Fn(char) -> bool) -> Vec<String> {
        let mut out = Vec::new(); // alloc: Cold
        let mut rest = text;
        while let Some(i) = rest.find(prefix) {
            let s = &rest[i..];
            let n = s.chars().take_while(|&c| tail(c)).count();
            out.push(s[..n].to_string()); // alloc: Cold
            rest = &s[n..];
        }
        out
    }

    /// Every `pgrust.gravity_*` and `PGRUST_GRAVITY_*` literal in the crate's sources has a row here (the collision the split removes comes back otherwise).
    #[test]
    fn every_knob_literal_in_exec_has_a_table_row() {
        let mut files = Vec::new(); // alloc: Cold
        rust_files(&manifest_dir().join("src"), &mut files);
        let mut missing = Vec::new(); // alloc: Cold
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let mut names = literals(&text, "pgrust.gravity_", |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
            names.extend(literals(&text, "PGRUST_GRAVITY_", |c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
            for n in names {
                let n = n.trim_end_matches('_');
                let row = format!("{}: {n}", f.display()); // alloc: Cold
                if n.len() > "PGRUST_GRAVITY_".len() && find(n).is_none() && !missing.contains(&row) {
                    missing.push(row);
                }
            }
        }
        assert!(missing.is_empty(), "knob literals without a knobs::KNOBS row (each goes in its feature file's KNOBS, TABLES above; an env knob too, D618):\n{}", missing.join("\n"));
        let mut names: Vec<&str> = table().map(|k| k.name).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(n, names.len(), "a knob named twice");
    }
}
