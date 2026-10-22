//! Props: road owner.count's shape (split from props/code.rs): the count cut `count_cut` (the one
//! predicate `Prop::CountOrder` and runs_topk_groups `count_top` read), a named tie's order `tie_key`, and the key check
//! `key_check` (a projection's integer arithmetic of keys the owner checks in every group, exec owner.rs `check_range`). Budget 200
//! lines (files.tsv, +25 % gate). Owner M3-OWNER.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::{FinalForm, GroupDecl, GroupForm, KeyDecl, NONE};
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::catalog::KeyReq;
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm, SortKey};
use gravity_decompose::{Op, PipelineNode};
use gravity_lower::Place;
use gravity_fn::typesem::integral::{int_arith, int_width, IntOp};
use gravity_program::{CheckStep, KeyCheck, LaneId, Rep, CHECK_ADD, CHECK_CAST, CHECK_KEY_NULLABLE, CHECK_MUL, CHECK_NEG, CHECK_RSUB, CHECK_SUB, KEY_CHECK_KEYS, MAX_REPS};
use mcx::{GrowIn, PgVec};
use types_core::catalog::{FLOAT4OID, FLOAT8OID, INT2OID, INT4OID, INT8OID, NUMERICOID};
use types_nodes::list::NodeList;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::frame::px::Px;
use crate::graph::Gx;

use crate::ops::sort::topn::strategies::owner_count::{order_word, top_slot};
use crate::props::code::entry;

/// Road owner.count's shape (V4R-113): `Prop::CountOrder` (so `applies::CountTopk`) and runs_topk_groups `count_top` both
/// read it, so the road applies exactly where its owner serves. `d` sorts group `g`'s finals on pipeline `p` after the
/// projection `input`. The owner's count cut (exec owner.rs) keeps each partition's n best records by (count descending,
/// keys ascending: a word signed, a text key by its representative's rendered bytes), and the emit gathers and projects
/// only those, so the top is exact when
/// - its first key is `count(*)` or `count(x)` DESC of the group in a `Count` slot (a pair count is keyed/pairs.rs'), or
///   any final the emit orders by its slot, either way, a grouped DISTINCT's pair count or rider over its accumulator
///   (`top_slot`, `order_word`: the owner keeps the emit's own order of it);
/// - each later key names the group's key of its place (key order), ascending, a scan column the owner orders as its type
///   (`tie_key`);
/// - its bound, LIMIT + OFFSET, is a constant from 1 (a u32: the owner keeps n records in a region the driver sizes from n, under the
///   grant; a region past the grant refuses at run time, never cuts), without ties;
/// - only projections run before the sort, and HAVING filters that drop a suffix of its order (`suffix_cut`, D3152), reading keys, constants, finals
///   that cannot raise and integer `+`, `-`, `*` chains with constants, `-` and int casts over int keys (NEW-46: a nullable
///   one too, a NULL skipped; NEW-46b: a key the producer computes, `hk + 0`, as well as a column), which the owner checks
///   in every group (`checks`: PostgreSQL projects every group, the cut only the kept ones).
pub(crate) fn count_cut<'p>(gx: &Gx<'_, '_>, p: &PipelineNode<'p>, g: &GroupDecl<'_>, d: &SortDesc<'p>, input: Option<&NodeList<'p>>) -> bool {
    // a tagged multi-argument DISTINCT's owner keeps every group (ops/group/pairs_multi.rs): the emit sorts them
    if crate::ops::shared::agg::tagged_multi(g) {
        return false;
    }
    let n = match d.form {
        SortForm::TopK { count: Bound::Const(c), offset: Bound::All, ties: false } => c,
        SortForm::TopK { count: Bound::Const(c), offset: Bound::Const(o), ties: false } => c.saturating_add(o),
        _ => return false,
    };
    // a sort target entry read through the projection before the sort (`through`, without its fallback)
    let at = |resno: i16| -> Option<Node<'p>> {
        let e = entry(d.tlist, resno)?;
        match (e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0), input) {
            (Some(v), Some(l)) => entry(l, v.varattno),
            _ => Some(e),
        }
    };
    let agg = |e: Node<'_>| e.as_aggref().and_then(|a| g.aggs.iter().find(|x| x.node.as_aggref().is_some_and(|b| b.aggno == a.aggno)));
    let Some((first, ties)) = d.keys.split_first() else { return false };
    let Some(c) = at(first.resno).and_then(agg) else { return false };
    // D563: a grouped count(DISTINCT) group's owner (keyed/pairs.rs) takes the count form over its accumulator record:
    // the pair count (a PairCount final, ACC_PAIRS) or a rider; its argument key is no tie key, and it checks no key
    let distinct = g.form == GroupForm::Distinct && crate::graph::grouped(g);
    let ordered = top_slot(g, c).is_some_and(|(s, kind)| order_word(gx.facts, Some(kind), s, c, first).is_some());
    if !(1..=i64::from(u32::MAX)).contains(&n) || !ordered || ties.len() > g.keys.len() - distinct as usize || g.set.is_some() {
        return false;
    }
    let named = ties.iter().zip(g.keys.iter()).enumerate().all(|(i, (sk, k))| {
        let key = at(sk.resno).and_then(|e| e.as_var()).is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == k.resno);
        key && tie_key(gx, g.producer.0 as usize, &g.keys[..i], k, sk, !distinct).is_some()
    });
    named && checks(gx, p, g, d).is_some_and(|(_, c)| !distinct || c.is_empty())
}

