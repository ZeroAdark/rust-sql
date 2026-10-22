//! The PSMA candidate row window (H02c, an A/B road; KEEP Gravity v1's `psma_window`): the granule host narrows a mixed
//! granule to the rows its lane's PSMA block leaves as candidates for an exact comparison predicate (`=`, `<`, `<=`, `>`,
//! `>=`; never `<>` or `IS NOT NULL`), publishing the window's rows outside it as unselected (hosts
//! `granule_source::psma_rows`, the intersection over the site's predicates). A row outside the window fails its
//! predicate, which is the qual itself (verdict_zone.rs declares only exact predicates), so the filter's answer is
//! unchanged. P5-13: a memcmp-ordered fixed lane's predicate on its order key (verdict_zone.rs `pred_of`) as well: uuid's
//! Coarse 8-byte prefix key bounds the candidates without a strict test (`key(x) = key(c)` for every `x = c`), macaddr's
//! Exact key as a word's. Registers road `verdict.psma`. Owner M1-SOURCE.
use gravity_program::{Rep, SiteDesc, ZoneOp, ZonePred};

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// PSMA candidate row window.
    VERDICT_PSMA = 11, "verdict.psma", PgrcFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_PSMA, &VerdictImpl(&Psma))], ..crate::ops::NONE };

struct Psma;

impl VerdictMech for Psma {
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool {
        // P5-13: a memcmp-ordered fixed lane's order-key predicate too (its PSMA is over the same keys: uuid's Coarse prefix,
        // read never strict; macaddr's Exact image), never a text prefix predicate
        let keyed = |z: &ZonePred| !z.prefix || matches!(site.lanes()[z.lane as usize].rep, Rep::FixedPtr { .. });
        crate::ops::scan::pgrc::verdict_zone::of(px, site, qual).is_some_and(|(z, exact)| exact && keyed(&z) && !matches!(z.op, ZoneOp::Ne | ZoneOp::NotNull | ZoneOp::IsNull) && site.zone_preds().contains(&z))
    }
}
