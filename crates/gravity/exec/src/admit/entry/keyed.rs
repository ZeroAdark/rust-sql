//! Rung 14 contract C3a, the served memo at the seam (docs/gravity/entry.md §11): a generic cached plan the seam
//! road admitted is filed under its plan key as bind files its own; the next execution takes the filed program
//! instead of admitting again and files it back at free (key recomputed: a replanned statement's program is dropped).
use crate::runner::{entry_key, give_back, take_served};
use crate::{Admitted, Backend};
use gravity_program::PlanKey;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_portal::{CachedPlanHandle, ParamListHandle};

/// `ident` (D3865 L5): the identity admission filed under the plan's identity key, `pstmt` the desc's own plan; it brings
/// its own plan copy (`Admitted::ident`), which its reader's kind is read over, as at its admission.
pub fn take(b: &mut Backend, cplan: CachedPlanHandle, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, ident: bool) -> PgResult<Option<(PlanKey, Box<Admitted>)>> {
    let Some((key, a)) = take_served(b, cplan, pstmt, params, ident) else { return Ok(None) };
    // D3865 L5: an identity key's admission without its copy (none is filed so): dropped, and this start admits again
    let read = match ident {
        true => a.pstmt.filter(|_| a.ident.is_some()),
        false => Some(pstmt),
    };
    let Some(read) = read else {
        b.drop_admission(key, a);
        return Ok(None);
    };
    // a program admitted for another root reader's kind (a named cursor's FETCH, or the reverse: GraphWords::partial) is
    // dropped, and this start admits again under its own; D2926: the counted start of the unnamed portal (`run_counted`)
    // leaves the whole reader's program filed for the plan's uncounted runs
    if a.graph.as_ref().is_some_and(|g| g.partial != super::partial_reader(read)) {
        if super::counted_start(pstmt) {
            give_back(b, key, cplan, pstmt, a);
        } else {
            b.drop_admission(key, a);
        }
        return Ok(None);
    }
    // B2-31: a program holding a timestamptz constant converted in the session TimeZone (`Admitted::timezone`), taken by a
    // reader whose rows come over several runs (a named cursor's FETCHes, a counted start): left filed for the plan's whole
    // reads, and this start admits again with nothing converted (normalize `Zone::active`, `entry::zone_off`)
    if a.timezone.is_some() && (super::partial_reader(pstmt) || super::zone_off()) {
        give_back(b, key, cplan, pstmt, a);
        return Ok(None);
    }
    b.note_memo_hit();
    Ok(Some((key, a)))
}

pub fn key_of(cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>) -> Option<PlanKey> {
    entry_key(cplan, pstmt)
}

/// D2190: a keyed execution's admission dropped, not filed back (a failed run's desc released): the key's parked Nodes
/// go with it (`Backend::drop_admission`).
pub fn forsake(home: usize, key: Option<PlanKey>, a: Box<Admitted>) {
    let Some(key) = key else { return };
    // D3346: off its home level (or no level free), the admission retires there (`reentry::retire`)
    let mut a = Some(a);
    let _ = crate::admit::reentry::enter(None, None, false, |b, depth| {
        if depth == home {
            b.drop_admission(key, a.take().expect("once"));
        }
    });
    if let Some(a) = a {
        crate::admit::reentry::retire(home, key, a);
    }
}

pub fn release(home: usize, key: Option<PlanKey>, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, a: Box<Admitted>) {
    let Some(key) = key else { return };
    let mut served = Some(a); // D1216: boxed since its take (or its admission): never copied back into a box
    let _ = crate::admit::reentry::enter(None, None, false, |b, depth| {
        if depth == home {
            give_back(b, key, cplan, pstmt, served.take().expect("once"));
        }
    });
    // D3346: off its home level, retired there
    if let Some(a) = served {
        crate::admit::reentry::retire(home, key, a);
    }
}
