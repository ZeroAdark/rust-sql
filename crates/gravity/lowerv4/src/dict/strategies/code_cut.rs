//! Road `code.cut` (cards/M6.md L4, H10, R4-L13, row I32): a text-first top over a pgrc Code key keeps each part's `n`
//! smallest codes and drops a row whose code is past its part's cut before any render. Every code top-k road registers
//! a `CodeTopkImpl` (M6-plan §3.7; face/runs.rs since V4R-294), which runs/topk_rows.rs and runs/topk_groups.rs read
//! through L0-f when `CodeRep::Cut` / `DictPolicy::Heads` is on.
//! Engaged form (L6 R3, V4R-294): a prefilter in front of runs.topk_rows's own end (which still orders every key, the
//! Code key by L0-a's arm: the render's prefix word, the render as the tail). A first key under a collation whose order
//! is bytes (a part's dictionary codes are in byte order) keeps, per part, the `n` smallest codes seen (a descending key:
//! the `n` largest, as the smallest complements; AGA-14) in the frame words (kernels `topk_cut`, v2
//! `tail/topk.rs:191-260`); a row whose code lies past its part's cut is dropped
//! before the top renders it: `n` rows of its own part order strictly before it, so no top of `n` (WITH TIES included)
//! keeps it. A NULL key, a `n` past the entry's capacity (`capacity`: the top's own `n` when the plan gives it, RUNS-12)
//! or a key that is not a code lane reach the top unfiltered.
//! Collated form (`By::Coll`): a first key under an ICU collation while key.coll is on (rep/varkey.rs `coll_prefix`;
//! PostgreSQL's abbreviated key: the sort key's first 8 bytes, `m6_coll_prefix`), ascending or descending, cuts over
//! that prefix word in one entry for every part (the prefixes of two parts compare as their values do). The word is
//! memoised per `part << 32 | code` in `MEMO` direct-mapped frame slots, so a part's dictionary is keyed once per code,
//! not once per row. A prefix orders as the values do where two
//! prefixes differ and says nothing where they tie, so only a row whose word lies strictly past the cut (`n` rows' words
//! strictly before it) is dropped; a tie reaches the top, whose comparator decides it.
//! Seeded claim (code form only, NEW-45): an entry fills from its part's rows, so a part whose rows descend lets every row
//! through until its cut holds. Where every row of the part reaches the cut (`seedable`: a pgrc range scan whose pipeline
//! only projects, the key a bare column read straight from its code lane) and the window says each of its part's
//! dictionary codes stands for a row it delivers (`site::PART_ENTRIES_SAFE`, the host's `entries_safe`: pgrc seals a
//! dictionary from its part's own values, and a part with a delete vector or a read under a transaction snapshot is not
//! safe), the part's `n` least codes are held by at least `n` of the rows the cut sees, so the claim takes the `n`-th
//! least code as the cut at once (`n - 1`; a descending key, the complement of `ncodes - n`), from the lane's code count,
//! where it holds more codes than `n` and no delete vector. A collated or word key keeps the entry's fill. Owner M6-CODES.
use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::site;
use gravity_abi::LANE_SIZE;
use gravity_decompose::graph::Bound;
use gravity_decompose::{Am, Op, Source};
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::catalog::typefacts::{facts_of, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::Rep;
use mcx::{CollectIn, PgBox, PgVec};
use types_nodes::primnodes::OUTER_VAR;

use crate::cell::At;

use crate::face::end::EndMech;
use crate::face::kinds::Face;
use crate::face::runs::{CodeTopkImpl, CodeTopkMech, TopDesc};
use crate::frame::env::Env;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::Reason;
use crate::ops::Table;

crate::strategies! {
    /// Per-part code cut before render.
    CODE_CUT = 79, "code.cut", CodeTopk;
}

pub(crate) const ROWS: Table = Table { strategies: &[(CODE_CUT, &CUT)], ..crate::ops::NONE };

/// The rows a prefilter keeps per part when the plan does not give the top's `n` (a parameter or a computed bound), or
/// gives one the entry cannot hold: its frame words are the cut, the owner, the length, the least code, then that many
/// codes (kernels `CUT_OFF_*`), the most the entry holds; a larger run-time `n` reaches the top unfiltered.
const NMAX: i64 = gravity_program::cost::TOPK_CUT_MAX_N as i64;
/// The entry's words before its codes.
const HEAD: i64 = (gravity_kernels::host_abi::CUT_OFF_CODES / 8) as i64;
const _: () = assert!(HEAD + NMAX <= u8::MAX as i64, "the entry's frame words are indexed by a u8");

/// RUNS-12 / B3-12: the entry's capacity from the top's Demand (frame::iface `Demand::rows`: `count + offset`, both
/// plan constants, an absent offset 0) up to `TOPK_CUT_MAX_N`; an unknown, zero or larger demand sizes it at `NMAX`, its
/// run-time `n` checked against it (past it the top runs unfiltered).
fn capacity(form: &SortForm<'_>) -> i64 {
    let (SortForm::TopK { count, offset, ties } | SortForm::Walk { count, offset, ties, .. }) = form else { return NMAX };
    let rows = crate::frame::iface::Demand { count, offset, ties: *ties }.rows();
    rows.and_then(|n| i64::try_from(n).ok()).filter(|n| (1..=NMAX).contains(n)).unwrap_or(NMAX)
}

/// The prefilter's frame words (`WordKey::Open { face: Runs, k, j: WORDS_J, i }`), apart from the top's own (`j: MAX`).
const WORDS_J: u64 = u64::MAX - 1;
/// The collated form's memo: `MEMO` slots of `[tag, word]` (`j: MEMO_J`), slot `code % MEMO`, the tag `(part << 32 | code)
/// + 1` (0: empty, every open).
const MEMO_J: u64 = u64::MAX - 2;
/// D2982: a top past the entry's capacity counts each part's entries (`[owner, count, most]`, `j: COUNT_J`).
const COUNT_J: u64 = u64::MAX - 3;
const MEMO: u8 = 16;

/// How the prefilter reads the first key: a part's code (code.cut), or its collation's memoised sort-key prefix in one
/// entry (code.cut's collated form).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum By {
    Code,
    Coll,
}

