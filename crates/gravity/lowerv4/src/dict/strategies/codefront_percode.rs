//! Per-code front: a derived key's rows counted per source code. Hooks / levers: H23 R4-L16. Budget
//! 200 lines (files-extra.tsv, +25 % gate). Owner M1-TABLES (written by M1-KEYED).
//!
//! A group keyed by one key derived from one code lane (code.front_dense's `derived` shape: e.g. `regexp_replace(col,
//! ..)`) whose every aggregate is a function of that source column's code folds a row as one count per code: `count(*)`,
//! `count(col)`, `sum` / `avg` of `octet_length` / `length` / `char_length(col)` (an int4 per code, raise-free: `n x f`
//! exact at the slot's width), and `min` / `max(col)` under a collation that orders by bytes (D247: a part's code rank is
//! its byte rank; any other declines, and today's path refuses `key.coll`). `col` must be the key's own source column.
//! B3-07: `count(*)` / `count(col)` under a FILTER that reads only `col` and calls no VOLATILE function counts `n x f`,
//! `f` the code's verdict (the predicate on the code's first row of the window, NULL false: 0 / 1), and the count walk
//! does not take it. KEYED-13: beside a length the verdict rides the per-code value's `PC_F_VERDICT` bit above the length
//! (`Fold::CountFv`), so one filtered count and one length share the code's one value. Any other FILTER, a second
//! filtered count, DISTINCT, an ordered set, a float slot, a marked pass or an admitted set declines.
//!
//! Per row: `counts[code] += 1` (a `u32` per code below 128 Ki); a code's first row in the window lists `[lo, hi, f << 32
//! | code]` (its key image and `f` read once per code per window). The list holds 4096 entries (96 KiB, the image riding
//! with the code); a full list folds at once, mid-window. At the window's end `agg_percode_window` folds each listed code's
//! rows into a map of one part's per-key records (8192): the counts, `n x f`, the key's representative and the extreme's
//! code. The map folds into the claim's T1 (`agg_front_flush`) when it fills, when a window of another part comes, and
//! at the claim's end, each extreme rendered once then through its part's dictionary into code_copy.rs's copy form.
//! A NULL key, a code at or past the capacity, a join multiplicity or a front the claim's arena refused (on the
//! statement's ledger) takes the per-row path into the same T1: one record format, which the owner merge cannot tell
//! apart. D2804: a serial claim whose sample of folds (a full fold, or smaller folds added up to as many rows) lists codes
//! and opens map records that cost the front at least what the per-row path pays for its rows (`cost::PERCODE_DECLINE`,
//! fitted) declines the front at run time (the host answers 1, the
//! capacity goes to 0): every later row takes the per-row path, and the map folds on at the claim's end. B (twopass.having): a group this front takes runs in one pass (keyed_hashagg `make`), its HAVING floor at the
//! emit. Hosts: agg_percode_open, agg_percode_window, agg_percode_flush (host_abi v4 m1).
use gravity_abi::breaker::{SlotKind, LAYOUT_IMPLICIT_COUNT};
use gravity_decompose::ops::shared::agg::{GroupForm, NONE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::dict::strategies::percode::{PC_CODES, PC_COUNTS, PC_F_VERDICT, PC_LIST, PC_TOUCH};
use gravity_kernels::dict::strategies::Fold;
use gravity_program::Rep;
use mcx::{CollectIn, Mcx, PgVec};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::table::bypass::{fail_on, host, state, HASHED};
use crate::table::emit_group::{key_shape, rep_src};
use crate::ops::agg::hashagg::{Agg, GroupRec};
use crate::dict::codefront::{FR_BASE, FR_CAP, FR_LIST, FR_N, FR_WORDS};
use crate::dict::face::CodeRep;
use crate::table::face::{TableDecl, TableMech};
use crate::table::keyimg::{FrontIndex, ImageWords};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;

/// The front's words after the hashed table's: codefront.rs's `FrontWords` (the base is the per-code front).
pub(crate) const WORDS: u32 = FR_WORDS;

/// A group the front takes: each slot's fold (`Fold`, 4 bits per slot), the source's dictionary lane, whether the image
/// ends in the flags word, the codes it counts (`PC_CODES`), the aggregate whose argument (`filtered` and no `fv`: whose
/// FILTER predicate, B3-07) is the per-code `f`, and (KEYED-13) the filtered count whose verdict rides beside a length.
#[derive(Clone, Copy)]
pub(crate) struct Percode {
    ops: u64,
    lane: u8,
    flags: bool,
    cap: u32,
    pub f: Option<usize>,
    filtered: bool,
    fv: Option<usize>,
    pub cw: Option<crate::dict::strategies::countwalk::Walk>,
}

/// The front's plan for group record `rec` at pipeline `k`, or `None` (see the module doc).
pub(crate) fn plan(px: &mut Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>) -> Option<Percode> {
    let (gx, g) = (px.gx, rec.g);
    let entries = px.site_mut(k).is_some_and(|s| s.entries.is_some());
    if !CodeRep::FrontDense.on() || entries || g.two_pass.is_some() || g.set.is_some() || !matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) || rec.layout.is_wide() {
        return None;
    }
    let (shape, _) = key_shape(gx, rec, false);
    let parted = (rec.layout.partition_keys() < rec.layout.nkeys()) as usize; // V4R-105: the part word ends the image (keyed_parted)
    if !shape.dense.is_some_and(|d| d.derived) || shape.code.is_some() || rec.layout.nkeys() != 2 + rec.nullable as usize + parted || g.aggs.len() != rec.aggs.len() {
        return None;
    }
    let src = rep_src(gx, g.producer.0 as usize, &g.keys[0], rec.node)?;
    let Rep::Code { dict } = src.rep else { return None };
    let is_src = |e: Node<'_>| e.as_relabel_type().map_or(e, |r| r.arg).as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varattno == src.col);
    let (mut ops, mut f, mut fid, mut filt) = (0u64, None, None, None);
    for j in 0..rec.layout.nslots() {
        let kind = rec.layout.slot_kind(j);
        let op = match rec.aggs.iter().position(|a| a.d.slot as usize == j) {
            _ if rec.reps[0].is_some() && j == g.keys[0].rep as usize => matches!(kind, SlotKind::MinCode).then_some(Fold::Rep)?,
            Some(i) => {
                let (op, id) = agg_op(&rec.aggs[i], kind, &is_src, px.mcx())?;
                if op == Fold::CountF {
                    // B3-07: one FILTER's verdict a code; a VOLATILE call differs per row
                    if filt.is_some() || rec.aggs[i].d.filter.is_none_or(|q| crate::effects::volatile(gx, q)) {
                        return None;
                    }
                    filt = Some((i, j));
                } else if id.is_some() {
                    // one length a code serves every slot: two spellings of one length (length / char_length) share it
                    if fid.is_some_and(|x| id.and_then(gravity_expr::percode::text_length) != gravity_expr::percode::text_length(x)) {
                        return None;
                    }
                    (fid, f) = (id, f.or(Some(i)));
                }
                op
            }
            None if rec.layout.flags & LAYOUT_IMPLICIT_COUNT != 0 && rec.layout.slot_off(j) == rec.layout.sentinel_off() => Fold::Count,
            None => return None,
        };
        ops |= op.at(j);
    }
    // KEYED-13: a filtered count beside a length reads the value's verdict bit; alone, the value is its verdict
    let (f, fv) = match (f, filt) {
        (Some(l), Some((i, j))) => {
            ops = ops & !(15 << (4 * j)) | Fold::CountFv.at(j);
            (Some(l), Some(i))
        }
        (None, Some((i, _))) => (Some(i), None),
        (l, None) => (l, None),
    };
    let filtered = filt.is_some();
    fires::fire("dict.front_percode"); // shapecheck fire signal (REGISTRY B3-07)
    if filtered {
        fires::fire("dict.front_percode.filter"); // shapecheck fire signal (REGISTRY B3-07)
    }
    if fv.is_some() {
        fires::fire("dict.front_percode.filter_len"); // shapecheck fire signal (REGISTRY KEYED-13)
    }
    let cw = if filtered { None } else { crate::dict::strategies::countwalk::plan(px, k, dict.lane, rec) };
    Some(Percode { ops, lane: dict.lane, flags: rec.nullable, cap: PC_CODES as u32, f, filtered, fv, cw })
}

