//! `ops::shared::object`: the runtime objects' ABI (M8): what every nest family's protocol lays into a kernel word
//! and its lowered IR calls through, the kinds, op codes and cells (entries.rs) and the host entries with the object
//! `Head` they call through (raw.rs). A home: it names no family.
pub mod entries;
pub mod raw;