/// The form a code.cut row takes for first key `k0`: its code under a collation that orders by bytes, else the collated
/// prefix under an ICU collation; either direction (AGA-14: a descending key is complemented, and a part's `n` smallest
/// complemented keys are its `n` largest).
fn cut_form(k0: &gravity_decompose::ops::shared::sort::SortKey) -> Option<By> {
    if k0.compare_fn() {
        return None;
    }
    if k0.image.is_text() && k0.coll.bytes_order {
        return Some(By::Code);
    }
    let collated = k0.image.text_like() && !k0.coll.bytes_order;
    (collated && crate::rep::varkey::coll_prefix(k0.coll.oid)).then_some(By::Coll)
}

/// NEW-45: whether a claim may seed (the module doc): producer `top.k` a pgrc range scan whose pipeline only projects
/// before its sort (no filter, so no zone predicate or per-code table either: every row of a part reaches the cut), the
/// top's first key, run column `at`, a bare column through each projection back to a scan column. The row's cell must
/// then be read straight from its code lane (`Pre::seed`).
fn seedable(px: &Px<'_, '_>, top: &TopDesc<'_, '_>, at: i16) -> bool {
    let p = &px.gx.pipelines()[top.k];
    let Source::Range { am: Am::Pgrcolumnar, .. } = p.source else { return false };
    let mut c = Some(at);
    for op in p.ops.iter().rev() {
        c = match (op, c) {
            (Op::Sort(_), c) => c,
            (Op::Project(list), Some(c)) => usize::try_from(c).ok().and_then(|c| c.checked_sub(1)).and_then(|i| list.iter().nth(i)).and_then(|e| crate::step::project::bare_var(e.as_target_entry().map_or(e, |t| t.expr))),
            _ => None,
        };
    }
    c.is_some()
}

/// Whether flag `m` is set in the zero-extended byte `v`.
fn flag(b: &mut FunctionBuilder<'_>, v: Value, m: u8, zero: Value) -> Value {
    let m = b.iconst(Type::I64, m as i64);
    let x = b.int_bin(IntBinOp::And, v, m);
    b.icmp(IntCond::Ne, x, zero)
}

/// The engaged row of code.cut.
pub(crate) struct Cut(pub(crate) By);

impl CodeTopkMech for Cut {
    fn engages(&self, _gx: &Gx<'_, '_>, top: &TopDesc<'_, '_>) -> bool {
        let k = top.sd.keys.as_slice();
        match (self.0, k) {
            // USEROP: a CompareFn first key (a user family's comparator) orders by no code or word
            (By::Code, [k0, ..]) => cut_form(k0).is_some(),
            _ => false,
        }
    }

    fn end<'g>(&self, px: &mut Px<'g, '_>, top: &TopDesc<'_, '_>, inner: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
        let k0 = top.sd.keys.as_slice()[0];
        let Some(te) = top.sd.tlist.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == k0.resno) else { return inner };
        let Some(v) = te.expr.as_var().filter(|v| v.varno == OUTER_VAR) else { return inner };
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        let Some(tf) = facts_of(px.gx.facts, typ) else { return inner };
        let mcx = px.mcx();
        let mut w = |j: u64, i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k: top.k, j, i }) as i32 * 8;
        let w_n = w(u64::MAX, runs_word::TOP_N); // the top's own n (its open stores it before any row)
        let by = if self.0 == By::Code { cut_form(&k0).unwrap_or(By::Code) } else { self.0 };
        let cap = capacity(&top.sd.form);
        let entry: PgVec<'_, i32> = (0..(HEAD + cap) as u8).map(|i| w(WORDS_J, i)).collect_in(mcx);
        let memo: PgVec<'_, i32> = if by == By::Coll { (0..2 * MEMO).map(|i| w(MEMO_J, i)).collect_in(mcx) } else { PgVec::new_in(mcx) };
        if entry.windows(2).chain(memo.windows(2)).any(|p| p[1] != p[0] + 8) {
            crate::refuse!(top.node, Reason::Bug { payload: "a code top-k prefilter's frame words are not one entry" });
        }
        // D2982: a run-time `n` the entry may not hold (the plan's demand unknown or past NMAX) counts entries instead
        let count = (by == By::Code && cap == NMAX).then(|| w(COUNT_J, 0));
        if count.is_some_and(|c| w(COUNT_J, 1) != c + 8 || w(COUNT_J, 2) != c + 16) {
            crate::refuse!(top.node, Reason::Bug { payload: "a code top-k prefilter's count words are not one triple" });
        }
        fires::fire("code.cut"); // shapecheck fire signal (REGISTRY RUNS-12)
        let memo = memo.first().copied();
        let seed = by == By::Code && seedable(px, top, v.varattno);
        // D2981: the drafted site walks each part's entries from its highest code (dict_entries.rs `entries_desc`)
        let entries_desc = px.site_mut(top.k).is_some_and(|s| s.entries_desc && s.entries == Some(0));
        // B3-24: a part's NULL row follows its entries (dict_entries.rs `entries_nulls`), so a window there never ends early
        let nulls_after = px.site_mut(top.k).is_some_and(|s| s.entries_nulls && s.entries == Some(0));
        mcx::pg_box_dyn!(mcx, Pre { inner, by, attno: v.varattno, tf, coll: k0.coll, desc: k0.desc, node: top.node, w_n, n_const: None, entries_only: false, null_ahead: false, w0: entry[0], cap, memo, seed, entries_desc, nulls_after, count } => dyn EndMech + 'g)
    }
}

