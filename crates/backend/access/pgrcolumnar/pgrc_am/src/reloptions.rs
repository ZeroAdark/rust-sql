//! The `sorted_by` reloption as the AM reads it.
//!
//! `WITH (sorted_by = 'a, b, c')` declares the key the writer sorts every
//! part by at seal (ASC NULLS LAST, lexicographic; `pgrc_write::seal`'s
//! cluster sort). Parsing and syntax live in the `reloptions` crate; this
//! module resolves the declared names against the live descriptor, tells
//! DDL which columns the writer can sort, gates `ALTER ... SET` on an empty
//! table, and answers the planner: the declared attnos when the whole
//! relation is no part, or one part whose SortKey section attests the key
//! (the order is per part, nothing is promised across parts — that is the
//! partition item; a front-sealed part is unsorted), else nothing.

use pgrc_format::sortkey::SortKeyEntry;
use pgrc_write::structural::{ClusterKeyDecl, NullsOrder, SortDir};
use types_error::PgResult;
use types_rel::Relation;
use types_tuple::FormData_pg_attribute;

/// The writer keeps the key (`ingest::open_writer` declares it to the seal);
/// were that wiring ever removed, this flips and the planner reads nothing.
pub const SORTED_BY_ENFORCED: bool = true;

fn declared<'a>(rel: &'a Relation<'_>) -> Option<&'a types_rel::PgrcolumnarOptions> {
    rel.rd_options.as_ref().and_then(|o| o.pgrcolumnar()).filter(|o| !o.sorted_by().is_empty())
}

fn live_att<'a>(atts: &'a [FormData_pg_attribute], col: &str) -> Option<&'a FormData_pg_attribute> {
    atts.iter().find(|a| !a.attisdropped && a.attname.name_str() == col.as_bytes())
}

/// SORTRUNS-E0 L3a: `sort_runs = on` (default off): the compaction worker's run pass merges the table's full parts
/// into key-ordered runs (`pgrc_front::compact::run_pass`; it needs a sort key too, `runmerge::table_sort_key`).
pub fn sort_runs(rel: &Relation<'_>) -> bool {
    rel.rd_options.as_ref().and_then(|o| o.pgrcolumnar()).is_some_and(|o| o.sort_runs())
}

/// The declared key as live attnos with each key's jsonb path (empty = the
/// column itself), in declaration order. `None` when undeclared or when a
/// name no longer resolves (renamed or dropped after the declaration): the
/// whole key is void, never a partial one.
pub fn sorted_by_keys(rel: &Relation<'_>) -> Option<Vec<(u32, Vec<Vec<u8>>)>> {
    let opts = declared(rel)?;
    opts.sorted_by_keys()
        .into_iter()
        .map(|k| {
            let a = live_att(&rel.rd_att.attrs, &k.col)?;
            // a path key reads a jsonb document (DDL checked it; a type
            // change since voids the key)
            if !k.path.is_empty() && a.atttypid != types_core::catalog::JSONBOID {
                return None;
            }
            Some((a.attnum as u32, k.path.into_iter().map(String::into_bytes).collect()))
        })
        .collect()
}

/// The declared key's leading plain columns as live attnos: the order a
/// consumer that knows only columns may read (a path key ends the prefix).
pub fn sorted_by_attnos(rel: &Relation<'_>) -> Option<Vec<u32>> {
    Some(sorted_by_keys(rel)?.into_iter().take_while(|(_, p)| p.is_empty()).map(|(a, _)| a).collect())
}

/// A backend's resolved leading keys, `(database, relid, attno)` (0: no key, or a void one), dropped by its relcache
/// callback ([`forget_lead`]): an ALTER of `sorted_by` or a RENAME / DROP COLUMN resolves again.
const LEAD_CAP: usize = 64;

thread_local! {
    static LEAD: core::cell::RefCell<Vec<(types_core::Oid, types_core::Oid, u32)>> = const { core::cell::RefCell::new(Vec::new()) };
}

