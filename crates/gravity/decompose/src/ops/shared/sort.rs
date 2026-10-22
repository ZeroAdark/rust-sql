//! The sort keys and descriptors other families read (`KeyImage` among them); their methods stay with ops/sort and
//! ops/agg.
use core::fmt::{self, Write};

use gravity_abi::breaker::KeyKind;
use gravity_program::{ArenaRef, ArenaVec, NumTypmod};
use types_nodes::list::NodeList;
use types_nodes::Node;

use crate::admit::Refusal;
use crate::catalog::Facts;
use crate::graph::{Bound, Layout};
use crate::ledger::Kind;
use crate::catalog::typefacts::{self, KeyColl};
use crate::walk::Builder;

pub use crate::catalog::keysem::image::KeyImage;

/// A sort key: the child's target-list column, the ordering operator, its direction, the record key kind, image and
/// numeric scale, and the Sort node's collation for the key (audit C8: never the expression's).
#[derive(Clone, Copy, Debug)]
pub struct SortKey {
    pub resno: i16,
    pub opno: u32,
    pub desc: bool,
    pub nulls_first: bool,
    pub kind: KeyKind,
    pub image: KeyImage,
    pub scale: i16,
    pub coll: KeyColl,
    /// D1346: `opno` is an ordering operator of a btree family that is not its type's default (`Facts::key`, `default_family` false):
    /// the key orders only through that family's compare (lowerv4 runs/keylane.rs `cmp_proc`, rep.hosted.order), never
    /// by an image, and no order a consumer matches (`step`) carries it.
    pub own_family: bool,
    /// W8-EQUIVCLASS-SORTOP: `(opcintype, opfamily)` when `opno`'s family has no order proc for its type
    /// (`Facts::key`'s `no_proc`): PG raises at the Sort's first call, so the key is never compared (its type's word order).
    pub no_proc: Option<(u32, u32)>,
    /// setop_core-0129: an anonymous `record` key whose two equal copies `record_cmp` cannot raise on (`record_eq_tie`), so
    /// the run ties them with no call (lowerv4 face/runs.rs `KFLAG_EQTIE`); false for every other key.
    pub eq_tie: bool,
    /// D3374 (e) (record keys E2): an anonymous `record` key whose columns are known only per row (no producer ROW()
    /// every row agrees with, `facts::record::key_row`): `record_cmp` raises on two layouts that meet on an equal prefix,
    /// naming the pair, so the run orders it by PostgreSQL's qsort in arrival order and stops at the first raise (lowerv4
    /// effects/visible/rep.rs `user_cmp`, keylane.rs `KFLAG_USER`), and a sort whose rows arrive in another order refuses (`USER_ARRIVAL`).
    pub per_row: bool,
}

/// A grouping / equality key (window partitions, set operations): no direction of its own.
#[derive(Clone, Copy, Debug)]
pub struct EqKey {
    pub resno: i16,
    pub opno: u32,
    pub kind: KeyKind,
    /// The key column's type (its `TypeFacts` through `typefacts::facts_of`, V4R-84).
    pub typid: u32,
}

pub enum SortForm<'p> {
    Runs,
    TopK { count: Bound<'p>, offset: Bound<'p>, ties: bool },
    Walk { attno: i16, desc: bool, nulls_first: bool, count: Bound<'p>, offset: Bound<'p>, ties: bool },
}

pub struct SortDesc<'p> {
    pub keys: ArenaVec<SortKey>,
    pub presorted: u8,
    pub form: SortForm<'p>,
    pub tlist: &'p NodeList<'p>,
}

pub fn column_type(tlist: &NodeList<'_>, resno: i16, k: Kind, id: i32) -> Result<u32, Refusal> {
    let e = (resno as usize).checked_sub(1).filter(|&i| i < tlist.len()).map(|i| tlist.nth(i)).and_then(|n| n.as_target_entry()).ok_or(Refusal::shape(k, id, "a key outside the target list"))?;
    crate::catalog::expr_type::type_of(e.expr).ok_or(Refusal::shape(k, id, "a key of unknown type"))
}

/// The key-word layout of a set of kinds (`Fp128` two words), unbounded (decision 78).
pub fn keys_layout<'p>(arena: ArenaRef, kinds: impl Iterator<Item = KeyKind>) -> Layout<'p> {
    Layout::new(arena, kinds.flat_map(|kind| core::iter::repeat_n(kind, if kind == KeyKind::Fp128 { 2 } else { 1 })), core::iter::empty(), true)
}

pub const ROW_LAYOUT: Layout<'static> = Layout::ROW;

pub const NO_SCALE: i16 = -1;

/// A sort / equality key's record kind: the type's catalog facts (`typefacts::facts_of`: an enum keys as its oid word, a
/// domain as its base; V4R-84); a type nothing knows keys by its canonical image.
pub fn key_kind_of(facts: &dyn Facts, typid: u32) -> KeyKind {
    typefacts::facts_of(facts, typid).map_or(KeyKind::Fp128, |r| typefacts::key_of(&r, NumTypmod::NONE).0)
}

pub fn column_kind_of_expr(facts: &dyn Facts, n: Node<'_>) -> KeyKind {
    crate::catalog::expr_type::type_of(n).map_or(KeyKind::I64, |t| key_kind_of(facts, t))
}