pub(crate) const CUT: CodeTopkImpl = CodeTopkImpl(&Cut(By::Code));

/// D3007: the top over a value-set group that code.cut's group form takes: `(n, first sort key)`. Producer `k`'s site
/// publishes its one code lane as each part's dictionary entries (dict_entries.rs: a group with no aggregate whose one
/// key is that column, `SELECT DISTINCT x` / `GROUP BY x`), the key a bare column (not derived), and the one pipeline
/// draining the group is a top (runs.topk_groups) whose first sort key is that key under a collation that orders by
/// bytes, through projections alone (no HAVING or other filter between the group and the top: a group dropped there
/// would let a cut one show), its demand (`count + offset`) a plan constant the entry holds. The top's `n` groups are
/// then the key's `n` least (greatest, descending) values, and a part's entries are its distinct values in code
/// (byte) order: a value with `n` values of its own part strictly before it is no top group, so its entry is dropped
/// before the group hashes it, and an ascending entries window ends at the first entry past its part's cut.
fn group_top<'g, 'p>(px: &Px<'g, 'p>, k: usize) -> Option<(i64, gravity_decompose::ops::shared::sort::SortKey, &'g gravity_decompose::ops::shared::agg::GroupDecl<'p>)> {
    let gx = px.gx;
    let g = gx.group_at(k)?;
    let [key] = g.keys.as_slice() else { return None };
    if !g.aggs.is_empty() || g.two_pass.is_some() || g.set.is_some() || key.kind != gravity_abi::breaker::KeyKind::Fp128 || key.expr.node().as_var().is_none() {
        return None;
    }
    let &[j] = &gx.drainers(g)[..] else { return None };
    if gx.end_need(j) != crate::face::kinds::KindId::RunsTopkGroups {
        return None;
    }
    let ops = &gx.pipelines()[j].ops;
    let at = ops.iter().position(|op| matches!(op, Op::Sort(_)))?;
    let Op::Sort(sd) = &ops[at] else { return None };
    if !ops[..at].iter().all(|op| matches!(op, Op::Project(_))) {
        return None;
    }
    let k0 = *sd.keys.as_slice().first()?;
    if cut_form(&k0) != Some(By::Code) {
        return None;
    }
    let e = crate::ops::sort::topn::order_slot::key_expr(&ops[..at], sd, k0.resno, px.mcx())?;
    if !e.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == key.resno) {
        return None;
    }
    let (SortForm::TopK { count, offset, ties } | SortForm::Walk { count, offset, ties, .. }) = &sd.form else { return None };
    let n = crate::frame::iface::Demand { count, offset, ties: *ties }.rows().and_then(|n| i64::try_from(n).ok()).filter(|n| (1..=NMAX).contains(n))?;
    Some((n, k0, g))
}

/// D3007: whether producer `k`'s group is under a top by its key descending (`group_top`): its entries site then walks
/// each part's codes from the highest (dict_entries.rs `entries_desc`), and the cut ends a window as an ascending one does.
pub(crate) fn group_desc(px: &Px<'_, '_>, k: usize) -> bool {
    crate::ops::sort::strategies::resident::on(px, crate::face::strategy::CODE_CUT) && group_top(px, k).is_some_and(|t| t.1.desc)
}

/// D3007: whether producer `k`'s keyed end takes code.cut's group form (`group_top`) over its drafted entries site.
pub(crate) fn group_cut(px: &mut Px<'_, '_>, k: usize) -> bool {
    let entries = px.site_mut(k).is_some_and(|s| s.entries == Some(0));
    entries && crate::ops::sort::strategies::resident::on(px, crate::face::strategy::CODE_CUT) && group_top(px, k).is_some()
}

