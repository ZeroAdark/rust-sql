//! Bloom `NoMatch` on equality (H02b; KEEP Gravity v1's AM consult, granule.rs zone_verdict's bloom arm): the granule host
//! proves an equality predicate's constant absent from a granule by its lane's bloom block and skips the granule unread
//! (hosts `granule_source::bloom_absent`, through the lane's profile, so only lanes whose byte equality is value equality
//! answer). The predicate is the site's exact zone `Eq` predicate (verdict_zone.rs); this road declares that the site
//! takes the bloom verdict for it (`VERDICT_BLOOM` in the `src.verdicts` protocol). An IN list over a by-value lane
//! (verdict_zone.rs `list_of`) has its members as the lane's `InMember` predicates, all of them or none (a member left
//! out could be the one present), added per execution from the array's word (NEW-72): the host skips a granule whose
//! bloom lacks every member. P5-11: a text `=` under a deterministic collation declares its constant's probe word
//! (`HashEq`, the writer's hash of the payload: verdict_zone.rs `hash_of`), which the host probes as the bytes; P5-13 the
//! same for a memcmp-ordered fixed lane's `=` (uuid, macaddr, macaddr8: the hash of the image). A text or fixed-image IN
//! list (verdict_zone.rs `hash_list_of`) has its members' probe words as the lane's `HashIn` predicates, added per
//! execution from the array's word (NEW-72): a list is never baked into the program. Registers road `verdict.bloom`.
//! Owner M1-SOURCE.
use gravity_program::{SiteDesc, ZoneOp};

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// Bloom NoMatch on equality.
    VERDICT_BLOOM = 10, "verdict.bloom", PgrcFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_BLOOM, &VerdictImpl(&Bloom))], ..crate::ops::NONE };

struct Bloom;

impl VerdictMech for Bloom {
    /// `true` when `qual` is an exact equality predicate the site declares, or an IN list whose members the site now
    /// holds (the bloom answers only equality).
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool {
        use crate::ops::scan::pgrc::verdict_zone::{hash_of, list_of, of};
        match of(px, site, qual) {
            Some((z, exact)) if !z.prefix => return exact && z.op == ZoneOp::Eq && site.zone_preds().contains(&z),
            // P5-11: a text equality (SRC-09's prefix predicate under a bytes-order collation, or none) by its probe word
            _ => {
                if let Some(z) = hash_of(px, site, qual) {
                    if !site.zone_preds().contains(&z) {
                        site.add_zone(z);
                    }
                    return true;
                }
            }
        }
        // NEW-72: an IN list's members are the execution's (exec's prepare adds them as the lane's `InMember` predicates)
        // D3009: an OR's disjunction the site holds, one of whose arms is an equality (its bloom proves that arm none)
        let any = || crate::ops::scan::pgrc::verdict_zone::any_of(px, site, qual).is_some_and(|g| g.iter().any(|z| z.op == ZoneOp::Eq) && site.zone_any_groups().any(|x| x == &g[..]));
        list_of(px, site, qual).is_some() || crate::ops::scan::pgrc::verdict_zone::hash_list_of(px, site, qual).is_some() || any()
    }
}