/// Aggregate `a`'s fold into its slot of kind `kind` and the function id of its per-code `f`; `None` declines.
fn agg_op(a: &Agg<'_>, kind: SlotKind, is_src: &dyn Fn(Node<'_>) -> bool, mcx: Mcx<'_>) -> Option<(Fold, Option<u32>)> {
    let m = a.d.mods;
    if m.distinct != 0 || m.order != 0 || m.direct != 0 || a.d.count_slot != NONE || a.d.dscale_slot != NONE {
        return None;
    }
    // D4565: a var / stddev over int2 / int4 folds Sx and N into its AvgI128 slot and Sx2 into the SumI128 companion
    // (decompose `stat_slot`): the per-code fold folds an average's two words only
    if matches!(a.d.class, gravity_decompose::ops::shared::agg_class::Class::Stat3I128(_)) {
        return None;
    }
    let r = a.d.node.as_aggref()?;
    let args: PgVec<'_, Node<'_>> = r.args.iter().map(|te| te.as_target_entry().map_or(te, |t| t.expr)).collect_in(mcx);
    if let Some(q) = a.d.filter {
        // B3-07: a count under a FILTER of the source column alone (the plan checks for VOLATILE calls)
        let count = matches!((kind, &args[..]), (SlotKind::Count, [])) || matches!((kind, &args[..]), (SlotKind::Count, [e]) if is_src(*e));
        return (count && reads_only(q, is_src)).then_some((Fold::CountF, None));
    }
    match (kind, &args[..]) {
        (SlotKind::Count, []) => Some((Fold::Count, None)),
        (SlotKind::Count, [e]) if is_src(*e) => Some((Fold::Count, None)),
        (SlotKind::SumI64 | SlotKind::SumI128 | SlotKind::AvgI128, [e]) => {
            let f = e.as_func_expr().filter(|f| gravity_expr::percode::text_length(f.funcid).is_some() && f.args.len() == 1 && is_src(f.args.nth(0)))?;
            let op = match kind {
                SlotKind::SumI64 => Fold::Sum64,
                SlotKind::SumI128 => Fold::Sum128,
                _ => Fold::Avg128,
            };
            Some((op, Some(f.funcid)))
        }
        // V4R-105: a deferred minimum (keyed_parted `CodeSlot`) keeps its code word, as the key's representative does
        (SlotKind::MinVar | SlotKind::MaxVar | SlotKind::MinCode, [e]) if is_src(*e) && crate::props::class_kind(&a.d) == KindId::ClassMinmaxCode && a.d.coll.bytes_order => {
            Some((match kind { SlotKind::MinVar => Fold::Min, SlotKind::MaxVar => Fold::Max, _ if crate::ops::sort::topn::strategies::parted::deferred(a.class) => Fold::Rep, _ => return None }, None))
        }
        _ => None,
    }
}

