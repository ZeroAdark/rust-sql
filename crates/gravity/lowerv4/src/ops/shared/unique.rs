//! A relation's unique keys, read by the write family (rowop.rs `one_row_chain`) and the hash join drain (drain/lower.rs
//! `right_once`): whether fixing some plain columns of a relation reaches at most one of its rows.

/// Relation `relid` has a unique, immediate, valid, non-partial btree index whose key columns are all plain columns in
/// `eqs`: a scan with those columns fixed reads at most one row.
pub(crate) fn unique_on(gx: &crate::graph::Gx<'_, '_>, relid: u32, eqs: &[i16]) -> bool {
    let mut hit = false;
    gx.facts.indexes_of(relid, &mut |i| {
        let e = gx.facts.index_ext(i.indexid);
        let n = usize::from(if e.nkeyatts == 0 { i.nkeys } else { e.nkeyatts.min(i.nkeys) });
        hit |= i.unique && i.btree && !i.partial && e.known && e.valid && !e.deferrable && n > 0 && i.keys[..n].iter().all(|&a| a > 0 && eqs.contains(&a));
    });
    hit
}
