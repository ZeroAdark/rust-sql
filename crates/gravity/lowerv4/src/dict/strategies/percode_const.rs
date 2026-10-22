//! The constant-key window (decision 14 (B), V4R-19). A keyed group of one by-value key whose
//! window the source proves constant (`WinConds::const_key`: the exact zone stats' min == max, no NULL, the key's datum
//! word; (A), M1-SOURCE) takes the window whole (`EndMech::whole`), before its rows and its key lane's decode: the host
//! (`agg_percode_const`) counts the window's rows and sums one dictionary column's per-code length over them in one pass
//! over its code lane, read from the length table the window's bind returned (the dictionary index, R4); the end folds
//! that answer (`answer.stats`) into the key's one record (`RunFold::close`). Folds `count(*)`, `count(col)`, `sum` /
//! `avg` / (D3154) `min` / `max` of one of `octet_length` / `length` / `char_length(col)` (or of one BG-03 word
//! function of it) and (KEYED-15) `min` / `max(col)` of a code register
//! under a collation that orders by bytes (the window's least / greatest kept code: its part's dictionary is byte-rank
//! sorted, so the carry class.minmax_code's rows leave), over one column; the pipeline's ops before the end
//! are projections and leading filters (pgrc) whose quals are each one code verdict of that column (countwalk.rs
//! `code_verdict`, mechanism D; B3-05, BG-03): any number of per-code verdict bits (a comparison with any constant, `''`
//! too, an IN list, a LIKE / ILIKE / regex / boolean body of it alone: percode.rs's `Bits` root), ANDed per code (fire
//! dict.percode_const.verdict).
//! Anything else takes the rows, and so does a window the host declines at run time: a column that may hold a NULL, a
//! verdict with a faulted entry, and a condition-cache MISS unless the source covers every filter op (`PK_COVERED`). A
//! miss window's survivor words are published by the granule host when the program asks for the next window; the rows
//! set them (the frame's `passed`), or, when the verdicts are every covered qual, the host's pass does (CACHE-MISS lever
//! a): consumed whole with neither they would publish all-zero, a wrong NoMatch for the next statement's scan of the
//! granule. Budget 160 lines (files-extra.tsv, +25 % gate). Owner M1-STEP.
use gravity_abi::breaker::KeyKind;
use gravity_abi::ctx::{answer, site_off};
use gravity_abi::site::site as site_word;
use gravity_decompose::ops::shared::agg::{GroupForm, Register as R, NONE};
use gravity_decompose::catalog::Am;
use gravity_decompose::{Op, Source};
use gravity_expr::emit::hash::{emit_key_hash, ImageValues};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntCond, Type};
use gravity_kernels::dict::strategies::PK_VERDICT;
use gravity_kernels::dict::strategies::percode_const::{PK_CODES, PK_COVERED, PK_LEN, PK_LENMM, PK_MINMAX, PK_SIGNED, PK_SIGNED_SHIFT};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::table::bypass::host;
use crate::ops::agg::hashagg::{Arg, GroupRec};
use crate::face::fold::{Acc, AnswerForm, AnswerImpl};
use crate::face::kinds::KindId;
use crate::table::face::RunFold;
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::px::{ExId, Px};
use crate::frame::row::Carry;

/// What an aggregate of the window folds: the kept rows, a length's sum (answered), the least / greatest kept code, or
/// (D3154) the least / greatest kept length (answered: `PK_LENMM`'s `answer.min` / `answer.max`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    Count,
    Len,
    Min,
    Max,
    LenExt,
}

impl Fold {
    /// The aggregate's register words (its class's `AccShape`).
    const fn words(self) -> u8 {
        match self {
            Fold::Count => 1,
            Fold::Len | Fold::LenExt => 2,
            Fold::Min | Fold::Max => 3,
        }
    }
}

/// A group this fold takes: per aggregate what it folds, the length argument's compiled expression, the column (attno,
/// site lane), the host's flags, whether the image ends in the flags word, whether the end counts rows, and the site table
/// of the pipeline's per-code verdict.
pub(crate) struct Plan<'g> {
    folds: PgVec<'g, Fold>,
    len: Option<ExId>,
    col: (i16, u8),
    flags: u64,
    nullable: bool,
    count: bool,
    verdicts: crate::dict::strategies::countwalk::Verdicts,
}

