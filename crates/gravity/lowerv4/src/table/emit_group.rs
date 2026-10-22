//! Group emit, kind `keyed.emit_group` (plan §3.1 the keyed face; H25 `PartEmit::Groups`): the source of a pipeline that
//! drains a keyed group. The fresh call gathers every owner's records into one pointer array (`agg_gather`, owner 0's table
//! space: a resumed call finds it again) and, under `twopass.having`, keeps only the records at or over the HAVING count
//! floor (keyed_twopass_having), before any final is taken. Each window is a run of the kept records; each row is one
//! group: its keys as cells by resno (a by-value key from its image word, a text key rendered from its representative code,
//! a derived text key re-evaluated over that render) and its aggregates' finals (`FoldClass::finish` over the record's
//! slots) in the row's aggregate area (`Env::aggs`), which the HAVING filter and the projection read as `Aggref`s. The
//! emit is serial (decompose's emit pipeline) unless it is `wide` (D3024 (b): each claim gathers its slices of the owner
//! runs, `agg_gather_claim`); its cursor is a frame word. When the pipeline ends in a top over the groups
//! (runs_topk_groups), the kept records are sorted by its keys before they stream (`agg_sort`), and the top's drain reads
//! the same array again (`drain`: the resumed gather). Budget 450 lines (files.tsv, +25 % gate). Owner M1-KEYED.
use gravity_abi::breaker::{KeyKind, MINCODE_NONE};
use gravity_abi::ctx::{hdr, RESUME_NONE};
use gravity_abi::limits::DESC_MAX_SORT;
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{agg_gather, agg_gather_at, agg_gather_claim, agg_gather_claim_pinned, agg_gather_next, agg_sort, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_PROCS, DESC_PROCS_AT_SHIFT, DESC_SORT, FORM_BOUND_MAX, FORM_BOUND_SHIFT, FORM_TIES, GATHER_FRESH};
use gravity_kernels::util::RESUME_OPEN;
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm, KeyDecl, NONE};
use gravity_decompose::Op;
use gravity_expr::emit::prim::datetime;
use gravity_program::{DictRef, NumTypmod, NumForm, Pipeline, Rep};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_core::catalog::{INT4OID, INT8OID, NUMERICOID, TIDOID};
use types_nodes::primnodes::{Var, INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::cell::{Cell, LaneId, Nullable, Row};
use crate::table::face::{DenseFront, KeyShape, TableDecl};
use crate::table::keyimg::ImageWords;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey, DRAFT_SITE};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::agg::hashagg::GroupRec;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::KeyedEmitGroup, &SourceMaker(make))], ..crate::ops::NONE };

crate::strategies! {
    /// D3024 (b) rest: a wide group emit whose pipeline ends in a draining probe's seeded end (`Emit::marks`). Switched off,
    /// that emit runs on the leader.
    EMIT_MARKS = 132, "emit.marks", Drain;
}

/// The records one window walks (a window's end is a cancel check).
const WINDOW: i64 = 4096;

/// `EmitState` (kernels table/emit.rs): the record pointers', and the kept count's byte offsets.
pub(crate) const ES_PTRS: i32 = gravity_kernels::table::EMIT_PTRS_OFF;
pub(crate) const ES_KEPT: i32 = gravity_kernels::table::EMIT_KEPT_OFF;

/// The emit's frame words: the cursor, the pointer array, the kept count and the gather's state (filed by the fresh call's
/// open, kept across calls: a resumed call skips the open).
const W_CURSOR: u8 = 0;
pub(crate) const W_PTRS: u8 = 1;
const W_KEPT: u8 = 2;
pub(crate) const W_STATE: u8 = 3;
/// How many records are in order: a bounded top whose HAVING can drop records (`Emit::more`).
pub(crate) const W_SORTED: u8 = 4;

/// Frame word `i` of the emit of pipeline `k` (its byte offset).
pub(crate) fn word(px: &mut Px<'_, '_>, k: usize, i: u8) -> i32 {
    px.frame_word(WordKey::Open { face: Face::Keyed, k, j: u64::MAX, i }) as i32 * 8
}

/// The current group's record: the emit files it after the finals in the row's aggregate area.
pub(crate) fn record_of(b: &mut FunctionBuilder<'_>, env: &Env, naggs: usize) -> Value {
    match env.aggs.as_ref() {
        Some(a) => b.load(Type::Ptr, a.rec, 16 * naggs as i32),
        None => bug(env.node, "a group's record read outside a group emit"),
    }
}

/// An emit descriptor (host_abi.rs `DESC_*`) on the stack: the records' layout, the runs form and a top's sort words, and
/// (M6-HOSTED SK_CMP) their comparator words, stored only when some word is an `SK_CMP` word (only such a word reads its
/// own: a sort with none stores nothing more).
pub(crate) fn desc(b: &mut FunctionBuilder<'_>, layout: &gravity_abi::breaker::RecordLayout, sort: &[u64], form: u64) -> Value {
    desc_cmp(b, layout, sort, &[], form)
}

/// `desc` with the sort words' comparator words (`DESC_PROCS`). Past `DESC_MAX_SORT` sort words (W8-SETOP-WIDEKEYS; D2024:
/// counted in sort words, not keys, since one key may take several) the sort words run on past `DESC_PROCS` and the
/// comparator words follow them, their offset in the count word's high half (`DESC_PROCS_AT_SHIFT`); every other
/// descriptor keeps its `DESC_BYTES`.
fn desc_cmp(b: &mut FunctionBuilder<'_>, layout: &gravity_abi::breaker::RecordLayout, sort: &[u64], procs: &[u64], form: u64) -> Value {
    let wide = sort.len() > DESC_MAX_SORT;
    let procs_at = if wide { DESC_SORT + 8 * sort.len() } else { DESC_PROCS };
    let slot = b.declare_stack_slot(if wide { (procs_at + 8 * sort.len()) as u32 } else { DESC_BYTES }, 8);
    let d = b.stack_addr(slot);
    let words: [u64; gravity_abi::breaker::LAYOUT_WORDS] = layout.words();
    for (i, w) in words.iter().enumerate() {
        let v = b.iconst(Type::I64, *w as i64);
        b.store(d, (DESC_LAYOUT + 8 * i) as i32, v);
    }
    let form = b.iconst(Type::I64, form as i64);
    b.store(d, DESC_FORM as i32, form);
    let n = b.iconst(Type::I64, (sort.len() as u64 | if wide { (procs_at as u64) << DESC_PROCS_AT_SHIFT } else { 0 }) as i64);
    b.store(d, DESC_NSORT as i32, n);
    for (i, w) in sort.iter().enumerate() {
        let v = b.iconst(Type::I64, *w as i64);
        b.store(d, (DESC_SORT + 8 * i) as i32, v);
    }
    for (i, p) in procs.iter().enumerate().filter(|(_, p)| **p != 0) {
        let v = b.iconst(Type::I64, *p as i64);
        b.store(d, (procs_at + 8 * i) as i32, v);
    }
    d
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// An `Fp128` key's representative: the producer row's column whose code it keeps (the key itself, or the one code
/// column a derived key is computed from), the scan attribute behind it and that lane's `Code` rep; or (`datum`, M6 L0-l)
/// the key's own column, whose value the record keeps a copy of (keyed_emit_key `keep_datums`), with no code to keep.
#[derive(Clone, Copy)]
pub(crate) struct RepSrc {
    pub col: i16,
    pub attno: i16,
    pub typid: u32,
    pub rep: Rep,
    pub derived: bool,
    /// key.codeif's form (step/percode_guard.rs): the column is its THEN arm's, the image picks the ELSE render.
    pub guard: bool,
    pub datum: bool,
    /// M6 L0-m: a float / numeric key, whose image (never the kept datum) orders and hashes it; a float's datum is its row
    /// word kept as is, a numeric's a copy as a text key's.
    pub imaged: bool,
    /// A jsonb path key's code, or where no lane row reached the group, its text (keyed/emit_group_nocode.rs).
    pub nocode: bool,
}

/// Whether key `k` can be NULL: only a NOT NULL column of the producer's scanned relation (or dense.range's x % c, x / c, x + c of one) cannot,
/// read by the producer itself or through the Sort breaker it drains (STEP-19: PostgreSQL's GroupAggregate over a Sort, so every
/// count(DISTINCT) group, reads its keys as the sort's pass-through columns; a sort moves rows and never changes a value).
pub(crate) fn key_nullable(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> bool {
    let Some(v) = crate::table::face::range_x(gx.catalog, k.expr.node()).unwrap_or(k.expr.node()).as_var() else { return true };
    !not_null_col(gx, producer, v)
}

/// Whether `v`, read on pipeline `p`, is a NOT NULL scan column: the pipeline's own scan's, or (an outer Var over a Sort breaker,
/// followed down through `p`'s earlier projections) the sorted pipeline's column at its sort, followed through that pipeline's
/// last projection; only filters and projections may stand on either pipeline (a join's row may carry its nullable side's NULL).
/// Anything else is nullable.
fn not_null_col(gx: &Gx<'_, '_>, p: usize, v: &Var<'_>) -> bool {
    let pl = &gx.pipelines()[p];
    if let Some((scanrelid, relid, _)) = pl.source.relation() {
        return v.varno == scanrelid as i32 && v.varlevelsup == 0 && gx.facts.att(relid, v.varattno).is_some_and(|a| a.notnull);
    }
    let gravity_decompose::Source::Breaker { slot } = pl.source else { return false };
    if v.varno != OUTER_VAR || v.varlevelsup != 0 || !pl.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) {
        return false;
    }
    // the key is p's last projection's entry: its column is one level down, through p's earlier projections to the drained row
    let mut a = v.varattno;
    for l in pl.ops.iter().rev().filter_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).skip(1) {
        let Some(w) = crate::props::code::entry(l, a).and_then(|e| e.as_var()).filter(|w| w.varno == OUTER_VAR && w.varlevelsup == 0) else { return false };
        a = w.varattno;
    }
    let Some(u) = gx.life_of(p, slot).map(|l| &gx.pipelines()[l.0 as usize]) else { return false };
    let Some((s, Op::Sort(d))) = u.ops.iter().enumerate().find(|(_, op)| matches!(op, Op::Sort(_))) else { return false };
    if !u.ops[..s].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) {
        return false;
    }
    let Some((scanrelid, relid, _)) = u.source.relation() else { return false };
    let mut e = crate::props::code::entry(d.tlist, a);
    if let Some(w) = e.and_then(|e| e.as_var()).filter(|w| w.varno == OUTER_VAR && w.varlevelsup == 0) {
        e = u.ops[..s].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).and_then(|l| crate::props::code::entry(l, w.varattno));
    }
    e.and_then(|e| e.as_var()).is_some_and(|w| w.varno == scanrelid as i32 && w.varlevelsup == 0 && gx.facts.att(relid, w.varattno).is_some_and(|a| a.notnull))
}

/// D3855 (c-random rnd-3532 over pgrc copies: `count(DISTINCT left(i_cmt, -2))` under a `date_trunc` group key): the
/// column a derived key's expression itself reads (`one_var`, else key.codeif's THEN arm's Var), by its own varattno. It
/// differs from `RepSrc::attno` (the scan column's) where the key reads the producer's projection: a DISTINCT argument
/// decompose left over outer.N reads column N, while its representative code is the scan column's. The emit's render row
/// (emit_key.rs `KeyOut::Derived`) puts the code at both.
pub(crate) fn derived_attno(gx: &Gx<'_, '_>, producer: usize, node: i32, e: Node<'_>) -> Option<i16> {
    one_var(e).or_else(|| crate::dict::strategies::percode_guard::then_var(gx, producer, node, e)).map(|v| v.varattno)
}

/// The one column a derived key reads: every `Var` under it (any node decompose's walker knows: functions, operators,
/// NULLIF, CASE, COALESCE, relabels) is the same column; `None` with two columns, no column or a node the walker does not know.
fn one_var<'p>(e: Node<'p>) -> Option<&'p Var<'p>> {
    let mut found: Option<&'p Var<'p>> = None;
    let mut one = true;
    let known = gravity_decompose::expr::walk(e, &mut |n| {
        if let Some(v) = n.as_var() {
            match found {
                None => found = Some(v),
                Some(f) => one &= f.varno == v.varno && f.varattno == v.varattno && f.varlevelsup == v.varlevelsup,
            }
        }
        true
    });
    found.filter(|_| one && known)
}