/// D3007: producer `k`'s keyed end `inner` behind code.cut's group form (`group_cut` said yes): the per-part cut over
/// the group key's code, entries windows alone, the top's plan `n`.
pub(crate) fn group_end<'g>(px: &mut Px<'g, '_>, k: usize, inner: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
    let Some((n, k0, g)) = group_top(px, k) else { return inner };
    let key = g.keys.as_slice()[0];
    let node = px.gx.pipelines()[k].node_id;
    let Some(v) = key.expr.node().as_var() else { return inner };
    let typ = gravity_decompose::catalog::expr_type::type_of(key.expr.node()).unwrap_or(v.vartype);
    let Some(tf) = facts_of(px.gx.facts, typ) else { return inner };
    let mcx = px.mcx();
    let mut w = |j: u64, i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j, i }) as i32 * 8;
    let entry: PgVec<'_, i32> = (0..(HEAD + n) as u8).map(|i| w(WORDS_J, i)).collect_in(mcx);
    if entry.windows(2).any(|p| p[1] != p[0] + 8) {
        crate::refuse!(node, Reason::Bug { payload: "a code top-k prefilter's frame words are not one entry" });
    }
    let entries_desc = px.site_mut(k).is_some_and(|s| s.entries_desc);
    fires::fire("code.cut"); // shapecheck fire signal (REGISTRY RUNS-12)
    mcx::pg_box_dyn!(mcx, Pre { inner, by: By::Code, attno: key.attno, tf, coll: k0.coll, desc: k0.desc, node, w_n: 0, n_const: Some(n), entries_only: true, null_ahead: k0.nulls_first, w0: entry[0], cap: n, memo: None, seed: false, entries_desc, nulls_after: false, count: None } => dyn EndMech + 'g)
}

/// The straight form (D2) code.heads registers: it declines every top.
pub(crate) struct Straight;

impl CodeTopkMech for Straight {
    fn engages(&self, _gx: &Gx<'_, '_>, _top: &TopDesc<'_, '_>) -> bool {
        false
    }
}

pub(crate) const STRAIGHT: CodeTopkImpl = CodeTopkImpl(&Straight);

/// The prefilter over the top's `inner` end: a row passes to it unless its first key lies past its entry's cut.
struct Pre<'g> {
    inner: PgBox<'g, dyn EndMech + 'g>,
    by: By,
    attno: i16,
    tf: TypeFacts,
    coll: gravity_decompose::catalog::typefacts::KeyColl,
    desc: bool,
    node: i32,
    w_n: i32,
    /// D3007: a value-set group's cut takes the top's `n` from the plan (the top runs after the group's producer).
    n_const: Option<i64>,
    /// D3007: the cut is kept over entries windows alone (a group's value set: each code once per part); a rows window,
    /// where a code repeats, reaches the end unfiltered.
    entries_only: bool,
    /// D3007: the group's NULL sorts ahead of every value (NULLS FIRST, a descending key's default). Hosts `stage_entries`
    /// puts a part's NULL row (`entries_nulls`) after its entries, so a window the cut ended would never deliver that
    /// row, a top group: the cut still drops an entry past it, but the window runs on to its end.
    null_ahead: bool,
    w0: i32,
    /// The entry's codes (`capacity`).
    cap: i64,
    /// The collated form's memo slots' first word.
    memo: Option<i32>,
    /// NEW-45: a claim may seed (`seedable`).
    seed: bool,
    /// D2981: the site's entries windows hold each part's codes descending (`SiteDesc::entries_desc`), so a descending
    /// key's cut ends an entries window as an ascending one does.
    entries_desc: bool,
    /// B3-24: the site's entries windows end with their part's NULL row (`SiteDesc::entries_nulls`): no early end.
    nulls_after: bool,
    /// D2982: the `[owner, count, most]` words of a top whose run-time `n` may pass the entry (`cap == NMAX`): an
    /// entries window holds each code of its part once, so once `n` entries of a part reached the top, a row of it past
    /// the most of them has `n` rows of the part strictly before it; in a window in the key's order every later row lies
    /// past it too, and the window ends there whatever `n` is.
    count: Option<i32>,
}