/// How the owner orders group key `k` (after the keys `before` it) where sort key `sk` names it after the count:
/// `Some(None)` by its one image word, signed (a word whose sort operator KeySem reads as its family's `<` in the type's
/// default btree family, two's-complement words: `Facts::key`; a nullable one NULLS LAST by its flag where `flags`);
/// `Some(Some((slot, lane)))` by its representative's rendered bytes (a text scan column's `MinCode` slot and dictionary
/// lane, among the first MAX_REPS fingerprint keys; a NULL renders nothing and orders last: kernels
/// `reps::count_better`); `None` where the owner's order is not the SQL's (a descending key, another family's order, a
/// nullable NULLS FIRST key, a nullable word key without `flags`, a derived or collated text key, a code, a float image).
/// NEW-46b: a word key is any key of producer `producer` whose one word is its value, a scan column or one the producer
/// computes (`hk + 0`, `k8 / 170`, a join row's column): the owner orders the record's word, and `key_nullable` (the
/// record's flag test) says whether it can be NULL.
pub(crate) fn tie_key(gx: &Gx<'_, '_>, producer: usize, before: &[KeyDecl<'_>], k: &KeyDecl<'_>, sk: &SortKey, flags: bool) -> Option<Option<(u8, u8)>> {
    let typ = gravity_decompose::catalog::expr_type::type_of(k.expr.node())?;
    if sk.desc {
        return None;
    }
    let signed = gx.facts.key(KeyReq::sort(sk.opno)).is_some_and(|o| !o.desc && o.default_family && gravity_fn::typesem::keysem::image_signed(o.opcintype));
    if k.words == 1 && k.image.is_raw() && matches!(k.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp) && signed {
        if !scanned(gx, producer) {
            return None;
        }
        let not_null = !crate::table::emit_group::key_nullable(gx, producer, k);
        return (not_null || flags && !sk.nulls_first).then_some(None);
    }
    // D3080: an integer's text keyed by the integer orders by its decimal text under a byte-order collation and its text
    // type's default `<` (the owner's `tie_text` compare, kernels `reps::int_text_less`); its NULL as a word key's
    if k.int_text() {
        let own = gx.facts.key(KeyReq::sort(sk.opno)).is_some_and(|o| !o.desc && o.default_family);
        if !own || !sk.coll.bytes_order || !scanned(gx, producer) {
            return None;
        }
        let not_null = !crate::table::emit_group::key_nullable(gx, producer, k);
        return (not_null || flags && !sk.nulls_first).then_some(None);
    }
    let v = k.expr.node().as_var()?;
    let (scanrelid, relid, site) = gx.pipelines()[producer].source.relation()?;
    if v.varno != scanrelid as i32 {
        return None;
    }
    let not_null = gx.facts.att(relid, v.varattno).is_some_and(|a| a.notnull);
    let text = before.iter().filter(|b| b.kind == KeyKind::Fp128 && b.words == 2).count();
    if k.words != 2 || k.kind != KeyKind::Fp128 || !k.image.is_text() || !gravity_fn::typesem::text_payload(typ) || !sk.coll.bytes_order || k.rep == NONE || text >= MAX_REPS || (sk.nulls_first && !not_null) {
        return None;
    }
    let lane = site.lanes.iter().find(|l| l.id == LaneId::Attr(v.varattno))?;
    // exec owner.rs packs a representative's lane in six bits
    let Rep::Code { dict } = lane.rep else { return None };
    (dict.lane < 64).then_some(Some((u8::try_from(k.rep).ok()?, dict.lane))) // the owner packs a representative slot in a byte
}

/// `x` with its relabels stripped.
fn bare(mut x: Node<'_>) -> Node<'_> {
    while let Some(a) = gravity_decompose::expr::through_relabel(x) {
        x = a;
    }
    x
}

/// A checked chain: its group key and its steps in evaluation order, `(op, the constant's width, the constant)`; a minus
/// or a narrowing cast has no constant (NEW-46).
type Chain<'g, 'p> = (usize, PgVec<'g, (u8, u8, Option<Node<'p>>)>);

/// Entry `e` as a chain over one group key (`key_of`) of integer `+`, `-`, `*` (typesem `int_arith`: exact, raising past
/// the result width) each with a by-value constant, and (NEW-46) `-` and int-to-int casts (keysem `dependent_form`: a
/// narrowing one raises past its result width, a widening one is no step): the chain, its value's width, and the one
/// width every step of it raises at (one 22003 message; 0: no step).
fn chain<'g, 'p>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>, e: Node<'p>) -> Option<(Chain<'g, 'p>, u8, u8)> {
    crate::stack_check();
    use gravity_fn::typesem::keysem::image::{dependent_form, Form};
    let one = |r: u8, w: u8| (r == 0 || r == w).then_some(w); // a step at `w` after steps raising at another width: two messages
    if e.as_var().is_some() {
        let (key, w) = key_of(gx, g, e)?;
        return Some(((key, PgVec::new_in(gx.mcx)), w, 0));
    }
    if let Some(f) = e.as_func_expr().filter(|f| f.args.len() == 1) {
        let Some(Form::IntCast { widening }) = dependent_form(f.funcid) else { return None };
        let (w, ((key, mut steps), _, r)) = (int_width(f.funcresulttype)?, chain(gx, g, bare(f.args.nth(0)))?);
        if widening {
            return Some(((key, steps), w, r));
        }
        steps.push_in((CHECK_CAST, w, None));
        return Some(((key, steps), w, one(r, w)?));
    }
    let o = e.as_op_expr()?;
    if o.args.len() == 1 && dependent_form(o.opfuncid) == Some(Form::IntNegate) {
        let (w, ((key, mut steps), vw, r)) = (int_width(o.opresulttype)?, chain(gx, g, bare(o.args.nth(0)))?);
        steps.push_in((CHECK_NEG, w, None));
        return (vw == w).then_some(((key, steps), w, one(r, w)?));
    }
    let a = int_arith(o.opfuncid).filter(|_| o.args.len() == 2)?;
    let (l, r) = (bare(o.args.nth(0)), bare(o.args.nth(1)));
    let konst = |x: Node<'p>, w: u8| x.as_const().filter(|c| c.constbyval && !c.constisnull && int_width(c.consttype) == Some(w)).map(|_| x);
    let op = match a.op { IntOp::Add => CHECK_ADD, IntOp::Sub => CHECK_SUB, IntOp::Mul => CHECK_MUL };
    let (v, vw, c, cw, op) = match (konst(r, a.right), konst(l, a.left)) {
        (Some(c), _) => (l, a.left, c, a.right, op),
        (None, Some(c)) => (r, a.right, c, a.left, if op == CHECK_SUB { CHECK_RSUB } else { op }),
        _ => return None,
    };
    // the value side: the key itself, or an inner chain whose value has this step's read width
    let ((key, mut steps), w, rw) = chain(gx, g, v)?;
    steps.push_in((op, cw, Some(c)));
    (w == vw).then_some(((key, steps), a.result, one(rw, a.result)?))
}