/// B3-07: `n` reads no column but the source (`is_src`), no parameter, sublink or other row-varying input: a value per code.
fn reads_only(n: Node<'_>, is_src: &dyn Fn(Node<'_>) -> bool) -> bool {
    crate::stack_check();
    use types_nodes::NodeTag as T;
    match n.node_tag() {
        T::T_Var => is_src(n),
        T::T_Const | T::T_CaseTestExpr => true,
        T::T_OpExpr | T::T_FuncExpr | T::T_BoolExpr | T::T_ScalarArrayOpExpr | T::T_NullTest | T::T_BooleanTest | T::T_CaseExpr | T::T_CaseWhen | T::T_CoalesceExpr | T::T_NullIfExpr | T::T_DistinctExpr | T::T_RelabelType | T::T_CoerceViaIO | T::T_MinMaxExpr => {
            let mut ok = true;
            let known = gravity_decompose::expr::kids(n, &mut |k| ok &= reads_only(k, is_src));
            ok && known
        }
        _ => false,
    }
}

/// The state's address and the front words' offset in it (after the hashed table's).
fn words(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> (Value, i32) {
    (state(b, env, t), HASHED.state_bytes(t) as i32)
}

impl Percode {
    /// At the claim's open, after the hashed table's: the front from the claim's arena, its list empty; a refused front
    /// has capacity 0, so every row takes the per-row path.
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        let (st, fr) = words(b, env, t);
        let f = host(b, "agg_percode_open", &[Type::Ptr, Type::I64], gravity_abi::host_words!(gravity_kernels::dict::strategies::percode::agg_percode_open(ctx, rec)));
        let size = b.iconst(Type::I64, t.layout.size() as i64);
        let p = b.call_host(f, &[env.ctx, size], Some(Type::I64)).expect("agg_percode_open returns an address or 0");
        let refused = b.icmp(IntCond::Eq, p, env.zero);
        let full = b.iconst(Type::I64, self.cap as i64);
        let cap = b.select(refused, env.zero, full);
        let base = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
        let list = b.ptr_add(base, env.zero, 1, PC_LIST as i32);
        b.store(st, fr + FR_BASE, base);
        b.store(st, fr + FR_LIST, list);
        b.store(st, fr + FR_N, env.zero);
        b.store(st, fr + FR_CAP, cap);
    }

    /// The row's per-code `f` (read on a code's first row of the window): aggregate `f`'s argument, or its FILTER's verdict
    /// (B3-07; true 1, false or NULL 0). `None`: no aggregate reads a per-code value.
    pub(crate) fn f_value(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, row: &RowVals) -> Option<Value> {
        let a = &rec.aggs[self.f?];
        let q = match (self.filtered && self.fv.is_none(), a.filter) {
            (false, _) => None,
            (true, Some(q)) => Some(q),
            (true, None) => crate::refuse!(env.node, Reason::Bug { payload: "a per-code FILTER verdict whose FILTER is not compiled" }),
        };
        let fq = match self.fv.map(|i| rec.aggs[i].filter) {
            None => None,
            Some(Some(q)) => Some(q),
            Some(None) => crate::refuse!(env.node, Reason::Bug { payload: "a per-code FILTER verdict whose FILTER is not compiled" }),
        };
        Some(env.aside(|env| {
            let v = match q {
                Some(q) => crate::step::expr::pred(b, env, rec.node, row, q),
                None => rec.args(b, env, row, a)[0].val,
            };
            let Some(fq) = fq else { return v };
            // KEYED-13: the length's bits, the FILTER's verdict at PC_F_VERDICT
            let p = crate::step::expr::pred(b, env, rec.node, row, fq);
            // D2946: the verdict may be a Bool, which iconst's int-or-Ptr contract refuses (a debug build's assert); its
            // zero of the verdict's own type is the same constant
            let z = b.const_raw(b.value_type(p), 0);
            let set = b.icmp(IntCond::Ne, p, z);
            let bit = b.conv(ConvOp::Zext, Type::I64, set);
            let v = if b.value_type(v) == Type::I64 { v } else { b.conv(ConvOp::Sext, Type::I64, v) };
            let s = b.iconst(Type::I64, i64::from(PC_F_VERDICT));
            let hi = b.int_bin(IntBinOp::Shl, bit, s);
            b.int_bin(IntBinOp::Or, v, hi)
        }))
    }

    /// The row of image `img` (its source code `img.index`): counted, and listed with `f` (the argument's per-code value,
    /// evaluated on the code's first row of the window) when fresh, the list folded when full; then `l_done`, which is
    /// returned. The builder is left in the per-row path's block (a NULL key, a code past the capacity), which the caller
    /// fills and ends at `l_done`.
    pub(crate) fn row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, f: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env) -> Option<Value>) -> Block {
        let Some(&FrontIndex::Source(code)) = img.index.as_ref() else { crate::refuse!(env.node, Reason::Bug { payload: "a per-code front row without its source code" }) };
        let (st, fr) = words(b, env, t);
        let (l_code, l_touch, l_full, l_row, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let cap = b.load(Type::I64, st, fr + FR_CAP);
        let mut ok = b.icmp(IntCond::Ult, code, cap);
        if self.flags {
            let set = b.icmp(IntCond::Eq, img.words.words[2], env.zero);
            ok = b.int_bin(IntBinOp::And, ok, set);
        }
        b.cond_br(ok, (l_code, &[]), (l_row, &[]));
        b.switch_to(l_code);
        let front = b.load(Type::Ptr, st, fr + FR_BASE);
        let at = b.ptr_add(front, code, 4, PC_COUNTS as i32);
        let c = b.load_zext(Type::I32, at, 0);
        let c1 = b.int_bin(IntBinOp::Add, c, env.one);
        let c1 = b.conv(ConvOp::Trunc, Type::I32, c1);
        b.store(at, 0, c1);
        let fresh = b.icmp(IntCond::Eq, c, env.zero);
        b.cond_br(fresh, (l_touch, &[]), (l_done, &[]));
        b.switch_to(l_touch);
        let (list, n) = (b.load(Type::Ptr, st, fr + FR_LIST), b.load(Type::I64, st, fr + FR_N));
        let e = b.ptr_add(list, n, 24, 0);
        b.store(e, 0, img.words.words[0]);
        b.store(e, 8, img.words.words[1]);
        let w = match f(b, env) {
            Some(v) => {
                let v = if b.value_type(v) == Type::I64 { v } else { b.conv(ConvOp::Sext, Type::I64, v) };
                let s32 = b.iconst(Type::I64, 32);
                let hi = b.int_bin(IntBinOp::Shl, v, s32);
                b.int_bin(IntBinOp::Or, hi, code)
            }
            None => code,
        };
        b.store(e, 16, w);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        b.store(st, fr + FR_N, n1);
        let lim = b.iconst(Type::I64, PC_TOUCH as i64);
        let full = b.icmp(IntCond::Eq, n1, lim);
        b.cond_br(full, (l_full, &[]), (l_done, &[]));
        b.switch_to(l_full);
        let x = env.x;
        self.fold(b, env, t, &x);
        b.br(l_done, &[]);
        b.switch_to(l_row);
        l_done
    }

    /// The listed codes folded into the map (`agg_percode_window`, while the window's part is bound), the list emptied.
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        let (st, fr) = words(b, env, t);
        let n = b.load(Type::I64, st, fr + FR_N);
        let (l_go, l_out) = (b.create_block(&[]), b.create_block(&[]));
        let any = b.icmp(IntCond::Ne, n, env.zero);
        b.cond_br(any, (l_go, &[]), (l_out, &[]));
        b.switch_to(l_go);
        let front = b.load(Type::Ptr, st, fr + FR_BASE);
        let (ops, at) = self.words(b, env);
        // D2804: the run-time decline's thresholds, the cost home's
        let decline = b.const_raw(Type::I64, gravity_program::cost::PERCODE_DECLINE);
        let h = host(b, "agg_percode_window", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], gravity_abi::host_words!(gravity_kernels::dict::strategies::percode::agg_percode_window(ctx, st, front, n, ops, at, decline)));
        let r = b.call_host(h, &[env.ctx, st, front, n, ops, at, decline], Some(Type::I64)).expect("agg_percode_window returns a status");
        fail_on(b, env, x, r);
        // D2804: 1, the front declines for the rest of the claim (its rows opened a record each or nearly): capacity 0
        let declined = b.icmp(IntCond::Eq, r, env.one);
        let cap = b.load(Type::I64, st, fr + FR_CAP);
        let cap = b.select(declined, env.zero, cap);
        b.store(st, fr + FR_CAP, cap);
        b.store(st, fr + FR_N, env.zero);
        b.br(l_out, &[]);
        b.switch_to(l_out);
    }

    /// The host's `ops` and `at` (`site << 8 | lane`) words.
    pub(crate) fn words(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
        let Some(site) = env.site else { crate::refuse!(env.node, Reason::Bug { payload: "a per-code front outside a site" }) };
        (b.const_raw(Type::I64, self.ops), b.iconst(Type::I64, (site << 8 | self.lane as usize) as i64))
    }

    /// At a window's end: the window's codes folded into the map.
    pub(crate) fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        let x = env.x;
        self.fold(b, env, t, &x);
    }

    /// At the claim's end, before the hashed table's: the map folded into T1.
    pub(crate) fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        self.fold(b, env, t, x);
        let (st, fr) = words(b, env, t);
        let front = b.load(Type::Ptr, st, fr + FR_BASE);
        let (ops, at) = self.words(b, env);
        let h = host(b, "agg_percode_flush", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(gravity_kernels::dict::strategies::percode::agg_percode_flush(ctx, st, front, ops, at)));
        let r = b.call_host(h, &[env.ctx, st, front, ops, at], Some(Type::I64)).expect("agg_percode_flush returns a status");
        fail_on(b, env, x, r);
    }
}