impl Pre<'_> {
    /// The row's first key as `(key, tag, null)`: a code with its part's tag, or the exact order word (unsigned order)
    /// with the one tag, complemented for a descending first key; `None` when the key is neither (the row goes to the top
    /// unfiltered).
    fn key(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<(Value, Value, Option<Value>)> {
        let cr = row.col(self.attno)?;
        let mut cx = env.lx(b, self.node);
        let out = match (self.by, cr.cell.rep) {
            // no early return while the memo is lent: a `?` here (a pipeline with no site, a group's emit) dropped it with
            // the function's bounded loop headers, and the cancel verifier refused the program (D4445)
            (By::Code, Rep::Code { .. }) => match env.site {
                Some(s) => {
                    let code = crate::rep::code_word(&mut cx, cr);
                    let part = cx.b.load_zext(Type::I32, env.ctx, (site_off(s) + site::PART_ID) as i32);
                    let s32 = cx.b.iconst(Type::I64, 32);
                    let hi = cx.b.int_bin(IntBinOp::Shl, part, s32);
                    let one = cx.b.iconst(Type::I64, 1);
                    Some((self.dir(cx.b, code), cx.b.int_bin(IntBinOp::Or, hi, one), cr.null))
                }
                None => None,
            },
            (By::Coll, Rep::Code { .. }) => match (env.site, env.words, self.memo) {
                (Some(s), Some(words), Some(base)) => Some(self.coll_key(&mut cx, env.ctx, s, words, base, cr)),
                _ => None,
            },
            _ => None,
        };
        env.memo = cx.memo;
        out
    }

    /// The collated form's key of a code lane cell: the memoised sort-key prefix of `part << 32 | code` (a miss renders
    /// the code and asks the collation, `m6_coll_prefix` through key.coll's order arm, then fills the slot), as an
    /// unsigned ascending key, complemented for a descending first key; one entry for every part.
    fn coll_key(&self, cx: &mut crate::rep::Lx<'_, '_, '_>, ctx: Value, s: usize, words: Value, base: i32, cr: crate::rep::CellRef<'_>) -> (Value, Value, Option<Value>) {
        let code = crate::rep::code_word(cx, cr);
        let part = cx.b.load_zext(Type::I32, ctx, (site_off(s) + site::PART_ID) as i32);
        let s32 = cx.b.iconst(Type::I64, 32);
        let hi = cx.b.int_bin(IntBinOp::Shl, part, s32);
        let pc = cx.b.int_bin(IntBinOp::Or, hi, code);
        let one = cx.b.iconst(Type::I64, 1);
        let tag = cx.b.int_bin(IntBinOp::Add, pc, one);
        let mask = cx.b.iconst(Type::I64, MEMO as i64 - 1);
        let slot = cx.b.int_bin(IntBinOp::And, code, mask);
        let four = cx.b.iconst(Type::I64, 4);
        let off = cx.b.int_bin(IntBinOp::Shl, slot, four);
        let at = cx.b.ptr_add(words, off, 1, base);
        let held = cx.b.load(Type::I64, at, 0);
        let hit = cx.b.icmp(IntCond::Eq, held, tag);
        let (l_hit, l_miss, l_join) = (cx.b.create_block(&[]), cx.b.create_block(&[]), cx.b.create_block(&[Type::I64]));
        cx.b.cond_br(hit, (l_hit, &[]), (l_miss, &[]));
        cx.b.switch_to(l_hit);
        let w = cx.b.load(Type::I64, at, 8);
        cx.b.br(l_join, &[w]);
        cx.b.switch_to(l_miss);
        // a NULL row fills nothing (its lane element is no code of the part); its key is never read
        if let Some(nl) = cr.null {
            let (l_fill, zero) = (cx.b.create_block(&[]), cx.b.iconst(Type::I64, 0));
            cx.b.cond_br(nl, (l_join, &[zero]), (l_fill, &[]));
            cx.b.switch_to(l_fill);
        }
        // the miss's render and prefix do not dominate the join: the top's own reads of the cell start over
        let outer = cx.memo.set_aside();
        let img = crate::rep::order_words(cx, cr, &self.tf, self.coll);
        cx.memo.restore(outer);
        let w = match img.w[0] {
            Some(w) => w,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a collated code key without an order word" }),
        };
        cx.b.store(at, 0, tag);
        cx.b.store(at, 8, w);
        cx.b.br(l_join, &[w]);
        cx.b.switch_to(l_join);
        let w = cx.b.block_params(l_join)[0];
        // the order word compares signed (the top bit flipped); as an unsigned key it is the prefix itself
        let flip = cx.b.const_raw(Type::I64, 1 << 63);
        let key = cx.b.int_bin(IntBinOp::Xor, w, flip);
        (self.dir(cx.b, key), one, cr.null)
    }

    /// NEW-45: the claim's seed (kernels `topk_cut`) for a row whose part does not hold the entry (`hit` false): the
    /// part's `n`-th least code where the window's part facts say each code stands for a delivered row
    /// (`site::PART_ENTRIES_SAFE`), its code lane carries no delete vector and holds more codes than `n`; else, and on any
    /// other row, `CUT_NO_SEED`.
    fn seed(&self, b: &mut FunctionBuilder<'_>, env: &Env, row: &RowVals, hit: Value, n: Value) -> Value {
        let none = b.iconst(Type::I64, gravity_kernels::host_abi::CUT_NO_SEED as i64);
        let (true, Some(s), Some(At::Lane(id))) = (self.seed, env.site, row.col(self.attno).map(|c| c.cell.at)) else { return none };
        if id.is_path() {
            return none;
        }
        let l = id.attno;
        let (l_seed, l_go) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(hit, (l_go, &[none]), (l_seed, &[]));
        b.switch_to(l_seed);
        let facts = b.load_zext(Type::I8, env.ctx, (site_off(s) + site::PART_FACTS) as i32);
        let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
        let at = l as i32 * LANE_SIZE as i32;
        let fl = b.load_zext(Type::I8, lanes, at + lane::FLAGS as i32);
        let nc = b.load_zext(Type::I32, lanes, at + lane::NCODES_OR_SCALE as i32);
        let safe = flag(b, facts, site::PART_ENTRIES_SAFE, env.zero);
        let dead = flag(b, fl, lane::FLAG_DELETES, env.zero);
        let t = b.bconst(true);
        let live = b.int_bin(IntBinOp::Xor, dead, t);
        let more = b.icmp(IntCond::Ugt, nc, n);
        let ok = b.int_bin(IntBinOp::And, safe, live);
        let ok = b.int_bin(IntBinOp::And, ok, more);
        let cut = if self.desc {
            let d = b.int_bin(IntBinOp::Sub, nc, n);
            let all = b.iconst(Type::I64, -1);
            b.int_bin(IntBinOp::Xor, d, all)
        } else {
            let one = b.iconst(Type::I64, 1);
            b.int_bin(IntBinOp::Sub, n, one)
        };
        let seed = b.select(ok, cut, none);
        b.br(l_go, &[seed]);
        b.switch_to(l_go);
        b.block_params(l_go)[0]
    }

    /// D2982: the block a valued row past the entry's capacity enters. In an entries window (its codes in the key's
    /// order, each once) a part's first `n` rows counted set its cut, the most of their keys (a value, so a worker that
    /// meets the part's windows in any order stays right): a row past it ends the window (every later row of the window
    /// lies past too); a row before `n` are counted is counted, and it and any row at or before the cut go on to `l_top`.
    /// A rows window, or a NULL key, goes to `l_top` uncounted.
    #[allow(clippy::too_many_arguments)]
    fn count_entries(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, s: usize, win: crate::frame::frame::Win, row: &RowVals, x: &Exits, cw: i32, key: Value, tag: Value, n: Value, null: Option<Value>, l_top: gravity_ir::Block) -> gravity_ir::Block {
        let l_big = b.create_block(&[]);
        b.switch_to(l_big);
        let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
        let fl = b.load_zext(Type::I8, lanes, lane::FLAGS as i32);
        let mut ent = flag(b, fl, lane::FLAG_ENTRIES, env.zero);
        if let Some(nl) = null {
            let t = b.bconst(true);
            let valued = b.int_bin(IntBinOp::Xor, nl, t);
            ent = b.int_bin(IntBinOp::And, ent, valued);
        }
        let l_ent = b.create_block(&[]);
        b.cond_br(ent, (l_ent, &[]), (l_top, &[]));
        b.switch_to(l_ent);
        let words = env.words.unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "a code top-k prefilter without the words site" }));
        let at = b.ptr_add(words, env.zero, 1, cw);
        let owner = b.load(Type::I64, at, 0);
        let count = b.load(Type::I64, at, 8);
        let most = b.load(Type::I64, at, 16);
        let same = b.icmp(IntCond::Eq, owner, tag);
        let count = b.select(same, count, env.zero);
        let most = b.select(same, most, env.zero);
        let full = b.icmp(IntCond::Sge, count, n);
        let (l_full, l_end, l_count) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(full, (l_full, &[]), (l_count, &[]));
        b.switch_to(l_full);
        let past = b.icmp(IntCond::Ugt, key, most);
        b.cond_br(past, (l_end, &[]), (l_top, &[]));
        b.switch_to(l_end);
        self.window_end(b, env, &row.carry);
        b.br(x.l_finish, &[row.carry.out, win.end]);
        b.switch_to(l_count);
        let one = b.iconst(Type::I64, 1);
        let count = b.int_bin(IntBinOp::Add, count, one);
        let above = b.icmp(IntCond::Ugt, key, most);
        let most = b.select(above, key, most);
        b.store(at, 0, tag);
        b.store(at, 8, count);
        b.store(at, 16, most);
        b.br(l_top, &[]);
        l_big
    }

    /// D3027: a valued row strictly past the top's armed bound (`EndMech::past_bound`: this worker's top already holds
    /// `n` rows strictly before it) cannot enter the top. In an entries window, whose codes run in the key's order (the
    /// arm that calls this), every later row of the window lies past it too, so the window ends there, as at a part's
    /// cut; in a rows window the row alone is dropped. Without it every part let its own `n` rows through to the top's
    /// test: 511 parts x 1000 rows for C025's LIMIT 1000.
    #[allow(clippy::too_many_arguments)]
    fn bound_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, s: usize, win: crate::frame::frame::Win, row: &RowVals, x: &Exits, null: Option<Value>, l_join: gravity_ir::Block) {
        let Some(mut past) = self.inner.past_bound(b, env, row) else { return };
        if let Some(nl) = null {
            let t = b.bconst(true);
            let valued = b.int_bin(IntBinOp::Xor, nl, t);
            past = b.int_bin(IntBinOp::And, past, valued);
        }
        let (l_past, l_end, l_go) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_past, &[]), (l_go, &[]));
        b.switch_to(l_past);
        let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
        let fl = b.load_zext(Type::I8, lanes, lane::FLAGS as i32);
        let ent = flag(b, fl, lane::FLAG_ENTRIES, env.zero);
        b.cond_br(ent, (l_end, &[]), (l_join, &[row.carry.out]));
        b.switch_to(l_end);
        self.window_end(b, env, &row.carry);
        b.br(x.l_finish, &[row.carry.out, win.end]);
        b.switch_to(l_go);
    }

    /// An unsigned ascending key as the cut reads it: complemented under a descending first key.
    fn dir(&self, b: &mut FunctionBuilder<'_>, key: Value) -> Value {
        if !self.desc {
            return key;
        }
        let all = b.iconst(Type::I64, -1);
        b.int_bin(IntBinOp::Xor, key, all)
    }
}