/// Key `k`'s representative, for an `Fp128` key: the code column of the producer's row it keeps (the key's own column, or
/// the one column a derived key reads, found in the producer's projection) and that scan lane's `Code` rep. A key with no
/// code to keep (over two columns, not a scan's column, a column without a dictionary) keeps its own value (L0-l).
pub(crate) fn rep_src(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>, node: i32) -> Option<RepSrc> {
    // M6 L0-m: a float / numeric key's image is not its output datum; its record keeps one member's (Decision 7: any member)
    // M6-TYPES R1c: a uuid / interval / timetz key likewise (its image is words, its datum a pointer)
    if crate::ops::sort::topn::copy_words::packed_key(gx, producer, k).is_some() {
        return None; // M6-CLOSE (a'): a packed lane's key keeps no copy; it renders from its record word (keyed/emit_key.rs)
    }
    // D1379: a tid key likewise (its image is bttidcmp's order word, rep.hosted.hash `tid_word`; its datum a 6-byte pointer)
    if (k.image.needs_datum() || k.tf == TIDOID) && k.resno != 0 {
        if k.rep == NONE {
            shape(node, "a float or numeric group key without a datum slot (a full record)");
        }
        let col = key_col(gx, producer, k).unwrap_or_else(|| shape(node, "a float or numeric group key outside the producer's row"));
        return Some(RepSrc { col, attno: 0, typid: 0, rep: Rep::VarlenaPtr, derived: false, guard: false, datum: true, imaged: true, nocode: false });
    }
    if k.kind != KeyKind::Fp128 {
        return None;
    }
    if k.rep == NONE {
        bug(node, "an Fp128 key without its representative slot");
    }
    if let Some(r) = crate::ops::agg::emit_group_nocode::rep_src(gx, producer, k) { return Some(r); }
    let e = k.expr.node();
    let derived = e.as_var().is_none();
    let datum = |why: &'static str| match key_col(gx, producer, k) {
        Some(col) => Some(RepSrc { col, attno: 0, typid: 0, rep: Rep::VarlenaPtr, derived: false, guard: false, datum: true, imaged: false, nocode: false }),
        // count(DISTINCT expr): the key's word is all its group reads (facts::demand `distinct_arg_unread`); group_core-0235:
        // a DISTINCT argument key (resno 0) the producer's row does not carry as a column (computed over a sorted input,
        // `lower(t)` under a FILTER): no emit renders it (a pair count or integer fold), so it keeps none
        None if k.resno == 0 => None,
        None => shape(node, why),
    };
    let Some(o) = one_var(e).or_else(|| crate::dict::strategies::percode_guard::then_var(gx, producer, node, e)) else { return datum("a derived text key over more than one column (or none)") };
    let p = &gx.pipelines()[producer];
    let ops = feed(gx, producer, k);
    // PS1-16: a group key over a column of the row entering the producer's last projection (a join's): the scan column
    // `source_var` walks it back to, at column resno
    let past = past_var(gx, producer, k);
    // a key over the projection (a DISTINCT argument decompose left over outer.N, k=25): entry N's scan column, at column N
    let outer = (past.is_none() && o.varno == OUTER_VAR && o.varlevelsup == 0).then(|| entry_var(ops, o.varattno)).flatten();
    let v = past.or(outer).unwrap_or(o);
    let Some((_, relid, site)) = p.source.relation().filter(|s| v.varno == s.0 as i32) else { return datum("a text group key over a column that is not a scan's") };
    let lane = site.lanes.iter().find(|l| l.id == gravity_program::LaneId::Attr(v.varattno));
    let Some(rep @ Rep::Code { .. }) = lane.map(|l| l.rep) else { return datum("a text group key over a column without a dictionary (no representative code)") };
    // a derived key whose code column the projection drops keeps its own value, as a key with no code does; a bpchar key
    // still refuses: its group image would be the lane's plain Fp128, which coarse_codes' D1353 / D767 checks (Code reps only) never see
    let Some(col) = past.map(|_| k.resno).or_else(|| outer.map(|_| o.varattno)).or_else(|| scan_col(ops, v)).or_else(|| hidden_col(gx, producer, k, v.varattno)) else {
        if crate::dict::coderep::sk_bpchar(gx, k.tf) != 0 {
            crate::admit::refusal::refuse_native(node, relid, crate::admit::refusal::ShapeWhy("a derived text key whose code column the projection drops"));
        }
        return datum("a derived text key whose code column the projection drops");
    };
    Some(RepSrc { col, attno: v.varattno, typid: v.vartype, rep, derived, guard: one_var(e).is_none(), datum: false, imaged: false, nocode: false })
}

/// Whether `e` is a DISTINCT argument key of pipeline `producer`'s group whose value no aggregate reads (facts::demand
/// `distinct_arg_unread`), by its expression: no emit renders its groups.
pub(crate) fn unread_key(gx: &Gx<'_, '_>, producer: usize, e: Node<'_>) -> bool {
    gx.group_at(producer).is_some_and(|g| gravity_decompose::facts::demand::distinct_arg_unread(g, e))
}

/// Each `Fp128` key's representative word `part << 32 | code` in the producer's `row` (`None` for other keys); a NULL source
/// row's is MINCODE_NONE, never marked valid (a later row's code fills it), and its derived key is the expression over NULL.
/// A datum key's word is 0 here: its copy is kept at the record's open (keyed_emit_key `keep_datums`).
pub(crate) fn rep_words<'e>(rec: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> PgVec<'e, Option<Value>> {
    let mut out = mcx::reserved_in(rec.reps.len(), env.mcx);
    for (i, r) in rec.reps.iter().enumerate() {
        out.push_in(r.map(|r| {
            if r.datum {
                return env.zero;
            }
            let c = row.col(r.col).unwrap_or_else(|| bug(rec.node, "a representative column the row does not carry"));
            if !matches!((c.cell.rep, r.rep), (Rep::Code { dict: a }, Rep::Code { dict: b }) if a.lane == b.lane) {
                bug(rec.node, "a representative whose code lane is not the site's");
            }
            fires::fire("code.carry"); // shapecheck fire signal (RD-code.carry): the record keeps the code, not a copy
            let mut cx = env.lx(b, rec.node);
            let kept = crate::rep::retain(&mut cx, c, &rec.tfs[i], gravity_program::Until::Life, crate::rep::RetainHome::Record(rec.home));
            env.memo = cx.memo;
            c.null.map_or(kept.val, |n| { let none = b.const_raw(Type::I64, MINCODE_NONE); b.select(n, none, kept.val) })
        }));
    }
    out
}

/// The plain code keys whose `Fp128` entries the row loop hints (prefetch.fp): each its attno and its lane's table in the
/// producer's drafted site (the source declared it, `fp_lanes`); a lane without one is not hinted.
pub(crate) fn prefetch_keys<'g>(px: &mut Px<'g, '_>, rec: &GroupRec<'_, '_>) -> PgVec<'g, (i16, u8)> {
    let (producer, mcx) = (rec.g.producer.0 as usize, px.mcx());
    let own = |i: &usize| rec.dep.as_ref().is_none_or(|d| !d.has(*i)); // D3324 (b): a build row's code is no lane of this site
    rec.reps.iter().enumerate().filter(|(i, _)| own(i)).filter_map(|(_, r)| r.as_ref()).filter(|r| !r.derived).filter_map(|r| match site_rep(px, producer, r.attno) {
        Some(Rep::Code { dict }) if dict.fp_table != DictRef::NO_TABLE => Some((r.attno, dict.fp_table)),
        _ => None,
    }).collect_in(mcx)
}

/// Scan attribute `attno`'s lane rep in the producer's site as its make drafted it (the source declares the `Fp128` tables
/// of `fp_lanes` there, so the rep carries its table), or as numbered once the producer is made (a reading pipeline).
fn site_rep(px: &mut Px<'_, '_>, producer: usize, attno: i16) -> Option<Rep> {
    let lane = |s: &gravity_program::SiteDesc| s.lanes.iter().find(|l| l.id == gravity_program::LaneId::Attr(attno)).map(|l| l.rep);
    if let Some(s) = px.site_mut(producer) {
        return lane(s);
    }
    px.pipeline_site(producer).and_then(|i| px.sites.get(i)).and_then(lane)
}

/// `x` with its relabels stripped.
pub(crate) fn bare(mut x: Node<'_>) -> Node<'_> {
    while let Some(a) = gravity_decompose::expr::through_relabel(x) {
        x = a;
    }
    x
}

/// D1071: the ops of pipeline `producer` that make the row group key `k` is read over: all of them for a group the
/// pipeline ends in; for a streamed group (`Op::GroupRun`), those before its marker (past it, the row is the group's).
fn feed<'g, 'p>(gx: &Gx<'g, 'p>, producer: usize, k: &KeyDecl<'_>) -> &'g [Op<'p>] {
    let ops: &'g [Op<'p>] = &gx.pipelines()[producer].ops;
    let owns = |gd: &GroupDecl<'_>| gd.keys.iter().any(|x| core::ptr::eq(x as *const _ as *const u8, k as *const _ as *const u8));
    let Some(s) = gx.streams().position(owns) else { return ops };
    ops.iter().position(|op| matches!(op, Op::GroupRun(i) if *i as usize == s)).map_or(ops, |at| &ops[..at])
}

/// PS1-16 (lanes/dict/DESIGN-PS1-16-17.md §2): the scan column (its Var, relabels stripped) column `a` of the row entering
/// op `pos` of pipeline `k` carries unchanged, walked back through the pipeline's ops: a filter keeps the row; a
/// projection's column is its entry's, read at that projection; an outer column past a hash probe that hands its outer
/// row's cells on (ops/hashjoin/outer_kept.rs) is the row's entering the probe. `None` for a build's (INNER_VAR) column, a
/// computed entry, an upper level's Var, an outer column no projection made, or any other op that shapes the row (a
/// merge, nest or drain join, a probe outer_kept declines, a SubPlan or Append step): the key keeps its value, as before.
/// The scan's Var it returns may still name another relation; the caller matches it to the source's.
pub(crate) fn source_var<'p>(gx: &Gx<'_, 'p>, k: usize, mut pos: usize, mut a: i16) -> Option<&'p Var<'p>> {
    let ops = &gx.pipelines()[k].ops;
    loop {
        let i = ops[..pos].iter().rposition(|op| !matches!(op, Op::Filter(_)))?;
        match &ops[i] {
            Op::Project(l) => {
                let v = bare(l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == a)?.expr).as_var()?;
                if v.varlevelsup != 0 || v.varno == INNER_VAR {
                    return None;
                }
                if v.varno != OUTER_VAR {
                    return Some(v);
                }
                (pos, a) = (i, v.varattno);
            }
            Op::Join(gravity_decompose::join::JoinDesc::Probe { .. }) if crate::ops::hashjoin::outer_kept::outer_kept(gx, k, i) => pos = i,
            _ => return None,
        }
    }
}

/// PS1-16: group key `k` (a key of the row, resno > 0, not derived) whose expression is a column of the row entering the
/// producer's last projection, its projection entry decompose stripped (a join's target list names the probe's rows): the
/// scan column `source_var` walks it back to, where its code pays (`code_pays`). `rep_src` and `fp_lanes` both read it,
/// so the lane the source declares an Fp128 table for and the representative the record keeps are one.
fn past_var<'p>(gx: &Gx<'_, 'p>, producer: usize, k: &KeyDecl<'_>) -> Option<&'p Var<'p>> {
    let v = k.expr.node().as_var().filter(|v| k.resno > 0 && v.varno == OUTER_VAR && v.varlevelsup == 0)?;
    let i = feed(gx, producer, k).iter().rposition(|op| matches!(op, Op::Project(_)))?;
    source_var(gx, producer, i, v.varattno).filter(|s| code_pays(gx, producer, s) || ranked_cut(gx, producer, k, s))
}