#[cfg(test)]
impl Percode {
    /// The testkit's front: slot folds `ops` over dictionary lane 0, `cap` codes.
    pub(crate) fn of(ops: u64, flags: bool, cap: u32) -> Percode {
        Percode { ops, lane: 0, flags, cap, f: None, filtered: false, fv: None, cw: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::breaker::SlotKind as S;
    use gravity_decompose::ops::shared::agg::fold_of;
    use gravity_decompose::ops::shared::agg::AggDecl;
    use gravity_decompose::ops::shared::sort::NO_SCALE;
    use gravity_decompose::ops::shared::agg_class::{Class, Ext, Mods, Split};
    use gravity_decompose::catalog::typefacts::KeyColl;
    use mcx::{Mcx, MemoryContext};
    use types_nodes::primnodes::{Aggref, FuncExpr};
    use types_nodes::NodeList;

    use crate::ops::agg::testkit::{fold_front, Case};

    /// Each key's `(rows, sum of f(k) = k % 11 + 1)` of the testkit's derived form (`k / 3`, NULL every `null`-th row).
    fn want(n: i64, key: fn(i64) -> i64, null: i64) -> std::collections::BTreeMap<i64, (i64, i64)> {
        let mut w = std::collections::BTreeMap::new();
        for i in 0..n {
            let k = key(i);
            let e = w.entry(if null != 0 && i % null == 0 { -1 } else { k / 3 }).or_insert((0, 0));
            *e = (e.0 + 1, e.1 + k % 11 + 1);
        }
        w
    }

    /// The front's counts and `n x f` folded per code, per window and per part equal the per-row path's rows and sums:
    /// NULL keys and codes at or past a small capacity take the per-row path into the same records as the front's (a
    /// key's three source codes straddle the capacity); parts turn every two windows mid-claim (the map flushed into T1
    /// between them); 30,000 codes a window, each on three rows, fill the touched list (4096) mid-window and 10,000 keys a
    /// part fill the map (8192). D2804: 30,000 codes a window, each on one row, decline the front at the first full list
    /// (the claim is serial), the map's records and the later rows' folding into the same records. A claim whose arena
    /// refuses the front's region folds every row per row, to the same records.
    #[test]
    fn the_front_folds_what_the_per_row_path_folds() {
        let small = |i: i64| i % 300;
        let c = |null| Case { n: 6000, win: 500, key: small, table: &HASHED, run: None, runs: None, code: true, null, dense: Some(true) };
        assert_eq!(fold_front(c(7), 200, 2, 4 << 20), (want(6000, small, 7), true), "NULL and past-cap codes beside the front's");
        assert_eq!(fold_front(c(0), 200, 2, 4 << 20), (want(6000, small, 0), true), "past-cap codes, no NULL");
        assert_eq!(fold_front(c(7), 200, 2, 1 << 16), (want(6000, small, 7), false), "a refused front: every row per row");
        let thrice = |i: i64| (i / 3) % 30_000;
        let c = Case { n: 180_000, win: 90_000, key: thrice, table: &HASHED, run: None, runs: None, code: true, null: 0, dense: Some(true) };
        assert_eq!(fold_front(c, PC_CODES as u32, 1, 4 << 20), (want(180_000, thrice, 0), true), "a full list mid-window, a full map");
        let big = |i: i64| i % 30_000;
        let c = Case { n: 60_000, win: 30_000, key: big, table: &HASHED, run: None, runs: None, code: true, null: 0, dense: Some(true) };
        assert_eq!(fold_front(c, PC_CODES as u32, 1, 4 << 20), (want(60_000, big, 0), true), "fresh codes decline the front mid-claim");
        let c = Case { n: 60_000, win: 1000, key: big, table: &HASHED, run: None, runs: None, code: true, null: 0, dense: Some(true) };
        assert_eq!(fold_front(c, PC_CODES as u32, 1, 4 << 20), (want(60_000, big, 0), true), "windows under a full list add up to a sample, then decline");
    }

    /// `funcid(a)` returning int4.
    fn call<'m>(m: Mcx<'m>, funcid: u32, a: Node<'m>) -> Node<'m> {
        Node::mk(m, FuncExpr { funcid, funcresulttype: 23, args: NodeList::make1(m, a).unwrap(), ..Default::default() }).unwrap()
    }

    /// `agg_op` of an aggregate of `class` (`fnoid`) over `arg` into a slot of `kind`, under `coll`, with FILTER `filter`;
    /// the source column is attno 1.
    #[allow(clippy::too_many_arguments)]
    fn op<'m>(m: Mcx<'m>, class: Class, fnoid: u32, kind: S, arg: Option<Node<'m>>, coll: KeyColl, filter: Option<Node<'m>>) -> Option<(Fold, Option<u32>)> {
        let (reg, trans, final_) = fold_of(class, fnoid, true, true);
        let args = match arg {
            Some(a) => NodeList::make1(m, a).unwrap(),
            None => NodeList::default(),
        };
        let node = Node::mk(m, Aggref { aggfnoid: fnoid, args, ..Default::default() }).unwrap();
        let mods = Mods { distinct: 0, order: 0, filter: filter.is_some(), direct: 0, split: Split::Simple, star: arg.is_none() };
        let d = AggDecl { aggno: 0, fnoid, reg, slot: 0, count_slot: NONE, final_, node, scale: NO_SCALE, filter, class, trans, mods, coll, dscale_slot: NONE, dscales: &[] };
        let c = crate::ops::agg::trans::plain::class_of(&0i32, &d, m);
        let a = Agg { d, class: c, shape: c.shape(&d), args: Vec::new(), filter: None };
        agg_op(&a, kind, &|e: Node<'_>| e.as_var().is_some_and(|v| v.varattno == 1), m)
    }

    /// `agg_op`: count(*), count(src), sum / avg of a length of src and min / max(src) under a bytes collation fold per
    /// code, and a count under a FILTER of src alone (B3-07); another column, another FILTER, a non-length function and a
    /// min / max under a collation that does not order by bytes (en-x-icu) decline (today's path refuses key.coll there).
    #[test]
    fn only_functions_of_the_source_code_fold_per_code() {
        let cx = MemoryContext::new("percode");
        let m = cx.mcx();
        let (src, other) = (Node::mk_var(m, OUTER_VAR, 1, 25, -1, 100, 0).unwrap(), Node::mk_var(m, OUTER_VAR, 2, 25, -1, 100, 0).unwrap());
        let (by, icu) = (KeyColl { oid: 950, bytes_eq: true, bytes_order: true }, KeyColl { oid: 12345, bytes_eq: true, bytes_order: false });
        assert_eq!(op(m, Class::CountStar, 2803, S::Count, None, by, None), Some((Fold::Count, None)));
        assert_eq!(op(m, Class::CountAny, 2147, S::Count, Some(src), by, None), Some((Fold::Count, None)));
        assert_eq!(op(m, Class::CountAny, 2147, S::Count, Some(other), by, None), None, "count of another column");
        assert_eq!(op(m, Class::SumI64, 2108, S::SumI64, Some(call(m, 1374, src)), by, None), Some((Fold::Sum64, Some(1374))));
        assert_eq!(op(m, Class::AvgI128, 2101, S::AvgI128, Some(call(m, 1317, src)), by, None), Some((Fold::Avg128, Some(1317))));
        assert_eq!(op(m, Class::SumI64, 2108, S::SumI64, Some(call(m, 1374, other)), by, None), None, "a length of another column");
        assert_eq!(op(m, Class::SumI64, 2108, S::SumI64, Some(call(m, 1375, src)), by, None), None, "not a length");
        assert_eq!(op(m, Class::SumI64, 2108, S::SumI64, Some(call(m, 1374, src)), by, Some(call(m, 1374, src))), None, "a sum under a FILTER");
        // B3-07: a count under a FILTER of the source column alone counts the code's verdict
        let (p, q) = (call(m, 1374, src), call(m, 1374, other));
        assert_eq!(op(m, Class::CountStar, 2803, S::Count, None, by, Some(p)), Some((Fold::CountF, None)), "count(*) FILTER over src");
        assert_eq!(op(m, Class::CountAny, 2147, S::Count, Some(src), by, Some(p)), Some((Fold::CountF, None)), "count(src) FILTER over src");
        assert_eq!(op(m, Class::CountStar, 2803, S::Count, None, by, Some(q)), None, "a FILTER over another column");
        assert_eq!(op(m, Class::CountAny, 2147, S::Count, Some(other), by, Some(p)), None, "count of another column under a FILTER");
        assert_eq!(op(m, Class::Min(Ext::Text), 2145, S::MinVar, Some(src), by, None), Some((Fold::Min, None)));
        assert_eq!(op(m, Class::Max(Ext::Text), 2129, S::MaxVar, Some(src), by, None), Some((Fold::Max, None)));
        assert_eq!(op(m, Class::Min(Ext::Text), 2145, S::MinVar, Some(src), icu, None), None, "a collation not ordering by bytes");
        assert_eq!(op(m, Class::Max(Ext::Text), 2129, S::MaxVar, Some(other), by, None), None, "a max of another column");
    }
}