impl EndMech for Pre<'_> {
    fn state_bytes(&self) -> u32 { self.inner.state_bytes() }
    fn open_words(&self, w: &mut OpenWords) {
        self.inner.open_words(w);
        // the owner 0 is no part's tag (every tag has bit 0 set): the first row of a part claims the entry
        w.declare(OpenWord::open("code top-k cut owner", Base::Words, self.w0 + 8, Type::I64, 0));
        if let Some(c) = self.count {
            w.declare(OpenWord::open("code top-k entries count owner", Base::Words, c, Type::I64, 0));
        }
        // the collated form's memo: every slot empty (tag 0) at each open
        for i in 0..self.memo.map_or(0, |_| MEMO as i32) {
            w.declare(OpenWord::open("code top-k collated memo tag", Base::Words, self.memo.unwrap_or(0) + 16 * i, Type::I64, 0));
        }
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) { self.inner.open(b, env) }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> { self.inner.carried(b, env) }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) { self.inner.window_end(b, env, c) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) { self.inner.finish(b, env, x) }
    fn sink(&self) -> crate::face::end::SinkKind { self.inner.sink() }
    fn wants_locator(&self) -> bool { self.inner.wants_locator() }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: gravity_program::Pipeline) -> gravity_program::Pipeline { self.inner.pipeline_desc(px, k, d) }

    /// The row's key against its entry: past the cut (the entry this part's, the key above its cut) the row is dropped;
    /// else it enters the entry (kernels `topk_cut`) and goes on to the top.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let Some((key, tag, null)) = self.key(b, env, row) else { return self.inner.lower(b, env, row, x) };
        let Some(words) = env.words else { crate::refuse!(self.node, Reason::Bug { payload: "a code top-k prefilter without the words site" }) };
        let entry = b.ptr_add(words, env.zero, 1, self.w0);
        let n = match self.n_const {
            Some(c) => b.iconst(Type::I64, c),
            None => env.load_word(b, self.w_n),
        };
        let nmax = b.iconst(Type::I64, self.cap);
        let mut on = b.icmp(IntCond::Sle, n, nmax);
        if self.entries_only {
            let Some(s) = env.site else { return self.inner.lower(b, env, row, x) };
            let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
            let fl = b.load_zext(Type::I8, lanes, lane::FLAGS as i32);
            let ent = flag(b, fl, lane::FLAG_ENTRIES, env.zero);
            on = b.int_bin(IntBinOp::And, on, ent);
        }
        if let Some(nl) = null {
            let t = b.bconst(true);
            let valued = b.int_bin(IntBinOp::Xor, nl, t);
            on = b.int_bin(IntBinOp::And, on, valued);
        }
        let owner = b.load(Type::I64, entry, gravity_kernels::host_abi::CUT_OFF_OWNER as i32);
        let cut = b.load(Type::I64, entry, gravity_kernels::host_abi::CUT_OFF_CUT as i32);
        let hit = b.icmp(IntCond::Eq, owner, tag);
        let past = b.icmp(IntCond::Ugt, key, cut);
        let drop = b.int_bin(IntBinOp::And, hit, past);
        let drop = b.int_bin(IntBinOp::And, drop, on);
        let (l_enter, l_top, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
        // D2982: the block a row past the entry's capacity goes to (the count path when the window may end early)
        let mut l_big = l_top;
        // D3027: the site and window of a key-ordered entries window (the arm below), where the top's bound ends it too
        let mut ordered = None;
        match (self.by, env.site, env.win) {
            // A part's dictionary codes are in byte order (pgrc format dict.rs, contractual) and an entries window
            // holds them ascending (hosts stage_entries: `lo..`, debug-asserted there), so the first row strictly past its
            // part's cut leaves every later row of the window past it too: the window ends there (the ladder's heads_run:
            // one branch per later window), its rows written back, instead of dropping them one by one. A row at the cut
            // stays. A descending key's cut keeps the largest codes: over ascending entries the window's later rows lie
            // before it, not past it; a site that walks its entries descending (`entries_desc`, D2981) puts them past.
            (By::Code, Some(s), Some(win)) if self.desc == self.entries_desc && !self.null_ahead && !self.nulls_after && env.carry_exits.is_none() && row.carry.accs.is_empty() => {
                let l_drop = b.create_block(&[]);
                b.cond_br(drop, (l_drop, &[]), (l_enter, &[]));
                b.switch_to(l_drop);
                let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
                let fl = b.load_zext(Type::I8, lanes, lane::FLAGS as i32);
                let m = { let bit = b.iconst(Type::I64, lane::FLAG_ENTRIES as i64); b.int_bin(IntBinOp::And, fl, bit) };
                let ent = b.icmp(IntCond::Ne, m, env.zero);
                let l_end = b.create_block(&[]);
                b.cond_br(ent, (l_end, &[]), (l_join, &[row.carry.out]));
                b.switch_to(l_end);
                self.window_end(b, env, &row.carry);
                b.br(x.l_finish, &[row.carry.out, win.end]);
                if let Some(cw) = self.count {
                    l_big = self.count_entries(b, env, s, win, row, x, cw, key, tag, n, null, l_top);
                }
                ordered = Some((s, win));
            }
            _ => b.cond_br(drop, (l_join, &[row.carry.out]), (l_enter, &[])),
        }
        b.switch_to(l_enter);
        if let Some((s, win)) = ordered {
            self.bound_end(b, env, s, win, row, x, null, l_join);
        }
        let l_cut = b.create_block(&[]);
        b.cond_br(on, (l_cut, &[]), (l_big, &[]));
        b.switch_to(l_cut);
        // B3-12: this part's entry while it rests (kernels `topk_cut`: a rest count in its length word from
        // `CUT_REST_SHIFT`) lets the row through untaken, one count off, as the kernel would, without the call
        let lw = b.load(Type::I64, entry, gravity_kernels::host_abi::CUT_OFF_LEN as i32);
        let one_rest = b.iconst(Type::I64, 1i64 << gravity_kernels::host_abi::CUT_REST_SHIFT);
        let rests = b.icmp(IntCond::Uge, lw, one_rest);
        let rests = b.int_bin(IntBinOp::And, rests, hit);
        let (l_rest, l_call) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(rests, (l_rest, &[]), (l_call, &[]));
        b.switch_to(l_rest);
        let lw1 = b.int_bin(IntBinOp::Sub, lw, one_rest);
        b.store(entry, gravity_kernels::host_abi::CUT_OFF_LEN as i32, lw1);
        b.br(l_top, &[]);
        b.switch_to(l_call);
        let seed = self.seed(b, env, row, hit, n);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("topk_cut", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::host_abi::topk_cut as *const () as usize, mem);
        b.call_host(h, &[entry, key, tag, n, seed], Some(Type::I64));
        b.br(l_top, &[]);
        b.switch_to(l_top);
        let c = self.inner.lower(b, env, row, x);
        if !c.accs.is_empty() {
            crate::refuse!(self.node, Reason::Bug { payload: "a code top-k prefilter over an end that carries accumulators" });
        }
        b.br(l_join, &[c.out]);
        b.switch_to(l_join);
        Carry::new(b.block_params(l_join)[0], env.mcx)
    }
}

