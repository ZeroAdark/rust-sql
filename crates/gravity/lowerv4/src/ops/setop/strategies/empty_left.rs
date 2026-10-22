//! Road `setop.emptyleft` (H29, R9-L09): PG's hashed SetOp fills its table from the left side first and never runs the
//! right side when the left is empty (nodeSetOp.c setop_fill_hash_table: the inner plan is not read after an empty outer),
//! so a volatile call there never happens and an error there is never raised. Budget 160 lines (files.tsv, +25 % gate).
//! Owner M9-SETOP.
//!
//! The right side is the pipelines after the left group's producer, up to the right group's producer (`(left, right]`,
//! in program order). Protocol `setop.setop`'s skip form (exec v4 m9.rs; no words) runs after the left producer: an empty
//! left region (no group; a spilled side is never read as empty) runs those pipelines as if no claim reached them
//! (`Exec::run_empty_words`, D276's mechanism), so the emit reads an empty right group. A right pipeline another pipeline
//! outside the side reads is not skipped: it runs as the straight form would. D4050: a side past the 64th pipeline (a left
//! of 70 UNION ALL arms) is skipped as any other (the mask was one u64 once, and ran that side's raise). A plain left side
//! (no column, F-ZERO) is empty when its one record counts no row.
use gravity_program::protos::{Proto, ProtoKind};

use crate::face::step::Rescan;
use crate::frame::px::Px;

crate::strategies! {
    /// Empty left input short-circuits the right.
    SETOP_EMPTYLEFT = 105, "setop.emptyleft", SetopAny;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(SETOP_EMPTYLEFT, &())], ..crate::ops::NONE };

/// The skip protocol of the set operation over left producer `lp` and right producer `rp`, when the road is on: it rides
/// `lp` and names `rp` as its peer, both of which frame/program.rs `to_program` maps to program order; exec v4 m9.rs
/// `skip_mask` picks the right side's pipelines from the program there (D1274: a mask of graph indices in `arg` named the
/// wrong pipelines once a pass reordered them). `arg`'s bits 48..64: 0 over an owned (keyed) left table, else
/// the plain left record's sentinel byte offset + 1 (F-ZERO).
pub(crate) fn declare(px: &mut Px<'_, '_>, lp: usize, rp: usize, left: u8) {
    declare_with(px, lp, rp, left, 0)
}

/// `declare` over a plain left side whose sentinel is at byte `sentinel` of its record.
pub(crate) fn declare_plain(px: &mut Px<'_, '_>, lp: usize, rp: usize, left: u8, sentinel: u64) {
    declare_with(px, lp, rp, left, sentinel + 1)
}

fn declare_with(px: &mut Px<'_, '_>, lp: usize, rp: usize, left: u8, plain: u64) {
    if Rescan::SkipEmpty.on() && rp > lp && plain <= 0xffff {
        fires::fire("setop.emptyleft"); // shapecheck fire signal (REGISTRY NEW-lowerv4-a2-38)
        px.declare_proto(Proto::new(ProtoKind::SetOp, lp as u16).peer(rp as u16).slot(left).arg(plain << SKIP_PLAIN_SHIFT));
    }
}

/// exec v4 m9.rs's `SKIP_PLAIN_SHIFT`.
pub const SKIP_PLAIN_SHIFT: u32 = 48;
