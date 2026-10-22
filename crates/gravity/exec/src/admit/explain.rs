//! M12 (EXPLAIN ANALYZE counters): this milestone's slots in the runtime protocol registry (ops/mod.rs `RUNGS`,
//! `PROTOCOLS`, append-only), empty. Its counted-kind lists (D813 / D1536 / D1537: which served plans an EXPLAIN ANALYZE
//! could print, the rest delegated by name) are gone: every EXPLAIN ANALYZE runs the statement's plain program
//! and prints what that run counts exactly (entry instrument.rs, docs/gravity/entry.md §4). The module goes when the
//! ENGINE report does.
use crate::admit::{Admit, Bind};

pub const ADMIT: &[Admit] = &[];
pub const BIND: &[Bind] = &[];
pub const PROTOCOLS: &[&dyn crate::ops::Protocol] = &[];