#[cfg(test)]
mod tests {
    use crate::dict::face::{CodeRep, DictPolicy};
    use crate::face::strategy::{CODE_CUT, CODE_HEADS};
    use crate::face::runs::CodeTopkImpl;

    /// The two code roads are built and on (a statement they apply to no longer refuses `NotBuilt M6` by their name),
    /// each as a `CodeTopkImpl` its reader finds by type.
    #[test]
    fn the_code_roads_are_built_as_code_topk_rows() {
        for r in [CODE_CUT, CODE_HEADS] {
            let m = crate::ops::strategy(r).unwrap_or_else(|| panic!("{r:?} has no registry row"));
            assert!(m.downcast_ref::<CodeTopkImpl>().is_some(), "{r:?}'s row is not a CodeTopkImpl");
        }
        assert!(CodeRep::Cut.on() && DictPolicy::Heads.on());
    }

    /// G4: the code roads apply only to a pgrc Code key under a top (`Applies::CodeTopk`); a Code key under a full
    /// sort, or a top over a heap text key, is not theirs.
    #[test]
    fn the_code_roads_do_not_apply_without_a_code_top() {
        use crate::applies::{holds, Applies, Prop as P, PropSet};
        use crate::face::kinds::{KindId as K, KindSet};
        let top = KindSet::of(&[K::SrcPgrc, K::RunsTopkRows]);
        let sort = KindSet::of(&[K::SrcPgrc, K::RunsSort]);
        let (mut code, mut heap) = (PropSet::default(), PropSet::default());
        code.add(P::Code);
        code.add(P::FpSortkey);
        heap.add(P::FpSortkey);
        assert!(holds(Applies::CodeTopk, top, code));
        assert!(!holds(Applies::CodeTopk, sort, code));
        assert!(!holds(Applies::CodeTopk, KindSet::of(&[K::SrcHeap, K::RunsTopkRows]), heap));
    }

