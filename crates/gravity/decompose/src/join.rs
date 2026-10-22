use core::fmt::{self, Write};

use gravity_abi::breaker::{BreakerKind, Budget, KeyKind, Spill, HASHBUILD_ENTRY};
use gravity_program::limits;
use gravity_program::{ArenaBox, ArenaVec, NumTypmod, WordsDesc};
use types_nodes::jointype::JoinType;
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Hash, HashJoin, Material, Memoize, MergeJoin, NestLoop};
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::ops::shared::agg::Stripped;
use crate::ops::shared::sort::NO_SCALE;
use crate::catalog::hash_eq::builtin_hash_eq;
use crate::expr;
use crate::facts::nest::{eq_conjunct, eq_keyed, NestFacts};
use crate::graph::{Col, Op, Order, OrderKey, PipelineNode};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::ops::shared::sort::column_kind_of_expr;
use crate::ops::shared::sort::{keys_layout, ROW_LAYOUT};
use crate::catalog::typefacts::{self, KeyColl, WordImage};
use crate::walk::{Builder, Open, STMT_NODE};

pub enum JoinDesc<'p> {
    /// `pg_tuple`: an upper bound on the bytes PG 18.6's hash table counts per tuple of this build (ExecHashTableInsert's
    /// `HJTUPLE_OVERHEAD + t_len` over the Hash's targetlist), `u32::MAX` when a column is not of fixed width or its type is
    /// unknown. No lowering decision reads it since D2713 (lowerv4 hashjoin/probe_kept.rs keeps a table at any size); it
    /// stays in the content key (lower inputs key.rs) until that home drops it.
    Build { keys: &'p NodeList<'p>, decls: ArenaVec<JoinKeyDecl<'p>>, pg_tuple: u32 },
    /// `inner_node`: the plan node the inner rows come from (the Hash, the merge inner, the nest inner's root), ENTRY's counters.
    /// `outer_first`: PG's ExecHashJoin fetches the first outer tuple before it builds (HJ_BUILD_HASHTABLE: not FILL_INNER, not
    /// a parallel-aware shared hash, and FILL_OUTER or the outer's startup cost under the Hash's total cost), so an empty
    /// outer never runs the build side (D237; the build's errors are deferred to the first row that reaches the probe).
    Probe { slot: u8, jointype: JoinType, clauses: &'p NodeList<'p>, keys: &'p NodeList<'p>, qual: &'p NodeList<'p>, unique: bool, marks: bool, decls: ArenaVec<JoinKeyDecl<'p>>, inner_node: i32, outer_first: bool, catalog: bool },
    Drain { matches: u8, jointype: JoinType },
    /// `families` / `collations`: each clause's btree operator family and collation (PG's `mergeFamilies` /
    /// `mergeCollations`): the compare the join runs is that family's support function under that collation (M11).
    Merge { slot: u8, jointype: JoinType, clauses: &'p NodeList<'p>, reversals: &'p [bool], nulls_first: &'p [bool], families: &'p [u32], collations: &'p [u32], qual: &'p NodeList<'p>, unique: bool, inner_node: i32 },
    Nest { jointype: JoinType, params: &'p NodeList<'p>, inner: ArenaBox<PipelineNode<'p>>, qual: &'p NodeList<'p>, unique: bool, inner_node: i32, facts: NestFacts<'p> },
    /// `ops` / `colls`: each key's hash equality operator and collation (`Memoize.hashOperators` / `.collations`, nest.memo).
    /// `purge`: PostgreSQL 18.6's ExecReScanMemoize purges the whole cache when the child's changed params hold one outside
    /// `keyparamids`: the lifetime keep table's Memoize arm at the widest rescan (`lifetime::at_any_rescan`: the child's
    /// `extParam` holds a param no key reads and the statement does not fix).
    /// `nonkey`: those params (`memo_nonkey`), whose owners say where the cache must purge.
    Memo { keys: &'p NodeList<'p>, entries: u32, singlerow: bool, binary: bool, ops: &'p [u32], colls: &'p [u32], purge: bool, nonkey: &'p [i32] },
    Spool,
}

/// A hash join key pair (lowerv3 design 2.2): the two sides' expressions and their unified key.
#[derive(Clone, Copy, Debug)]
pub struct JoinKeyDecl<'p> {
    pub outer: Stripped<'p>,
    pub inner: Stripped<'p>,
    pub key: JoinKey,
    /// D2849: where PostgreSQL's hash of the key raises (`key_raise`).
    pub raise: KeyRaise,
}

/// D2849: where PostgreSQL 18.6's hash of a join key raises 42P22 ("could not determine which collation to use for string
/// hashing"): a key its clause compares under no collation (two conflicting implicit collations) whose hash proc reads
/// one. PG hashes a key where its Hash node inserts an inner row and where the join takes an outer row
/// (ExecHashJoinOuterGetTuple), and never a NULL key (strict hashing, or a kept NULL's hash 0); the lowering raises there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyRaise {
    /// The key's hash raises nothing.
    #[default]
    None,
    /// A text / varchar / bpchar key (`hashtext` / `hashbpchar`, effects::agg `hash_raises`): at a non-NULL value.
    Value,
    /// A built-in array of one (`hash_array` calls the element type's hash per element): at a non-NULL ELEMENT, so a NULL
    /// array, an empty one or one of only NULL elements hashes quietly.
    Elems,
}

/// D2849 (`KeyRaise`): how PG's hash of a key of type `typid` under clause collation `coll` raises; `None` for a raise
/// `KeyRaise` does not name (the caller keeps D1292's refusal).
pub fn key_raise(facts: &dyn crate::catalog::Facts, typid: u32, coll: u32) -> Option<KeyRaise> {
    typefacts::facts_of(facts, typid).map(|t| key_raise_of(&t, coll)).filter(|&r| r != KeyRaise::None)
}

/// `key_raise` over the key type's facts `t` (the lowering's hash join keys read it where they hash). An array's element is
/// read off the built-in string arrays only (pg_type.dat's `_text`, `_varchar`, `_bpchar`, whose elements hash by
/// `hashtext` / `hashbpchar`).
pub fn key_raise_of(t: &typefacts::TypeFacts, coll: u32) -> KeyRaise {
    const VARCHARARRAYOID: u32 = 1015;
    const BPCHARARRAYOID: u32 = 1014;
    match t.typid {
        _ if crate::effects::agg::hash_raises_of(t, coll) => KeyRaise::Value,
        types_core::TEXTARRAYOID | VARCHARARRAYOID | BPCHARARRAYOID if coll == 0 => KeyRaise::Elems,
        _ => KeyRaise::None,
    }
}

/// The kind both sides' words take (`None`: they cannot be made equal), a numeric pair's common scale, the clause's
/// collation and the dictionary rule (4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinKey {
    pub kind: Option<KeyKind>,
    pub image: Option<WordImage>,
    pub scale: i16,
    pub coll: KeyColl,
    pub dict: JoinDict,
    pub types: (u32, u32),
}

/// A code on either side keys both sides by the fingerprint of the rendered string, never by the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinDict {
    None,
    Fingerprint,
}

/// One side of a key: its type, numeric facts and whether it is a dictionary code column.
#[derive(Clone, Copy, Debug)]
pub struct Side {
    pub typid: Option<u32>,
    pub num: NumTypmod,
    pub code: bool,
}

pub use crate::catalog::keysem::image::pair;

pub use gravity_program::cost::spill_bits;

/// The join family, one row per `JoinType` (decompose.md §2.7's matrix): `drains` — the inner's unmatched (or, for
/// right-semi, matched) entries are emitted after the probe (`marks` + `Drain` under a hash join; a merge walks both
/// sides in one op); `nest` — a nested loop may carry it (the executor drives no right-form loop: the planner builds
/// none); `planned` — false for the types that never leave the planner. A new type is a row here.
pub struct JoinForm {
    pub jt: JoinType,
    pub drains: bool,
    pub nest: bool,
    pub planned: bool,
}

pub const JOIN_FORMS: [JoinForm; 10] = [
    JoinForm { jt: JoinType::JOIN_INNER, drains: false, nest: true, planned: true },
    JoinForm { jt: JoinType::JOIN_LEFT, drains: false, nest: true, planned: true },
    JoinForm { jt: JoinType::JOIN_SEMI, drains: false, nest: true, planned: true },
    JoinForm { jt: JoinType::JOIN_ANTI, drains: false, nest: true, planned: true },
    JoinForm { jt: JoinType::JOIN_RIGHT, drains: true, nest: false, planned: true },
    JoinForm { jt: JoinType::JOIN_FULL, drains: true, nest: false, planned: true },
    JoinForm { jt: JoinType::JOIN_RIGHT_SEMI, drains: true, nest: false, planned: true },
    JoinForm { jt: JoinType::JOIN_RIGHT_ANTI, drains: true, nest: false, planned: true },
    JoinForm { jt: JoinType::JOIN_UNIQUE_OUTER, drains: false, nest: false, planned: false },
    JoinForm { jt: JoinType::JOIN_UNIQUE_INNER, drains: false, nest: false, planned: false },
];