/// Group key `x` names and its width: a key of an integer type whose one word is its value (`Raw`, I32 for smallint and
/// integer, I64 for bigint), whatever the producer computes it from: a scan column, an expression (`hk + 0`, `k8 / 170`,
/// NEW-46b), a column of a join's row. The check reads the record's word, so where the value came from does not matter;
/// an expression that raises does so at the producer, before any group exists. NEW-46: a nullable key where every key
/// before it is one word (its NULL flag is then bit `key` of the record's flags word, exec owner.rs `key_check`, which
/// skips a NULL: no step raises on it); `key_nullable` calls every key nullable but a NOT NULL column (or a dense range
/// form of one).
fn key_of(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, x: Node<'_>) -> Option<(usize, u8)> {
    let v = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?;
    let i = g.keys.iter().position(|k| k.resno == v.varattno)?;
    let k = &g.keys[i];
    let width = int_width(gravity_decompose::catalog::expr_type::type_of(k.expr.node())?)?;
    let kind = if width == 8 { KeyKind::I64 } else { KeyKind::I32 };
    let ok = k.words == 1 && k.image.is_raw() && k.kind == kind && scanned(gx, g.producer.0 as usize);
    let flagged = !nullable(gx, g, i) || g.keys[..i].iter().all(|b| b.words == 1);
    (ok && flagged).then_some((i, width))
}