/// F7 (c): the code of a verbatim scan column (`att_dict` false: its parts rank their values, and the Fp128 table its key
/// images by is the ranks' own, built with its codes) pays at any entry count when the group streams into a top over the
/// groups that sorts by finals alone (order_slot `top_reads_finals`): the record carries the code instead of a copy of
/// the value, and only the kept records render. The value form renders every row (a whole-part render table for each
/// part past `RENDER_WALK_AFTER` renders), hashes its bytes and copies them into the record. A bpchar key takes it only
/// over a bpchar column of declared length (coarse_codes' D767 / D1353: no other bpchar code images as hashbpchar); a
/// key under a collation whose equal values are not equal bytes keeps its value (key.coll images it either way).
fn ranked_cut(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>, v: &Var<'_>) -> bool {
    let Some((_, relid, _)) = gx.pipelines()[producer].source.relation().filter(|s| v.varno == s.0 as i32 && gx.facts.coll(k.coll).bytes_eq) else { return false };
    let bpchar_ok = crate::dict::coderep::sk_bpchar(gx, k.tf) == 0 || gx.facts.att(relid, v.varattno).is_some_and(|a| a.typmod >= 0 && crate::dict::coderep::sk_bpchar(gx, a.typid) != 0);
    let cut = gx.facts.att_entries(relid, v.varattno).is_some() && !gx.facts.att_dict(relid, v.varattno) && bpchar_ok && gx.group_at(producer).is_some_and(|g| crate::ops::sort::topn::order_slot::top_reads_finals(gx, g));
    if cut {
        fires::fire("key.past.ranked_cut"); // shapecheck fire signal (F7 (c)): a verbatim key past a join carries its code into a top
    }
    cut
}

/// PS1-16: whether the code of scan column `v` pays as the representative of a key `past_var` reaches, against the value
/// these keys kept before: the cost home's `code_key_pays` over the column's dictionary entries and the relation's rows
/// (`Facts::att_entries`, `rel_rows`: pgrc's parts, as percode.rs `col_counts` reads them): at most `CODE_KEY_MAX_ENTRIES`
/// entries and `LIGHT_ROWS_A_COUNT` rows an entry. Past that the code pays per entry (its Fp128 table, a render a group,
/// the count front's arrays a claim) more than it saves a row, and the value stays. A column without those facts keeps
/// the code (a stale count only picks the slower of two forms with the same answers).
fn code_pays(gx: &Gx<'_, '_>, producer: usize, v: &Var<'_>) -> bool {
    let Some((_, relid, _)) = gx.pipelines()[producer].source.relation().filter(|s| v.varno == s.0 as i32) else { return true };
    match (gx.facts.att_entries(relid, v.varattno), gx.facts.rel_rows(relid)) {
        (Some(entries), Some(rows)) => gravity_abi::breaker::code_key_pays(entries, rows),
        _ => true,
    }
}

/// Entry `resno` of the last projection in `ops` as a Var (relabels stripped), when it is one.
pub(crate) fn entry_var<'p>(ops: &[Op<'p>], resno: i16) -> Option<&'p Var<'p>> {
    let Some(Op::Project(l)) = ops.iter().rev().find(|op| matches!(op, Op::Project(_))) else { return None };
    bare(l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno)?.expr).as_var()
}

/// The producer row's column carrying scan column `v`: the projection's entry for it in `ops`, or `v`'s own attribute.
fn scan_col(ops: &[Op<'_>], v: &Var<'_>) -> Option<i16> {
    match ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(l) } else { None }) {
        None => Some(v.varattno),
        Some(l) => l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.expr.as_var().is_some_and(|w| w.varno == v.varno && w.varattno == v.varattno)).map(|te| te.resno),
    }
}

/// The group the projection at op `j` of pipeline `k` feeds as the last op before it: the group the pipeline ends in
/// (`j` its last op), or a streamed group whose `Op::GroupRun` marker follows it (D1071).
pub(crate) fn fed_group<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize) -> Option<&'g GroupDecl<'p>> {
    match gx.pipelines()[k].ops.get(j + 1) {
        None => gx.group_at(k),
        Some(Op::GroupRun(s)) => gx.stream(*s as usize),
        Some(_) => None,
    }
}

/// The source attnos, in key order, of the derived text group keys of the group the projection at op `j` of pipeline
/// `k` feeds (`fed_group`) whose one code column that projection does not forward: step_project carries each as a
/// hidden column at resno `list.len() + 1 + i` (no sink reads it: the group reads the row next), the representative the
/// emit renders from.
pub(crate) fn hidden_reps<'g>(gx: &Gx<'g, '_>, k: usize, j: usize) -> PgVec<'g, i16> {
    let p = &gx.pipelines()[k];
    let (Some(Op::Project(_)), Some(g), Some((scanrelid, _, _))) = (p.ops.get(j), fed_group(gx, k, j), p.source.relation()) else { return PgVec::new_in(gx.mcx) };
    let mut out = PgVec::new_in(gx.mcx);
    for key in g.keys.iter().filter(|x| x.kind == KeyKind::Fp128 && x.resno > 0 && x.expr.node().as_var().is_none()) {
        if let Some((a, _)) = crate::ops::agg::emit_group_nocode::key_path(gx, k, key) { out.extend_in((!out.contains(&a)).then_some(a)); continue; }
        match one_var(key.expr.node()).or_else(|| crate::dict::strategies::percode_guard::then_var(gx, k, p.node_id, key.expr.node())) {
            Some(v) if v.varno == scanrelid as i32 && scan_col(&p.ops[..=j], v).is_none() && !out.contains(&v.varattno) => out.push_in(v.varattno),
            _ => {}
        }
    }
    out
}

/// Scan column `attno`'s hidden resno in the projection key `key`'s group reads the row of (`feed`, `hidden_reps`), if it
/// carries one.
pub(crate) fn hidden_col(gx: &Gx<'_, '_>, k: usize, key: &KeyDecl<'_>, attno: i16) -> Option<i16> {
    let j = feed(gx, k, key).len().checked_sub(1)?;
    let Some(Op::Project(l)) = gx.pipelines()[k].ops.get(j) else { return None };
    hidden_reps(gx, k, j).iter().position(|&a| a == attno).map(|i| (l.len() + 1 + i) as i16)
}

/// RD-key.codeif (dict/strategies/key_consts.rs): the keys of the group the projection at op `j` of pipeline `k` feeds that
/// key by their arms' constants, each with its projection entry's expression: step_project carries each image hidden
/// after `hidden_reps`' columns (resno `list.len() + 1 + hidden_reps.len() + i`), the group's key source (`image_col`).
pub(crate) fn image_keys<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize) -> PgVec<'g, (&'g KeyDecl<'p>, Node<'p>)> {
    let (Some(Op::Project(l)), Some(g)) = (gx.pipelines()[k].ops.get(j), fed_group(gx, k, j)) else { return PgVec::new_in(gx.mcx) };
    let entry = |kd: &KeyDecl<'_>| l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == kd.resno).map(|te| te.expr);
    g.keys.iter().filter_map(|kd| entry(kd).filter(|&e| crate::dict::strategies::key_consts::applies(gx, k, g.form, kd, e)).map(|e| (kd, e))).collect_in(gx.mcx)
}

/// Key `kd`'s hidden image column in the producer's row (`image_keys`), if it has one.
pub(crate) fn image_col(gx: &Gx<'_, '_>, producer: usize, kd: &KeyDecl<'_>) -> Option<i16> {
    let j = feed(gx, producer, kd).len().checked_sub(1)?;
    let Some(Op::Project(l)) = gx.pipelines()[producer].ops.get(j) else { return None };
    let i = image_keys(gx, producer, j).iter().position(|x| core::ptr::eq(x.0 as *const _ as *const u8, kd as *const _ as *const u8))?;
    Some((l.len() + 1 + hidden_reps(gx, producer, j).len() + i) as i16)
}

/// Key `k`'s column in the producer's row: a group key's resno; a DISTINCT argument (resno 0) that is a column of the row
/// (the projection's entry decompose stripped it to, an outer column, or a scan column the projection carries); `None`:
/// an expression, compiled.
pub(crate) fn key_col(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> Option<i16> {
    if k.resno > 0 {
        return Some(k.resno);
    }
    let p = &gx.pipelines()[producer];
    let ops = feed(gx, producer, k);
    if let Some(Op::Project(l)) = ops.iter().rev().find(|op| matches!(op, Op::Project(_))) {
        if let Some(te) = l.iter().filter_map(|n| n.as_target_entry()).find(|te| bare(te.expr).ptr_eq(k.expr.node())) {
            return Some(te.resno);
        }
    }
    let v = k.expr.node().as_var()?;
    match p.source.relation() {
        _ if v.varno == OUTER_VAR => Some(v.varattno),
        Some((scanrelid, _, _)) if v.varno == scanrelid as i32 => scan_col(ops, v),
        _ => None,
    }
}

/// D531: the producer's last projection entry (its resno) computing group key `kd` as the integer word of an integral
/// `extract(unit FROM timestamp)`, which decompose declared I32 / Raw / int4 (agg.rs `key_kind`): gravity_expr's datetime
/// form at the word root (`RowRefs::word_root`) (PG 18.6: an infinite timestamp's field of an oscillating unit is NULL, the
/// word's validity; D3149: a monotonic unit's is ±Infinity, the words WORD_NINF / WORD_PINF, kept out of a cast root), in
/// a hashed, sorted or grouped DISTINCT-pair group (runs.group's end and emit are the keyed ones; its emit orders the
/// records by the key word, the integer's own order) no aggregate of which reads that column. The emit renders the word
/// back as its numeric (`KeyOut::Num`, dscale 0; a monotonic unit's `KeyOut::NumInf`). `None` otherwise: the key keys through its numeric
/// datum (rep.hosted.hash).
pub(crate) fn extract_word_key(gx: &Gx<'_, '_>, producer: usize, g: &GroupDecl<'_>, kd: &KeyDecl<'_>) -> Option<i16> {
    extract_word_at(gx, producer, g, kd).map(|(resno, _)| resno)
}

/// `extract_word_key`'s key whose value is the extract's numeric (its output takes the numeric of the word); a key under
/// an integer cast root (`extract(hour FROM ts)::int`) is that integer type's own word.
pub(crate) fn extract_word_numeric(gx: &Gx<'_, '_>, producer: usize, g: &GroupDecl<'_>, kd: &KeyDecl<'_>) -> bool {
    extract_word_at(gx, producer, g, kd).is_some_and(|(_, cast)| !cast)
}

/// D3149: `extract_word_numeric`'s key over a monotonic unit (year, decade, century, millennium), whose word may be
/// gravity_expr's WORD_NINF / WORD_PINF (an infinite source's ±Infinity): the emit maps them to the packed sentinels.
pub(crate) fn extract_word_monotonic(gx: &Gx<'_, '_>, producer: usize, g: &GroupDecl<'_>, kd: &KeyDecl<'_>) -> bool {
    if !extract_word_numeric(gx, producer, g, kd) {
        return false;
    }
    let Some(resno) = key_col(gx, producer, kd) else { return false };
    let Some(Op::Project(l)) = gx.pipelines()[producer].ops.last() else { return false };
    let Some(te) = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno) else { return false };
    let Some(f) = word_cast_arg(te.expr).0.as_func_expr().filter(|f| f.args.len() == 2) else { return false };
    let unit = f.args.nth(0).as_const().filter(|c| !c.constisnull).and_then(gravity_decompose::expr::const_bytes);
    unit.and_then(|u| datetime::extract_unit(f.funcid, u)).is_some_and(|u| u.monotonic())
}

/// B2-31: the root under an `int4(numeric)` / `int8(numeric)` cast (`extract(hour FROM ts)::int`), and whether it was one
/// (gravity_expr `datetime::word_node`, which reads the compiled program the same way).
pub(crate) fn word_cast_arg(x: Node<'_>) -> (Node<'_>, bool) {
    let x = bare(x);
    match x.as_func_expr().filter(|f| f.args.len() == 1 && matches!(gravity_fn::row_of(f.funcid).map(|r| r.name), Some("numeric_int4" | "numeric_int8"))) {
        Some(f) => (bare(f.args.nth(0)), true),
        None => (x, false),
    }
}

