//! The count-walk front (V4R-30, cards/M2.md "the count-walk front"; a fused `count_codes` + part walk, attribution
//! levers L-COUNT and L-WALK): a window the per-code front (keyed_codefront_percode) could take row by row is taken whole
//! by the host (`agg_cw_window`, host_abi v4 m1_countwalk.rs): its rows counted per source code, no row loop, no key
//! image per row, no touched list; at the part's end the part's live codes are walked in code order, each code's key
//! image and `f` read once per code, a run of one key's codes folded once into the part's per-key map, which folds on as
//! the per-code front's does (one record format, which the owner merge cannot tell apart from the row path's).
//!
//! Taken when the pipeline's only ops before the end are projections and leading filters whose quals are each one code
//! verdict of the key's source column (`code_verdict`, mechanism D; BG-02): any number of per-code verdict bits (B3-06:
//! a comparison with any constant, `''` too, an IN list, a LIKE / ILIKE / regex / any boolean body of that column alone,
//! percode.rs's `Bits` root, the bits ANDed per code, not per row), and (KEYED-14) quals over other columns in leading filter ops the pgrc source covers
//! (`SourceMech::covers`): a covered window (`WinConds::covered`) is counted whole through its selection, which every
//! covered qual already passed (the frame elides their steps there); any other window of such a pipeline runs its rows.
//! The key's image is its
//! derived Fp128 table's entry (step_percode_key's form: the hosts' regexp table, or HITS9-E1 any other key of the column
//! as its swept `PROGRAM` table, an entry that raised or came out NULL `[0, 0]`, its part's rows run) and the `f`
//! aggregate reads a length table (the dictionary index, R4). A window the host declines and a join multiplicity take the rows. Budget 250 lines
//! (files-extra.tsv, +25 % gate). Owner M2-COUNTWALK.
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type};
use gravity_expr::percode::{CodeCol, Form};
use gravity_kernels::dict::strategies::CW_VERDICT;
use gravity_kernels::dict::strategies::countwalk::CW_LEN;
use gravity_kernels::host_abi::BIND_OUT_WORDS;
use gravity_program::LaneId;
use mcx::CollectIn;
use types_nodes::Node;

use crate::table::bypass::{fail_on, host, state, HASHED};
use crate::dict::strategies::codefront_percode::Percode;
use crate::ops::agg::hashagg::{Arg, GroupRec};
use crate::table::face::{TableDecl as KeyedTable, TableMech};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::px::{ExId, Px};
use crate::frame::row::Carry;

/// A front the host counts whole: the key's source column `(attno, site lane)`, the site table of the key's derived
/// Fp128 form, the site tables of the pipeline's per-code verdicts, and whether a qual
/// over another column (a covered op's) leaves only the covered windows to count whole.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Walk {
    col: (i16, u8),
    key: u16,
    verdicts: Verdicts,
    covered: bool,
}

/// KEYED-14: every comparison in qual `n` is under no collation or one that orders by bytes (an operator's, a function's,
/// an `= ANY`'s input collation); a node kind the walker does not know refuses.
fn bytes_ordered(gx: &crate::graph::Gx<'_, '_>, n: Node<'_>) -> bool {
    use types_nodes::NodeTag as T;
    gravity_decompose::expr::walk(n, &mut |x| {
        let c = match x.node_tag() {
            T::T_OpExpr => x.as_op_expr().map(|o| o.inputcollid),
            T::T_FuncExpr => x.as_func_expr().map(|f| f.inputcollid),
            T::T_ScalarArrayOpExpr => x.as_scalar_array_op_expr().map(|s| s.inputcollid),
            // the other comparisons that carry an input collation: refused rather than read
            T::T_DistinctExpr | T::T_NullIfExpr | T::T_MinMaxExpr | T::T_RowCompareExpr | T::T_SubPlan => None,
            _ => Some(0),
        };
        c.is_some_and(|c| c == 0 || gx.facts.coll(c).bytes_order)
    })
}

/// A qual whole as one code verdict of a column (mechanism D, BG-02 / BG-03): one per-code verdict bit (`Bits`, a site
/// table), whatever the constant (`col <> ''` is the empty entry's code set as `col <> 'x'` is the entry of 'x').
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Bits(u16),
}

/// The quals of one code column a whole-window fold absorbs: the per-code verdict tables, ANDed per code (any number: a
/// set over the site's table slots, which are u8 by type, percode_guard `fits`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Verdicts {
    set: [u64; 4],
}

impl Verdicts {
    /// Absorbs verdict `v`; false when a table's slot is past the site's slot type.
    pub(crate) fn add(&mut self, v: Verdict) -> bool {
        match v {
            Verdict::Bits(t) if t < 256 => self.set[t as usize >> 6] |= 1 << (t & 63),
            Verdict::Bits(_) => return false,
        }
        true
    }