/// Whether producer `p` reads a scan (or a join over one), as every word key `key_of` and `tie_key` took before NEW-46b: a
/// producer fed by a breaker (PostgreSQL's GroupAggregate over a Sort, a count(DISTINCT) group's) keeps the generic top,
/// where the pair owner's count top read 1.26x / 1.34x at W0 / W8 (c-perturb av2_v2group-0022, box 69 cliff).
fn scanned(gx: &Gx<'_, '_>, p: usize) -> bool {
    gx.pipelines()[p].source.relation().is_some()
}

/// Whether group key `i` can be NULL (its record carries the NULL flags word then: rec.rs `flags_word`).
fn nullable(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, i: usize) -> bool {
    crate::table::emit_group::key_nullable(gx, g.producer.0 as usize, &g.keys[i])
}

/// The count cut's projections (`p`'s before its sort, and the sort's own list) as the owner checks them: `Some` when
/// every entry is a column, a constant, a final that cannot raise or a checked `chain`, the chains at one width over at
/// most `KEY_CHECK_KEYS` keys (an interval each in the descriptor's words), of any length; that width (0: none) and the chains.
fn checks<'g, 'p>(gx: &Gx<'g, '_>, p: &PipelineNode<'p>, g: &GroupDecl<'_>, d: &SortDesc<'p>) -> Option<(u8, PgVec<'g, Chain<'g, 'p>>)> {
    let s = p.ops.iter().position(|op| matches!(op, Op::Sort(_)))?;
    let agg = |e: Node<'_>| e.as_aggref().and_then(|a| g.aggs.iter().find(|x| x.node.as_aggref().is_some_and(|b| b.aggno == a.aggno)));
    let (mut width, mut out) = (0, PgVec::new_in(gx.mcx));
    // D3152: a suffix-cut HAVING runs over the kept records alone, as PG's over the groups that pass (no check of the rest)
    let cut = |op: &&Op<'p>| matches!(op, Op::Filter(f) if suffix_cut(p, g, d, *f));
    let filtered = p.ops[..s].iter().any(|op| cut(&op));
    let lists = p.ops[..s].iter().filter(|op| !cut(op)).map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).chain([Some(d.tlist)]);
    for l in lists {
        for t in l?.iter().filter_map(|n| n.as_target_entry()) {
            let e = bare(t.expr);
            if !(e.as_var().is_some() || e.as_const().is_some() || agg(e).is_some_and(|a| !crate::effects::visible::topn::raises(a.class))) {
                let (c, _, w) = chain(gx, g, e)?;
                if c.1.is_empty() {
                    continue; // widening casts alone: nothing raises
                }
                if width != 0 && w != width {
                    return None;
                }
                width = w;
                out.push_in(c);
            }
        }
    }
    let keys = out.iter().enumerate().filter(|(n, c)| out[..*n].iter().all(|b| b.0 != c.0)).count();
    // the owner checks a chain in every group, where PG projects only the groups a HAVING passes
    (keys <= KEY_CHECK_KEYS && !(filtered && !out.is_empty())).then_some((width, out))
}