/// Group record `rec` at pipeline `k` (its arguments compiled), its end counting rows when `count`; `None` declines.
pub(crate) fn plan<'g>(px: &mut Px<'g, '_>, k: usize, rec: &GroupRec<'_, '_>, count: bool) -> Option<Plan<'g>> {
    let (gx, g) = (px.gx, rec.g);
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, .. } = &p.source else { return None };
    let key = g.keys.first().filter(|x| g.keys.len() == 1 && x.words == 1 && matches!(x.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp))?;
    if g.two_pass.is_some() || g.set.is_some() || !matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) || rec.layout.is_wide() || rec.reps.iter().any(Option::is_some) || g.aggs.len() != rec.aggs.len() || key.expr.node().as_var().is_none() {
        return None;
    }
    // a scan column, through the projection when the node reads the producer's row
    let col = |n: Node<'_>| {
        let v = crate::table::emit_group::bare(n).as_var()?;
        let v = if v.varno == OUTER_VAR { entry(p, v.varattno)? } else { v };
        (v.varno == *scanrelid as i32 && v.varlevelsup == 0 && v.varattno > 0).then_some(v.varattno)
    };
    let (mut attno, mut folds, mut len, mut fid) = (None, PgVec::new_in(px.mcx()), None, None);
    let mut signed: Option<(u16, u8)> = None;
    let mut same = |a: Option<i16>| a.is_some_and(|a| attno.replace(a).is_none_or(|b| b == a));
    for a in &rec.aggs {
        let (m, r) = (a.d.mods, a.d.node.as_aggref()?);
        let f = match a.d.reg {
            R::CountStar | R::CountAny => Fold::Count,
            R::MinCode => Fold::Min,
            R::MaxCode => Fold::Max,
            R::MinInt | R::MaxInt => Fold::LenExt,
            _ => Fold::Len,
        };
        if a.d.filter.is_some() || m.distinct != 0 || m.order != 0 || m.direct != 0 || a.d.count_slot != NONE || a.d.dscale_slot != NONE || a.shape.words != f.words() {
            return None;
        }
        let args: PgVec<'_, Node<'_>> = r.args.iter().map(|te| te.as_target_entry().map_or(te, |t| t.expr)).collect_in(px.mcx());
        match (a.d.reg, &args[..], a.args.first()) {
            // count(*), and count(x) of a column a valid NOT NULL constraint guards: normalize's count_star gives it the
            // count(*) register (it counts every row), its spelling keeps the argument
            (R::CountStar, _, _) => {}
            (R::CountAny, [e], _) if same(col(*e)) => {}
            // a code extremum of the column's lane (class.minmax_code: its carry `[part << 32 | code, site << 8 | lane, n]`),
            // codes ranked as bytes under the aggregate's collation
            (R::MinCode | R::MaxCode, [e], Some(Arg::Col(_))) if a.d.coll.bytes_order && crate::props::class_kind(&a.d) == KindId::ClassMinmaxCode && same(col(*e)) => {}
            // D3154: min / max of the same table (the kept rows' least / greatest entry, idempotent: no count needed)
            (R::SumInt | R::AvgInt | R::MinInt | R::MaxInt, [e], Some(Arg::Ex { id, .. })) if a.class.answer(&a.d) == Some(AnswerForm::Stats) => {
                match e.as_func_expr().filter(|f| gravity_expr::percode::text_length(f.funcid).is_some() && f.args.len() == 1) {
                    // a length of the column: the dictionary index's length table (R4)
                    Some(f) => {
                        if !same(col(f.args.nth(0))) || signed.is_some() || fid.replace(f.funcid).is_some_and(|x| gravity_expr::percode::text_length(x) != gravity_expr::percode::text_length(f.funcid)) {
                            return None;
                        }
                    }
                    // BG-03 / B3-05: any integer function of the column alone, one per-code word table (summed signed at its
                    // width by the host), the same table for every such aggregate
                    None => {
                        let (c, t, w) = word_table(px, *id)?;
                        if !same(Some(c)) || fid.is_some() || signed.replace((t, w)).is_some_and(|x| x != (t, w)) {
                            return None;
                        }
                    }
                }
                len = Some(*id);
            }
            _ => return None,
        }
        folds.push_in(f);
    }
    let mut verdicts = crate::dict::strategies::countwalk::Verdicts::default();
    for (j, op) in p.ops.iter().enumerate() {
        match op {
            Op::Project(_) => {}
            Op::Filter(q) => {
                for i in 0..q.as_list().map_or(1, |l| l.len()) {
                    // B3-05 / BG-03: each qual one code verdict of the column (countwalk `code_verdict`: a per-code verdict
                    // bit), any number ANDed per code; a pgrc scan's leading filter, its quals
                    // compiled over the drafted site (pgrc/lower.rs): nothing compiles here
                    if !matches!(p.source, Source::Range { am: Am::Pgrcolumnar, .. }) || !p.ops.iter().take(j).all(|op| matches!(op, Op::Filter(_))) {
                        return None;
                    }
                    let q = *crate::step::expr::quals(px, k, j).get(i)?;
                    // the column an aggregate or the verdict reads (so the host decoded its lane)
                    let a = px.placed(q).ext.as_deref()?.downcast_ref::<crate::dict::percode::Plan>()?.cols.first()?.0;
                    let v = crate::dict::strategies::countwalk::code_verdict(px, q, a).filter(|_| same(Some(a)))?;
                    if !verdicts.add(v) {
                        return None;
                    }
                }
            }
            _ => return None,
        }
    }
    let col = match attno {
        Some(a) => (a, px.site_mut(k)?.lane_index(gravity_program::LaneId::Attr(a)).filter(|&l| l < 256)? as u8),
        None => (0, 0),
    };
    let minmax = folds.iter().any(|f| matches!(f, Fold::Min | Fold::Max));
    // D3154: a length's extremes ride in the answer's min / max, which a code extremum's own: one or the other
    let lenmm = folds.contains(&Fold::LenExt);
    if minmax && lenmm {
        return None;
    }
    let len_flags = match (len, signed) {
        (None, _) => 0,
        (Some(_), None) => PK_LEN,
        (Some(_), Some((_, w))) => PK_SIGNED | u64::from(w) << PK_SIGNED_SHIFT,
    };
    if signed.is_some() {
        fires::fire("dict.percode_const.words"); // shapecheck fire signal (REGISTRY BG-03): a word table's signed sum
    }
    // CACHE-MISS lever a: the source covers every filter op (the condition cache's ops when it is on), so the verdicts are
    // its covered quals and the host's pass fills a miss window's survivor words
    let covers = crate::ops::shared::scan::covered_ops(px, k);
    let covered = p.ops.iter().enumerate().all(|(j, op)| !matches!(op, Op::Filter(_)) || (j < 64 && covers >> j & 1 != 0));
    let flags = (if attno.is_some() { PK_CODES } else { 0 }) | len_flags | (if minmax { PK_MINMAX } else { 0 }) | (if lenmm { PK_LENMM } else { 0 }) | (if covered && verdicts.n() > 0 { PK_COVERED } else { 0 });
    fires::fire("dict.percode_const"); // shapecheck fire signal (REGISTRY KEYED-15, B3-05)
    if verdicts.n() > 0 {
        fires::fire("dict.percode_const.verdict"); // shapecheck fire signal (REGISTRY B3-05, BG-03)
    }
    if verdicts.n() > 1 {
        fires::fire("dict.percode_const.verdicts"); // shapecheck fire signal (REGISTRY BG-03): two or more verdicts
    }
    if minmax {
        fires::fire("dict.percode_const.minmax"); // shapecheck fire signal (REGISTRY KEYED-15)
    }
    if lenmm {
        fires::fire("dict.percode_const.lenmm"); // shapecheck fire signal (D3154): min / max of a per-code length or word
    }
    Some(Plan { folds, len, col, flags, nullable: rec.nullable, count, verdicts })
}