fn join_form(jt: JoinType, k: Kind, id: i32) -> Result<&'static JoinForm, Refusal> {
    JOIN_FORMS.iter().find(|f| f.jt == jt && f.planned).ok_or(Refusal::shape(k, id, "a planner-internal join type"))
}

/// Whether a hash join of type `jt` drains (its probe pipeline closes into the drain's spool inside `map_hash_join`):
/// its probe's SubPlans go on the probe there (subplan.rs `attach_probe`), not at the node's `attach`.
pub fn hash_drains(jt: JoinType) -> bool {
    join_form(jt, Kind::HashJoin, STMT_NODE).is_ok_and(|f| f.drains)
}

fn bits_word(b: &mut Builder<'_, '_>, k: Kind, id: i32) -> Result<WordsDesc, Refusal> {
    let w = b.word_block(1).ok_or(Refusal::shape(k, id, "more kernel words than the word plan holds (`limits::KERNEL_WORDS`)"))?;
    Ok(WordsDesc::bits_only(w, w + 1))
}

/// D1901's Shape at a hash join (V4R-508's in-row refusal): a key holding a multirange under a clause operator that is
/// not a built-in hashable equality.
pub const USER_HASH_OP: &str = "a multirange hash join key under a user equality operator or hash family (D1901)";
/// D1901's Shape at a group (agg.rs `map`): an Agg / Group / Unique key holding a multirange under a user equality operator.
pub const USER_GROUP_OP: &str = "a multirange grouping key under a user equality operator (D1901)";

/// `effects::user_hash_clause` of an operator clause.
pub(crate) fn hash_valued(facts: &dyn crate::catalog::Facts, o: &types_nodes::primnodes::OpExpr<'_>) -> bool {
    if !builtin_hash_eq(facts, o.opno) {
        return true;
    }
    let mut t = o.args.iter().map(crate::catalog::expr_type::type_of);
    match (t.next().flatten(), t.next().flatten(), t.next()) {
        (Some(a), Some(b), None) => !crate::catalog::keysem::image::images_pair(facts, (a, b), facts.coll(o.inputcollid)),
        _ => false,
    }
}

pub use crate::catalog::keysem::image::{images_pair, name_text, one_integer, same_rep};

/// D2036's Shapes (`user_keys`).
pub const USER_HASH_FNS: &str = "a hash join under a user hash operator whose operator or hash functions are volatile or unknown (D2036)";
pub const USER_HASH_KEY_EXPR: &str = "a hash join under a user hash operator over a volatile or SubPlan key (D2036)";

/// D2036 (D1901's trunk half): PostgreSQL's hash join compares a hash clause by its operator and buckets it by the
/// operator's hash functions (nodeHashjoin.c ExecInitHashJoin: `get_op_hash_functions`, the outer's the left input's,
/// the inner's the right's; ExecScanHashBucket runs the clauses on each tuple of an equal hash value), which no key image
/// keeps for an operator that is no builtin hashable equality, nor (D2725) for a builtin one across two types whose images
/// do not pair (`effects::user_hash_clause`). Such a clause's keys become that hash value: each side's key
/// `x` reads `COALESCE(hash(x), CASE WHEN x IS NULL THEN n ELSE 0 END)`, or `CASE WHEN x IS NULL THEN n ELSE
/// COALESCE(hash(x), 0) END` for a hash function that is not strict (PG never calls it on NULL), where a NULL key reads
/// `n`: NULL under a strict operator (its hash expression skips the row: EEOP_HASHDATUM_FIRST_STRICT), 0 under another
/// (EEOP_HASHDATUM_FIRST hashes it as 0, the row kept and the clause run on it); a hash call that returns NULL reads 0
/// (its datum). Every other clause keeps its keys (their images compare their values). Equal images are PG's hash-equal
/// candidates and the probe runs the clauses, in order, on each (`effects::user_hash_clause`). Several keys (D2726): PG compares
/// their combined hash value, the probe each key's image, so a clause runs on the tuples PG runs it on (the clauses before
/// it true: their keys equal, so their hash values equal) less those whose later keys' combined value only collides: a
/// call count, and a call count is free. Refused by name: an operator or hash function VOLATILE or unknown to the facts, a
/// volatile key or one holding a SubPlan, or no node allocator. `None`: every clause is a builtin hashable equality whose
/// images pair, or the facts read no hash functions (a plan capture, `Facts::reads_hash_functions`).
fn user_keys<'p>(b: &Builder<'p, '_>, hj: &'p HashJoin<'p>, hash: &'p Hash<'p>, k: Kind, id: i32) -> Result<Option<(&'p NodeList<'p>, &'p NodeList<'p>)>, Refusal> {
    use types_nodes::primnodes::{CaseExpr, CaseWhen, CoalesceExpr, FuncExpr, NullTest, NullTestType, OpExpr};
    if !b.facts.reads_hash_functions() || !hj.hashclauses.iter().any(|c| crate::effects::user_hash_clause(b.facts, c)) {
        return Ok(None);
    }
    let shape = |why: &'static str| Refusal::shape(k, id, why);
    let n = hj.hashclauses.len();
    if hj.hashkeys.len() != n || hash.hashkeys.len() != n {
        return Err(shape("a hash join whose hash clauses are not its keys (D2036)"));
    }
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let strict = |f: u32| b.facts.func(f).is_some_and(|p| p.strict);
    let calm = |x: Node<'p>| !crate::expr::has_subplan(x) && !crate::effects::of_expr(&cx, x, &crate::effects::PLAIN).acts;
    // each hash-valued clause with its hash functions (outer, inner), checked before any node is made
    let mut valued: ArenaVec<Option<(&OpExpr<'p>, u32, u32)>> = ArenaVec::with_capacity_in(n, b.arena); // alloc: Cold
    for (i, c) in hj.hashclauses.iter().enumerate() {
        if !crate::effects::user_hash_clause(b.facts, c) {
            valued.push(None);
            continue;
        }
        let Some(c) = c.as_op_expr().filter(|o| o.args.len() == 2) else { return Err(shape(USER_HASH_FNS)) };
        let (lh, rh) = crate::catalog::keysem::hash_join_procs(b.facts, c.opno, c.inputcollid).ok_or(shape(USER_HASH_FNS))?;
        if [c.opfuncid, lh, rh].into_iter().any(|f| crate::effects::of_call(b.facts, f).acts) {
            return Err(shape(USER_HASH_FNS));
        }
        if !calm(hj.hashkeys.nth(i)) || !calm(hash.hashkeys.nth(i)) {
            return Err(shape(USER_HASH_KEY_EXPR));
        }
        valued.push(Some((c, lh, rh)));
    }
    let mcx = b.alloc.ok_or(shape("a hash join under a user hash operator: no node allocator (decompose_in; D2036)"))?;
    let key = |x: Node<'p>, h: u32, c: &OpExpr<'p>| -> types_error::PgResult<Node<'p>> {
        let skip = strict(c.opfuncid);
        let int4 = |v: i32, null: bool| Node::mk_const(mcx, 23, -1, 0, 4, datum::Datum::from_i32(v), null, true);
        let call = Node::mk(mcx, FuncExpr { funcid: h, funcresulttype: 23, inputcollid: c.inputcollid, args: NodeList::make1(mcx, x)?, location: -1, ..Default::default() })?;
        let coalesce = |a: Node<'p>, z: Node<'p>| Node::mk(mcx, CoalesceExpr { coalescetype: 23, coalescecollid: 0, args: NodeList::make2(mcx, a, z)?, location: -1 });
        let test = Node::mk(mcx, NullTest { arg: Some(x), nulltesttype: NullTestType::IS_NULL, argisrow: false, location: -1 })?;
        let when = Node::mk(mcx, CaseWhen { expr: Some(test), result: Some(int4(0, skip)?), location: -1 })?;
        let other = if strict(h) { int4(0, false)? } else { coalesce(call, int4(0, false)?)? };
        let case = Node::mk(mcx, CaseExpr { casetype: 23, casecollid: 0, arg: None, args: NodeList::make1(mcx, when)?, defresult: Some(other), location: -1 })?;
        if strict(h) { coalesce(call, case) } else { Ok(case) }
    };
    let lists = || -> types_error::PgResult<(&'p NodeList<'p>, &'p NodeList<'p>)> {
        let (mut o, mut i) = (NodeList::with_capacity(mcx, n)?, NodeList::with_capacity(mcx, n)?);
        for (j, v) in valued.iter().enumerate() {
            let (oe, ie) = (hj.hashkeys.nth(j), hash.hashkeys.nth(j));
            let (oe, ie) = match *v {
                Some((c, lh, rh)) => (key(oe, lh, c)?, key(ie, rh, c)?),
                None => (oe, ie),
            };
            o.lappend(mcx, oe)?;
            i.lappend(mcx, ie)?;
        }
        let list = |l| -> types_error::PgResult<&'p NodeList<'p>> { Ok(Node::mk_list(mcx, l)?.as_list().expect("a list")) };
        Ok((list(o)?, list(i)?))
    };
    lists().map(Some).map_err(|_| shape("a hash join under a user hash operator: its key nodes (out of memory; D2036)"))
}