/// pg_opfamily `integer_ops` (btree over int2 / int4 / int8) and its strategies `<`, `<=`, `>=`, `>` (stratnum.h).
const INTEGER_OPS: u32 = 1976;
const BT_LT: u8 = 1;
const BT_LE: u8 = 2;
const BT_GE: u8 = 4;
const BT_GT: u8 = 5;

/// D3152 (cbgen D022): whether HAVING filter `f` (a qual, or its implicit AND) drops only a suffix of the count cut's
/// order, so the owner's n best records, filtered by the pipeline's own Filter step, are the n best of the groups that
/// pass: each qual compares the sort's first key, a count final of `g` (never NULL; the order's later keys break its
/// ties), with an integer constant or parameter on the right (normalize's `commute`), `>` / `>=` under DESC or `<` /
/// `<=` under ASC (an integer btree compare raises nothing). `=` keeps a band of the order, not a suffix; any other qual
/// keeps the emit's full sort.
pub(crate) fn suffix_cut<'p>(p: &PipelineNode<'p>, g: &GroupDecl<'_>, d: &SortDesc<'p>, f: Node<'p>) -> bool {
    suffix_cut_on(p, g, d, f, false)
}

/// pg_opfamily `float_ops` (btree over float4 / float8) and `numeric_ops`.
const FLOAT_OPS: u32 = 1970;
const NUMERIC_OPS: u32 = 1988;

/// `suffix_cut`, with `sums` also over a first key that is a sum final of `g` (5c-F6, topn lower.rs `top_lead`): each
/// qual compares it, on the left, with a constant or parameter of its own btree family (int8 against an integer,
/// numeric against numeric, a float against a float: compares that raise nothing), `>` / `>=` under DESC or `<` / `<=`
/// under ASC. A NULL sum passes no such qual, so the cut ranks it last (`CountLead::nulls_first`). NaN sorts above every
/// value and passes `>` / `>=` and fails `<` / `<=` of every other, as the order puts it.
pub(crate) fn suffix_cut_on<'p>(p: &PipelineNode<'p>, g: &GroupDecl<'_>, d: &SortDesc<'p>, f: Node<'p>, sums: bool) -> bool {
    let input = p.ops.iter().take_while(|op| !matches!(op, Op::Sort(_))).filter_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).last();
    let Some(first) = d.keys.first() else { return false };
    let key = entry(d.tlist, first.resno).and_then(|e| match (e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0), input) {
        (Some(v), Some(l)) => entry(l, v.varattno),
        _ => Some(e),
    });
    let Some(a) = key.and_then(|e| e.as_aggref()) else { return false };
    let Some(x) = g.aggs.iter().find(|x| x.node.as_aggref().is_some_and(|b| b.aggno == a.aggno)) else { return false };
    let summed = sums && matches!(x.class, Class::SumI64 | Class::SumI128 | Class::SumNum | Class::SumF32 | Class::SumF64);
    if !summed && !matches!(x.class, Class::CountStar | Class::CountAny) {
        return false;
    }
    // the compare's family and argument types: a count's or an integer sum's int8 against an integer, a numeric sum
    // against a numeric, a float sum against a float
    let typed = |family: u32, left: u32, right: u32| match (family, summed) {
        (INTEGER_OPS, _) => left == INT8OID && left == a.aggtype && (matches!(right, INT8OID | INT4OID) || (summed && right == INT2OID)),
        (NUMERIC_OPS, true) => left == NUMERICOID && left == a.aggtype && right == NUMERICOID,
        (FLOAT_OPS, true) => matches!(left, FLOAT4OID | FLOAT8OID) && left == a.aggtype && matches!(right, FLOAT4OID | FLOAT8OID),
        _ => false,
    };
    let one = |q: Node<'p>| {
        let Some(o) = q.as_op_expr().filter(|o| o.args.len() == 2) else { return false };
        let Some((family, left, right, strategy)) = gravity_decompose::effects::builtin_btree_member(o.opno) else { return false };
        let k = o.args.nth(1);
        typed(family, left, right)
            && matches!((first.desc, strategy), (true, BT_GE | BT_GT) | (false, BT_LT | BT_LE))
            && o.args.nth(0).as_aggref().is_some_and(|x| x.aggno == a.aggno)
            && (k.as_const().is_some() || k.as_param().is_some())
    };
    match f.as_list() {
        Some(l) => l.iter().all(one),
        None => one(f),
    }
}