fn extract_word_at(gx: &Gx<'_, '_>, producer: usize, g: &GroupDecl<'_>, kd: &KeyDecl<'_>) -> Option<(i16, bool)> {
    // extractint: or a grouped DISTINCT aggregate's pair form (keyed.distinct_grouped: its records and emit are the hashed
    // group's, the group keys before the argument's resno-0 key): a COUNT(DISTINCT ..) beside extract keys over a join
    let form = matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) || (g.form == GroupForm::Distinct && crate::graph::grouped(g));
    if !matches!((kd.kind, kd.tf), (KeyKind::I32, INT4OID) | (KeyKind::I64, INT8OID)) || !form || !kd.image.is_raw() {
        return None;
    }
    let resno = key_col(gx, producer, kd)?;
    let Some(Op::Project(l)) = gx.pipelines()[producer].ops.last() else { return None };
    let te = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno)?;
    let (root, cast) = word_cast_arg(te.expr);
    // an int8 key is only the int8 cast's (a bare extract's word key is int4)
    if kd.tf == INT8OID && !cast {
        return None;
    }
    let f = root.as_func_expr().filter(|f| f.args.len() == 2)?;
    // a cast root's word only over a column (or expr.fusion's timestamptz claim): an argument computed by a host call per
    // row (`ts AT TIME ZONE 'UTC'`, timezone()) keyed by its word measured 1.5x base at W8 (ABAB atz-hour-int), so such a
    // key keeps the hosted form
    if cast && bare(f.args.nth(1)).as_var().is_none() {
        return None;
    }
    let unit = f.args.nth(0).as_const().filter(|c| !c.constisnull).and_then(gravity_decompose::expr::const_bytes)?;
    // B2-31: or a timestamptz's under expr.fusion's word claim (`word_claimable`: the same test compile_word claims by)
    // D3149: a monotonic unit's word spells ±Infinity as gravity_expr's WORD_NINF / WORD_PINF (the emit renders them as
    // the numeric's), never under a cast root (PostgreSQL's int4 / int8 of ±Infinity raises)
    if !crate::step::expr_fuse::word_claimable(te.expr) {
        datetime::extract_unit(f.funcid, unit).filter(|u| !(cast && u.monotonic()))?;
    }
    let reads = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varattno == resno);
    let clean = |n: Node<'_>| gravity_decompose::expr::walk(n, &mut |x| !reads(x));
    // an aggregate by its arguments and filter: its aggorder / aggdistinct SortGroupClauses name argument entries and read
    // nothing (a node the walk does not know, so walked whole a COUNT(DISTINCT ..) beside the key never read as clean)
    let agg = |n: Node<'_>| match n.as_aggref() {
        Some(r) => r.aggdirectargs.iter().chain(r.args.iter()).all(clean) && r.aggfilter.is_none_or(clean),
        None => clean(n),
    };
    g.aggs.iter().all(|a| agg(a.node) && a.filter.is_none_or(clean)).then_some((resno, cast))
}

/// D3080 (H025): the producer row column (resno) of a hashed group's integer-text key (decompose `KeyDecl::int_text`: keyed
/// by the integer, its expression the cast) that the producer's last projection computes as the integer's word, never
/// its text: step/project.rs compiles that entry's argument. `None` for any other key; such a key whose entry is not
/// there arrives as text and refuses at its image (rep/hosted/hash.rs).
pub(crate) fn int_text_word_key(gx: &Gx<'_, '_>, producer: usize, g: &GroupDecl<'_>, kd: &KeyDecl<'_>) -> Option<i16> {
    if g.form != GroupForm::Hashed || !kd.int_text() {
        return None;
    }
    let resno = key_col(gx, producer, kd)?;
    let Some(Op::Project(l)) = gx.pipelines()[producer].ops.last() else { return None };
    let te = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno)?;
    gravity_decompose::ops::shared::agg::int_text_arg(bare(te.expr)).map(|_| resno)
}

/// The site lane of a record's one key when its bounds are exact per execution (`Domain::Bound`, the runner's DenseLo /
/// DenseHi from the lane's stats): a NOT NULL scan column whose key image is one integer word (or dense.range's `x / c`,
/// `x + c` of one), or (V4R-368) such a nullable column over a pgrc site, its image `(value, flags)`. One integer word
/// (KEYED-23): the datum's own bits (the integers, date, time, the timestamps, money, oid, bool, "char", an enum's label)
/// or a numeric's mantissa at its decided scale, never a float's folded bits, a fingerprint or a two-word image; the
/// run time takes the dense form only where the lane's exact extrema are datum words and the span fits (else hashed).
pub(crate) fn bound_lane(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>) -> Option<u8> {
    let ([k], n) = (&rec.g.keys[..], rec.layout.nkeys()) else { return None };
    // V4R-368 (C12): a nullable column's image is (value, flags): table.small keeps NULL in the slot after the domain
    let range = || crate::table::face::range_key(gx.catalog, k.expr.node()).filter(|r| r.0.bound() && n == 1).and_then(|r| r.1.as_var());
    // a nullable key over a site with a DenseLo / DenseHi source: pgrc's exact part extrema, or (SRC-38 slice 3) a heap
    // relation's statistics (exec statement.rs stats_range: a RangeSource / IndexSource site); a statistics range holds no
    // NULL, and NULL takes the slot after the domain as over pgrc
    let src = &gx.pipelines()[rec.g.producer.0 as usize].source;
    let nulls = rec.nullable
        && matches!(src, gravity_decompose::Source::Range { am: gravity_decompose::Am::Pgrcolumnar | gravity_decompose::Am::Heap, .. } | gravity_decompose::Source::Index { am: gravity_decompose::Am::Heap, .. });
    let word = k.words == 1 && (k.image.is_raw() || k.image.is_mantissa());
    let v = k.expr.node().as_var().or_else(range).filter(|_| (n == 1 || nulls && n == 2) && word)?;
    let (scanrelid, _, site) = gx.pipelines()[rec.g.producer.0 as usize].source.relation()?;
    let i = site.lanes.iter().position(|l| l.id == gravity_program::LaneId::Attr(v.varattno)).filter(|_| v.varno == scanrelid as i32)?;
    u8::try_from(i).ok()
}

/// SRC-38 slice 2: whether `rec`'s one key is an integer word whose range the table's generic owners can observe
/// (`OwnerDesc::observe_key`; exec owner.rs `observable_key` reads the same kinds off the record): int2 / int4 / int8, a
/// date or a timestamp imaged raw in one word, NOT NULL (the record's one key word) or nullable (its `(value, flags)`
/// image, V4R-368's: table.small keeps NULL in the slot after the domain).
pub(crate) fn observable_key(rec: &GroupRec<'_, '_>) -> bool {
    let ([k], n) = (&rec.g.keys[..], rec.layout.nkeys()) else { return false };
    (n == 1 || rec.nullable && n == 2) && k.words == 1 && k.image.is_raw() && matches!(k.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp)
}

/// SRC-38 slice 2: how the owners of `rec`'s table on `domain` observe its key range (`OwnerDesc::observe_key`): a bound
/// domain over a heap relation (its statistics may be absent or not pay: exec statement.rs `Facts` then takes the observed
/// range), never pgrc's exact part extrema, nor a key with no bound lane (an expression, a join's output: no dense table
/// to open; its table.small would check the dense arm per row even where no range ever pays).
pub(crate) fn observes(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>, domain: crate::table::face::Domain) -> u8 {
    let heap = || matches!(gx.pipelines()[rec.g.producer.0 as usize].source, gravity_decompose::Source::Range { am: gravity_decompose::Am::Heap, .. } | gravity_decompose::Source::Index { am: gravity_decompose::Am::Heap, .. });
    let on = domain == crate::table::face::Domain::Bound && observable_key(rec) && heap();
    match (on, rec.layout.nkeys()) {
        (false, _) => gravity_program::OBSERVE_NONE,
        (true, 1) => gravity_program::OBSERVE_KEY,
        (true, _) => gravity_program::OBSERVE_KEY_FLAGGED,
    }
}

/// The site lanes of pipeline `k` its keyed end hashes at Program scope: each group key or DISTINCT argument that is the
/// scan's code column itself (decompose keys text `Fp128`; the image is the entry's Fp128, rep/code/hash.rs). SOURCE
/// declares their Fp128 tables at site draft (pgrc.rs), before the row cells copy the lanes' Reps. A derived key's image
/// is its per-entry Fp128 (percode.table's), never its source lane's.
pub(crate) fn fp_lanes<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, u8> {
    let (Some(g), Some((scanrelid, _, site))) = (gx.group_at(k), gx.pipelines()[k].source.relation()) else { return PgVec::new_in(gx.mcx) };
    let mut out: PgVec<'g, u8> = PgVec::new_in(gx.mcx);
    // PS1-16: or the scan column a key past a join is (`past_var`)
    for v in g.keys.iter().filter(|key| key.kind == KeyKind::Fp128).filter_map(|key| key.expr.node().as_var().filter(|v| v.varno == scanrelid as i32).or_else(|| past_var(gx, k, key))).filter(|v| v.varno == scanrelid as i32) {
        let lane = site.lanes.iter().position(|l| l.id == gravity_program::LaneId::Attr(v.varattno) && matches!(l.rep, Rep::Code { .. }));
        if let Some(i) = lane.and_then(|i| u8::try_from(i).ok()).filter(|i| !out.contains(i)) {
            out.push_in(i);
        }
    }
    out
}

/// A table's key shape (face/keyed.rs `KeyShape`) and its bound lane: `code`, one code lane, when the record keeps its one
/// key's representative code; code.front_dense's keys (`dense`) for two or more scan-column code keys (`n` of them), or
/// for one key derived from one code lane (`n` 1, `derived`); the domain bound on `bound_lane`'s lane.
/// M1 serves no VOLATILE expression (step/expr.rs refuses expr.volatile), so every served key is IMMUTABLE or STABLE, as
/// the front needs; a marked pass 1's image ends in its part word, so it takes no front.
/// An entries site (dict.entries) publishes each code once per part: a code front has no repeat to fold.
pub(crate) fn key_shape(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>, entries: bool) -> (KeyShape, Option<u8>) {
    let lane = bound_lane(gx, rec);
    let at = |r: Option<&RepSrc>| match r {
        Some(RepSrc { rep: Rep::Code { dict }, derived, guard: false, .. }) => Some(((LaneId::col(dict.lane as u16), DRAFT_SITE), *derived)),
        _ => None,
    };
    // D823: a key under a nondeterministic collation images by its sort key, never by a code or a code's byte Fp128
    let bytes = rec.colls.iter().all(|c| c.bytes_eq);
    let code = match (&rec.reps[..], &rec.g.keys[..]) {
        _ if !bytes => None,
        ([r], [_]) => at(r.as_ref()).filter(|x| !x.1 && !entries).map(|x| x.0),
        _ => None,
    };
    let srcs: PgVec<'_, _> = rec.g.keys.iter().map(|k| at(rep_src(gx, rec.g.producer.0 as usize, k, rec.node).as_ref())).collect_in(gx.mcx);
    // NEW-numeric-1 (P): a derived key's front holds a record per source code, two of one key where two codes derive
    // one value, and its flush folds them in list order; an order-key layout takes the newer member at the flush, so
    // it needs the window's rows of a key in one record (the hashed table, row by row)
    let keyed = rec.layout.flags & gravity_abi::breaker::LAYOUT_KEY_TIES != 0;
    let dense = match srcs[..] {
        _ if !bytes => None,
        [Some((_, true))] if keyed => None,
        [Some((_, true))] => Some(DenseFront { n: 1, derived: true, groups: rec.g.groups }),
        [_, _, ..] if srcs.iter().all(|s| matches!(s, Some((_, false)))) => u8::try_from(srcs.len()).ok().map(|n| DenseFront { n, derived: false, groups: rec.g.groups }),
        _ => None,
    };
    (KeyShape { code, dense, domain: crate::table::face::key_domain(gx.catalog, &rec.g.keys, rec.layout.nkeys(), lane.is_some()) }, lane)
}

/// The producer row's column of the code lane a derived front (`key_shape`) indexes by.
pub(crate) fn derived_col(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>, t: &TableDecl) -> Option<i16> {
    t.key.dense.filter(|d| d.derived)?;
    rep_src(gx, rec.g.producer.0 as usize, &rec.g.keys[0], rec.node).map(|r| r.col)
}

/// A derived front's `ImageWords.index` (`derived_col`'s column `col`): the source code, all ones on a NULL row
/// (keyimg's `one_code`), set after the image, which cannot see derivedness.
pub(crate) fn derived_index(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, col: Option<i16>, img: &mut ImageWords) {
    let Some(col) = col else { return };
    let c = row.col(col).unwrap_or_else(|| bug(node, "a derived key's code column the row does not carry"));
    let mut cx = env.lx(b, node);
    let code = crate::rep::code_word(&mut cx, c);
    env.memo = cx.memo;
    let code = c.null.map_or(code, |n| {
        let ones = b.iconst(Type::I64, -1);
        b.select(n, ones, code)
    });
    img.index = Some(crate::table::keyimg::FrontIndex::Source(code));
}

