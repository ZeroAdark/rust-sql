//! M3's by-rowid fetch site (road topk.fetch, V4R-169; lowerv4 runs/fetch.rs): a pgrc or heap range site that carries a
//! `RowId` lane is the fetch emit's site, not a scan's: over pgrc, hosts `FetchRecipe` over its attribute lanes, bound as
//! a fetch source (`HostRegistry::push_fetch`; hosts fetch_site.rs `Source::fetch`); over a heap, hosts `HeapFetchRecipe`
//! (ops/sort/heap_fetch.rs: the winners by TID), bound as a rung recipe. The scan admissions pass such a site here.
//! Owner M3-TOPS.
use gravity_decompose::{Am, Source};
use gravity_hosts::ops::sort::heap_fetch::HeapFetchRecipe;
use gravity_hosts::{FetchRecipe, HostRegistry};
use gravity_program::{LaneId, SiteDesc};
use types_core::Oid;
use types_error::PgResult;
use types_storage::lock::NoLock;

use crate::admit::{refuse, AdmitCx, AdmitResult, AdmitTpl, BindCx, BindResult, RecipeTpl, Refused};
use crate::shapes;

pub(crate) fn admit(cx: &AdmitCx<'_>) -> AdmitResult {
    let Some(am) = claims(cx.source, cx.site) else { return Ok(Ok(None)) };
    debug_assert!(cx.attnos.iter().all(|&a| a > 0), "a fetch site's attnos are its attribute lanes, never the RowId lane");
    let rel = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let tpl = match am {
        Am::Heap => heap_template(HeapFetchRecipe::from_plan(&rel, cx.attnos)),
        _ => template(FetchRecipe::from_plan(&rel, cx.attnos)),
    };
    rel.close(NoLock)?;
    Ok(tpl)
}

/// The sites this admit takes, by their relation's AM: a pgrc or heap range whose site carries a `RowId` lane.
fn claims(source: &Source<'_>, site: &SiteDesc) -> Option<Am> {
    match source {
        Source::Range { am: am @ (Am::Pgrcolumnar | Am::Heap), .. } if site.lanes().iter().any(|l| l.id == LaneId::RowId) => Some(*am),
        _ => None,
    }
}

fn heap_template(recipe: PgResult<HeapFetchRecipe>) -> Result<Option<AdmitTpl>, Refused> {
    match recipe {
        Ok(r) => Ok(Some(RecipeTpl::HeapFetch(r))),
        Err(e) => refuse("hosts", e.message()),
    }
}

fn template(recipe: PgResult<FetchRecipe>) -> Result<Option<AdmitTpl>, Refused> {
    match recipe {
        Ok(r) => Ok(Some(RecipeTpl::Fetch(r))),
        Err(e) => refuse("hosts", e.message()),
    }
}

pub(crate) fn bind(t: &RecipeTpl, reg: &mut HostRegistry, _cx: &BindCx<'_>, _exprs: &mut shapes::KeyExprs) -> BindResult {
    match t {
        RecipeTpl::Fetch(r) => reg.push_fetch(r.clone()),
        RecipeTpl::HeapFetch(r) => reg.push_rung(Box::new(r.clone())), // alloc: Program
        _ => return Ok(Ok(false)),
    }
    Ok(Ok(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::{Arena, LaneDesc};

    /// V4R-169 (HOSTS review): a pgrc or heap range site with a RowId lane is this admit's (the pgrc one admits as
    /// `RecipeTpl::Fetch`); a range without the lane (a scan), or another AM's, is not.
    #[test]
    fn a_rowid_site_over_pgrc_or_heap_is_a_fetch() {
        let arena = Arena::new(1 << 16);
        let site = |rowid: bool| {
            let s = SiteDesc::range_source(arena.handle(), 7, &[1, 2]);
            if rowid { s.with_lane(LaneDesc::rowid(), false) } else { s }
        };
        let range = |am, rowid| Source::Range { site: site(rowid), am, scanrelid: 1, relid: 7 };
        assert_eq!(claims(&range(Am::Pgrcolumnar, true), &site(true)), Some(Am::Pgrcolumnar));
        assert_eq!(claims(&range(Am::Heap, true), &site(true)), Some(Am::Heap), "a heap wide top fetches its winners by TID (D519)");
        assert_eq!(claims(&range(Am::Pgrcolumnar, false), &site(false)), None, "a pgrc scan site is M1's granule admit's");
        assert_eq!(claims(&range(Am::Heap, false), &site(false)), None, "a heap scan site is M2's range admit's");
        let reg = std::sync::Arc::new(pgrc_read::PartRegistry::new(1 << 20));
        let r = FetchRecipe::for_test(7, &[1, 2], &[gravity_hosts::ops::scan::pgrc::fetch_site::KIND_INT, gravity_hosts::ops::scan::pgrc::fetch_site::KIND_TEXT], reg);
        assert!(matches!(template(Ok(r)), Ok(Some(RecipeTpl::Fetch(_)))));
        assert!(template(Err(Box::new(types_error::PgError::error("gravity: fetch attno")))).is_err_and(|r| r.what.contains("fetch attno"))); // alloc: Cold
    }
}