/// The key check of the count cut over group `g` (drained on pipeline `j`, sorted by `d`): each chain's key word (marked
/// `CHECK_KEY_NULLABLE` where the key can be NULL), its steps and each step's constant as a claimed word (a by-value Const is
/// a word, never the program's key: exec reads it at bind); and whether some checked key can be NULL (the owner then reads
/// the record's NULL flags word, owner_count.rs).
pub(crate) fn key_check<'p>(px: &mut Px<'_, 'p>, j: usize, g: &GroupDecl<'_>, d: &SortDesc<'p>) -> Option<(KeyCheck, bool)> {
    let gx = px.gx;
    let (width, c) = checks(gx, &gx.pipelines()[j], g, d)?;
    if c.is_empty() {
        return Some((KeyCheck::NONE, false));
    }
    let mut all: PgVec<'_, CheckStep> = mcx::reserved_in(c.iter().map(|s| s.1.len()).sum(), px.mcx());
    let mut nulls = false;
    for (i, steps) in &c {
        let word = g.keys[..*i].iter().map(|k| k.words as usize).sum::<usize>() + 1;
        let null = nullable(gx, g, *i);
        nulls |= null;
        let key = u8::try_from(word).ok().filter(|&w| w & CHECK_KEY_NULLABLE == 0)? | if null { CHECK_KEY_NULLABLE } else { 0 };
        for (n, &(op, cwidth, k)) in steps.iter().enumerate() {
            let word = match k {
                Some(k) => {
                    let Place::Word(w) = px.claim_const(k)? else { return None };
                    u16::try_from(w).ok()?
                }
                None => 0,
            };
            all.push_in(CheckStep { key: if n == 0 { key } else { 0 }, op, cwidth, word });
        }
    }
    Some((KeyCheck::in_arena(px.arena, width, &all), nulls))
}