fn key_decls<'p>(b: &Builder<'p, '_>, probe: &Open<'p>, hj: &'p HashJoin<'p>, okeys: &'p NodeList<'p>, inner: &[(Stripped<'p>, Side)], k: Kind, id: i32) -> Result<ArenaVec<JoinKeyDecl<'p>>, Refusal> {
    let mut v = ArenaVec::with_capacity_in(inner.len(), b.arena); // alloc: Cold
    for (i, (e, &(ie, side))) in okeys.iter().zip(inner.iter()).enumerate() {
        let clause = hj.hashclauses.iter().nth(i).and_then(|c| c.as_op_expr());
        // D2036: a hash-valued clause's keys are its int4 hash values (`user_keys`), under no collation
        let coll = clause.filter(|o| builtin_hash_eq(b.facts, o.opno) && !(b.facts.reads_hash_functions() && hash_valued(b.facts, o))).map_or(0, |o| o.inputcollid);
        let (oe, oside) = crate::ops::agg::key_side(b, probe, e);
        // D1901 (V4R-508's in-row refusal): a multirange key keys by multirange_eq's canonical form, PostgreSQL by the
        // clause's operator (D2036: a user operator's keys are its hash values, never a multirange)
        let opno = clause.map(|o| o.opno).or_else(|| hj.hashoperators.as_slice().get(i).copied()).unwrap_or(0);
        if crate::catalog::keysem::hash_join_multirange(b.facts, opno, [oside.typid, side.typid]) {
            return Err(Refusal::shape(k, id, USER_HASH_OP));
        }
        // D1353: a key that is bpchar only through a relabel of a varchar / text value (`f.b = d.v::bpchar`) keys as bpchar,
        // its clause's type: `key_side` types that side bpchar (D1309) and lowerv4 keys.rs takes the key's facts from the
        // unstripped hash key, so both sides image hashbpchar's trimmed bytes; a code lane of another type refuses there
        // (keys_bpchar.rs: its Fp128 table is the untrimmed image)
        // D1292: texteq / hashtext raise under no collation. D2849: an inner or semi join whose key's hash raises where
        // `KeyRaise` names (one type on both sides, PG's own hashable `=`) keys it, and the lowering raises where PG
        // hashes; any other stays refused
        let raise = match oside.typid.filter(|&t| crate::ops::shared::sort::indeterminate(b.facts, t, coll)) {
            None => KeyRaise::None,
            Some(t) => match key_raise(b.facts, t, coll) {
                Some(r) if side.typid == Some(t) && builtin_hash_eq(b.facts, opno) && matches!(hj.join.jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI) => r,
                _ => return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL)),
            },
        };
        v.push(JoinKeyDecl { outer: oe, inner: ie, key: pair(b.facts, oside, side, coll), raise });
    }
    Ok(v)
}

/// PG 18.6 nodeHashjoin.c HJ_BUILD_HASHTABLE: whether the first outer tuple is fetched before the hash table is built.
/// HJ_FILL_INNER (a null outer slot: RIGHT, RIGHT ANTI, FULL) and a parallel-aware join (a shared table) build first;
/// otherwise HJ_FILL_OUTER (a null inner slot: LEFT, ANTI, FULL) or the outer's startup cost under the Hash's total cost
/// fetch first (`hj_OuterNotEmpty` is false on a first scan; rescans keep it, nest kinds).
pub fn outer_first(jt: JoinType, parallel_aware: bool, outer_startup: f64, hash_total: f64) -> bool {
    let fill_inner = matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_FULL);
    let fill_outer = matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL);
    !fill_inner && !parallel_aware && (fill_outer || outer_startup < hash_total)
}

/// An upper bound on the bytes PG 18.6's hash table counts for one tuple of a Hash node's targetlist
/// (ExecHashTableInsert: `HJTUPLE_OVERHEAD` 16 + the minimal tuple's `t_len`, its header `MAXALIGN(15 + null bitmap)` and
/// each column at most its length plus 7 bytes of alignment); `u32::MAX` when a column has no bounded width or its type is
/// not known (the live catalog's `type_row`; fixtures carry none).
fn pg_tuple(b: &Builder<'_, '_>, hash: &types_nodes::plannodes::Plan<'_>) -> u32 {
    let mut data = 0u64;
    let mut natts = 0u64;
    for te in hash.targetlist.iter() {
        let Some(len) = crate::catalog::expr_type::type_of(te).and_then(|t| max_width(b.facts, t)) else { return u32::MAX };
        data += len as u64 + 7;
        natts += 1;
    }
    let header = (15 + natts.div_ceil(8) + 7) & !7;
    u32::try_from(16 + header + data).unwrap_or(u32::MAX)
}

/// The most bytes a datum of type `t` (a domain read through to its base) takes in a tuple: its length when fixed, and
/// for a range over a fixed-width subtype (K2.kh1e / kh1x: a hash join keyed by a user range) PG 18.6's range_serialize
/// size with both bounds present and the widest alignment: the 4-byte varlena header and `rangetypid` (8), each bound
/// aligned to 8 then its length, and the flags byte (a datum no wider than that is never compressed or toasted, and a
/// short header only shrinks it). None for any other type of no fixed width (text, numeric, a multirange: unbounded).
pub(crate) fn max_width(f: &dyn crate::catalog::Facts, t: u32) -> Option<u64> {
    let r = f.type_row(t)?;
    if r.typlen > 0 {
        return Some(r.typlen as u64);
    }
    let base = if r.typtype == b'd' { r.base } else { t };
    let sub = f.range_subtype(base).and_then(|s| f.type_row(s)).map(|s| s.typlen).filter(|&l| l > 0)? as u64;
    let a8 = |n: u64| (n + 7) & !7;
    Some(a8(8 + sub) + sub + 1)
}

