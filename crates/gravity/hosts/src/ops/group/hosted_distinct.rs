//! The hosted aggregate bracket's DISTINCT form (a `#[path]` child of hosts v4/m6_agg.rs, D1210): PG 18.6's single-input
//! DISTINCT aggregate (nodeAgg.c process_ordered_aggregate_single), and (D1283) the multi-input one
//! (process_ordered_aggregate_multi: the rows sorted as tuples by every DISTINCT column, a row equal to the one before it
//! in every column by that column's eqop under its collation dropped). Each row of a group that passes the strict input
//! check (a strict transfn's NULL input is dropped before the sort, execExpr.c's EEOP_AGG_STRICT_INPUT_CHECK_NULLS) is
//! kept in the group's run, a by-reference value copied into the aggregate context; at the final the run is sorted by
//! the DISTINCT clause's sort operator, collation and NULLs placement (tuplesort_begin_datum at work_mem: a run past it
//! spills to disk as PG's does), a value equal to the one before it by the clause's equality operator under the
//! aggregate's input collation is dropped (NULL equals NULL), and the transfn folds the rest in that order from the
//! initcond before the finalfn: exact for float, numeric, text and collations. The run is a list of chunks in the
//! aggregate context (charged to the arena's bytes; D3267: no chunk near MaxAllocSize, and past work_mem, per participant,
//! its later chunks and images go to temp files mapped shared, as PG's per-aggregate sort spills); a keyed group's slot is
//! its home's, so a group keeps one run. Created by M6-AGG. The run's pointer work and the forms' row / combine / final
//! are unsafe, so they live in the family's raw half (raw/distinct.rs).
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{Aggref, AGGKIND_NORMAL};

/// One DISTINCT aggregate's sort (ExecInitAgg's pertrans sort columns): each input's `(typlen, byval)`, then per DISTINCT
/// column its input's attno, sort operator, collation (the input expression's) and NULLs placement.
pub(crate) struct Sorted {
    pub(super) typ: Oid,
    pub(crate) cols: Vec<(i16, bool)>, // alloc: Exec
    pub(crate) keys: Vec<(i16, Oid, Oid, bool)>, // alloc: Exec
    /// Each DISTINCT column's equality function (`get_opcode(eqop)`), bound per slot (bind.rs `Calls::eq`); none for an
    /// ORDER BY without DISTINCT (D458), which keeps every row.
    pub(crate) eqfns: Vec<Oid>, // alloc: Exec
    /// The transfn's inputs (`numTransInputs`): the leading columns; an ORDER BY's own resjunk columns follow them (D458).
    pub(crate) ninputs: usize,
    /// D1596: an aggpresorted DISTINCT the Agg's input feeds in its order, folded per row: the last row (m6_agg_presorted.rs).
    pub(crate) fed: Option<crate::ops::group::raw::presorted::Last>,
}

pub(crate) fn shape(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: {what}"))) // alloc: Cold
}

