//! Verdicts against runtime constants (H02h, `verdict.rtconst`; R5-L04 at M1, R4-L04 at M3, R11-L10 at M11): a pgrc
//! site skips the granules a constant set during execution proves empty. At M1 the one such constant is a finished hash
//! build's key range / key set (face/consts.rs `RtConst::BuildSet`): join/rtfilter.rs declares `ProtoKind::RtFilter` on
//! the probing scan's site, exec's RtFilter protocol arms the site's verdict cell with the build's keys after the build
//! pipeline, and the granule host skips a granule whose key column's zone no build key can match, by measurement (hosts
//! `rt_arm` / `rt_cuts`; a skip never changes rows). The road's gate on that filter is ops/shared/scan.rs `accepts`: the probe
//! must drop every row without a match (inner, semi, and the nullable probe of a right join or its semi / anti forms),
//! never a preserved probe (left, full, anti), which would lose its NULL-extended rows. No filter qual takes a runtime
//! constant here at M1 (the top-k bound is M3's `topk.boundfirst`, an InitPlan value M8's `nest.eagersafe`), so
//! `declare` takes none. Registers road `verdict.rtconst`. Owner M1-SOURCE.
use gravity_program::SiteDesc;

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// Verdicts against runtime constants.
    VERDICT_RTCONST = 15, "verdict.rtconst", PgrcJoinOrBound;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_RTCONST, &VerdictImpl(&RtConst))], ..crate::ops::NONE };

struct RtConst;

impl VerdictMech for RtConst {
    /// No leading filter qual reads a runtime constant at M1: the build's filter is taken by the site as a whole.
    fn declare(&self, _px: &mut Px<'_, '_>, _site: &mut SiteDesc, _qual: ExId) -> bool {
        false
    }
}