fn map_hash_join<'p>(b: &mut Builder<'p, '_>, hj: &'p HashJoin<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&hj.join.plan, Kind::HashJoin);
    let id = plan.plan_node_id;
    let form = join_form(hj.join.jointype, k, id)?;
    let jt = form.jt;
    let hash: &'p Hash<'p> = plan.righttree.and_then(|n| n.as_hash()).ok_or(Refusal::shape(k, id, "the inner is not a Hash"))?;
    let outer = plan.lefttree.ok_or(Refusal::shape(k, id, "no outer subtree"))?;
    let hid = hash.plan.plan_node_id;
    if plan.righttree.is_some_and(|h| crate::rerun::of_build(b.stmt, b.facts, b.arena, h).user_call) {
        b.graph.user_hashes.push(hid); // alloc: Cold
    }
    // D2036: a hash clause under no builtin hashable equality keys both sides by PG's hash value (`user_keys`)
    let (okeys, ikeys): (&'p NodeList<'p>, &'p NodeList<'p>) = user_keys(b, hj, hash, k, id)?.unwrap_or((&hj.hashkeys, &hash.hashkeys));
    let mut build = b.child(&hash.plan, Kind::Hash)?;
    let eqs = crate::ops::shared::order::eqs_of(b, &build); // D3856: an inner probe's (order.rs `step`)
    if !hash.plan.qual.is_nil() {
        return Err(Refusal::shape(Kind::Hash, hid, "a qual on Hash"));
    }
    let layout = keys_layout(b.arena, ikeys.iter().map(|e| column_kind_of_expr(b.facts, e)));
    let mut sides: ArenaVec<(Stripped<'p>, Side)> = ArenaVec::new_in(b.arena); // alloc: Cold
    sides.extend(ikeys.iter().map(|e| crate::ops::agg::key_side(b, &build, e)));
    build.push(Op::Join(JoinDesc::Build { keys: ikeys, decls: ArenaVec::new_in(b.arena), pg_tuple: pg_tuple(b, &hash.plan) }));
    build.desc.words = Some(bits_word(b, Kind::Hash, hid)?);
    // V4R-481: the build is sized by the rows its Hash receives, not by its driving scan's (`predicted_rows`)
    build.desc.build_rows = Builder::rows(&hash.plan);
    let bits = spill_bits(Builder::rows(&hash.plan), HASHBUILD_ENTRY as u32);
    // the inner keys' SubPlans, before the build's end keys the row (PG's Hash node evaluates them per inserted row)
    if hash.hashkeys.iter().any(crate::expr::has_subplan) {
        let cur = b.graph.cur_node.take(); // a per-row SubPlan's hashed set is no node's breaker (walk.rs `map_node`)
        let r = crate::ops::subplan::attach_keys(b, &hash.hashkeys, Kind::Hash, hid, build);
        b.graph.cur_node = cur;
        build = r?;
    }
    build.close_node(hid); // the Hash is its build's last node, its only entry (no consumer pipeline lists it)
    // D3307: a build whose within-key match order PostgreSQL's reader can see stays serial, its chains PostgreSQL's
    // (facts/chain_order.rs; the reason is the catalog build's: records in the inner's scan order)
    if build.desc.is_eligible() && crate::facts::chain_order::visible(b, hj) {
        let site = build.desc.driving_site.max(0);
        build.desc = gravity_program::Pipeline { parallel: gravity_program::Parallel::Ineligible { site, reason: gravity_program::Reason::SortedSource }, ..build.desc };
    }
    let cur = b.graph.cur_node.replace(hid);
    let declared = b.graph.breakers.len();
    let slot = crate::ops::shared::breaker::close(b, build, BreakerKind::HashBuild, layout, Budget::Share, Spill::Partitioned { bits }, Kind::Hash, hid);
    b.graph.cur_node = cur;
    let slot = slot?;
    for d in b.graph.breakers[declared..].iter_mut() {
        d.order = eqs;
    }
    b.closed_by(plan.righttree.unwrap());
    b.graph.note(Kind::Hash);
    let probe = b.plan(outer)?;
    let mut probe = crate::ops::shared::breaker::respool(b, probe, k, id)?;
    b.closed_by(outer);
    crate::ops::shared::breaker::consume(b, &mut probe, slot);
    let decls = key_decls(b, &probe, hj, okeys, &sides, k, id)?;
    // D2849: the keys whose hash raises, both sides' nodes (lowerv4 effects `raise_free` reads them)
    for ((d, o), i) in decls.iter().zip(okeys.iter()).zip(ikeys.iter()) {
        if d.raise != KeyRaise::None {
            b.graph.raising_keys.extend([o, i]); // alloc: Cold
        }
    }
    if let Some(d) = crate::breakers::decl_of(&b.graph, slot).map(|d| d.producer as usize) {
        for op in b.graph.pipelines[d].ops.iter_mut() {
            if let Op::Join(JoinDesc::Build { decls: bd, .. }) = op {
                bd.extend(decls.iter().copied());
            }
        }
    }
    let outer_first = outer_first(jt, plan.parallel_aware, outer.as_plan().map_or(0.0, |o| o.startup_cost), hash.plan.total_cost);
    probe.push(Op::Join(JoinDesc::Probe { slot, jointype: jt, clauses: &hj.hashclauses, keys: okeys, qual: &hj.join.joinqual, unique: hj.join.inner_unique, marks: form.drains, decls, inner_node: hid, outer_first, catalog: false }));
    // a draining join's probe closes into the spool below: its join qual's and outer keys' SubPlans go on it first
    if form.drains && (hj.join.joinqual.iter().any(crate::expr::has_subplan) || hj.hashkeys.iter().any(crate::expr::has_subplan)) {
        let cur = b.graph.cur_node.take(); // a per-row SubPlan's hashed set is no node's breaker (walk.rs `map_node`)
        let r = crate::ops::subplan::attach_probe(b, hj, k, id, probe);
        b.graph.cur_node = cur;
        probe = r?;
    }
    let mut out = if form.drains {
        let m = crate::ops::shared::breaker::close(b, probe, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)?;
        let mut out = crate::ops::shared::breaker::emit(b, id, slot);
        crate::ops::shared::breaker::consume(b, &mut out, m);
        out.push(Op::Join(JoinDesc::Drain { matches: m, jointype: jt }));
        out
    } else {
        probe
    };
    // D3510: a RIGHT SEMI drain's qual and targetlist read a probe column as the build key its hash clause equates it to,
    // and a bare probe column no reader above reads as a NULL (a plain SELECT: a write or a row lock over the drain hands
    // the probe row on in the spooled form, lowerv4 drain/lower.rs `semi_hands`)
    let plain = b.stmt.commandType == types_nodes::nodes_enums::CmdType::CMD_SELECT && b.stmt.rowMarks.iter().next().is_none() && !b.stmt.hasModifyingCTE;
    let (qual, tlist) = match form.drains && jt == JoinType::JOIN_RIGHT_SEMI && plain {
        true => {
            let tlist = semi_build_keys(b, hj, &plan.targetlist, k, id)?;
            (semi_build_keys(b, hj, &plan.qual, k, id)?, crate::facts::demand::readers::rsemi_unread(b, hj, tlist))
        }
        false => (&plan.qual, &plan.targetlist),
    };
    ops::filters(&mut out, qual);
    ops::project(&mut out, if jt == JoinType::JOIN_SEMI { crate::facts::demand::readers::semi_unread(b, hj) } else { tlist });
    b.graph.note(k);
    Ok(out)
}

/// The order a merge join's side must arrive in: the clauses' columns on that side (`OUTER_VAR` left, `INNER_VAR`
/// right, through relabels) with the plan's reversal and null placement.
fn merge_side_order(facts: &dyn crate::catalog::Facts, mj: &MergeJoin<'_>, varno: i32, k: Kind, id: i32) -> Result<Order, Refusal> {
    let n = mj.mergeclauses.len();
    if mj.mergeReversals.len() < n || mj.mergeNullsFirst.len() < n || mj.mergeFamilies.len() < n || mj.mergeCollations.len() < n {
        return Err(Refusal::shape(k, id, "merge arrays shorter than the clauses"));
    }
    let mut o = Order::NONE;
    for (i, c) in mj.mergeclauses.iter().enumerate() {
        let op = c.as_op_expr().filter(|o| o.args.len() == 2).ok_or(Refusal::shape(k, id, "a merge clause that is not a binary operator"))?;
        let arg = op.args.nth(if varno == OUTER_VAR { 0 } else { 1 });
        // D858 (aggregates#241): a relabel between int4 and a zero-extended 4-byte type re-forms the word, so the column's
        // order (int4's signed one) is not the clause's (oid's unsigned one). The side asks the clause family's order of
        // the column itself, which only a Sort by that family's operator declares (sort.rs `reformed_family`: PG sorts
        // the column by oid's `<`, btoidcmp reading its word unsigned); the clause's compare reads the relabel (gravity_expr
        // `Canon` re-forms the word), so both order and compare are PG's.
        // D3630: judged by base types, so a domain over oid relabelled to int4 re-forms too
        let reformed = arg.as_relabel_type().filter(|_| expr::through_relabel_in(facts, arg).is_none()).map(|r| r.arg).filter(|a| Col::of(*a).is_some());
        let col = Col::of(reformed.unwrap_or(arg)).filter(|c| c.varno == varno).ok_or(Refusal::shape(k, id, "a merge clause over an expression rather than a column"))?;
        if crate::catalog::expr_type::type_of(arg).is_some_and(|t| crate::ops::shared::sort::indeterminate(facts, t, mj.mergeCollations[i])) {
            return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL)); // D1292
        }
        // record-image order (matview#113): a clause of a family that is not its side's type's default asks that family's
        // order; a re-formed column's (D858) is never its type's default
        let fam = match reformed {
            Some(_) => mj.mergeFamilies[i],
            None => crate::catalog::expr_type::type_of(arg).and_then(|t| facts.default_family(t)).map_or(0, |d| if d == mj.mergeFamilies[i] { 0 } else { mj.mergeFamilies[i] }),
        };
        o.push_fam(OrderKey { col: Col::outer(col.attno), desc: mj.mergeReversals[i], nulls_first: mj.mergeNullsFirst[i] }, fam);
    }
    Ok(o)
}

fn map_merge_join<'p>(b: &mut Builder<'p, '_>, mj: &'p MergeJoin<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&mj.join.plan, Kind::MergeJoin);
    let id = plan.plan_node_id;
    let jt = join_form(mj.join.jointype, k, id)?.jt;
    let inner = plan.righttree.ok_or(Refusal::shape(k, id, "no inner subtree"))?;
    let outer = plan.lefttree.ok_or(Refusal::shape(k, id, "no outer subtree"))?;
    let (want_outer, want_inner) = (merge_side_order(b.facts, mj, OUTER_VAR, k, id)?, merge_side_order(b.facts, mj, INNER_VAR, k, id)?);
    let inner_open = b.plan(inner)?;
    crate::ops::shared::order::ordered_as_order(&crate::ops::shared::order::order_of(b, &inner_open), crate::ops::shared::order::projection(&inner_open), &want_inner, k, id, "a merge join's inner with no known order", "a merge join's inner not in the clauses' order")?;
    let slot = crate::ops::shared::breaker::spool_run(b, inner_open, k, id)?;
    crate::ops::mergejoin::ask_exact(b, inner); // jmerge-0012: an acting scan inner, pulled as PG fetches it
    b.closed_by(inner);
    let probe = b.plan(outer)?;
    let mut probe = crate::ops::shared::breaker::respool(b, probe, k, id)?;
    b.closed_by(outer);
    crate::ops::shared::order::ordered_as_order(&crate::ops::shared::order::order_of(b, &probe), crate::ops::shared::order::projection(&probe), &want_outer, k, id, "a merge join's outer with no known order", "a merge join's outer not in the clauses' order")?;
    crate::ops::shared::breaker::consume(b, &mut probe, slot);
    probe.push(Op::Join(JoinDesc::Merge { slot, jointype: jt, clauses: &mj.mergeclauses, reversals: mj.mergeReversals, nulls_first: mj.mergeNullsFirst, families: mj.mergeFamilies, collations: mj.mergeCollations, qual: &mj.join.joinqual, unique: mj.join.inner_unique, inner_node: inner.as_plan().map_or(STMT_NODE, |p| p.plan_node_id) }));
    ops::filters(&mut probe, &plan.qual);
    ops::project(&mut probe, &plan.targetlist);
    b.graph.note(k);
    Ok(probe)
}