impl Sorted {
    /// `a`'s DISTINCT sort (every input a DISTINCT column: the parser's transformDistinctClause), or (D458) its ORDER BY
    /// sort (ExecInitAgg's `numSortCols` over `aggorder`: its columns may be resjunk ones of its own, kept beside the
    /// inputs and never passed to the transfn); `None` without either.
    pub(crate) fn of(a: &Aggref<'_>) -> PgResult<Option<Sorted>> {
        if a.aggkind != AGGKIND_NORMAL {
            return Ok(None); // an ordered-set / hypothetical aggregate's WITHIN GROUP order is its transfn's own sort
        }
        let dedup = !a.aggdistinct.is_nil();
        let clause = if dedup { &a.aggdistinct } else { &a.aggorder };
        if clause.is_nil() {
            return Ok(None);
        }
        let (mut keys, mut eqfns, mut cols) = (Vec::new(), Vec::new(), Vec::new()); // alloc: Exec
        // bounded("other: the sort clauses of one aggregate call (its DISTINCT or its ORDER BY), read once at bind: the query text bounds them, as PG's ExecInitAgg walks the same clauses with no poll")
        for sc in clause.iter() {
            let scl = sc.as_sort_group_clause().ok_or_else(|| shape("an aggregate sort entry that is no SortGroupClause"))?;
            let tle = a.args.iter().filter_map(|n| n.as_target_entry()).find(|t| t.ressortgroupref == scl.tleSortGroupRef && scl.sortop != 0);
            let tle = tle.ok_or_else(|| shape("a hosted sorted aggregate whose sort clause is not one of its arguments"))?;
            let coll = execscan::expr_collation(tle.expr);
            let coll = if dedup && clause.len() == 1 { count_sort_collation(a, scl.sortop, coll)? } else { coll };
            keys.push((tle.resno, scl.sortop, coll, scl.nulls_first));
            if dedup {
                eqfns.push(lsyscache::get_opcode(scl.eqop)?);
            }
        }
        let ninputs = a.aggargtypes.iter().count();
        let mut typ = None;
        // bounded("other: the argument entries of one aggregate call (its inputs and its ORDER BY's own columns), read once at bind: the query text bounds them, as PG's ExecInitAgg walks the same list with no poll")
        for (c, t) in a.args.iter().filter_map(|n| n.as_target_entry()).enumerate() {
            let t = if c < ninputs { a.aggargtypes.iter().nth(c).unwrap_or(0) } else { execexpr::expr_type(t.expr) };
            typ.get_or_insert(t);
            cols.push(gravity_fn::typesem::storage::len_byval(t)?);
        }
        let typ = typ.ok_or_else(|| shape("a hosted sorted aggregate without an input"))?;
        if cols.len() != a.args.len() || ninputs > cols.len() || (dedup && (keys.len() != cols.len() || ninputs != cols.len())) {
            return Err(shape("a hosted DISTINCT aggregate whose inputs are not all DISTINCT columns"));
        }
        Ok(Some(Sorted { typ, cols, keys, eqfns, ninputs, fed: None }))
    }

    /// Whether equal rows are dropped (a DISTINCT); an ORDER BY alone keeps them all.
    pub(crate) fn dedup(&self) -> bool {
        !self.eqfns.is_empty()
    }
}

/// The collation a count(DISTINCT x)'s run sorts under: count reads only which values are equal, never their order, and
/// under a deterministic collation equal text is equal bytes (texteq / bpchareq compare the bytes, bpchar's trimmed), so
/// a run of a character type sorted by its default btree order (text's, bpchar's `<` / `>`) sorts in byte order (C:
/// memcmp, no strcoll per compare) and the eqop, still under the input collation at the fold, drops the same duplicates:
/// the same count. Any other aggregate (its transfn sees the values in sort order), any other sort operator and a
/// nondeterministic collation (equal values of other bytes) keep `coll`.
fn count_sort_collation(a: &Aggref<'_>, sortop: Oid, coll: Oid) -> PgResult<Oid> {
    use gravity_decompose::ops::shared::agg_class::Class;
    use gravity_fn::typesem::collation::{Facts, C_COLLATION_OID};
    if coll == 0 || !gravity_decompose::ops::agg::trans::table::row(a.aggfnoid).is_some_and(|r| r.class == Class::CountAny) {
        return Ok(coll);
    }
    let by = gravity_fn::typesem::keysem::sort_order(sortop)?;
    if !by.is_some_and(|o| o.default_family && gravity_fn::typesem::character_type(o.opcintype)) {
        return Ok(coll);
    }
    let f = Facts::of(coll)?;
    if !f.deterministic || f.collate_is_c {
        return Ok(coll);
    }
    fires::fire("agg.distinct.bytes"); // shapecheck fire signal: a count(DISTINCT text) run sorted in byte order
    Ok(C_COLLATION_OID)
}