/// [`sorted_by_attnos`]' first attno, resolved once per backend and relcache entry (a served statement rebuilds its
/// granule recipe every execution; the resolution parses the option and walks the descriptor). A backend that cannot
/// register the relcache callback resolves every time.
pub fn sorted_by_lead_attno(rel: &Relation<'_>) -> Option<u32> {
    let resolve = || sorted_by_attnos(rel).and_then(|k| k.first().copied());
    if crate::inval::ensure_inval_registered().is_err() {
        return resolve();
    }
    let (db, relid) = (init_small::globals::MyDatabaseId(), rel.rd_id);
    if let Some(a) = LEAD.with(|l| l.borrow().iter().find(|e| e.0 == db && e.1 == relid).map(|e| e.2)) {
        return (a != 0).then_some(a);
    }
    let a = resolve();
    LEAD.with(|l| {
        let mut l = l.borrow_mut();
        if l.len() >= LEAD_CAP {
            l.clear();
        }
        l.push((db, relid, a.unwrap_or(0))); // alloc: Cold (a backend's first tables)
    });
    a
}

/// The relcache callback's (`inval`): `relid`'s resolved leading keys go, in every database (0 = every relation's).
pub(crate) fn forget_lead(relid: types_core::Oid) {
    LEAD.with(|l| {
        let mut l = l.borrow_mut();
        if relid == 0 {
            l.clear();
        } else {
            l.retain(|e| e.1 != relid);
        }
    });
}

/// The writer's cluster key for this relation (empty = undeclared).
pub fn cluster_key(rel: &Relation<'_>) -> Vec<ClusterKeyDecl> {
    sorted_by_keys(rel)
        .unwrap_or_default()
        .into_iter()
        .map(|(attno, path)| ClusterKeyDecl { attno, dir: SortDir::Asc, nulls: NullsOrder::Last, path })
        .collect()
}

/// DDL verdict for one `sorted_by` name: `None` = no such live column,
/// `Some(sortable)` = whether the writer sorts its stored form.
pub fn column_sortable(atts: &[FormData_pg_attribute], col: &str) -> Option<bool> {
    let att = live_att(atts, col)?;
    Some(crate::schema::derive_one(att).is_ok_and(|s| pgrc_write::seal::cluster_sortable(&s)))
}

/// Does the table hold rows a new key would have to reorder? Committed
/// parts, or a writer this session opened on it (rows buffered or sealed
/// but not yet published).
pub fn table_has_data(rel: &Relation<'_>) -> PgResult<bool> {
    if crate::footer::part_count(rel)?.is_some_and(|p| p > 0) {
        return Ok(true);
    }
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    Ok(crate::session::with_writers(|w| w.registered_stamps().any(|(r, _)| r == relfilenumber)))
}

/// What the planner may claim: the declared attnos (1-based) when the
/// order is enforced and the committed data attests it — no part at all,
/// or exactly one part whose SortKey section vouches for the declared key
/// ([`attested_prefix`]). The declared key alone is never the witness: a
/// front segment's seal keeps heap order (its rowid map) and writes
/// `nkeys = 0`, so a table fed only by INSERTs is one UNSORTED part.
/// R15-RR-CLAIM (ROAD2, main 10-15; the root of the P0 PGRC-SORTED-CLAIM-SNAPSHOT): the claim holds in the newest
/// committed generation AND in `view`, the generation the statement's snapshot sees (`pgrc_front::ring::statement_view`;
/// `None` = none taken yet). A REPEATABLE READ reader opened before a compaction reads its own generation's parts: a
/// claim from the newest generation alone (one attested part after `pgrc_compact 'sorted'`) dropped the Sort over a scan
/// of the reader's two or more parts (wrong order; with D4469's merge mark, 'no sort witness' over unattested parts).
/// A composed view claims nothing.
pub fn planner_sorted_attnos(rel: &Relation<'_>, view: Option<&crate::catalog::Generation>) -> PgResult<Vec<i16>> {
    if !SORTED_BY_ENFORCED {
        return Ok(Vec::new());
    }
    // sortpath: a key set on a table that held parts holds only for the parts written after it
    if declared(rel).is_some_and(|o| o.sorted_by_partial()) {
        return Ok(Vec::new());
    }
    let Some(attnos) = sorted_by_attnos(rel) else { return Ok(Vec::new()) };
    let mut n = match crate::footer::committed_order(rel)? {
        crate::footer::CommittedOrder::NoParts => attnos.len(),
        crate::footer::CommittedOrder::OnePart(keys) => attested_prefix(&attnos, &keys),
        crate::footer::CommittedOrder::Many => 0,
    };
    if let Some(v) = view {
        n = if v.composed() { 0 } else { n.min(claim_len(&attnos, Some(v))?) };
    }
    Ok(attnos[..n].iter().map(|&a| a as i16).collect())
}