    /// RUNS-12 / B3-12: the entry holds the top's own `n` when the plan gives count and offset as constants, up to
    /// `TOPK_CUT_MAX_N` (a top of 65..251 rows is cut too); past it, or a zero, NULL, negative or non-constant bound, takes
    /// `NMAX`, the most the entry holds.
    #[test]
    fn the_entry_is_sized_from_the_tops_demand() {
        use super::{capacity, NMAX};
        use gravity_decompose::graph::Bound::{All, Const};
        use gravity_decompose::ops::shared::sort::SortForm;
        let max = i64::from(gravity_program::cost::TOPK_CUT_MAX_N);
        assert_eq!(NMAX, max);
        let top = |count, offset| capacity(&SortForm::TopK { count, offset, ties: false });
        assert_eq!(top(Const(10), All), 10);
        assert_eq!(top(Const(50), All), 50, "a top past the old 32 is cut");
        assert_eq!(top(Const(100), All), 100, "a top past the old 64 is cut");
        assert_eq!(top(Const(10), Const(40)), 50, "count + offset");
        assert_eq!(top(Const(max), All), max);
        assert_eq!(top(Const(max), Const(1)), NMAX, "past the bound: the run-time n then unfiltered");
        assert_eq!(top(Const(0), All), NMAX);
        assert_eq!(top(Const(5), Const(-1)), NMAX);
        assert_eq!(top(Const(i64::MAX), Const(1)), NMAX, "an overflowing n");
        assert_eq!(top(All, All), NMAX);
        assert_eq!(capacity(&SortForm::Walk { attno: 1, desc: false, nulls_first: false, count: Const(40), offset: All, ties: true }), 40);
        assert_eq!(capacity(&SortForm::Runs), NMAX);
    }
}