fn map_nest_loop<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&nl.join.plan, Kind::NestLoop);
    let id = plan.plan_node_id;
    let form = join_form(nl.join.jointype, k, id)?;
    let jt = form.jt;
    if !form.nest {
        return Err(Refusal::shape(k, id, "a right-driven nested loop"));
    }
    let inner = plan.righttree.ok_or(Refusal::shape(k, id, "no inner subtree"))?;
    let outer = plan.lefttree.ok_or(Refusal::shape(k, id, "no outer subtree"))?;
    let outer_plan = outer;
    let mut outer = b.plan(outer)?;
    let p0 = b.graph.pipelines.len();
    let inner_node = inner.as_plan().map_or(STMT_NODE, |p| p.plan_node_id);
    let inner_plan = inner;
    // D3646: the inner's slots stay held while it is mapped and leased for the statement once it is (`breakers::hold_range`,
    // `release_range`). D4584: every nest's inner, a param-free one too: it runs inside its owner's pipeline as well (PG
    // fetches the outer first, so the inner's fills build at its first re-open and a hash build it keeps stays live through
    // the owner), so the owner's own breaker (an ORDER BY's Sort over a cross join of two aggregates) took the
    // slot a fill of the inner opens ('slot produced again', L3) when only a NestLoopParam inner was held.
    let held = crate::breakers::hold_range(&mut b.graph, p0);
    // D3227 K3: a plan Material inner the keyed form may read through maps as its child's rows alone
    let through = crate::self_spool::through_material(b, nl, inner_plan, &outer, eq_keyed(b, &nl.join.joinqual));
    let saved = core::mem::replace(&mut b.keyed_material, through.map(|m| m.plan.plan_node_id));
    let inner = b.plan(inner).map(|i| crate::ops::shared::union::settle(&mut b.graph, i)); // embedded: one pipeline
    b.keyed_material = saved;
    crate::breakers::release_range(&mut b.graph, p0, held);
    let mut inner = inner?;
    // a settled union's arms were pushed after plan() attributed the inner's pipelines: the inner closed them, so
    // declare_rerun reads its allParam (every child's, as ExecReScanAppend rescans every child)
    b.closed_by(inner_plan);
    // K3: keyed over the Material's child, or the Materialize closed here as `map_material` closes it, before anything
    // reads the inner's pipelines
    let mut through_keyed = None;
    if let Some(m) = through {
        through_keyed = crate::self_spool::keyed(b, nl, inner_plan, &outer, &inner, eq_keyed(b, &nl.join.joinqual), p0);
        if through_keyed.is_none() {
            crate::self_spool::material_close(b, m, inner_plan, &mut inner)?;
        }
    }
    let nest_params = crate::facts::demand::readers::uniq_params(b, nl, &mut inner)?; // V4R-566
    let mut params: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
    params.extend(nest_params.iter().filter_map(|p| p.as_nest_loop_param()).map(|p| p.paramno));
    crate::rerun::declare_rerun(b, p0, id, 0, &params)?;
    if let Some(kd) = through_keyed {
        return map_catalog(b, nl, outer, inner, kd.sides, jt, inner_node, Some((kd.clauses, kd.qual))); // K3: keyed through the Material
    }
    // D3749: the CTEs the outer's first row ran, before a Material inner runs (rerun.rs `of_inner_after`; lowerv4's fill
    // of a Material that reads such a CTE waits for that row: rerun/spool/form.rs `pulls_loud_cte`)
    let (mut whole, mut started) = (ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena)); // alloc: Cold
    if inner_plan.node_tag() == types_nodes::NodeTag::T_Material {
        crate::rerun::first_row(b.stmt, outer_plan, &mut whole, &mut started);
    }
    let f = crate::rerun::of_inner_after(b.stmt, b.facts, b.arena, inner_plan, (&whole, &started));
    let invariant = crate::rerun::invariant(b.stmt, b.arena, inner_plan, f);
    if let crate::graph::Source::Breaker { slot } = &inner.source {
        // D3014 build-once: a parameter-free inner read from its own breaker (an aggregate's or a sort's emit) is built once
        // into the keyed form's table, before any rerun set would re-run its fill per outer row
        if let Some(kd) = crate::self_spool::keyed(b, nl, inner_plan, &outer, &inner, eq_keyed(b, &nl.join.joinqual), p0) {
            return map_catalog(b, nl, outer, inner, kd.sides, jt, inner_node, Some((kd.clauses, kd.qual)));
        }
        let right = match inner.ops.first() {
            Some(Op::SetOp(crate::ops::shared::setop::SetDesc::Set { right, .. })) => Some(*right),
            _ => None,
        };
        crate::rerun::declare_breaker_rerun(b, p0, (id, 0), *slot, right, crate::effects::Effects::of_found(f).acts); // D1018, item 9 (a1) / (b)
    }
    // a nest on another nest's inner side keeps the Nest form: lowerv4 serves no catalog probe among a nest inner's steps
    // (rerun/inner_probe.rs `ok`), where the Nest form serves (a key lookup nest with a constant-pattern filter under a semi nest)
    if let Some(clause) = catalog_lookup(b, nl, &outer, nest_params, &inner).filter(|_| !on_nest_inner(b, nl)) {
        return map_catalog(b, nl, outer, inner, clause, jt, inner_node, None);
    }
    // D2013 slice 2: a SubPlan in the inner's index keys or pruning, over the nest's parameters, hoisted onto the outer row
    let hoisted = crate::facts::hoist::plan(b, nl, nest_params, p0, &mut inner, &outer)?;
    let nest_params = hoisted.as_ref().map_or(nest_params, |h| h.params);
    if let Some(kd) = crate::self_spool::keyed(b, nl, inner_plan, &outer, &inner, eq_keyed(b, &nl.join.joinqual), p0) {
        return map_catalog(b, nl, outer, inner, kd.sides, jt, inner_node, Some((kd.clauses, kd.qual))); // nest.eqkey: the kept inner keyed
    }
    crate::self_spool::wrap(b, nl, inner_plan, &mut inner)?; // nest.selfspool: a kept plain inner read through a synthetic Materialize
    crate::self_spool::wrap_tree(b, nl, inner_plan, &mut inner, p0)?; // D3803: its tree form (a probe or a nest among its steps)
    outer.desc = crate::breakers::embed(b.arena, outer.desc, &inner.desc, Some(&mut b.graph.reads)).map_err(|_| Refusal::shape(k, id, "a nest inner reads a slot its outer reads from another life"))?;
    let inner = ArenaBox::new_in(inner, b.arena);
    if !nl.nestParams.is_nil() {
        b.graph.note(Kind::NestLoopParam);
    }
    let facts = NestFacts { node: id, invariant, volatility: f.volatility, raise_free: f.raise_free, eq: eq_conjunct(b, &nl.join.joinqual) };
    // D4569: a plain SELECT's parameter-free nest with a calm inner and a quiet join qual may be copied into the union arms
    // below it (arm_nests.rs `per_arm`): each copy evaluates the qual over its own arm's rows, which no order of arms shows
    let select = b.stmt.commandType == types_nodes::nodes_enums::CmdType::CMD_SELECT && b.stmt.rowMarks.iter().next().is_none() && !b.stmt.hasModifyingCTE;
    if crate::admit::union_nests_on() && select && nest_params.is_nil() {
        let qual = nl.join.joinqual.iter().fold(crate::effects::Effects::NONE, |e, c| e.and(crate::effects::Effects::of_found(crate::rerun::of_expr(b.stmt, b.facts, b.arena, c))));
        if crate::effects::union_nest_quiet(invariant, f.raise_free, f.volatility, qual) {
            b.graph.union_nests.push(id); // alloc: Cold
        }
    }
    outer.push(Op::Join(JoinDesc::Nest { jointype: jt, params: nest_params, inner, qual: &nl.join.joinqual, unique: nl.join.inner_unique, inner_node, facts }));
    for (s, ahead) in hoisted.iter().flat_map(|h| h.subs.iter().copied()) {
        outer = crate::ops::subplan::map_hoisted(b, s, k, id, outer, ahead)?;
    }
    ops::filters(&mut outer, &plan.qual);
    ops::project(&mut outer, &plan.targetlist);
    b.graph.note(k);
    Ok(outer)
}

/// Whether NestLoop `nl` sits on another NestLoop's inner side: some plan node on its path up (demand/readers.rs `parent_of`,
/// over left / right children, outside any Append / MergeAppend member, which ends the walk) is a NestLoop's inner child,
/// or the walk ends at the root of a correlated sub-plan.
fn on_nest_inner<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>) -> bool {
    let mut at: &'p types_nodes::plannodes::Plan<'p> = &nl.join.plan;
    while let Some((p, right)) = crate::facts::demand::readers::parent_of(b, at) {
        if right && p.as_nest_loop().is_some() {
            return true;
        }
        let Some(pp) = p.as_plan() else { return false };
        at = pp;
    }
    // D3378 (facts_one_row_fact): the root of a sub-plan that reads a parameter from outside it (extParam: a correlated
    // SubPlan, re-run per call) is an inner to lowerv4 too (rerun/inner.rs), which serves no catalog probe among its steps
    !at.extParam.is_empty() && b.stmt.subplans.iter().flatten().any(|r| r.as_plan().is_some_and(|r| core::ptr::eq(r, at)))
}