pub(crate) struct Emit<'g, 'p> {
    pub(crate) rec: GroupRec<'g, 'p>,
    node: i32,
    keys: PgVec<'g, crate::table::emit_key::KeyOut<'g>>,
    pub(crate) words: [i32; 4],
    floor: Option<crate::ops::sort::topn::strategies::twopass_having::Floor>,
    /// The top's sort words and form word (`DESC_SORT`, `DESC_FORM`), when a top over the groups follows, and its
    /// order-expression pass (keyed/order_slot.rs, D472).
    sort: Option<(PgVec<'g, u64>, u64, Option<crate::ops::sort::topn::order_slot::Pass<'g>>)>,
    /// The sort words' comparator words (`DESC_PROCS`, runs/copy_words.rs `procs`), resolved while the catalog is at hand.
    procs: PgVec<'g, u64>,
    /// The top's drain: the gather's state is found again (no floor, no sort).
    drain: bool,
    /// V4R-375's copy: the drain walks its own copy of the array, its record holding hosted states or (aggtrans T6i) its
    /// pipeline streaming a group (D1071, the plan-order run's form) whose key copies and hosted finals take that window.
    copy: bool,
    /// D1339 row C (nest/rerun.rs `emit_ok`): an inner's group index among its owner's consumed regions, gathered fresh there,
    /// and its array's cache word (host_abi `kept_array`). D2561: also a top-level emit kept past its call (`pinned`).
    at: Option<(u64, i32)>,
    /// D2561 (`keep_past_call`): a top-level group sort's emit (form G) whose array the top's drain reads in a later call:
    /// the array goes to the pinned cache (`kept_array`), not the call's scratch, which is free once its call ends and which
    /// a nested run's pins or the drain's own call (D4476) take. Its resume word is marked around the gather as a scratch
    /// gather's is.
    pinned: bool,
    /// The widest resno the row carries.
    ncols: usize,
    /// KEYED-08: a parameter bound's cut, stored in the descriptor's form word before the sort (topn `late_cut`).
    late: Option<crate::ops::sort::topn::lower::TopN>,
    /// A constant-bound top whose HAVING filter can drop records, or one WITH TIES (RUNS-18): the fresh call orders the
    /// first `n` (its frame word, the records in order, and `n`), and the drain orders the next ones as it reaches the
    /// ordered ones' end (`sort_more`).
    more: Option<(i32, i64)>,
    /// D3024 (b): the emit runs wide: its claims are slices of the owner runs of the group's table (`ClaimKind::OwnerRuns`),
    /// each claim's records gathered by `agg_gather_claim`. A plain emit only (no sort, floor, drain, copy, inner, hosted
    /// state or two-pass split) whose pipeline ends in a hash build or a hash aggregate, each worker's partial merged as
    /// any producer's (a build keys its rows itself: a semi-join over a grouped HAVING; an aggregate over the groups), or in
    /// a row sort or top that takes section ends (ops/sort/lower.rs `wide_row_end`: each claim thread's run sealed at its
    /// section end, the reader merging the runs), or in the client's sink (each claim's rows on its leg). A rerun set's emit
    /// stays serial.
    wide: bool,
    /// A wide emit whose pipeline ends in the client's sink (its rows' order free: no sort).
    to_sink: bool,
    /// A wide emit whose pipeline ends in a draining probe's seeded end (drain/lower.rs `seeds_drain`, road `emit.marks`).
    marks: bool,
    /// D3024 (b), sorted (table/emit_runs.rs): a group emit that sorts its groups in place for a top or a full sort (form
    /// G's runs form) runs wide too: each claim's array is a statement pin (`agg_gather_claim_pinned`), sorted as the serial
    /// emit's, and its top files the run in the run table. Only where no record leaves the claim but through its top: no
    /// floor, inner, copy, two-pass split, hosted state, comparator call (each claim sorts on a pool thread), count top,
    /// WITH TIES, kept-column side array or VOLATILE order slot. A sorted emit never refills (`refills` is a plain one's).
    sorted_wide: bool,
    /// D3024 (b), sorted: the top's drain over a sorted wide emit merges its claims' runs (`emit_runs::merge`) where the
    /// drain of a serial one finds the gather's state again.
    merge: Option<crate::table::emit_runs::SortedRuns<'g>>,
    /// C10 2b (D3582): a plain serial emit over a spilling hash aggregate or distinct (its records hold no key copy): at its
    /// records' end it asks for the next batch of its slot (`refill`, kernels `agg_gather_next`), which the driver batches
    /// when it spilled past the statement's room (driver mem/batch.rs); 0 back ends it as before (`Pipeline::refills`).
    refills: bool,
    /// D3141 (a): a sorted emit's HAVING quals, compiled for the keep pass (`keep_having`) that drops the groups failing them
    /// before the in-place sort, so the sort orders the kept groups alone; empty: no keep pass (`having_first`). With the
    /// plan node whose quals they are (EXPLAIN ANALYZE's Rows Removed by Filter).
    having: PgVec<'g, ExId>,
    having_node: i32,
    /// D4620: a cascade's base emit (ops/agg/cascade.rs `extras`): the consumed regions of its cascaded sets' groups,
    /// gathered after its own records (`[n, k1 .. kn]` on the stack, kernels `agg_gather_x`).
    extras: Option<PgVec<'g, u64>>,
    /// D3336 (A1a, face/partfill.rs `cut_form`): a merging drain that reads its claim's share of K-cut parts into a parted
    /// window's object (`agg_runs_claim`): the window's object table word and the frame word of the claim's object.
    cut: Option<(u16, i32)>,
}

/// D3141 (a): the HAVING quals of sorted emit `k` (its pipeline's filters before the Sort, each ahead of every projection,
/// as the Agg runs its qual over its own row), compiled over the group's row, when every one calls no user or VOLATILE
/// function: the filter step runs them again over the kept groups, a second call that changes nothing. PostgreSQL's Agg runs
/// HAVING before its Sort sees a group, so a keep pass ahead of the sort finalizes and tests every group where PG does.
/// Only over aggregates whose finals act on nothing (no user-defined aggregate, no ORDER BY / DISTINCT / ordered-set one):
/// the keep pass builds every group's row in the table's order before the sort, where the emit builds the groups' rows in
/// sort order, PostgreSQL's GroupAggregate order; a user aggregate's final-time fold (D1371: its transfn calls run at the
/// final) or a final-time sort's comparator would show the other order (e2e v4/m6_d1371_user_ordered: a nextval count).
fn having_first<'g>(px: &mut Px<'g, '_>, k: usize, node: i32, g: &GroupDecl<'_>) -> (PgVec<'g, ExId>, i32) {
    use gravity_decompose::ops::shared::agg_class::Class;
    let acts = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| d.class == Class::User || matches!(d.class, Class::OrderedSet(_) | Class::Hypothetical(_)) || d.mods.order > 0 || d.mods.distinct > 0;
    let mcx = px.mcx();
    if g.aggs.iter().any(acts) {
        return (PgVec::new_in(mcx), node);
    }
    let gx = px.gx;
    let Some(p) = gx.pipelines().get(k) else { return (PgVec::new_in(mcx), node) };
    let s = p.ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(p.ops.len());
    let proj = p.ops[..s].iter().position(|op| matches!(op, Op::Project(_))).unwrap_or(s);
    if !p.ops[..s].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) || p.ops[proj..s].iter().any(|op| matches!(op, Op::Filter(_))) {
        return (PgVec::new_in(mcx), node);
    }
    let quals: PgVec<'_, Node<'_>> = p.ops[..proj].iter().filter_map(|op| if let Op::Filter(q) = op { Some(*q) } else { None }).collect_in(mcx);
    if quals.is_empty() || quals.iter().any(|&q| crate::effects::user_or_volatile(gx, q)) {
        return (PgVec::new_in(mcx), node);
    }
    // the node whose quals they are: the one the first filter op runs under (`node_ops`)
    let at = p.ops[..proj].iter().position(|op| matches!(op, Op::Filter(_))).unwrap_or(0);
    let qnode = p.node_ops.iter().rev().find(|o| o.first_op as usize <= at).map_or(node, |o| o.node_id);
    fires::fire("emit.having_first"); // shapecheck fire signal (D3141 (a)): a sorted group emit tests HAVING before its sort
    (quals.into_iter().map(|q| crate::step::expr::compile(px, q, None, node)).collect_in(mcx), qnode)
}

/// D3024 (b): whether wide emit pipeline `k` is kept on the leader (`Emit::pipeline_desc`): a hosted call a pool thread cannot
/// run (an fmgr site, a volatile or parallel-unsafe call, among the program's expressions made so far) or a serial SRF in the
/// pipeline (D1182, as the spill pair). Its row sort or top then seals its own run (no section end runs on a site-less
/// pipeline): they read this after their own expressions are made, as `pipeline_desc` does right after them.
pub(crate) fn kept_on_leader(px: &Px<'_, '_>, k: usize) -> bool {
    let catalog = px.gx.catalog;
    let unsafe_host = (0..px.nexs() as u32).any(|i| {
        let c = px.ex(crate::frame::px::ExId(i));
        c.host_calls() > 0 && crate::hir::pool_unsafe_site(c, &mut |f| catalog.func(f).ok().and_then(|x| x.parallel)).is_some()
    });
    unsafe_host || crate::hir::srf_serial(px, k)
}

pub(crate) fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let Some(g) = px.gx.drained_group(k) else { bug(node, "a group emit over a breaker holding no group") };
    mcx::pg_box_dyn!(px.mcx(), Emit::new(px, k, g, false) => dyn SourceMech + 'g)
}

/// agg_shop-0033 perf: `n + 1` (0: every record) when the group node whose key-order emit sources pipeline `k` is asked
/// for at most `n` rows (frame::iface's request: a Limit over it, count + offset) and its own ops past the emit are
/// projections alone (a HAVING filter drops records the bound would count; a set-returning call is a node of its own,
/// which hands no bound down), so its consumer takes the first `n` records in key order: agg_sort orders only those (its
/// bound; the rest follow unordered and are never read). Which groups' finals and projections run is unchanged: the
/// emit walks the records in order and stops where its consumer stops. Every key must order by its type's built-in
/// comparator reaching no member's user code and not be rowless (`effects::cmp_free`, `effects::rowless`; group's review):
/// btrecordcmp over an anonymous record raises on SOME pairs only (column counts or types that differ past an equal
/// prefix), and PostgreSQL's full Sort under its GroupAggregate meets every pair adjacent in the order, as agg_sort's
/// whole-array sort does, where a bounded one (kernels `top_sort`) may not: such a group keeps the whole-array sort.
fn limit_cut(px: &Px<'_, '_>, k: usize, g: &GroupDecl<'_>) -> u64 {
    let c = px.gx.catalog;
    let total = |kd: &KeyDecl<'_>| gravity_decompose::catalog::expr_type::type_of(kd.expr.node()).is_some_and(|t| crate::effects::cmp_free(c, t) && !crate::effects::rowless(c, t));
    if !g.keys.iter().all(total) {
        return 0;
    }
    let p = &px.gx.pipelines()[k];
    let Some(own) = p.node_ops.first() else { return 0 };
    let end = p.node_ops.get(1).map_or(p.ops.len(), |x| x.first_op as usize);
    if !p.ops.get(own.first_op as usize..end).unwrap_or(&[]).iter().all(|op| matches!(op, Op::Project(_))) {
        return 0;
    }
    match px.request(own.node_id).bound {
        crate::frame::iface::Bound::Rows(n) if n <= gravity_kernels::host_abi::FORM_BOUND_MAX => n + 1,
        _ => 0,
    }
}