pub fn column_kind(facts: &dyn Facts, tlist: &NodeList<'_>, resno: i16, k: Kind, id: i32) -> Result<KeyKind, Refusal> {
    Ok(key_kind_of(facts, column_type(tlist, resno, k, id)?))
}

/// D1292: a key over a collatable type whose collation is InvalidOid (a UNION ALL column over conflicting implicit
/// collations, `ORDER BY 2` over it) orders or compares under no collation: PG raises 42P22 ("could not determine which
/// collation to use for string comparison", or "... string hashing") at its first compare or hash. `KeyColl::of(0)`
/// reads as byte order, so every road would sort by bytes; the key refuses by this name at the decode instead, and PG
/// raises its own error under the fallback (the served raise is D1292's).
/// A full Sort (`SortForm::Runs`, or `TopK` under a constant count past 0) decodes such a key and the effects home's
/// `sort_raises` names it; lowerv4 serves PG's raise at the Sort's start (ops/sort/sort_raise.rs) or refuses by this
/// name. Every other site keeps the refusal.
pub const INDETERMINATE_COLL: &str = "D1292: a key over a collatable type with no collation (PG raises 42P22 at its first compare)";

/// A key's collation (`coll`, the node's) over `typid`, or D1292's refusal when the type is collatable and `coll` is 0.
pub fn key_coll(facts: &dyn Facts, typid: u32, coll: u32, k: Kind, id: i32) -> Result<KeyColl, Refusal> {
    if indeterminate(facts, typid, coll) {
        return Err(Refusal::shape(k, id, INDETERMINATE_COLL));
    }
    Ok(facts.coll(coll))
}

/// D1292: `coll` is InvalidOid over a collatable type (the type table's `collatable`, a domain's base's).
pub fn indeterminate(facts: &dyn Facts, typid: u32, coll: u32) -> bool {
    coll == 0 && typefacts::facts_of(facts, typid).is_some_and(|t| t.collatable)
}

pub fn dump_eq_keys(keys: &[EqKey], out: &mut dyn Write) -> fmt::Result {
    for (i, key) in keys.iter().enumerate() {
        write!(out, "{}{}={}:{:?}", if i > 0 { "," } else { "" }, key.resno, key.opno, key.kind)?;
    }
    Ok(())
}

pub fn dump_keys(keys: &[SortKey], out: &mut dyn Write) -> fmt::Result {
    for (i, key) in keys.iter().enumerate() {
        write!(out, "{}{}{}{}:{:?}", if i > 0 { "," } else { "" }, key.resno, if key.desc { "-" } else { "+" }, if key.nulls_first { "n" } else { "" }, key.kind)?;
        if !key.image.dump_plain() {
            write!(out, "/{:?}", key.image)?;
        }
        if key.scale != NO_SCALE {
            write!(out, ".{}", key.scale)?;
        }
        if key.coll.oid != 0 {
            write!(out, "@{}", key.coll.oid)?;
        }
        if key.is_name() {
            write!(out, "#19")?;
        }
        if let Some((t, f)) = key.no_proc {
            write!(out, "!noproc({t},{f})")?;
        }
    }
    Ok(())
}

/// Whether PostgreSQL reads plan node `id`'s every row when the statement is read to its end: it sits in the statement's
/// plan tree (not a SubPlan's) under nodes that each read their whole input before their own end (a Result without a
/// one-time qual, a projection set, a subquery scan, a sort, an aggregate, a group, a unique, a Materialize, a row lock,
/// an Append or MergeAppend member, a Gather).
pub(crate) fn whole_read(b: &Builder<'_, '_>, id: i32) -> bool {
    fn go(n: Node<'_>, id: i32) -> Option<bool> {
        crate::walk::stack_check(); // D3748
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(true);
        }
        let (found, _) = below(n, |c| go(c, id))?;
        Some(found && reads_whole(n))
    }
    b.stmt.planTree.and_then(|r| go(r, id)).unwrap_or(false)
}

/// `whole_read`'s nodes: each reads its whole input before its own end.
pub(crate) fn reads_whole(n: Node<'_>) -> bool {
    n.as_result().is_some_and(|r| r.resconstantqual.is_none())
        || n.as_project_set().is_some()
        || n.as_subquery_scan().is_some()
        || n.as_sort().is_some()
        || n.as_incremental_sort().is_some()
        || n.as_agg().is_some()
        || n.as_group().is_some()
        || n.as_unique().is_some()
        || n.as_material().is_some()
        || n.as_lock_rows().is_some()
        || n.as_append().is_some()
        || n.as_merge_append().is_some()
        || n.as_gather().is_some()
        || n.as_gather_merge().is_some()
}

/// The first of `n`'s children (its outer, its inner, a subquery scan's plan, the members) for which `f` answers, with
/// whether it is the outer.
pub(crate) fn below<'a, T>(n: Node<'a>, mut f: impl FnMut(Node<'a>) -> Option<T>) -> Option<(T, bool)> {
    let p = n.as_plan()?;
    let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
    let sub = n.as_subquery_scan().and_then(|s| s.subplan);
    let found = [(p.lefttree, true), (p.righttree, false), (sub, true)].into_iter().filter_map(|(c, o)| Some((c?, o)));
    found.chain(members.into_iter().flat_map(|l| l.iter().map(|c| (c, true)))).find_map(|(c, o)| f(c).map(|t| (t, o)))
}