/// M3 decision 16 (V4R-142), generalized (path100 DC-14 / NEST-01: no catalog relation named): a nest the hash join's
/// form answers exactly, the inner read once into a keyed table after the first outer row (lowerv4 nest.catalog). An INNER
/// or LEFT nest with no parameters; its outer a heap / index scan; its inner a heap / index scan of filters and
/// projections whose code neither acts nor may raise (`fx`, its effects: the Nest calls it per outer row, the table once; a
/// unique inner's Nest stops at an outer row's match, ExecNestLoop's single_match, so it may never reach a row whose
/// filter raises where the table reads every row); no SubPlan in the nest's own target list or qual (the build row carries
/// the inner columns the probe's projection reads, not a SubPlan's arguments: psql `\d`'s toast-relation lookup); one join qual
/// `outer = inner` over two columns, its operator the key type's equality and raise-free; the inner key the inner
/// relation's whole PRIMARY KEY (facts: a non-deferrable key), so an outer row meets at most one inner row and the rows
/// keep the Nest's order (outer order, one match each). Chosen by the planner's estimate (R: the estimate chooses): the
/// inner's estimated table fits one partition (cost::PARTITION_BYTES), so it never spills, which would partition the outer
/// rows and lose their order; the Nest reads outer x inner rows where the table reads outer + inner, never fewer but by
/// the table's hashing. psql `\d`'s pg_class x pg_namespace lookup is one such. The clause, in (outer, inner) order;
/// `None`: the Nest form.
pub fn catalog_lookup<'p>(b: &Builder<'p, '_>, nl: &'p NestLoop<'p>, outer: &PipelineNode<'p>, params: &NodeList<'_>, inner: &PipelineNode<'p>) -> Option<(Node<'p>, Node<'p>)> {
    let scan = |p: &PipelineNode<'_>| matches!(p.source, crate::graph::Source::Range { .. } | crate::graph::Source::Index { .. });
    let rows = |n: Option<Node<'p>>| n.and_then(|n| n.as_plan()).map_or(0, Builder::rows);
    let ni = rows(nl.join.plan.righttree);
    let qual = &nl.join.joinqual;
    // the inner's code by the effects home's per-expression rule (`quiet`: a constant-pattern LIKE / regex whose pattern is
    // good, semantics DC-14), not the plan walk's per-call flags
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let fx = crate::effects::of_source(&cx, &inner.source, &crate::effects::PLAIN).and(crate::effects::of_ops(&cx, &inner.ops, &crate::effects::PLAIN));
    let shape = b.alloc.is_some() && matches!(nl.join.jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT) && params.is_nil() && scan(outer) && scan(inner) && !fx.acts && !fx.may_raise
        && inner.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) && qual.len() == 1
        && !nl.join.plan.targetlist.iter().chain(nl.join.plan.qual.iter()).any(crate::expr::has_subplan)
        && ni > 0 && ni.saturating_mul(HASHBUILD_ENTRY as u64) <= gravity_program::cost::PARTITION_BYTES;
    let o = qual.iter().next().filter(|_| shape)?.as_op_expr().filter(|o| o.args.len() == 2)?;
    let f = crate::catalog::expr_type::type_of(o.args.iter().next()?).and_then(|t| typefacts::facts_of(b.facts, t))?;
    let side = |varno: i32| o.args.iter().find(|a| a.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0));
    (f.eq != 0 && f.eq == o.opfuncid && crate::effects::quiet_call(b.facts, o.opfuncid)).then_some(())?;
    let (oe, ie) = (side(OUTER_VAR)?, side(INNER_VAR)?);
    // the inner key is the inner relation's whole primary key: its column (the Var's syntactic origin) of the scanned relation
    let iv = ie.as_var()?;
    let (scanrelid, relid, _) = inner.source.relation()?;
    let (pk, n) = b.facts.primary_key(relid)?;
    (iv.varnosyn > 0 && iv.varnosyn as u32 == scanrelid as u32 && n == 1 && pk[0] == iv.varattnosyn).then_some((oe, ie))
}

/// `catalog_lookup`'s nest in the hash join's form: the inner closes into a HashBuild keyed by its side of the clause and
/// the outer probes it (`catalog`: lowerv4's nest.catalog, NL-exact: an empty build ends nothing, LEFT fills the null row,
/// the build waits for the first outer row). The pipeline is serial, so the rows keep the outer's order (the Nest's pathkeys);
/// a keyed nest whose rows no reader orders (`order_free`) leaves its build and probe eligible (D3227 B5).
/// `keyed` (nest.eqkey, self_spool.rs `keyed`): the Probe's hash clause and join qual, and a serial build whose records
/// keep the inner's scan order (lowerv4 chains a catalog probe's equal keys in record order: the Nest's order within an
/// outer row); `None`: a catalog lookup's (its one-clause joinqual, no join qual).
fn map_catalog<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, outer: Open<'p>, inner: Open<'p>, (oe, ie): (Node<'p>, Node<'p>), jt: JoinType, inner_node: i32, keyed: Option<(&'p NodeList<'p>, &'p NodeList<'p>)>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&nl.join.plan, Kind::NestLoop);
    let id = plan.plan_node_id;
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, "a catalog lookup: no node allocator (decompose_in)"))?;
    if ie.as_var().is_none() && keyed.is_none() {
        return Err(Refusal::shape(k, id, "a catalog lookup's inner key is not a column"));
    }
    let lists = || -> types_error::PgResult<(&'p NodeList<'p>, &'p NodeList<'p>, &'p NodeList<'p>)> {
        // the build row is the inner's output (its columns read as OUTER_VAR there); a computed key (D3014 build-once,
        // facts/nest.rs `eq_keyed`) is evaluated over it once per inner row
        let bk = match ie.as_var() {
            Some(v) => Node::mk_var(mcx, OUTER_VAR, v.varattno, v.vartype, v.vartypmod, v.varcollid, 0)?,
            None => build_side(mcx, ie)?.unwrap_or(ie),
        };
        let one = |n: Node<'p>| -> types_error::PgResult<&'p NodeList<'p>> {
            let mut l = NodeList::nil();
            l.lappend(mcx, n)?;
            Ok(Node::mk_list(mcx, l)?.as_list().expect("a list"))
        };
        Ok((one(bk)?, one(oe)?, Node::mk_list(mcx, NodeList::nil())?.as_list().expect("a list")))
    };
    let (bkeys, okeys, nil) = lists().map_err(|_| Refusal::shape(k, id, "a catalog lookup's key lists (out of memory)"))?;
    let mut build = inner;
    let layout = keys_layout(b.arena, bkeys.iter().map(|e| column_kind_of_expr(b.facts, e)));
    let side = crate::ops::agg::key_side(b, &build, bk_of(bkeys));
    build.push(Op::Join(JoinDesc::Build { keys: bkeys, decls: ArenaVec::new_in(b.arena), pg_tuple: u32::MAX }));
    build.desc.words = Some(bits_word(b, Kind::Hash, inner_node)?);
    build.desc.build_rows = nl.join.plan.righttree.and_then(|n| n.as_plan()).map_or(0, Builder::rows); // V4R-481
    // D3227 B5: a keyed nest whose rows no reader above orders (`order_free`, facts/order.rs: a NestLoop hands it to its
    // outer, so a chain of them stays free) builds and probes wide; the marks keep the Nest's order where a reader needs it
    let ordered = keyed.is_none() || !b.order_free;
    if keyed.is_some() && ordered {
        let site = build.desc.driving_site.max(0);
        build.desc = gravity_program::Pipeline { parallel: gravity_program::Parallel::Ineligible { site, reason: gravity_program::Reason::SortedSource }, ..build.desc };
    }
    let bits = spill_bits(build.desc.build_rows, HASHBUILD_ENTRY as u32);
    let slot = crate::ops::shared::breaker::close(b, build, BreakerKind::HashBuild, layout, Budget::Share, Spill::Partitioned { bits }, k, inner_node)?;
    let mut probe = crate::ops::shared::breaker::respool(b, outer, k, id)?;
    crate::ops::shared::breaker::consume(b, &mut probe, slot);
    let (os, oside) = crate::ops::agg::key_side(b, &probe, oe);
    if os.bpchar() || side.0.bpchar() {
        return Err(Refusal::shape(k, id, crate::ops::agg::RELABEL_BPCHAR)); // D1353
    }
    let (clauses, qual) = keyed.unwrap_or((&nl.join.joinqual, nil));
    let coll = clauses.iter().next().and_then(|c| c.as_op_expr()).map_or(0, |o| o.inputcollid);
    if oside.typid.is_some_and(|t| crate::ops::shared::sort::indeterminate(b.facts, t, coll)) {
        return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL)); // D1292
    }
    let mut decls = ArenaVec::with_capacity_in(1, b.arena); // alloc: Cold
    decls.push(JoinKeyDecl { outer: os, inner: side.0, key: pair(b.facts, oside, side.1, coll), raise: KeyRaise::None });
    if let Some(d) = crate::breakers::decl_of(&b.graph, slot).map(|d| d.producer as usize) {
        for op in b.graph.pipelines[d].ops.iter_mut() {
            if let Op::Join(JoinDesc::Build { decls: bd, .. }) = op {
                bd.extend(decls.iter().copied());
            }
        }
    }
    if ordered {
        let site = probe.desc.driving_site.max(0);
        probe.desc = gravity_program::Pipeline { parallel: gravity_program::Parallel::Ineligible { site, reason: gravity_program::Reason::SortedSource }, ..probe.desc };
    }
    probe.push(Op::Join(JoinDesc::Probe { slot, jointype: jt, clauses, keys: okeys, qual, unique: nl.join.inner_unique, marks: false, decls, inner_node, outer_first: true, catalog: true }));
    ops::filters(&mut probe, &plan.qual);
    ops::project(&mut probe, &plan.targetlist);
    b.graph.note(k);
    Ok(probe)
}