    /// The verdict tables' site slots, ascending.
    pub(crate) fn tables(&self) -> impl Iterator<Item = u16> + '_ {
        (0..256u16).filter(|&t| self.set[t as usize >> 6] >> (t & 63) & 1 != 0)
    }

    /// How many verdict tables.
    pub(crate) fn n(&self) -> usize {
        self.set.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// The host's verdict list `[n, d_1 .. d_n]` (kernels `Verdicts`): each table's slot relative to `base`'s, in a stack
    /// slot of the window's code; a null pointer when there is none.
    pub(crate) fn list(&self, b: &mut FunctionBuilder<'_>, env: &Env, base: u16) -> gravity_ir::Value {
        if self.n() == 0 {
            return b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero);
        }
        let slot = b.declare_stack_slot((8 * (1 + self.n())) as u32, 8);
        let at = b.stack_addr(slot);
        let words = std::iter::once(self.n() as i64).chain(self.tables().map(|t| i64::from(t) - i64::from(base)));
        for (i, w) in words.enumerate() {
            let v = b.iconst(Type::I64, w);
            b.store(at, (8 * i) as i32, v);
        }
        at
    }
}

/// B3-06 / BG-02: qual `q` whole as one code verdict of column `attno` (`Verdict`): its per-code plan reads that column
/// alone, its one root the qual's own root, a `Bits` table (the site table).
pub(crate) fn code_verdict(px: &Px<'_, '_>, q: ExId, attno: i16) -> Option<Verdict> {
    let ex = px.placed(q);
    let plan = ex.ext.as_deref()?.downcast_ref::<crate::dict::percode::Plan>()?;
    let &[(a, _, _)] = &plan.cols[..] else { return None };
    let pc = ex.c.percode(&[CodeCol { attno }], px.mcx()).ok()?;
    let [r] = &pc.roots[..] else { return None };
    if a != attno || r.attno != attno || r.node != ex.c.root {
        return None;
    }
    match (r.form, &plan.tables[..]) {
        (Form::Bits, &[(t, crate::dict::percode::Class::Bits | crate::dict::percode::Class::Set)]) if r.table == Some(0) && r.null.is_none() => Some(Verdict::Bits(t)),
        _ => None,
    }
}

/// Pipeline `k`'s walk for the per-code front over dictionary lane `lane` of record `rec` (see the module doc); `None`
/// takes the rows. The key is the last projection's column, a derived fingerprint (step_percode_key `derived`) whose
/// table the projection's key form declares; declared here too, one table (step_percode `declare_table` dedupes); any
/// other key of the column, its projection's swept Fp128 table (HITS9-E1, percode_key `swept_table`).
pub(crate) fn plan(px: &mut Px<'_, '_>, k: usize, lane: u8, rec: &GroupRec<'_, '_>) -> Option<Walk> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let Source::Range { .. } = &p.source else { return None };
    let LaneId::Attr(attno) = px.site_mut(k)?.lanes().get(lane as usize)?.id else { return None };
    let (Some(Some(Arg::Col(resno))), Some(Op::Project(list))) = (rec.ksrc.first(), p.ops.last()) else { return None };
    let te = list.iter().nth((*resno as usize).checked_sub(1)?)?.as_target_entry()?;
    let derived = crate::dict::strategies::percode_key::derived(te.expr, px.mcx());
    // HITS9-E1: any other key of the column, its entries' Fp128 swept once per part (percode_key's PROGRAM table); none
    // takes the rows before a qual is read
    let swept = match derived {
        Some(_) => None,
        None => Some(crate::dict::strategies::percode_key::swept_table(px, k, lane, attno)?),
    };
    let (mut verdicts, mut covered) = (Verdicts::default(), false);
    // KEYED-14: the leading filter ops the source covers (a covered window's selection passes every qual of them)
    let covers = crate::ops::shared::scan::covered_ops(px, k);
    for (j, op) in p.ops.iter().enumerate() {
        match op {
            Op::Project(_) => {}
            Op::Filter(q) => {
                let mcx = px.mcx();
                for (i, q) in q.as_list().map_or_else(|| mcx::pg_vec![in mcx; *q], |l| l.iter().collect_in(mcx)).into_iter().enumerate() {
                    // a leading filter, its quals compiled over the drafted site (pgrc/lower.rs): nothing compiles here
                    if !p.ops.iter().take(j).all(|op| matches!(op, Op::Filter(_))) {
                        return None;
                    }
                    let node = q;
                    let q = *crate::step::expr::quals(px, k, j).get(i)?;
                    let Some(v) = code_verdict(px, q, attno) else {
                        // a qual over another column (or any qual no verdict bit answers): a covered op's, counted
                        // through a covered window's selection, when it compares under no collation or one that orders by
                        // bytes (a text qual under another collation refuses: the cover stays a bytes-exact one); else the rows
                        if j < 64 && covers >> j & 1 != 0 && bytes_ordered(gx, node) {
                            covered = true;
                            continue;
                        }
                        return None;
                    };
                    // BG-02: any number of verdicts of the key's column, ANDed per code
                    if !verdicts.add(v) {
                        return None;
                    }
                }
            }
            _ => return None,
        }
    }
    let key = match derived {
        Some(d) => {
            let decl = d.decl(px.site_mut(k)?, lane);
            crate::dict::percode::declare_table(px, k, decl, crate::dict::percode::Class::Fp)?
        }
        None => {
            fires::fire("dict.countwalk.swept"); // shapecheck fire signal (REGISTRY HITS9-E1)
            swept?
        }
    };
    fires::fire("dict.countwalk"); // shapecheck fire signal (REGISTRY KEYED-14, B3-06)
    if verdicts.n() > 0 {
        fires::fire("dict.countwalk.verdict"); // shapecheck fire signal (REGISTRY B3-06, BG-02)
    }
    if verdicts.n() > 1 {
        fires::fire("dict.countwalk.verdicts"); // shapecheck fire signal (REGISTRY BG-02): two or more verdicts
    }
    if covered {
        fires::fire("dict.countwalk.covered"); // shapecheck fire signal (REGISTRY KEYED-14)
    }
    Some(Walk { col: (attno, lane), key, verdicts, covered })
}

