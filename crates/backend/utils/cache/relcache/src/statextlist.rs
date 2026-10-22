use std::rc::Rc;

use mcx::{Mcx, PgVec};
use types_core::Oid;
use types_error::PgResult;

use crate::with_state;

// RelationGetStatExtList (relcache.c): rd_statlist equivalent, keyed off the
#[allow(non_snake_case)]
pub fn RelationGetStatExtList<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<PgVec<'mcx, Oid>> {
    // C3: a recorded lowering depends on the relation (its invalidation drops this list); the fill's scan is the list's
    gravity_deps::note_rel(relid);
    if let Some(hit) = with_state(|st| st.statext_cache.get(&relid).cloned()) {
        return copy_out(mcx, &hit);
    }
    let gen = match crate::l2core::statext_probe(relid)? {
        crate::l2core::StatExtProbe::Hit(shared) => {
            with_state(|st| st.statext_cache.insert(relid, Rc::clone(&shared)));
            return copy_out(mcx, &shared);
        }
        crate::l2core::StatExtProbe::Miss(gen) => gen,
    };
    let oids = gravity_deps::suppressed(|| relcache_build_seams::scan_pg_statistic_ext_oids::call(mcx, relid))?;
    let built: Rc<[Oid]> = Rc::from(&oids[..]);
    if let Some(gen) = gen {
        crate::l2core::statext_publish(relid, gen, &built);
    }
    with_state(|st| st.statext_cache.insert(relid, built));
    Ok(oids)
}

fn copy_out<'mcx>(mcx: Mcx<'mcx>, oids: &[Oid]) -> PgResult<PgVec<'mcx, Oid>> {
    let mut out: PgVec<'mcx, Oid> = mcx::vec_with_capacity_in(mcx, oids.len())?;
    out.extend_from_slice(oids);
    Ok(out)
}

pub(crate) fn forget(relid: Oid) {
    with_state(|st| {
        st.statext_cache.remove(&relid);
    });
}