/// D3510 (Workers IO sig-2afb1ecec48d: three nested `IN` give no row): list `l`, a RIGHT SEMI drain's qual or targetlist,
/// with each probe column it reads (`OUTER_VAR`) read as the build key its hash clause equates it to. PostgreSQL's Hash
/// Right Semi Join hands each build row on joined with its first matching probe row, and the planner keys an upper join by
/// any member of an equivalence class (`t_2.v = t.v` above `t_2.v = t_1.v`), so the list may read the probe side; the
/// drain's marks form hands build records on with no probe row (lowerv4 drain/source.rs). Exact when the clause's `=` makes
/// equal values one image of one type (keysem `image_eq_same`, or the type's own `=` with `one_image` under the clause's
/// collation): the first matching probe row's value is then the build row's. Any other probe column stays `OUTER_VAR`
/// (demand/readers.rs `rsemi_unread` makes an unread bare one a NULL).
fn semi_build_keys<'p>(b: &Builder<'p, '_>, hj: &'p HashJoin<'p>, l: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    fn probe_col<'a>(n: Node<'a>) -> Option<&'a types_nodes::primnodes::Var<'a>> {
        n.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)
    }
    fn swap<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>, eq: &[(i16, Node<'p>)]) -> types_error::PgResult<Option<Node<'p>>> {
        crate::walk::stack_check(); // D3748
        if let Some(v) = probe_col(n) {
            return Ok(eq.iter().find(|e| e.0 == v.varattno).map(|e| e.1));
        }
        nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| swap(mcx, y, eq))
    }
    let reads = |n: Node<'p>| {
        let mut hit = false;
        expr::walk(n, &mut |x| {
            hit |= probe_col(x).is_some();
            !hit
        });
        hit
    };
    let Some(mcx) = b.alloc.filter(|_| l.iter().any(reads)) else { return Ok(l) };
    let mut eq: ArenaVec<(i16, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for o in hj.hashclauses.iter().filter_map(|c| c.as_op_expr()).filter(|o| o.args.len() == 2) {
        let side = |varno: i32| o.args.iter().find(|a| a.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0));
        let (Some(on), Some(inn)) = (side(OUTER_VAR), side(INNER_VAR)) else { continue };
        let (Some(ov), Some(iv)) = (on.as_var(), inn.as_var()) else { continue };
        let ty = ov.vartype;
        let one = crate::catalog::keysem::image_eq_same(b.facts, o.opno) == Some(ty)
            || crate::catalog::keysem::own_equivalence(b.facts, o.opno, ty) && crate::catalog::keysem::one_image(b.facts, ty, o.inputcollid);
        if iv.vartype == ty && one {
            eq.push((ov.varattno, inn));
        }
    }
    let swapped = || -> types_error::PgResult<&'p NodeList<'p>> {
        let mut out = NodeList::nil();
        for n in l.iter() {
            out.lappend(mcx, swap(mcx, n, &eq)?.unwrap_or(n))?;
        }
        Ok(Node::mk_list(mcx, out)?.as_list().expect("a list"))
    };
    swapped().map_err(|_| Refusal::shape(k, id, "a RIGHT SEMI drain's probe columns (out of memory)"))
}

/// `n`, an expression over the inner side's columns (`INNER_VAR`), as the build pipeline reads them: its input row's
/// (`OUTER_VAR`, the same resnos). `None`: nothing to change.
fn build_side<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(v) = n.as_var().filter(|v| v.varno == INNER_VAR && v.varlevelsup == 0) {
        return Node::mk_var(mcx, OUTER_VAR, v.varattno, v.vartype, v.vartypmod, v.varcollid, 0).map(Some);
    }
    nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| build_side(mcx, y))
}

fn bk_of<'p>(l: &'p NodeList<'p>) -> Node<'p> {
    l.iter().next().expect("one key")
}

fn map_material<'p>(b: &mut Builder<'p, '_>, m: &'p Material<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&m.plan, Kind::Material);
    let id = plan.plan_node_id;
    let mut open = b.child(plan, k)?;
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on Material"));
    }
    // D3227 K3: a keyed nest's inner read through the Material (self_spool.rs `through_material`): the nest builds its
    // keyed table from the child's rows, or closes the Materialize itself (`material_close`)
    if read_once(b, m) || b.keyed_material == Some(id) {
        ops::project(&mut open, &plan.targetlist);
        b.graph.note(k);
        return Ok(open);
    }
    open.push(Op::Join(JoinDesc::Spool));
    let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)?;
    let mut out = crate::ops::shared::breaker::emit(b, id, slot);
    ops::project(&mut out, &plan.targetlist);
    b.graph.note(k);
    Ok(out)
}

/// D3281: a Material PostgreSQL initializes with no REWIND, BACKWARD or MARK flag (`lifetime::eflags_in` 0) keeps no
/// store: ExecMaterial hands each child row straight up and ExecReScanMaterial rescans the child (nodeMaterial.c, no
/// tuplestore when `eflags == 0`). Mapped as that pass-through where nothing re-opens it other than a parameterized nest:
/// under the main plan's root (not the root itself), no ancestor a merge join's or recursive union's inner side, a nest's
/// inner side without NestLoopParams, a Memoize or a Gather (e.g. the planner's Material over a non-repeatable TABLESAMPLE
/// in a join, read once by an Aggregate on a nest's outer side, or re-run per outer row under a LATERAL nest's inner).
fn read_once<'p>(b: &mut Builder<'p, '_>, m: &'p Material<'p>) -> bool {
    use types_nodes::NodeTag;
    if crate::lifetime::eflags_in(b.stmt, m.plan.plan_node_id) != Some(0) {
        return false;
    }
    let mut at: &'p types_nodes::plannodes::Plan<'p> = &m.plan;
    loop {
        let Some((p, right)) = crate::facts::demand::readers::parent_of(b, at) else {
            let root = b.stmt.planTree.and_then(|r| r.as_plan());
            return !core::ptr::eq(at, &m.plan) && root.is_some_and(|r| core::ptr::eq(r, at));
        };
        let reopens = match p.node_tag() {
            // a nest with NestLoopParams initializes its inner without REWIND (nodeNestloop.c) and rescans it at every outer
            // row with the new values: PG's eflags-0 Material there re-scans its child each time, as the pass-through re-opens
            // it with the inner (contrib tsm_system_time's LATERAL `system_time (time)`, a Material over the sample)
            NodeTag::T_NestLoop => right && p.as_nest_loop().is_none_or(|l| l.nestParams.is_nil()),
            NodeTag::T_MergeJoin | NodeTag::T_RecursiveUnion => right,
            NodeTag::T_Memoize | NodeTag::T_Gather | NodeTag::T_GatherMerge => true,
            _ => false,
        };
        let Some(up) = p.as_plan().filter(|_| !reopens) else { return false };
        at = up;
    }
}

fn map_memoize<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, mz: &'p Memoize<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&mz.plan, Kind::Memoize);
    let id = plan.plan_node_id;
    let mut open = b.child(plan, k)?;
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on Memoize"));
    }
    if mz.param_exprs.is_nil() {
        return Err(Refusal::shape(k, id, "a Memoize without keys"));
    }
    let fixed = crate::rerun::fixed_params(b.stmt, b.arena);
    let child = || plan.lefttree.and_then(|c| c.as_plan()).map(|c| c.extParam.iter());
    let key = |x: i32| mz.keyparamids.is_member(x);
    // memoize item 5 (the v4 row MEMO-RERUN-R2's rule): purged unless the keep table keeps the cache at every rescan; the
    // child's chgParam is the memo's met with the child's params, never its own InitPlans' outputs (its rescan re-derives them)
    let purge = !matches!(crate::lifetime::at_any_rescan(b.stmt, node, &fixed, b.arena), crate::lifetime::Keep::Keep { .. });
    let mut nonkey: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
    nonkey.extend(child().into_iter().flat_map(|ps| memo_nonkey(ps, key, &fixed)));
    let nonkey = nonkey.leak();
    open.push(Op::Join(JoinDesc::Memo { keys: &mz.param_exprs, entries: mz.est_entries, singlerow: mz.singlerow, binary: mz.binary_mode, ops: mz.hashOperators, colls: mz.collations, purge, nonkey }));
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