/// `EndMech::whole` of the keyed end over table `t` (record `rec`) for its per-code front `pc`: the window counted by the
/// host, then the row loop's exit with the start carry `c` and the rows counted; the rows when the host declines. False
/// when this window's code emits no branch (no walk, a multiplicity, no key or length table).
#[allow(clippy::too_many_arguments)]
pub(crate) fn whole(pc: &Percode, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, t: &KeyedTable, win: &Win, c: &Carry, l_wend: Block) -> bool {
    let (Some(w), Some(tables), None) = (pc.cw, env.tables, env.mult) else { return false };
    if !env.codes.iter().any(|x| (x.attno, x.ordinal) == w.col) {
        return false;
    }
    // KEYED-14: a walk over a covered op's qual counts only a covered window (its selection passed the qual)
    let cov = match (w.covered, win.facts.covered) {
        (false, _) => None,
        (true, Some(cv)) => Some(cv),
        (true, None) => return false,
    };
    let kt = w.key;
    let lt = match pc.f.map(|i| rec.aggs[i].args.first()) {
        None => None,
        Some(Some(Arg::Ex { id, .. })) => match crate::step::expr::plan_of(env, *id).and_then(|p| p.tables.first().copied()) {
            Some((t, crate::dict::percode::Class::Len)) => Some(t),
            _ => return false,
        },
        Some(_) => return false,
    };
    // a covered walk's other windows take the rows (a walk over the window's own quals keeps its IR: no early block)
    let l_cov_rows = cov.map(|cv| {
        let (l_call, l_rows) = (b.create_block(&[]), b.create_block(&[]));
        let cv = crate::frame::frame::all(b, env, &[cv]).unwrap_or(cv);
        b.cond_br(cv, (l_call, &[]), (l_rows, &[]));
        b.switch_to(l_call);
        l_rows
    });
    let at_table = |b: &mut FunctionBuilder<'_>, t: u16| b.ptr_add(tables, env.zero, 1, (BIND_OUT_WORDS * t as usize * 8) as i32);
    let key = at_table(b, kt);
    let len = match lt {
        Some(t) => at_table(b, t),
        None => b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero),
    };
    // the front's address is the first of its words after the hashed table's (keyed_codefront_percode `PW_FRONT`)
    let st = state(b, env, t);
    let front = b.load(Type::Ptr, st, HASHED.state_bytes(t) as i32);
    let (ops, at) = pc.words(b, env);
    // the verdicts' bind slots ride in a list, relative to the key's (one out array): a walk without one passes null
    let v = &w.verdicts;
    let vs = v.list(b, env, kt);
    let verdict = if v.n() > 0 { CW_VERDICT } else { 0 };
    let flags = b.iconst(Type::I64, ((if lt.is_some() { CW_LEN } else { 0 }) | verdict) as i64);
    let slot = b.declare_stack_slot(8, 8);
    let rows = b.stack_addr(slot);
    let f = host(b, "agg_cw_window", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr, Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::Ptr], gravity_abi::host_words!(gravity_kernels::dict::strategies::countwalk::agg_cw_window(ctx, st, front, at, cursor, end, key, len, flags, vs, ops, rows)));
    let r = b.call_host(f, &[env.ctx, st, front, at, win.cursor, win.end, key, len, flags, vs, ops, rows], Some(Type::I64)).expect("agg_cw_window returns a status");
    let x = env.x;
    fail_on(b, env, &x, r);
    let (l_took, l_rows) = (b.create_block(&[]), l_cov_rows.unwrap_or_else(|| b.create_block(&[])));
    let took = b.icmp(IntCond::Eq, r, env.zero);
    b.cond_br(took, (l_took, &[]), (l_rows, &[]));
    b.switch_to(l_took);
    let n = b.load(Type::I64, rows, 0);
    let mut params = c.params();
    params[0] = b.int_bin(IntBinOp::Add, c.out, n);
    b.br(l_wend, &params);
    b.switch_to(l_rows);
    true
}