/// BG-03 / B3-05: compiled aggregate argument `id` whole as one per-code word of one code column (its per-code plan reads
/// that column alone through one `Words` table whose root is the argument's own root, strict), an integer result:
/// `(the column, the site table, the result's width in bytes)`.
fn word_table(px: &Px<'_, '_>, id: ExId) -> Option<(i16, u16, u8)> {
    use gravity_expr::percode::{CodeCol, Form};
    let ex = px.placed(id);
    let plan = ex.ext.as_deref()?.downcast_ref::<crate::dict::percode::Plan>()?;
    let (&[(a, _, _)], &[(t, crate::dict::percode::Class::Words)]) = (&plan.cols[..], &plan.tables[..]) else { return None };
    let pc = ex.c.percode(&[CodeCol { attno: a }], px.mcx()).ok()?;
    let [r] = &pc.roots[..] else { return None };
    let w = gravity_fn::typesem::integral::int_width(ex.c.result_type)?;
    (r.attno == a && r.node == ex.c.root && r.form == Form::Words && r.table == Some(0) && r.null.is_none()).then_some((a, t, w))
}

/// Entry `resno` of the pipeline's last projection as a Var.
fn entry<'p>(p: &gravity_decompose::PipelineNode<'p>, resno: i16) -> Option<&'p types_nodes::primnodes::Var<'p>> {
    let Some(Op::Project(l)) = p.ops.iter().rev().find(|op| matches!(op, Op::Project(_))) else { return None };
    crate::table::emit_group::bare(l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno)?.expr).as_var()
}