/// The length of the claim [`planner_sorted_attnos`] grants over generation `gen` (`None` = no generation yet, no
/// part) for the declared key `attnos`: all of it at no part, the attested prefix at one part, none at two or more.
fn claim_len(attnos: &[u32], gen: Option<&crate::catalog::Generation>) -> PgResult<usize> {
    let Some(g) = gen else { return Ok(attnos.len()) };
    Ok(match g.parts.len() {
        0 => attnos.len(),
        1 => attested_prefix(attnos, &g.sort_key(|part| Ok(crate::footer::part_sort_key(part)?.keys))?),
        _ => 0,
    })
}

/// D1159: a publish that moves [`planner_sorted_attnos`]'s answer — a second part, a first part that does not
/// attest the key, a compaction back to one attested part — invalidates `rel`'s relcache entry, so plans cached
/// under the old claim (PREPARE, plpgsql, the same transaction) are rebuilt (PG's CacheInvalidateRelcache:
/// registered in this transaction, processed at its next CommandCounterIncrement and sent at commit; an abort
/// drops it with the publish). Only a `sorted_by` table pays, and only on a change: `prev_gen` is the generation
/// the publish built on (0 = none), `gen` the published one. A base the catalog no longer holds counts as changed, and
/// so does every publish when `stacked` (CONCLOAD: a bulk load over another transaction's parts in progress; plans
/// hold a claim only over a whole generation, never a view that hides parts, so the invalidation at its commit reaches
/// every plan that could hold one).
pub fn inval_on_claim_change(rel: &Relation<'_>, slot: crate::catalog::RelSlot, prev_gen: u64, gen: &crate::catalog::Generation, stacked: bool) -> PgResult<()> {
    if !SORTED_BY_ENFORCED {
        return Ok(());
    }
    let Some(attnos) = sorted_by_attnos(rel) else { return Ok(()) };
    if stacked {
        inval::invalidate::CacheInvalidateRelcache(rel)?;
        let _ = elog::elog(types_error::DEBUG1, format!("pgrc claim change: relcache invalidation of \"{}\" (stacked bulk load)", rel.name())); // alloc: Cold
        return Ok(());
    }
    let after = claim_len(&attnos, Some(gen))?;
    let before = if prev_gen == 0 {
        Some(attnos.len())
    } else {
        match crate::catalog::generations(slot).and_then(|(gens, _)| gens.into_iter().find(|g| g.gen == prev_gen)) {
            Some(g) => Some(claim_len(&attnos, Some(&g))?),
            None => None,
        }
    };
    if before != Some(after) {
        inval::invalidate::CacheInvalidateRelcache(rel)?;
        let _ = elog::elog(types_error::DEBUG1, format!("pgrc claim change: relcache invalidation of \"{}\" (claim {before:?} -> {after})", rel.name())); // alloc: Cold
    }
    Ok(())
}

/// A `sorted_by` key is declared and enforced (the planner may claim it): pgrc_front's cheap pre-check.
pub fn declares_sorted_by(rel: &Relation<'_>) -> bool {
    SORTED_BY_ENFORCED && declared(rel).is_some()
}

/// How much of the declared key `declared` a part's SortKey entries
/// `attested` vouch for: the longest common prefix whose entries name the
/// same attno in the same position, ASC NULLS LAST (the only order the
/// planner's claim states). A lexicographic order on a key is an order on
/// every prefix of it, so the prefix is claimable on its own; a part sorted
/// by a longer key still yields the whole declared key, one sorted by a
/// different leading column (or `nkeys = 0`) yields nothing, and so does a
/// path key (sortpath: it orders a value of the column, not the column).
/// Collation is not compared: the entry's class was derived from the same
/// column schema the relation resolves under (the schema fingerprint check
/// refuses any other).
pub fn attested_prefix(declared: &[u32], attested: &[SortKeyEntry]) -> usize {
    declared
        .iter()
        .zip(attested)
        .take_while(|&(&a, k)| k.attno == a && k.pad == 0 && k.dir == SortDir::Asc as u8 && k.nulls == NullsOrder::Last as u8)
        .count()
}