impl<'g, 'p> Emit<'g, 'p> {
    /// The emit of group `g` in pipeline `k`; `drain`: the top's drain over the array the emit left.
    pub(crate) fn new(px: &mut Px<'g, 'p>, k: usize, g: &'g gravity_decompose::ops::shared::agg::GroupDecl<'p>, drain: bool) -> Emit<'g, 'p> {
        let gx = px.gx;
        let node = gx.pipelines()[k].node_id;
        // D1398 / D1403: a group whose finals run once (props::finals_once) never reaches a drain: its top orders rows by value
        if drain && crate::props::finals_once(gx, g) {
            bug(node, "a top's drain over a group whose finals run once (D1403: drains_grouping / group_sort route it by value)");
        }
        let mut rec = GroupRec::new(px, g).emit_view(px);
        rec.arena(px, k); // V4R-375: a hosted state's final calls through the Agg's arena
        let keys = crate::table::emit_key::key_outs(px, node, g, &rec);
        let words = [word(px, k, W_CURSOR), word(px, k, W_PTRS), word(px, k, W_KEPT), word(px, k, W_STATE)];
        // C10 2b (D3582): the serial emit refills over a keyed end that spills by partition (decompose's declaration, its
        // records holding no key copy: emit_key.rs `resident` keeps such a table whole)
        let partitioned = crate::frame::iface::end_breaker_of(gx, g.producer.0 as usize).is_some_and(|b| matches!(b.kind, gravity_abi::breaker::BreakerKind::HashAgg | gravity_abi::breaker::BreakerKind::Distinct) && matches!(b.spill, gravity_abi::breaker::Spill::Partitioned { .. }));
        let batches = partitioned && crate::table::emit_key::spills(&rec);
        // form G: V4R-118; M3 runs.group / runs.unique in key order where it declares it (D3644); M6 L0-o a grouped DISTINCT's (D530)
        let in_place = !drain && (gx.end_need(k) == KindId::RunsTopkGroups || crate::frame::iface::sorts_group_in_place(gx, k));
        let key_order = !drain && ((matches!(crate::graph::group_kind(g), KindId::RunsGroup | KindId::RunsUnique) && crate::frame::iface::key_ordered(gx, g)) || crate::frame::iface::order_read(gx, g));
        // a HAVING bound's keep pass makes the emit no plain one, so an unsorted emit over a table that refills by partition
        // keeps no floor: the owner's runs form bounds the records it lays (`owner_floor`) and the refill batches keep the
        // memory inside the grant
        let refilled = batches && !in_place && !key_order && g.two_pass.is_none();
        let floor = if drain || refilled { None } else { crate::ops::sort::topn::strategies::twopass_having::floor(px, k, &rec) };
        let sort = in_place.then(|| crate::ops::sort::topn::order_slot::sorted(px, k, &rec)).or_else(|| key_order.then(|| crate::ops::sort::topn::lower::key_words(px, &rec)).map(|(w, f)| (w, f | limit_cut(px, k, g) << gravity_kernels::host_abi::FORM_BOUND_SHIFT, None)));
        if sort.is_some() && rec.bare != 0 {
            // textkeys step 3: key_canon.rs `bare_keys` keeps no copy only where this emit sorts nothing (`sorted_at_emit`)
            bug(node, "a sorted emit over keys whose record keeps no copy (key.canon bare_keys)");
        }
        let ncols = g.keys.iter().map(|kd| kd.resno.max(0) as usize).max().unwrap_or(0);
        let procs = sort.as_ref().map_or_else(|| PgVec::new_in(px.mcx()), |(w, _, _)| crate::ops::sort::topn::copy_words::procs(px, &rec, w));
        let late = (sort.is_some() && gx.end_need(k) == KindId::RunsTopkGroups).then(|| crate::ops::sort::topn::lower::late_cut(px, k, &rec)).flatten();
        // D3545: an inner gathers at the running call's index (rerun/sets.rs `run_region`, as a sort run's reader and a
        // count(DISTINCT) emit): a spooled drain's tail declares its own list, its spool slot moved last, where the decompose
        // list's index (`Pipes::consumed_at`) named the spool's region and the gather found no table (kind 2)
        let at = (!drain && k >= gx.pipelines().len()).then(|| (crate::rerun::sets::run_region(px, k, g.slot).unwrap_or_else(|| bug(node, "a group emit inner whose owner does not consume its slot")) as u64, word(px, k, 7)));
        let copy = drain && (rec.handle.is_some() || gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::GroupRun(_))));
        let bound = sort.as_ref().map_or(0, |(_, f, _)| ((f & 0xffff_ffff) >> FORM_BOUND_SHIFT) as i64);
        let ordered = !drain && late.is_none() && gx.end_need(k) == KindId::RunsTopkGroups;
        let more = (ordered && bound > 1 && (crate::ops::sort::topn::lower::drops(gx, k) || crate::ops::sort::topn::lower::ties_bound(px, k).is_some())).then(|| (word(px, k, W_SORTED), bound - 1));
        let plain = !drain && sort.is_none() && floor.is_none() && at.is_none() && !copy && late.is_none() && more.is_none() && g.two_pass.is_none();
        let hosted = rec.handle.is_some() || rec.layout.slot_kinds().contains(&gravity_abi::breaker::SlotKind::AggState);
        // a rerun set's member (a nest's / SubPlan's / LATERAL call) runs the emit on the leader at each outer row, where a
        // section never pays: it keeps the serial emit. Any other path that runs a wide emit on the leader (a lazy build's
        // width-1 arm, a pull) reads every slice, so the groups it emits never depend on the path (exec runner/plan.rs).
        let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
        // D3024 (b): a hash build or hash aggregate end, or a row sort / top end that takes section ends from a wide source
        // (ops/sort/lower.rs `wide_row_end`: the run each claim thread fills is sealed at its section end and merged). The
        // row end only when the group's producer has a claims site, the wide emit's nominal driving site (`nominal_site`):
        // its drafted site, or the nominal one of a producer that is itself wide over no site of its own (another group's
        // wide emit, a co-split merge, a spool reader). Over a producer with neither (a serial emit, a window's or a run's
        // reader) the emit stays site-less on the leader's emit arm, which runs no section end, so a run that waits for its
        // section end would never be sealed (no row)
        let sited = px.claims_site(g.producer.0 as usize).is_some();
        // or the client's sink, over a producer with a claims site: a plain emit has no sort, so no ORDER BY reaches its rows
        // and their order is free; each claim's groups go out on its own leg (parallel section.rs: a sink without legs gets
        // the width-1 arm, every slice). Not over a table that refills by partition: its batches bound the memory the
        // claims' whole read would not
        let to_sink = sited && k < gx.pipelines().len() && crate::frame::iface::ends_in_sink(gx, k) && !batches;
        // or a draining probe's seeded end (a group's emit into an inner probe, then a RIGHT SEMI one): each claim's probe
        // marks the build and hands its rows to the drain's end as a scan-driven probe section's claims do. Over a table
        // that spills by partition it reads the table whole, as a hash build or hash aggregate end's wide emit does (no
        // refill: driver mem/batch.rs `read_by_refill` batches a refilling serial emit's slot alone)
        let marks = sited && crate::ops::hashjoin::drain::lower::seeds_drain(gx, k) && !crate::face::strategy::off().has(EMIT_MARKS);
        // D3336 (road merge.wideemit): or a merge join's inner Sort that takes the claims' runs (face/slot_run.rs `emit_fill`)
        let ends = matches!(crate::frame::iface::end_breaker(gx, k), Some(gravity_abi::breaker::BreakerKind::HashBuild | gravity_abi::breaker::BreakerKind::HashAgg))
            || sited && k < gx.pipelines().len() && (crate::ops::sort::lower::wide_row_end(gx, k) || !drain && crate::face::slot_run::emit_fill(px, k))
            || to_sink
            || marks;
        // a LIMIT under the end counts on one open (D281, as a heap source's: source/heap.rs `pipeline_desc`)
        let limit = k < gx.pipelines().len() && crate::frame::iface::holds_limit(&gx.pipelines()[k].ops);
        // a statement that writes or locks rows reruns pipelines in its recheck (EvalPlanQual): the emit stays the leader's
        let recheck = gx.graph.recheck || gx.pipelines().iter().any(|q| q.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
        let wide = plain && !hosted && k < gx.pipelines().len() && ends && !limit && !recheck && !rerun(k) && !rerun(g.producer.0 as usize);
        // D3024 (b), sorted: the sort is form G's own (a top over the groups or a full sort of their records, the first arm of
        // `sort` above), in the runs form, with a pool-safe built-in order (no comparator word), and the top keeps nothing
        // apart from the records it cuts (no ties lead, no kept columns, no VOLATILE order slot read back)
        let form_g = !drain && (gx.end_need(k) == KindId::RunsTopkGroups || crate::frame::iface::sorts_group_in_place(gx, k));
        let runs_form = sort.as_ref().is_some_and(|(_, f, _)| f & gravity_kernels::host_abi::FORM_KIND_MASK == gravity_kernels::host_abi::FORM_RUNS && f & FORM_TIES == 0);
        let own_order = |px: &Px<'_, '_>| procs.iter().all(|&p| p == 0) && crate::ops::sort::topn::lower::count_top(px, k).is_none() && !crate::ops::sort::topn::lower::with_ties(px, k);
        let keeps_none = || crate::ops::sort::topn::order_slot::kept_cols(gx, g).is_empty() && crate::ops::sort::topn::order_slot::read_back_cols(gx, g).is_empty();
        let sorted_wide = form_g && runs_form && floor.is_none() && at.is_none() && !copy && g.two_pass.is_none() && !hosted && sited && k < gx.pipelines().len()
            && px.ident(k) == k && !limit && !recheck && !rerun(k) && !rerun(g.producer.0 as usize) && own_order(px) && keeps_none();
        if let (true, Some((w, f, _))) = (sorted_wide, sort.as_ref()) {
            // D3336 (A1a): its parts cut on a window's PARTITION BY when one reads its drain (face/partfill.rs)
            let kcut = crate::face::partfill::kcut(px, k, w.len());
            crate::table::emit_runs::declare(px, k, g.producer.0 as u16, &rec.layout, w, *f, kcut);
        }
        // D3141 (a); D3266 (L1): under EXPLAIN ANALYZE too, the keep pass counting the groups it removes (`keep_having`)
        let (having, having_node) = if !drain && sort.is_some() && at.is_none() { having_first(px, k, node, g) } else { (PgVec::new_in(px.mcx()), node) };
        if wide && sited {
            px.wide.push_in(k);
        }
        // D4620: a cascade's base's records and its cascaded sets' (never batched: the slot has more readers than the emit)
        let extras = crate::ops::agg::cascade::extras(gx, k, g, drain);
        let refills = plain && !hosted && !wide && k < gx.pipelines().len() && !recheck && !rerun(k) && !rerun(g.producer.0 as usize) && batches && extras.is_none();
        Emit { rec, node, keys, words, floor, sort, procs, drain, copy, ncols, late, at, pinned: false, more, wide, to_sink: wide && to_sink, marks: wide && marks, sorted_wide, merge: None, refills, having, having_node, extras, cut: None }
    }

    /// D3024 (b), sorted: the drain of a top whose emit runs sorted and wide (`runs`: its run table) merges the runs.
    pub(crate) fn merging(self, runs: Option<crate::table::emit_runs::SortedRuns<'g>>) -> Emit<'g, 'p> {
        Emit { merge: runs.filter(|_| self.drain), ..self }
    }

    /// D3336 (A1a): the drain reads its claim's share of the K-cut parts into the object in frame word `cut.1` of the
    /// window whose object table is word `cut.0` (`partfill::cut_form`); `None` keeps the drain's one read of every record.
    pub(crate) fn cutting(self, cut: Option<(u16, i32)>) -> Emit<'g, 'p> {
        Emit { cut: cut.filter(|_| self.plain_drain()), ..self }
    }

    /// The merging drain's run table (`merging`).
    pub(crate) fn runs(&self) -> Option<&crate::table::emit_runs::SortedRuns<'g>> {
        self.merge.as_ref()
    }

    /// D3024 (b), sorted: whether the emit's claims sort their own runs (`sorted_wide`).
    pub(crate) fn sorted_wide(&self) -> bool {
        self.sorted_wide
    }

    /// D3336 (A1a): the drain reads every merged record as it is: no bound, copy, HAVING or floor pass, order slot or kept
    /// side (a K-cut drain's claims each read a range of them).
    pub(crate) fn plain_drain(&self) -> bool {
        let unbounded = self.merge.as_ref().is_some_and(|r| r.n.is_none());
        self.drain && unbounded && !self.copy && self.having.is_empty() && self.floor.is_none() && self.late.is_none() && self.more.is_none() && self.sort.is_none() && !self.refills
    }

    /// The emit descriptor (host_abi.rs `DESC_*`) on the stack: the record's layout, the runs form, the top's sort words.
    fn desc(&self, b: &mut FunctionBuilder<'_>) -> Value {
        let (sort, form) = self.sort.as_ref().map_or((&[][..], 0), |(w, f, _)| (&w[..], *f));
        // EXPLAIN ANALYZE: a cut emit hands on fewer records than the group has (PostgreSQL's Sort reads every one)
        if (form & 0xffff_ffff) >> FORM_BOUND_SHIFT != 0 {
            crate::frame::instr::partial();
        }
        desc_cmp(b, &self.rec.layout, sort, &self.procs, form)
    }

    fn gather(&self, b: &mut FunctionBuilder<'_>) -> HostFn {
        if self.extras.is_some() {
            return self.gather_x(b);
        }
        let (name, args, f) = if self.sorted_wide {
            ("agg_gather_claim_pinned", &[Type::Ptr, Type::Ptr][..], agg_gather_claim_pinned as *const () as usize)
        } else if self.wide {
            ("agg_gather_claim", &[Type::Ptr, Type::Ptr][..], agg_gather_claim as *const () as usize)
        } else if self.at.is_some() { ("agg_gather_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr][..], agg_gather_at as *const () as usize) } else { ("agg_gather", &[Type::Ptr, Type::Ptr][..], agg_gather as *const () as usize) };
        b.declare_host_once(name, args, Some(Type::Ptr), f, HostFlags { touches_memory: true, ..HostFlags::default() })
    }

    /// D4620: `gather`'s forms over the base's region and its cascaded sets' (`extras`, their last argument).
    fn gather_x(&self, b: &mut FunctionBuilder<'_>) -> HostFn {
        use gravity_kernels::host_abi::{agg_gather_at_x, agg_gather_claim_pinned_x, agg_gather_claim_x, agg_gather_x};
        let (name, args, f) = if self.sorted_wide {
            ("agg_gather_claim_pinned_x", &[Type::Ptr, Type::Ptr, Type::Ptr][..], agg_gather_claim_pinned_x as *const () as usize)
        } else if self.wide {
            ("agg_gather_claim_x", &[Type::Ptr, Type::Ptr, Type::Ptr][..], agg_gather_claim_x as *const () as usize)
        } else if self.at.is_some() {
            ("agg_gather_at_x", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr, Type::Ptr][..], agg_gather_at_x as *const () as usize)
        } else {
            ("agg_gather_x", &[Type::Ptr, Type::Ptr, Type::Ptr][..], agg_gather_x as *const () as usize)
        };
        b.declare_host_once(name, args, Some(Type::Ptr), f, HostFlags { touches_memory: true, ..HostFlags::default() })
    }

    /// D4620: the extra regions' list `[n, k1 .. kn]` on the stack (`gather_x`'s last argument).
    fn extras_arg(&self, b: &mut FunctionBuilder<'_>) -> Option<Value> {
        let x = self.extras.as_ref()?;
        let slot = b.declare_stack_slot(8 * (x.len() as u32 + 1), 8);
        let p = b.stack_addr(slot);
        for (i, w) in core::iter::once(x.len() as u64).chain(x.iter().copied()).enumerate() {
            let v = b.const_raw(Type::I64, w);
            b.store(p, 8 * i as i32, v);
        }
        Some(p)
    }

    /// RUNS-18: the top's form word carries a bound WITH TIES (`FORM_TIES`).
    fn ties(&self) -> bool {
        self.sort.as_ref().is_some_and(|(_, f, _)| f & FORM_TIES != 0)
    }

    /// The window's end within the records in order. At the in-order end the next `max(n, cursor)` records are ordered
    /// (doubling: every record is ordered in O(m log m) at most), a failed compare leaving by `l_eof`. The kernel's own
    /// `agg_sort` orders them: the state's records are narrowed to the rest for the call (the pointers past `cursor`, their
    /// count) and the descriptor's bound is the count, then the state is put back. The order is total (agg_sort's key
    /// words end every comparison), so the chunks follow one another as one sort of every record would order them.
    #[allow(clippy::too_many_arguments)]
    fn sort_more(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block, at: i32, n: i64, cursor: Value, kept: Value, end: Value) -> Value {
        let sorted = env.load_word(b, at);
        let need = b.icmp(IntCond::Sge, cursor, sorted);
        let (l_sort, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(need, (l_sort, &[]), (l_go, &[]));
        b.switch_to(l_sort);
        let n = b.iconst(Type::I64, n);
        let small = b.icmp(IntCond::Slt, cursor, n);
        let count = b.select(small, n, cursor);
        let st = env.load_word(b, self.words[W_STATE as usize]);
        let st = b.conv(ConvOp::IntToPtr, Type::Ptr, st);
        let (ptrs, all) = (b.load(Type::Ptr, st, ES_PTRS), b.load(Type::I64, st, ES_KEPT));
        let rest = b.ptr_add(ptrs, cursor, 8, 0);
        let left = b.int_bin(IntBinOp::Sub, all, cursor);
        b.store(st, ES_PTRS, rest);
        b.store(st, ES_KEPT, left);
        let desc = self.desc(b);
        // the bound field holds `count + 1`; past its width every remaining record is ordered (0: no bound). A top WITH TIES
        // orders the next `count` here like a HAVING top (`FORM_TIES` off: the call answers 0)
        let form = self.sort.as_ref().map_or(0, |(_, f, _)| *f) & !((FORM_BOUND_MAX + 1) << FORM_BOUND_SHIFT | FORM_TIES);
        let one = b.iconst(Type::I64, 1);
        let field = b.int_bin(IntBinOp::Add, count, one);
        let max = b.iconst(Type::I64, FORM_BOUND_MAX as i64);
        let wide = b.icmp(IntCond::Sgt, count, max);
        let field = b.select(wide, env.zero, field);
        let shift = b.iconst(Type::I64, FORM_BOUND_SHIFT as i64);
        let field = b.int_bin(IntBinOp::Shl, field, shift);
        let form = b.iconst(Type::I64, form as i64);
        let form = b.int_bin(IntBinOp::Or, form, field);
        b.store(desc, DESC_FORM as i32, form);
        let h = b.declare_host_once("agg_sort", &[Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), agg_sort as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
        let r = b.call_host(h, &[env.ctx, st, desc], Some(Type::I64)).expect("agg_sort returns its status");
        b.store(st, ES_PTRS, ptrs);
        b.store(st, ES_KEPT, all);
        let (failed, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(failed, (l_eof, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        let to = b.int_bin(IntBinOp::Add, cursor, count);
        let past = b.icmp(IntCond::Sgt, to, kept);
        let to = b.select(past, kept, to);
        env.store_word(b, at, to);
        b.br(l_go, &[]);
        b.switch_to(l_go);
        let sorted = env.load_word(b, at);
        let past = b.icmp(IntCond::Sgt, end, sorted);
        b.select(past, sorted, end)
    }

    /// Record `j` of the kept array.
    /// D3141 (a): the records of `ptrs[0 .. len)` whose group passes every HAVING qual (`having`), compacted in order to the
    /// front: their number. Each group's row is the one the stream builds (`group_row`: its keys and finals), its renders
    /// given back after its test (PostgreSQL's per-tuple reset). Under EXPLAIN ANALYZE the removed ones count as the HAVING
    /// filter's (`Ctr::Filtered1`), where that filter counts its failures.
    fn keep_having(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ptrs: Value, len: Value) -> Value {
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, ptrs);
        env.store_word(b, self.words[W_PTRS as usize], pi); // group_row's read (filed again after)
        let (memo, aggs) = (env.memo.set_aside(), core::mem::take(&mut env.aggs)); // the loop's values reach no code after it
        let (h, body) = crate::frame::cancel::header(b, env.ctx, env.l_cancel, &[Type::I64, Type::I64], env.mcx);
        let l_done = b.create_block(&[Type::I64]);
        b.br(h, &[env.zero, env.zero]);
        b.switch_to(body);
        let (i, kept) = (b.block_params(body)[0], b.block_params(body)[1]);
        let (more, l_step, l_next) = (b.icmp(IntCond::Slt, i, len), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(more, (l_step, &[]), (l_done, &[kept]));
        b.switch_to(l_step);
        let mark = env.hir.mark(b);
        let row = self.group_row(b, env, i, Carry::new(env.zero, env.mcx), env.zero);
        for &q in &self.having {
            let (ok, l_ok) = (crate::step::expr::pred(b, env, self.node, &row, q), b.create_block(&[]));
            b.cond_br(ok, (l_ok, &[]), (l_next, &[kept]));
            b.switch_to(l_ok);
        }
        let rec = self.record(b, env, i);
        let to = b.ptr_add(ptrs, kept, 8, 0);
        b.store(to, 0, rec);
        let one = b.iconst(Type::I64, 1);
        let k2 = b.int_bin(IntBinOp::Add, kept, one);
        b.br(l_next, &[k2]);
        b.switch_to(l_next);
        let kept2 = b.block_params(l_next)[0];
        if let Some(m) = mark {
            env.hir.give_back(b, m);
        }
        let one = b.iconst(Type::I64, 1);
        let i2 = b.int_bin(IntBinOp::Add, i, one);
        b.br(h, &[i2, kept2]);
        b.switch_to(l_done);
        env.memo.restore(memo);
        env.aggs = aggs;
        let kept = b.block_params(l_done)[0];
        // EXPLAIN ANALYZE: the groups the pass removed are the HAVING filter's (its step then passes every kept one)
        if crate::frame::instr::filters_counted(self.having_node, env.node) {
            let removed = b.int_bin(IntBinOp::Sub, len, kept);
            crate::frame::instr::count(b, env.ctx, self.having_node, crate::frame::instr::Ctr::Filtered1, removed);
        }
        kept
    }

    fn record(&self, b: &mut FunctionBuilder<'_>, env: &Env, j: Value) -> Value {
        let ptrs = env.load_word(b, self.words[W_PTRS as usize]);
        let ptrs = b.conv(ConvOp::IntToPtr, Type::Ptr, ptrs);
        let at = b.ptr_add(ptrs, j, 8, 0);
        b.load(Type::Ptr, at, 0)
    }
}

impl<'g, 'p> SourceMech for Emit<'g, 'p> {
    /// D2561: a top-level emit that orders its array keeps it past its call (the group's records are consumed region 0 of a
    /// top-level emit, agg_gather's own); a drain, an unordered emit or an inner's (already cached) cannot be asked to.
    fn keep_past_call(&mut self, px: &mut Px<'_, '_>, k: usize) -> bool {
        // D3024 (b), sorted: each claim's array is a statement pin already, and the drain merges the runs (no gather again)
        if self.sorted_wide {
            return true;
        }
        if self.drain || self.sort.is_none() || self.at.is_some() || k >= px.gx.pipelines().len() {
            return self.pinned;
        }
        self.at = Some((0, word(px, k, 7)));
        self.pinned = true;
        true
    }

    /// A split group's records are its pass 2's (keyed_twopass). D3024 (b): a wide emit is eligible, its claims the owner
    /// runs' slices, its driving site the producer's, nominal (`nominal_site`); a hosted call a pool thread cannot run (an
    /// fmgr site, a volatile or parallel-unsafe call) or a serial SRF in the pipeline keeps it on the leader (D1182, as the
    /// spill pair), and frame/mechs.rs and frame/program.rs keep a lazy build's probe, a serial step, a pulled set's member
    /// and an InitPlan's caller there too.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = crate::ops::sort::topn::strategies::twopass::rehome(px.arena, px.gx.pipelines()[k].desc, self.rec.g.slot, self.rec.g.producer, self.rec.home);
        let d = Pipeline { refills: self.refills, ..d };
        let Some(site) = self.nominal_site(px) else { return d };
        // a wide emit kept on the leader stays site-less, one call: its row sort or top (ops/sort/lower.rs, topk_rows.rs)
        // read the same `kept_on_leader` and seal their run themselves. A nominal site there would run the ALL claim and
        // the section end, two calls, each evaluating the pipeline's one-time gate (e2e v4/result_d2571_gate_once)
        match kept_on_leader(px, k) {
            true => d,
            false => {
                if px.pipeline_site(self.rec.g.producer.0 as usize).is_none() {
                    fires::fire("emit.wide.nominal"); // shapecheck fire signal: a wide emit over a producer wide on a nominal site
                }
                if self.to_sink {
                    fires::fire("emit.wide.sink"); // shapecheck fire signal: a wide emit's claims go to the client's legs
                }
                if self.marks {
                    fires::fire("emit.wide.marks"); // shapecheck fire signal: a wide emit's claims probe and mark a drained build
                }
                if self.sorted_wide {
                    fires::fire("emit.sorted.wide"); // shapecheck fire signal: a group sort's claims sort their own runs
                }
                Pipeline { driving_site: site as i16, parallel: gravity_program::Parallel::Eligible, claims: gravity_program::ClaimKind::OwnerRuns, ..d }
            }
        }
    }

    /// D3024 (b): a wide emit's nominal driving site: its group's producer's claims site (`Px::claims_site`; the runner
    /// reads no row through it).
    fn nominal_site(&self, px: &Px<'_, '_>) -> Option<usize> {
        if !self.wide && !self.sorted_wide { return None; }
        px.claims_site(self.rec.g.producer.0 as usize)
    }

    fn open_words(&self, w: &mut OpenWords) {
        for (i, what) in ["emit cursor", "emit records", "emit kept", "emit state"].into_iter().enumerate() {
            w.declare(OpenWord::open(what, Base::Words, self.words[i], Type::I64, 0));
        }
        if let Some((at, _)) = self.more {
            w.declare(OpenWord::open("emit sorted", Base::Words, at, Type::I64, 0));
        }
        if let Some((_, at)) = self.cut {
            w.declare(OpenWord::open("drain object", Base::Words, at, Type::I64, 0));
        }
    }

    /// The fresh call: the gather (the kernel's fresh test is the resume word, so it reads NONE around the call; an inner's
    /// is fresh by its flag, `at`), the floor's kept pass, the array and its kept count filed in the frame words.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        // D3024 (b), sorted: a merging drain's descriptor is its top's (the drain sorts nothing itself)
        let desc = match &self.merge {
            Some(r) => desc(b, &self.rec.layout, r.words, r.form),
            None => self.desc(b),
        };
        // D3336 (A1a): a K-cut drain's claim: the records it reads and the object it fills (none: no merge, no record); its
        // exit block made first (any other emit's blocks as before)
        let cut = match (&self.merge, self.cut) {
            (Some(r), Some((tbl, obj))) => {
                let l_out = b.create_block(&[Type::I64]);
                Some((crate::table::emit_runs::claim(b, env, r, tbl, obj, self.words[W_KEPT as usize], l_out), l_out))
            }
            _ => None,
        };
        let claim = cut.map(|c| c.0);
        let (st, merged) = match &self.merge {
            // the claims' runs merged
            Some(r) => {
                let v = crate::table::emit_runs::merge(b, env, desc, r);
                (b.conv(ConvOp::IntToPtr, Type::Ptr, v), Some(v))
            }
            None => {
                let gather = self.gather(b);
                let at = self.at.map(|(i, w)| [b.iconst(Type::I64, (i | GATHER_FRESH) as i64), b.ptr_add(env.words.expect("an inner's frame words"), env.zero, 1, w)]);
                let mark = |b: &mut FunctionBuilder<'_>, w: i64| if !self.drain && (at.is_none() || self.pinned) { let w = b.iconst(Type::I64, w); b.store(env.ctx, hdr::RESUME as i32, w) };
                mark(b, RESUME_NONE);
                let extras = self.extras_arg(b);
                let st = b.call_host(gather, &[&[env.ctx, desc][..], at.as_ref().map_or(&[][..], |a| &a[..]), extras.as_slice()].concat(), Some(Type::Ptr)).expect("agg_gather returns its state");
                mark(b, RESUME_OPEN);
                (st, None)
            }
        };
        let (sti, l_run) = (b.conv(ConvOp::PtrToInt, Type::I64, st), b.create_block(&[]));
        let l_out = match cut {
            Some((_, l)) => l,
            None => b.create_block(&[Type::I64]),
        };
        // a failed compare of the merge leaves with its status
        if let Some(v) = merged {
            let (failed, l_merged) = (b.icmp(IntCond::Slt, v, env.zero), b.create_block(&[]));
            b.cond_br(failed, (l_out, &[v]), (l_merged, &[]));
            b.switch_to(l_merged);
        }
        let refused = b.icmp(IntCond::Eq, sti, env.zero);
        let unsup = b.iconst(Type::I64, unsupported(UNSUPPORTED_SCRATCH));
        b.cond_br(refused, (l_out, &[unsup]), (l_run, &[]));
        b.switch_to(l_run);
        let (ptrs, kept) = (b.load(Type::Ptr, st, ES_PTRS), b.load(Type::I64, st, ES_KEPT));
        let ptrs = if self.copy { crate::ops::agg::trans::hosted::drain_copy(b, env, ptrs, kept, l_out) } else { ptrs }; // V4R-375
        let kept = if self.having.is_empty() { kept } else { self.keep_having(b, env, ptrs, kept) }; // D3141 (a)
        crate::ops::sort::topn::order_slot::run(b, env, self, ptrs, kept, &self.sort); // before any keep: every group's key, as PG's Sort
        let kept = match &self.floor {
            Some(f) => crate::ops::sort::topn::strategies::twopass_having::keep(b, env, ptrs, kept, f),
            None => kept,
        };
        if claim.is_none() {
            b.store(st, ES_KEPT, kept);
        }
        if let (Some(t), Some((_, form, _))) = (self.late, self.sort.as_ref()) {
            t.store_cut(b, env, desc, *form);
        }
        if self.sort.is_some() {
            let h = b.declare_host_once("agg_sort", &[Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), agg_sort as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            let r = b.call_host(h, &[env.ctx, st, desc], Some(Type::I64)).expect("agg_sort returns its status");
            let (failed, l_sorted) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
            b.cond_br(failed, (l_out, &[r]), (l_sorted, &[]));
            b.switch_to(l_sorted);
            if let Some((at, n)) = self.more {
                // RUNS-18: a bound WITH TIES returns how many records it ordered (the ties and the record past them)
                let sorted = if self.ties() {
                    r
                } else {
                    let n = b.iconst(Type::I64, n);
                    let past = b.icmp(IntCond::Sgt, n, kept);
                    b.select(past, kept, n)
                };
                env.store_word(b, at, sorted);
            }
        }
        env.store_word(b, self.words[W_STATE as usize], sti);
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, ptrs);
        env.store_word(b, self.words[W_PTRS as usize], pi);
        // D3336 (A1a): a K-cut claim's range of the merged records, each end at most their count (a plain drain: the
        // state's array shared by every claim, read as it is)
        let kept = match claim {
            Some((lo, hi)) => {
                let lo = crate::table::emit_runs::clamp_u(b, lo, kept);
                env.store_word(b, self.words[W_CURSOR as usize], lo);
                crate::table::emit_runs::clamp_u(b, hi, kept)
            }
            None => kept,
        };
        env.store_word(b, self.words[W_KEPT as usize], kept);
        b.br(l_out, &[env.zero]);
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, kept) = if self.refills {
            self.refill(b, env, l_eof)
        } else {
            let kept = env.load_word(b, self.words[W_KEPT as usize]);
            let cursor = env.load_word(b, self.words[W_CURSOR as usize]);
            let at_end = b.icmp(IntCond::Sge, cursor, kept);
            let l_win = b.create_block(&[]);
            b.cond_br(at_end, (l_eof, &[env.zero]), (l_win, &[]));
            b.switch_to(l_win);
            (cursor, kept)
        };
        let w = b.iconst(Type::I64, WINDOW);
        let end = b.int_bin(IntBinOp::Add, cursor, w);
        let over = b.icmp(IntCond::Sgt, end, kept);
        let end = b.select(over, kept, end);
        let end = match self.more {
            Some((at, n)) => self.sort_more(b, env, l_eof, at, n, cursor, kept, end),
            None => end,
        };
        Win { cursor, end, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.group_row(b, env, j, carry, next)
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.words[W_CURSOR as usize], cur);
    }
}

impl<'g, 'p> Emit<'g, 'p> {
    /// C10 2b (D3582): the window's start of a refilling emit (`refills`): at its records' end it asks for the next batch
    /// (`agg_gather_next`: the batch's emit state, 0 past the last batch or for a slot not batched, a negative status for
    /// the driver's error), files the state's array and kept count in the frame words with the cursor back at 0, and looks
    /// again (a batch may keep no record); `(cursor, kept)` on the window's block.
    fn refill(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> (Value, Value) {
        let (l_head, l_win, l_more, l_got) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.br(l_head, &[]);
        b.switch_to(l_head);
        let kept = env.load_word(b, self.words[W_KEPT as usize]);
        let cursor = env.load_word(b, self.words[W_CURSOR as usize]);
        let at_end = b.icmp(IntCond::Sge, cursor, kept);
        b.cond_br(at_end, (l_more, &[]), (l_win, &[]));
        b.switch_to(l_more);
        let desc = self.desc(b);
        let next = b.declare_host_once("agg_gather_next", &[Type::Ptr, Type::Ptr], Some(Type::I64), agg_gather_next as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
        let r = b.call_host(next, &[env.ctx, desc], Some(Type::I64)).expect("agg_gather_next returns its state");
        let (none, failed) = (b.icmp(IntCond::Sle, r, env.zero), b.icmp(IntCond::Slt, r, env.zero));
        let status = b.select(failed, r, env.zero);
        b.cond_br(none, (l_eof, &[status]), (l_got, &[]));
        b.switch_to(l_got);
        let st = b.conv(ConvOp::IntToPtr, Type::Ptr, r);
        let (ptrs, got) = (b.load(Type::Ptr, st, ES_PTRS), b.load(Type::I64, st, ES_KEPT));
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, ptrs);
        env.store_word(b, self.words[W_STATE as usize], r);
        env.store_word(b, self.words[W_PTRS as usize], pi);
        env.store_word(b, self.words[W_KEPT as usize], got);
        env.store_word(b, self.words[W_CURSOR as usize], env.zero);
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        b.br(l_head, &[]);
        b.switch_to(l_win);
        (cursor, kept)
    }

    /// Kept record `j` as a row: its keys by resno, its finals as the row's aggregate cells.
    pub(crate) fn group_row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let rec = self.record(b, env, j);
        record_row(&self.rec, &self.keys, self.node, self.ncols, b, env, rec, (carry, next, j))
    }
}

/// Group record `rec` of `r` as a row at `(carry, next, at)`: its keys by resno (`keys`, the widest `ncols`), its finals as
/// the row's aggregate cells. The emit's kept record, and a streamed group's open one (keyed/stream.rs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_row<'e>(r: &GroupRec<'_, '_>, keys: &[crate::table::emit_key::KeyOut<'_>], node: i32, ncols: usize, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rec: Value, (carry, next, at): (Carry<'e>, Value, Value)) -> RowVals<'e> {
    // CAP-28: the flags words (one per 64 keys), each loaded once, before the keys read them
    let flags: PgVec<'_, Value> = (0..r.flag_count()).filter_map(|w| r.null_flag(64 * w)).map(|(w, _)| b.load(Type::I64, rec, 8 * w as i32)).collect_in(env.mcx);
    let mut row = RowVals { row: Row { cells: mcx::reserved_in(keys.len(), env.mcx) }, vals: mcx::reserved_in(keys.len(), env.mcx), cols: mcx::pg_vec![in env.mcx; None; ncols], carry, next, at };
    for i in 0..keys.len() {
        let (w, shift) = gravity_abi::breaker::flag_at(i);
        let null = flags.get(w).map(|&f| {
            let s = b.iconst(Type::I64, shift as i64);
            let bit = b.int_bin(IntBinOp::Ushr, f, s);
            let one = b.iconst(Type::I64, 1);
            let bit = b.int_bin(IntBinOp::And, bit, one);
            b.icmp(IntCond::Ne, bit, env.zero)
        });
        let (cell, v, n) = crate::table::emit_key::key_cell(r, keys, node, b, env, i, rec, null, &row);
        let resno = r.g.keys[i].resno;
        if resno > 0 {
            row.cols[resno as usize - 1] = Some(row.row.cells.len() as u16);
        }
        row.row.cells.push_in(cell);
        row.vals.push_in((v, n));
    }
    env.aggs = Some(crate::ops::group::stream::finals_of(r, b, env, rec, &row));
    row
}