impl Plan<'_> {
    /// `EndMech::whole` for the keyed end `fold` of `rec`: a constant window's answer folded into its key's record, then
    /// the row loop's exit with the start carry `c` and the rows folded. False when this window's code emits no branch
    /// (no constant fact, a length the window's bind did not bind).
    pub(crate) fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, fold: &dyn RunFold, win: &Win, c: &Carry, l_wend: Block) -> bool {
        let (Some((konst, datum)), Some(site)) = (win.facts.const_key, env.site) else { return false };
        // the window's code lane is the column's (the source's `env.codes`, set by its next_window)
        if self.flags & PK_CODES != 0 && !env.codes.iter().any(|c| (c.attno, c.ordinal) == self.col) {
            return false;
        }
        let lt = match self.len {
            Some(id) => match (crate::step::expr::plan_of(env, id).and_then(|p| p.tables.first().copied()), self.flags & PK_SIGNED != 0) {
                (Some((t, crate::dict::percode::Class::Len)), false) | (Some((t, crate::dict::percode::Class::Words)), true) => Some(t),
                _ => return false,
            },
            None => None,
        };
        // the length's table, else the first verdict's; the verdicts' slots ride in a list, relative (one out array): a
        // fold without one passes null
        let base = lt.or(self.verdicts.tables().next());
        let tab = match (base, env.tables) {
            (Some(t), Some(tp)) => b.ptr_add(tp, env.zero, 1, (gravity_kernels::host_abi::BIND_OUT_WORDS * t as usize * 8) as i32),
            (Some(_), None) => return false,
            (None, _) => b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, env.zero),
        };
        let vs = self.verdicts.list(b, env, base.unwrap_or(0));
        let verdict = if self.verdicts.n() > 0 { PK_VERDICT } else { 0 };
        let (l_try, l_fold, l_close, l_done, l_rows) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(konst, (l_try, &[]), (l_rows, &[]));
        b.switch_to(l_try);
        let slot = b.declare_stack_slot(answer::SIZE as u32, 8);
        let out = b.stack_addr(slot);
        let f = host(b, "agg_percode_const", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr, Type::I64, Type::Ptr, Type::Ptr], gravity_abi::host_words!(gravity_kernels::dict::strategies::percode_const::agg_percode_const(ctx, at, cursor, end, tab, flags, vs, out)));
        let (at, flags) = (b.iconst(Type::I64, (site << 8 | self.col.1 as usize) as i64), b.iconst(Type::I64, (self.flags | verdict) as i64));
        let r = b.call_host(f, &[env.ctx, at, win.cursor, win.end, tab, flags, vs, out], Some(Type::I64)).expect("agg_percode_const returns 0 or 1");
        let took = b.icmp(IntCond::Eq, r, env.zero);
        b.cond_br(took, (l_fold, &[]), (l_rows, &[]));
        b.switch_to(l_fold);
        let n = b.load(Type::I64, out, answer::NONNULL as i32);
        let any = b.icmp(IntCond::Ne, n, env.zero);
        b.cond_br(any, (l_close, &[]), (l_done, &[]));
        b.switch_to(l_close);
        // the one key's image: the datum word (an integer key's image, rep/word/hash.rs), its flags word clear (no NULL)
        let words = if self.nullable { vec![datum, env.zero] } else { vec![datum] }; // alloc: Program
        let img = ImageWords::plain(ImageValues { words }, true);
        let h = emit_key_hash(b, &img.words);
        let mut accs = PgVec::new_in(env.mcx);
        for (a, &f) in rec.aggs.iter().zip(self.folds.iter()) {
            let mut w = mcx::pg_vec![in env.mcx; env.zero; a.shape.words as usize];
            match f {
                Fold::Count => w[0] = n,
                // a length's sum, or (D3154) its extreme: the Stats answer reads SUM_LO or MIN / MAX by the register
                Fold::Len | Fold::LenExt => AnswerForm::Stats.need::<AnswerImpl>(&env.node).0.fold(b, env, &a.d, Acc::Regs(&mut w[..]), out),
                // class.minmax_code's carry of the window's rows: the kept extreme's word in the window's part, rendered
                // through the column's lane of this site at the close, over the n kept rows
                Fold::Min | Fold::Max => {
                    let field = if f == Fold::Min { answer::MIN } else { answer::MAX };
                    let code = b.load(Type::I64, out, field as i32);
                    let pid = b.load_zext(Type::I32, env.ctx, (site_off(site) + site_word::PART_ID) as i32);
                    let s32 = b.iconst(Type::I64, 32);
                    let part = b.int_bin(gravity_ir::IntBinOp::Shl, pid, s32);
                    w[0] = b.int_bin(gravity_ir::IntBinOp::Or, code, part);
                    w[1] = b.iconst(Type::I64, (site << 8 | self.col.1 as usize) as i64);
                    w[2] = n;
                }
            }
            accs.extend_in(w);
        }
        accs.extend_in(self.count.then_some(n));
        fold.close(b, env, &img, h, &accs[..]);
        b.br(l_done, &[]);
        b.switch_to(l_done);
        let mut params = c.params();
        params[0] = b.int_bin(gravity_ir::IntBinOp::Add, c.out, n);
        b.br(l_wend, &params);
        b.switch_to(l_rows);
        true
    }
}
