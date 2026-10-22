//! Partition hash at the run's open. Hooks / levers: H21 R3-L14. Budget 160 lines (files.tsv, +25 % gate). Owner
//! M1-TABLES.
//!
//! The run carry takes the image's KeyHash when a run opens and carries it to the close (the table's upsert, its home and
//! route), so the hash chain overlaps the run's rows and only the image words and the row index stay live across the
//! compare (the kernel's `hand.rs` pair loop). An AB road: forced off (`pgrust.gravity_road_off = hash.runopen`), the close
//! hashes the carried image instead.
//!
//! Registers: road `hash.runopen` (`HASH_RUNOPEN`).
use gravity_expr::emit::hash::emit_key_hash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::table::keyimg::{ImageForm, ImageWords};

crate::strategies! {
    /// Partition hash at the run's open.
    HASH_RUNOPEN = 43, "hash.runopen", Keyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(HASH_RUNOPEN, &())], ..crate::ops::NONE };

/// The hash a run opening at `img` carries: its KeyHash with the road on, else `zero` (the close hashes).
pub(crate) fn at_open(b: &mut FunctionBuilder<'_>, img: &ImageWords, zero: Value) -> Value {
    if ImageForm::RunOpen.on() { emit_key_hash(b, &img.words) } else { zero }
}

/// The hash a run closing at `img` folds with: the one it carried from its open, or its image's taken now.
pub(crate) fn at_close(b: &mut FunctionBuilder<'_>, img: &ImageWords, carried: Value) -> Value {
    if ImageForm::RunOpen.on() { carried } else { emit_key_hash(b, &img.words) }
}