/// `JoinDesc::Memo.nonkey`: the child's params (`ps`, its `extParam`) no key reads and the statement does not fix.
pub fn memo_nonkey<'a>(ps: impl Iterator<Item = i32> + 'a, key: impl Fn(i32) -> bool + 'a, fixed: &'a [i32]) -> impl Iterator<Item = i32> + 'a {
    ps.filter(move |&x| !key(x) && !fixed.contains(&x))
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    let id = node.as_plan().map_or(STMT_NODE, |p| p.plan_node_id);
    match k {
        Kind::HashJoin => map_hash_join(b, node.as_hash_join().unwrap()),
        Kind::MergeJoin => map_merge_join(b, node.as_merge_join().unwrap()),
        Kind::NestLoop => map_nest_loop(b, node.as_nest_loop().unwrap()),
        Kind::Material => map_material(b, node.as_material().unwrap()),
        Kind::Memoize => map_memoize(b, node, node.as_memoize().unwrap()),
        Kind::Hash => Err(Refusal::shape(k, id, "a Hash outside a HashJoin")),
        _ => Err(Refusal::shape(k, id, "a NestLoopParam outside a NestLoop")),
    }
}

impl<'p> JoinDesc<'p> {
    /// A hash join's keys over the op's row: the build's (the Hash node's inner keys) or the probe's (the outer keys),
    /// where a key's SubPlan is read (subplan.rs `attach_keys`).
    pub fn keys(&self) -> Option<&'p NodeList<'p>> {
        match self {
            JoinDesc::Build { keys, .. } | JoinDesc::Probe { keys, .. } => Some(keys),
            _ => None,
        }
    }

    /// The join's own quals (`joinqual`): where a `SubSite::JoinQual` SubPlan is read.
    pub fn qual(&self) -> Option<&'p NodeList<'p>> {
        match self {
            JoinDesc::Probe { qual, .. } | JoinDesc::Merge { qual, .. } | JoinDesc::Nest { qual, .. } => Some(qual),
            _ => None,
        }
    }

    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        match self {
            JoinDesc::Build { keys, decls, .. } => {
                write!(out, "  build keys=")?;
                expr::args(out, keys)?;
                dump_decls(decls, out)
            }
            JoinDesc::Probe { slot, jointype, clauses, keys, qual, unique, marks, decls, inner_node, outer_first, catalog } => {
                write!(out, "  probe slot={slot} inner=n{inner_node} {jointype:?}{}{}{}{} keys=", if *unique { " unique" } else { "" }, if *marks { " marks" } else { "" }, if *outer_first { "" } else { " build-first" }, if *catalog { " catalog" } else { "" })?;
                expr::args(out, keys)?;
                dump_decls(decls, out)?;
                write!(out, " clauses=")?;
                expr::args(out, clauses)?;
                if !qual.is_nil() {
                    write!(out, " qual=")?;
                    expr::args(out, qual)?;
                }
                Ok(())
            }
            JoinDesc::Drain { matches, jointype } => write!(out, "  drain matches=slot{matches} {jointype:?}"),
            JoinDesc::Merge { slot, jointype, clauses, reversals, nulls_first, qual, unique, inner_node, .. } => {
                write!(out, "  merge slot={slot} inner=n{inner_node} {jointype:?}{} clauses=", if *unique { " unique" } else { "" })?;
                expr::args(out, clauses)?;
                write!(out, " reversals={reversals:?} nulls_first={nulls_first:?}")?;
                if !qual.is_nil() {
                    write!(out, " qual=")?;
                    expr::args(out, qual)?;
                }
                Ok(())
            }
            JoinDesc::Nest { jointype, params, inner, qual, unique, inner_node, facts } => {
                write!(out, "  nest inner=n{inner_node} {jointype:?}{}", if *unique { " unique" } else { "" })?;
                facts.dump(out)?;
                write!(out, " params=(")?;
                for (i, p) in params.iter().enumerate() {
                    let p = p.as_nest_loop_param().expect("a nestParams cell is a NestLoopParam");
                    write!(out, "{}$exec{}=", if i > 0 { ", " } else { "" }, p.paramno)?;
                    expr::print(p.paramval, out)?;
                }
                write!(out, ")")?;
                if !qual.is_nil() {
                    write!(out, " qual=")?;
                    expr::args(out, qual)?;
                }
                writeln!(out)?;
                write!(out, "  inner ")?;
                crate::dump::nested(inner, out)
            }
            JoinDesc::Memo { keys, entries, singlerow, binary, .. } => {
                write!(out, "  memo entries={entries}{}{} keys=", if *singlerow { " singlerow" } else { "" }, if *binary { " binary" } else { "" })?;
                expr::args(out, keys)
            }
            JoinDesc::Spool => write!(out, "  spool"),
        }
    }
}

fn dump_decls(decls: &[JoinKeyDecl<'_>], out: &mut dyn Write) -> fmt::Result {
    write!(out, " decls=")?;
    for (i, d) in decls.iter().map(|d| d.key).enumerate() {
        write!(out, "{}{}/{}:", if i > 0 { "," } else { "" }, d.types.0, d.types.1)?;
        match d.kind {
            Some(k) => write!(out, "{k:?}")?,
            None => write!(out, "none")?,
        }
        if d.scale != NO_SCALE {
            write!(out, ".{}", d.scale)?;
        }
        if d.coll.oid != 0 {
            write!(out, "@{}", d.coll.oid)?;
        }
        if d.dict == JoinDict::Fingerprint {
            write!(out, "/fp")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::max_width;
    use crate::catalog::{AttFact, ExprFp, Facts, IndexFact, RelFact, TypeRow};

    /// int4 (23), int8 (20), text (25), numeric (1700); int4range (3904), int8range (3926), numrange (3906); 50000 a user
    /// range over int4 (a user subtype opclass: K2's k2rv), 50001 a domain over it.
    struct Rng;

    impl Facts for Rng {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn type_row(&self, typid: u32) -> Option<TypeRow> {
            let (typlen, typtype, base) = match typid {
                23 => (4, b'b', 23),
                20 => (8, b'b', 20),
                25 | 1700 => (-1, b'b', typid),
                3904 | 3926 | 3906 | 50000 => (-1, b'r', typid),
                50001 => (-1, b'd', 50000),
                _ => return None,
            };
            Some(TypeRow { typlen, byval: typlen > 0, typtype, base })
        }
        fn range_subtype(&self, typid: u32) -> Option<u32> {
            match typid {
                3904 | 50000 => Some(23),
                3926 => Some(20),
                3906 => Some(1700),
                _ => None,
            }
        }
    }

    /// K2.kh1e / kh1x: a range over a fixed-width subtype has PG's range_serialize bound (8 bytes of header and type, each
    /// bound aligned to 8, the flags byte: int4range's 17 bytes at most 21 here, int8range's 25), a domain over one too; a
    /// range over a varlena subtype, text and an unknown type have none (pg_tuple stays unbounded).
    #[test]
    fn a_range_over_a_fixed_width_subtype_has_a_bounded_width() {
        let w = |t: u32| max_width(&Rng, t);
        assert_eq!((w(23), w(20)), (Some(4), Some(8)), "fixed widths");
        assert_eq!((w(3904), w(50000), w(50001)), (Some(21), Some(21), Some(21)), "a range over int4, a user one, a domain over it");
        assert_eq!(w(3926), Some(25), "a range over int8: PG's exact size");
        assert_eq!((w(3906), w(25), w(1700), w(60000)), (None, None, None, None), "numrange, text, numeric, an unknown type");
    }

    /// D2725: the key images pair one type, two integer types, one representation (varchar = text) and a name beside a
    /// text under a set deterministic collation; float4 = float8 and int4 = numeric do not (their clauses key by hash
    /// value), nor a name beside a text under no collation.
    #[test]
    fn images_pair_only_where_the_words_compare_values() {
        use crate::catalog::NoFacts;
        use crate::catalog::typefacts::{KeyColl, C_COLLATION};
        let (c, none) = (KeyColl::of(C_COLLATION), KeyColl::NONE);
        for t in [(23, 23), (23, 20), (21, 20), (1043, 25)] {
            assert!(super::images_pair(&NoFacts, t, none), "{t:?}");
        }
        assert!(super::images_pair(&NoFacts, (19, 25), c) && super::images_pair(&NoFacts, (25, 19), c), "name beside text");
        for t in [(700, 701), (701, 700), (23, 1700)] {
            assert!(!super::images_pair(&NoFacts, t, c), "{t:?}");
        }
        assert!(!super::images_pair(&NoFacts, (19, 25), none), "a name beside a text under no collation");
    }

    /// D2849: PG's hash of a join key under no collation raises at a text / varchar / bpchar value and at a non-NULL
    /// element of their built-in arrays; under a set collation, or for another type, it raises nothing.
    #[test]
    fn a_string_key_under_no_collation_raises_at_its_value_and_its_array_at_an_element() {
        use super::{key_raise_of, KeyRaise};
        use crate::catalog::typefacts::{facts_of, C_COLLATION};
        use crate::catalog::NoFacts;
        let r = |t: u32, coll: u32| key_raise_of(&facts_of(&NoFacts, t).expect("a built-in type row"), coll);
        assert_eq!([r(25, 0), r(1043, 0), r(1042, 0)], [KeyRaise::Value; 3], "text, varchar, bpchar");
        assert_eq!([r(1009, 0), r(1015, 0), r(1014, 0)], [KeyRaise::Elems; 3], "their arrays");
        assert_eq!([r(25, C_COLLATION), r(1009, C_COLLATION), r(23, 0), r(1007, 0)], [KeyRaise::None; 4], "a set collation, int4, int4[]");
    }
}
