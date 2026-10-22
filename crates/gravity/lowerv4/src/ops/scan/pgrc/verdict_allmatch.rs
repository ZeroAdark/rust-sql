//! AllMatch skips the covered steps (H02d; KEEP lowerv2 source/range.rs next_window's all-match fold and compose.rs's
//! unswitch, v3-r1-a4b's `zone_covered`): a leading filter op is covered when each of its quals is an exact zone predicate
//! the site declares (verdict_zone.rs), every earlier op covered too. A window whose zone verdict is `AllMatch` (the host
//! sets `VERDICT_ALL_MATCH` in every lane's flags only when every registered predicate holds for every row, nulls counted,
//! `granule_source::zone_verdict`) runs the frame's covered body with those steps elided (`WinConds::covered`,
//! `SourceMech::covers`). Deleted rows are already out of the window's selection, so the verdict and the delete vector
//! compose. Registers road `verdict.allmatch`. Owner M1-SOURCE.
use gravity_abi::ctx::lane;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::SiteDesc;

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// AllMatch skips covered steps.
    VERDICT_ALLMATCH = 12, "verdict.allmatch", PgrcFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_ALLMATCH, &VerdictImpl(&AllMatch))], ..crate::ops::NONE };

struct AllMatch;

impl VerdictMech for AllMatch {
    /// `true` when the zone verdict `AllMatch` means `qual` holds for every row: its exact predicate is on the site.
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool {
        crate::ops::scan::pgrc::verdict_zone::of(px, site, qual).is_some_and(|(z, exact)| exact && site.zone_preds().contains(&z))
    }
}

/// The window's zone verdict is `AllMatch`: lane `k`'s verdict bits (every lane of the window carries the same bits).
pub(crate) fn covered(b: &mut FunctionBuilder<'_>, lanes: Value, k: usize) -> Value {
    let f = b.load(Type::I8, lanes, (k * lane::SIZE + lane::FLAGS) as i32);
    let m = b.iconst(Type::I8, lane::VERDICT_MASK as i64);
    let v = b.int_bin(IntBinOp::And, f, m);
    let all = b.iconst(Type::I8, lane::VERDICT_ALL_MATCH as i64);
    b.icmp(IntCond::Eq, v, all)
}
