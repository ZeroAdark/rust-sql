//! rep's visible decisions in the effects home: `user_reach`, whether a key type may reach user code behind its
//! comparator (rep/mod.rs re-exports it where its callers name it: rep/keylane.rs, ops/sort/topn/copy_words.rs,
//! ops/append/merge_admit.rs); from rep/keylane.rs, whether a hosted key's comparator may be the user's (`user_cmp`) and
//! whether a sort's user comparator meets a hash join's batches (`user_batches`, ops/sort/lower.rs).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::ops::shared::sort::SortKey;

/// D2039 / D2041 (in-row): whether a key of type `t` (a domain's base) may run user code behind its internal
/// comparator, which the key reach past sixteen sort keys (runs/keylane.rs) refuses (past eight group keys,
/// copy_words.rs, it no longer does: USEROP D2039, the call count is free): a composite or pseudo type (record,
/// anyarray: each field's / element's comparator), a user range or multirange (its subtype opclass), an array over a
/// user element or over such a type. Conservative: no subtype or field walk; a catalog miss is `true`.
pub(crate) fn user_reach<'m>(catalog: &dyn gravity_expr::Catalog<'m>, t: u32) -> bool {
    crate::stack_check();
    match catalog.type_facts(t) {
        Ok(f) if f.len == -1 && f.elem != 0 => f.elem >= gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID || user_reach(catalog, f.elem),
        Ok(f) => matches!(f.typtype as u8, b'c' | b'p') || t >= gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID && matches!(f.typtype as u8, b'r' | b'm'),
        Err(_) => true,
    }
}

/// USEROP (`Key::user`): whether a hosted key's comparator may be the user's: a CompareFn key's (its family's own, whatever
/// the type), a type's with no order image (only an opclass the user made orders it), a user range's or multirange's, an
/// array's of a user type (`rep::user_reach`). A record's `record_cmp` is not: its run keeps its equal-copy tie
/// (`KFLAG_EQTIE`) and its sort; but a per-row record's is (decompose `SortKey::per_row`, D3374 (e): it raises on pairs
/// of two layouts, so its first raise must be PostgreSQL's).
pub(crate) fn user_cmp<'m>(catalog: &dyn gravity_expr::Catalog<'m>, sk: &SortKey, tf: &TypeFacts) -> bool {
    let record = matches!(catalog.type_facts(tf.base), Ok(f) if matches!(f.typtype as u8, b'c' | b'p'));
    sk.compare_fn() || sk.per_row || crate::rep::keylane::no_image(tf) || !record && user_reach(catalog, tf.base)
}

/// Whether pipeline `k`'s sort is rep/keylane.rs `USER_BATCHES`' shape: a sort keyed by another family's comparator in SQL
/// or a procedural language (or a per-row record key) over a hash join in its pipeline.
pub(crate) fn user_batches(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::{join::JoinDesc, Op};
    let ops = &gx.pipelines()[k].ops;
    ops.iter().any(|op| matches!(op, Op::Sort(sd) if sd.keys.iter().any(|key| key.per_row || key.compare_fn() && !crate::rep::keylane::c_level_cmp(gx, key.opno))))
        && ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { catalog: false, .. } | JoinDesc::Drain { .. })))
}