/// Group `g`'s pair record `l` (keyed_hashagg `GroupRec::new`), partitioned by its group words where the one-round fold
/// takes it (D563 L2; exec owner.rs `distinct_fold`: a group's pairs meet in one partition, round 1's accumulators are
/// never re-sunk): a grouped Distinct group whose planned groups route by the group (kernels `table::route::group_routed`, STEP-19:
/// the groups spread over the partitions evenly enough that the second round costs more than the fullest partition's
/// excess), no flags word (exec's `declared` reads the words after the group's as the pair's), and no count top by a rider
/// under order word 0 (a rider's count descending: round 2's count gate serves it, the fold measured 1.24x slower there);
/// the pair count either way and a rider under its slot word take the fold (hosts distinct_fold.rs carries the order slot
/// and word; round 2 runs ungated under a word, measured 1.22x slower over 3M groups).
pub(crate) fn pair_partition(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, l: gravity_abi::breaker::RecordLayout) -> gravity_abi::breaker::RecordLayout {
    // a tagged multi-argument DISTINCT routes its pairs by the whole record (ops/group/pairs_multi.rs)
    if crate::ops::shared::agg::tagged_multi(g) {
        return l;
    }
    let Some(arg) = g.keys.iter().position(|k| k.resno == 0).filter(|_| g.form == GroupForm::Distinct && crate::graph::grouped(g)) else { return l };
    let gw: usize = g.keys[..arg].iter().map(|k| k.words as usize).sum();
    let aw: usize = g.keys[arg..].iter().map(|k| k.words as usize).sum();
    if gw == 0 || l.nkeys() != gw + aw || l.pkeys != 0 {
        return l;
    }
    let producer = &gx.pipelines()[g.producer.0 as usize].desc;
    let parallel = matches!(producer.parallel, gravity_program::Parallel::Eligible);
    // KEY-PREDICTED-ROWS road (b): routing decided on the producer's estimate is a guard of the program
    // D3784: and on the largest group's share (`GroupDecl::top`): one group holding most rows routed by the group is one
    // owner's alone
    if gravity_lower::inputs::key::decide(g.producer.0 as usize, false, producer.predicted_rows, routed, [g.groups, l.size() as u64, parallel as u64, g.top as u64]) == 0 {
        return l;
    }
    let drain = gx.drainers(g).first().copied();
    let rider = drain.is_some_and(|j| {
        let p = &gx.pipelines()[j];
        let Some(s) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { return false };
        let Op::Sort(d) = &p.ops[s] else { return false };
        let input = p.ops[..s].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None });
        if !count_cut(gx, p, g, d, input) {
            return false;
        }
        let first = d.keys.first().and_then(|k| entry(d.tlist, k.resno));
        let first = match (first.and_then(|e| e.as_var()).filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0), input) {
            (Some(v), Some(l)) => entry(l, v.varattno),
            _ => first,
        };
        let c = first.and_then(|e| e.as_aggref()).and_then(|a| g.aggs.iter().find(|x| x.node.as_aggref().is_some_and(|b| b.aggno == a.aggno)));
        c.is_none_or(|c| c.final_ != FinalForm::PairCount && top_slot(g, c).and_then(|(s, kind)| order_word(gx.facts, Some(kind), s, c, &d.keys[0])) == Some(0))
    });
    if rider {
        return l;
    }
    fires::fire("pairs.partition"); // shapecheck fire signal (REGISTRY STEP-19, table::route::group_routed)
    l.partitioned_by(gw as u8)
}

/// Whether `a[0]` planned groups of `a[1]`-byte pair records route by the group over `rows` producer rows (`a[2]`: a
/// parallel producer; `a[3]`: the largest group's share, 2^-16, 0 unknown): `pair_partition`'s guarded decision.
fn routed(rows: u64, a: &[u64; 4]) -> u64 {
    gravity_kernels::table::route::group_routed_skewed(a[0], gravity_kernels::table::params::spill_bits(a[0], a[1] as u32), rows, a[2] != 0, a[3] as u32) as u64
}

/// Whether `l` is `pair_partition`'s: a grouped Distinct pair routed by its group words, no part word after the pair's
/// (keyed_parted's layout keeps one more word than the group's and the argument's; keyed_hashagg `image` reads it).
pub(crate) fn pair_routed(g: &GroupDecl<'_>, l: &gravity_abi::breaker::RecordLayout) -> bool {
    let Some(arg) = g.keys.iter().position(|k| k.resno == 0).filter(|_| g.form == GroupForm::Distinct) else { return false };
    let gw: usize = g.keys[..arg].iter().map(|k| k.words as usize).sum();
    let aw: usize = g.keys[arg..].iter().map(|k| k.words as usize).sum();
    gw > 0 && l.pkeys as usize == gw && l.nkeys() == gw + aw
}
