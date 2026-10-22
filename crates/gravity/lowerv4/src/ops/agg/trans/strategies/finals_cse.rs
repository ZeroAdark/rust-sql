//! CSE across once-row finals (many shifted sums of one column). Hooks / levers: H35 R2-L14. Budget 160 lines
//! (files.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Road `finals.cse` (AB twin `gravity_road_off=finals.cse`; applies under `Fold`). The planner's aggregate algebra
//! (agg_algebra.rs) rewrites `sum(x ± k)` over x's aggregates; every guarded one it builds over an int2 / int4 `x` and an
//! integer literal `k` it marks: the Agg carries a `GravityFinal` per such target entry (its resno and the aggnos of
//! min(x), max(x), sum(x), count(x)), which decompose hands to the group (`GroupDecl::finals`). This road reads that mark,
//! never the entry's spelling (catalog NEW-5 / AR-47 / AR-49 / AR-50): one producer, one source of truth, whichever form
//! the planner wrote (the bare int8 form or the count-guarded one, so int4 columns and joins too). The mark's contract
//! fixes the entry's guard, `(min(x) OP k) IS NOT NULL AND (max(x) OP k) IS NOT NULL`: from it the road takes OP and k's
//! literal (its word), and checks the aggregates the mark names. Each member is a record and ONE pure host call
//! (`members`) evaluates them all in list order, each in PG's order: the edges, which raise where a row at an edge of x
//! would, then PostgreSQL's int8 sum of `x ± k` (int2_sum / int4_sum wrap past 2^63), returning the first raise's mask.
//! Served where two or more are found and no entry before the last can raise (a bare Aggref, Var or Const): the planner's
//! cost is the one threshold (agg_algebra rewrites for this road at two or more guarded sums), no count of aggregates
//! (catalog FOLD-05 / BG-12). Anything else is the straight form, never a refusal. Widths are typesem's integral-width and integer-arithmetic properties.
use gravity_abi::status::raise;
use gravity_expr::{emit::{AggCell, CellForm, CellValid, ParamNeed}, NullableDatum};
use gravity_fn::typesem::integral::{int_arith, int_width, IntOp};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::{primnodes::{BoolExprType, NullTestType}, Node, NodeTag};

use crate::frame::{env::Env, px::Px, row::RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};

crate::strategies! {
    /// CSE across once-row finals (several aggregates over shared argument expressions).
    FINALS_CSE = 29, "finals.cse", Fold;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(FINALS_CSE, &Road)], ..crate::ops::NONE };

/// The road's registration value.
struct Road;

/// A record: the entries of min(x), max(x), sum(x), count(x) in the drain's area (16 bits each; `emit` maps each aggno to
/// its entry), flags, the literal k. Flags: OP's result width (bits 0..2; 0 int2, 1 int4, 2 int8), OP `-` (2), the
/// literal left of OP (3), x's width (8..10), k's width (10..12), k's null byte (byte 4).
pub(crate) const WORDS: usize = 3;
const MINUS: u64 = 1 << 2;
const LIT_FIRST: u64 = 1 << 3;

/// One member: its record's fixed words and its literal (a word of the bind image, or a folded value and null).
pub(crate) struct Member { aggnos: u64, flags: u64, lit: (Place, i64, bool) }

/// A projection's members: `of[r]` = entry `r`'s member, if it is one.
pub(crate) struct Families<'g> { members: PgVec<'g, Member>, pub of: PgVec<'g, Option<u16>> }

/// A width in bytes (2, 4, 8) as its record code (0, 1, 2).
fn code(bytes: u8) -> u64 {
    u64::from(bytes.trailing_zeros()) - 1
}

/// One guard edge `(agg OP k) IS NOT NULL`, its aggregate aggno `aggno`: (OP's function, k, the literal left of OP, the
/// aggregate's result type).
fn edge<'p>(n: Node<'p>, aggno: i32) -> Option<(u32, Node<'p>, bool, u32)> {
    let t = n.as_null_test().filter(|t| t.nulltesttype == NullTestType::IS_NOT_NULL && !t.argisrow)?;
    let o = t.arg?.as_op_expr()?;
    let mut args = o.args.iter();
    let (l, r) = (args.next()?, args.next()?);
    let (a, k, first) = if l.node_tag() == NodeTag::T_Const { (r, l, true) } else { (l, r, false) };
    let a = a.as_aggref().filter(|a| a.aggno == aggno)?;
    k.as_const().is_some_and(|c| !c.constisnull).then_some((o.opfuncid, k, first, a.aggtype))
}

/// Entry `e` under its mark, the aggnos `[min, max, sum, count]`: (aggnos, flags, k). OP is an integer `+` / `-` of x and
/// k at their declared widths, read from the first edge (min(x)'s), the second (max(x)'s) of the same function, side and
/// literal type; else `None` (the straight form). The mark's contract is one k: it is read once, from the first edge
/// (its word), and no other copy's value decides anything (D407: the program is the same for every literal).
fn member<'p>(e: Node<'p>, no: [i32; 4]) -> Option<(u64, u64, Node<'p>)> {
    let w = e.as_case_expr()?.args.iter().next()?.as_case_when()?;
    let and = w.expr?.as_bool_expr().filter(|b| b.boolop == BoolExprType::AND_EXPR)?;
    let mut edges = and.args.iter();
    let ((f, k, first, t), (f2, k2, first2, t2)) = (edge(edges.next()?, no[0])?, edge(edges.next()?, no[1])?);
    let a = int_arith(f).filter(|a| a.op != IntOp::Mul)?;
    let (kc, kc2) = (k.as_const()?, k2.as_const()?);
    let (xw, kw) = if first { (a.right, a.left) } else { (a.left, a.right) };
    (f2 == f && first2 == first && kc2.consttype == kc.consttype).then_some(())?;
    (int_width(kc.consttype) == Some(kw) && int_width(t) == Some(xw) && int_width(t2) == Some(xw)).then_some(())?;
    let at = |i: usize| u64::try_from(no[i]).ok().filter(|&n| n < 1 << 16);
    let aggnos = at(0)? | at(1)? << 16 | at(2)? << 32 | at(3)? << 48;
    let flags = code(a.result) | if a.op == IntOp::Sub { MINUS } else { 0 } | if first { LIT_FIRST } else { 0 } | code(xw) << 8 | code(kw) << 10;
    Some((aggnos, flags, k))
}
/// The members of pipeline `k`'s projection `list` when the road serves it (a plain or keyed drain; two marked members or
/// more; every entry before the last member a member or quiet). The marks index the Agg's own target list, so they are
/// read only where `list` is that list (decompose rewrote no entry of it). A keyed group's emit evaluates its projection
/// over each group's finals as the plain drain does over its one group's, and the planner marks a grouped Agg's members
/// under the same order_safe (agg_algebra "Errors": no HAVING, quiet quals and targets, one raise type), so each group's
/// first raising member raises what its guard would. Places the members' literals, their only compile.
pub(crate) fn families<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, list: &types_nodes::NodeList<'p>) -> Option<Families<'g>> {
    use gravity_decompose::ops::shared::agg::GroupForm;
    let g = px.gx.drained_group(k).filter(|g| matches!(g.form, GroupForm::Plain | GroupForm::Hashed | GroupForm::Sorted))?;
    let (_, marks) = g.finals.filter(|f| f.0 == core::ptr::from_ref(list) as usize)?;
    if !px.strategies.has(FINALS_CSE) {
        return None;
    }
    let expr = |t: Node<'p>| t.as_target_entry().map_or(t, |te| te.expr);
    let mark = |r: usize| marks.iter().find(|m| usize::try_from(m.resno).is_ok_and(|n| n == r + 1)).map(|m| [m.min, m.max, m.sum, m.count]);
    let found: PgVec<'_, _> = list.iter().enumerate().map(|(r, t)| mark(r).and_then(|no| member(expr(t), no))).collect_in(px.mcx());
    let last = found.iter().rposition(Option::is_some)?;
    if found.iter().flatten().count() < 2 || !list.iter().zip(found.iter()).take(last).all(|(t, m)| m.is_some() || crate::effects::visible::agg::quiet(expr(t))) {
        return None;
    }
    let mcx = px.mcx();
    let (mut members, node) = (PgVec::new_in(mcx), px.gx.pipelines()[k].node_id);
    let of = found.into_iter().map(|m| m.map(|(aggnos, flags, k)| {
        members.push_in(Member { aggnos, flags, lit: place(px, k, node) });
        (members.len() - 1) as u16
    }));
    let of = of.collect_in(mcx);
    fires::fire("finals.cse"); // shapecheck fire signal (REGISTRY FOLD-05 / NEW-5 / BG-12 / OIDW-70..71 / AR-46..50)
    Some(Families { members, of })
}

/// A literal's word through the word plan (a claim first, as step/consts.rs), or its folded value.
fn place<'p>(px: &mut Px<'_, 'p>, n: Node<'p>, node: i32) -> (Place, i64, bool) {
    let c = n.as_const().expect("a member literal is a Const");
    let nd = if c.constisnull { NullableDatum::null() } else { NullableDatum::value(gravity_expr::nodes::konst::word_canon(c.consttype, c.constvalue)) };
    let p = px.take_claim(n).unwrap_or_else(|| px.plan.need_at(&ParamNeed::Const(nd), c.consttype, c.constbyval, n));
    // a folded literal's value is in the code; a by-reference one would be its image's address, freed with the plan (D684)
    if p == Place::Fold && !c.constbyval && !c.constisnull { crate::refuse!(node, Reason::Shape { why: ShapeWhy("a by-reference literal past the word budget") }) }
    let v = if p == Place::Fold { gravity_decompose::bake::read(c) } else { c.constvalue };
    (p, v.as_i64(), c.constisnull)
}

/// The 16-byte entry of aggregate `aggno`'s final in the drain's area: a datum word at `16 e`, valid at `16 e + 8` (bit 0
/// of the plain drain's byte, the keyed emit's 0 / 1 word: `members` reads bit 0 of that byte either way).
fn entry(aggs: &crate::frame::env::AggRow, aggno: u64) -> Option<u64> {
    let c: &AggCell = aggs.cells.get(aggno as usize)?;
    let valid = match c.valid { CellValid::Bit { off, bit: 0 } | CellValid::NonZeroAt(off) => off, _ => return None };
    (c.form == CellForm::Word && c.off % 16 == 0 && valid == c.off + 8).then_some(u64::from(c.off / 16))
}

/// Every member's `(value, null)`: the records written (each final named by its entry in the drain's area), one `members`
/// call, its raise mask through the row's raise block.
pub(crate) fn emit<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, row: &RowVals, f: &Families) -> PgVec<'e, (Value, Value)> {
    let Some(aggs) = env.aggs.as_ref() else { crate::refuse!(env.node, Reason::Bug { payload: "finals.cse outside a drain" }) };
    let at_area = |m: &Member| (0..4).try_fold(0u64, |acc, i| entry(aggs, m.aggnos >> (16 * i) & 0xffff).filter(|&e| e < 1 << 16).map(|e| acc | e << (16 * i)));
    let mut entries: PgVec<'_, u64> = mcx::reserved_in(f.members.len(), env.mcx);
    for m in &f.members {
        let Some(e) = at_area(m) else {
            crate::refuse!(env.node, Reason::Bug { payload: "finals.cse: a final off the drain's area" })
        };
        entries.push_in(e);
    }
    let n = f.members.len();
    let (recs, out) = (b.declare_stack_slot((8 * WORDS * n) as u32, 8), b.declare_stack_slot((16 * n) as u32, 8));
    let (recs, out) = (b.stack_addr(recs), b.stack_addr(out));
    for (i, m) in f.members.iter().enumerate() {
        let at = (8 * WORDS * i) as i32;
        let a = b.iconst(Type::I64, entries[i] as i64);
        b.store(recs, at, a);
        let (p, v, null) = m.lit;
        let mut flags = b.iconst(Type::I64, (m.flags | if !matches!(p, Place::Word(_)) && null { 1 << 32 } else { 0 }) as i64);
        let (v, word) = match p { Place::Word(w) => (b.load(Type::I64, env.params, w as i32 * 8), Some(w)), _ => (b.iconst(Type::I64, v), None) };
        b.store(recs, at + 16, v);
        if let Some(w) = word {
            let (nb, sh) = (b.load_zext(Type::I8, env.params_null, w as i32), b.iconst(Type::I64, 32));
            let bit = b.int_bin(IntBinOp::Shl, nb, sh);
            flags = b.int_bin(IntBinOp::Or, flags, bit);
        }
        b.store(recs, at + 8, flags);
    }
    let fl = HostFlags { touches_memory: true, ..HostFlags::default() };
    let h = b.declare_host_once("finals_cse::members", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr], Some(Type::I64), crate::ops::agg::raw::members as *const () as usize, fl);
    let nv = b.iconst(Type::I64, n as i64);
    let mask = b.call_host(h, &[aggs.rec, recs, nv, out], Some(Type::I64)).expect("returns its raise mask");
    crate::step::expr::raise_if(b, env, row, mask);
    let at = |b: &mut FunctionBuilder<'_>, o: i32| (b.load(Type::I64, out, o), b.load_zext(Type::I8, out, o + 8));
    (0..n as i32).map(|i| at(b, 16 * i)).collect_in(env.mcx).into_iter().map(|(v, nb)| (v, b.icmp(IntCond::Ne, nb, env.zero))).collect_in(env.mcx)
}

/// `r` in the range of width code `w`, else that type's raise code.
fn fit(r: i128, w: u64) -> Result<i64, u8> {
    let (half, code) = [(1i128 << 15, raise::INT2_RANGE), (1 << 31, raise::INT4_RANGE), (1 << 63, raise::INT8_RANGE)][w as usize];
    if r < -half || r >= half { Err(code) } else { Ok(r as i64) }
}

/// `a OP b` exact, in OP's result range (`w`), else that type's raise code.
fn op(a: i64, b: i64, minus: bool, w: u64) -> Result<i64, u8> {
    fit(if minus { i128::from(a) - i128::from(b) } else { i128::from(a) + i128::from(b) }, w)
}

/// One member in PG's order over the finals min(x), max(x), sum(x), count(x) and k: `Ok(None)` is its NULL (an edge
/// NULL: no row, the guard false, the ELSE taken); an edge raises as a row at that edge of x would; else PostgreSQL's
/// int8 sum of `x OP k` over the rows, `sum(x) ± k · count(x)` taken modulo 2^64 as int2_sum / int4_sum wrap (sum(x) is
/// that accumulator's own wrapped value, and the wrap is a ring map). Both of the planner's spellings compute it.
pub(crate) fn eval(fin: [Option<i64>; 4], k: Option<i64>, flags: u64) -> Result<Option<i64>, u8> {
    let (w, minus, first) = (flags & 3, flags & MINUS != 0, flags & LIT_FIRST != 0);
    let Some(k) = k else { return Ok(None) };
    for e in [fin[0], fin[1]] {
        let Some(e) = e else { return Ok(None) };
        if first { op(k, e, minus, w)? } else { op(e, k, minus, w)? };
    }
    let (Some(s), Some(n)) = (fin[2], fin[3]) else { return Ok(None) };
    let kn = k.wrapping_mul(n);
    Ok(Some(match (minus, first) { (false, _) => s.wrapping_add(kn), (true, false) => s.wrapping_sub(kn), (true, true) => kn.wrapping_sub(s) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The straight compile's per-op form (gravity_expr `prim::emit`, what an OpExpr of these functions lowers to) under
    /// the interpreter: the value, or the raise mask.
    struct Straight(HashMap<u32, (gravity_ir::Program, gravity_ir::Func)>);

    impl Straight {
        fn op(&mut self, fid: u32, x: i64, y: i64) -> Result<i64, u64> {
            let (p, f) = self.0.entry(fid).or_insert_with(|| {
                let mut pb = gravity_ir::builder::ProgramBuilder::new();
                let f = pb.declare_func("op", &[Type::I64, Type::I64], &[Type::I64, Type::I64]);
                let mut b = pb.define(f);
                let (x, y) = (b.params()[0], b.params()[1]);
                let o = gravity_expr::emit::prim::emit(&mut b, fid, &[x, y]).expect("a tier-A integer op");
                b.ret(&[o.value, o.err]);
                drop(b);
                (pb.finish(), f)
            });
            let r = gravity_interp::Interp::new(p).run(*f, &[x as u64, y as u64]).expect("runs");
            if r[1] != 0 { Err(r[1]) } else { Ok(r[0] as i64) }
        }

        /// The planner's guard the straight way: the AND's edges left to right (a NULL edge: the AND false, the ELSE
        /// NULL); then the value: PostgreSQL's int8 sum of `x OP k`, `sum ± k · count` modulo 2^64 (`wrapped`).
        fn member(&mut self, m: &Case) -> Result<Option<i64>, u64> {
            let (fid, first, fin, k) = *m;
            let Some(k) = k else { return Ok(None) };
            for e in [fin[0], fin[1]] {
                let Some(e) = e else { return Ok(None) };
                if first { self.op(fid, k, e)? } else { self.op(fid, e, k)? };
            }
            Ok(fin[2].zip(fin[3]).map(|(s, n)| wrapped(int_arith(fid).unwrap().op == IntOp::Sub, first, s, k, n)))
        }

        /// The planner's bare int8 form (agg_algebra `sum_expr`: `sum + k8 * count`, k8 = -k for `x - k`; `k * count -
        /// sum` for `k - x`) through int8mul / int8pl / int8mi: its value where it does not raise.
        fn bare(&mut self, minus: bool, first: bool, s: i64, k: i64, n: i64) -> Option<i64> {
            match (minus, first) {
                (true, true) => self.op(465, k, n).and_then(|p| self.op(464, p, s)).ok(),
                (true, false) => self.op(465, k.checked_neg()?, n).and_then(|p| self.op(463, s, p)).ok(),
                (false, _) => self.op(465, k, n).and_then(|p| self.op(463, s, p)).ok(),
            }
        }
    }

    /// `s ± k · n` (or `k · n - s`) exact, taken modulo 2^64 into int8.
    fn wrapped(minus: bool, first: bool, s: i64, k: i64, n: i64) -> i64 {
        let kn = i128::from(k) * i128::from(n);
        let v = match (minus, first) { (false, _) => i128::from(s) + kn, (true, false) => i128::from(s) - kn, (true, true) => kn - i128::from(s) };
        v as i64
    }

    /// (OP's function, literal left of OP, finals min / max / sum / count, literal k).
    type Case = (u32, bool, [Option<i64>; 4], Option<i64>);

    fn max(w: u64) -> i64 {
        [i16::MAX as i64, i32::MAX as i64, i64::MAX][w as usize]
    }

    fn edge_values(w: u64) -> [i64; 9] {
        let m = max(w);
        [0, 1, -1, 7, -300, m, -m - 1, m - 1, -m]
    }

    /// `members` over records laid out as `member` and `emit` write them (a NULL literal folded into the flags; widths
    /// from the function's declared operands): each member's result, and the returned mask. Narrow finals carry junk
    /// above their width, as a word may.
    fn host(cases: &[Case]) -> (Vec<Option<i64>>, i64) {
        let mut fin = vec![0u8; 16 * 4 * cases.len()];
        let mut words = Vec::new();
        for (i, &(fid, first, f, k)) in cases.iter().enumerate() {
            let a = int_arith(fid).expect("an OP");
            let (xw, kw) = if first { (code(a.right), code(a.left)) } else { (code(a.left), code(a.right)) };
            let mut flags = code(a.result) | if a.op == IntOp::Sub { MINUS } else { 0 } | if first { LIT_FIRST } else { 0 } | xw << 8 | kw << 10;
            let mut aggnos = 0;
            for (j, v) in f.iter().enumerate() {
                let at = 4 * i + (3 - j);
                aggnos |= (at as u64) << (16 * j);
                let fw = if j < 2 { xw } else { 2 };
                let junk = if fw < 2 { 0x5a5a_u64 << 48 } else { 0 };
                let word = v.map_or(0xdead_beef, |v| if junk != 0 { (v as u64 & ((1u64 << (16 << fw)) - 1)) | junk } else { v as u64 });
                fin[16 * at..16 * at + 8].copy_from_slice(&word.to_le_bytes());
                fin[16 * at + 8] = if v.is_some() { 1 } else { 0xfe };
            }
            flags |= u64::from(k.is_none()) << 32;
            words.extend([aggnos, flags, k.unwrap_or(9) as u64]);
        }
        let mut out = vec![7u64; 2 * cases.len()];
        let mask = crate::ops::agg::raw::members_of(&fin, &words, &mut out);
        (out.chunks(2).map(|c| (c[1] == 0).then_some(c[0] as i64)).collect(), mask)
    }

    fn check(s: &mut Straight, case: Case) -> bool {
        let (got, mask) = host(&[case]);
        match s.member(&case) {
            Ok(v) => (assert_eq!((got[0], mask), (v, 0), "{case:?}"), false).1,
            Err(m) => (assert_eq!(mask as u64, m, "{case:?}"), true).1,
        }
    }

    /// Every integer `+` / `-` typesem names (a mark's OP), both literal sides: the edges' values and raise codes as the
    /// straight guard's, then PostgreSQL's wrapping int8 sum, which equals the planner's bare int8 form wherever that form
    /// does not raise (the members finals.cse served before the mark keep their values).
    #[test]
    fn members_match_the_straight_guard_and_postgres_sum() {
        let mut s = Straight(HashMap::new());
        let (mut checked, mut raised, mut bare) = (0, 0, 0);
        let edges: Vec<u32> = gravity_fn::ROWS.iter().map(|r| r.oid).filter(|&f| int_arith(f).is_some_and(|a| a.op != IntOp::Mul)).collect();
        assert_eq!(edges.len(), 18);
        for &fid in &edges {
            let a = int_arith(fid).unwrap();
            for first in [false, true] {
                let (xw, kw) = if first { (code(a.right), code(a.left)) } else { (code(a.left), code(a.right)) };
                for e in edge_values(xw) {
                    for k in edge_values(kw) {
                        for (sum, count) in [(e.wrapping_mul(3), 3), (i64::MAX - 2, 5), (e, i64::MAX / 2), (i64::MIN, 1)] {
                            let fin = [Some(e), Some(e.saturating_add(1).min(max(xw))), Some(sum), Some(count)];
                            raised += u32::from(check(&mut s, (fid, first, fin, Some(k))));
                            checked += 1;
                            if let (Ok(Some(v)), Some(b)) = (s.member(&(fid, first, fin, Some(k))), s.bare(a.op == IntOp::Sub, first, sum, k, count)) {
                                assert_eq!(v, b, "bare form {fid} {first} {fin:?} {k}");
                                bare += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 10_000 && raised > 1_000 && bare > 1_000, "{checked} checked, {raised} raised, {bare} bare");
    }

    /// The count-guarded form's case: past 2^63 the value is int4_sum's wrapped accumulator. Rows `x` repeated `c` times:
    /// PostgreSQL adds `x OP k` per row into an int8 that wraps; the drain's sum(x) is the same wrapping total of x.
    #[test]
    fn the_value_wraps_as_postgres_accumulates() {
        let (add, sub) = (177, 181); // int4pl, int4mi
        for (fid, first, rows, k) in [(add, false, [(i32::MAX as i64 - 9, 1i64 << 40), (5, 3)], 9i64), (sub, false, [(i32::MIN as i64 + 9, 1 << 41), (-1, 7)], 9), (sub, true, [(-(i32::MAX as i64) + 2, 1 << 42), (0, 1)], 1)] {
            let minus = fid == sub;
            let per_row = |x: i64| if minus { if first { k - x } else { x - k } } else { x + k };
            let pg = rows.iter().fold(0i64, |acc, &(x, c)| acc.wrapping_add(per_row(x).wrapping_mul(c)));
            let s = rows.iter().fold(0i64, |acc, &(x, c)| acc.wrapping_add(x.wrapping_mul(c)));
            let n = rows.iter().map(|r| r.1).sum::<i64>();
            let (lo, hi) = (rows.iter().map(|r| r.0).min().unwrap(), rows.iter().map(|r| r.0).max().unwrap());
            let exact: i128 = rows.iter().map(|&(x, c)| i128::from(per_row(x)) * i128::from(c)).sum();
            assert!(exact > i128::from(i64::MAX) || exact < i128::from(i64::MIN), "the total leaves int8");
            assert_eq!(host(&[(fid, first, [Some(lo), Some(hi), Some(s), Some(n)], Some(k))]), (vec![Some(pg)], 0));
        }
    }

    /// AGA-19: a final's entry in the drain's area is read off its cell, the plain drain's (by aggno, a valid bit) and the
    /// keyed emit's (by its place in the group, a valid word) alike; any other cell is no entry, so `emit` refuses by name.
    #[test]
    fn a_final_is_named_by_its_entry_in_either_drain_area() {
        let cx = mcx::MemoryContext::new("test");
        let row = |cells: Vec<AggCell>| crate::frame::env::AggRow { rec: Value(0), cells: mcx::to_vec_in(&cells[..], cx.mcx()) };
        let plain = row((0..3).map(|a| AggCell::word(16 * a, CellValid::Bit { off: 16 * a + 8, bit: 0 })).collect());
        assert_eq!((0..3).map(|a| entry(&plain, a)).collect::<Vec<_>>(), [Some(0), Some(1), Some(2)]);
        // a keyed group's emit: aggnos 0, 1, 2 at its 2nd, 0th and 1st entries
        let keyed = row([1, 2, 0].map(|e| AggCell::word(16 * e, CellValid::NonZeroAt(16 * e + 8))).to_vec());
        assert_eq!((0..3).map(|a| entry(&keyed, a)).collect::<Vec<_>>(), [Some(1), Some(2), Some(0)]);
        let off = row(vec![AggCell::word(8, CellValid::NonZeroAt(16)), AggCell::word(16, CellValid::Bit { off: 24, bit: 1 }), AggCell::word(0, CellValid::Always), AggCell::count(0), AggCell::word(0, CellValid::NonZeroAt(16))]);
        assert!((0..6).all(|a| entry(&off, a).is_none()), "off the area: no entry");
    }

    #[test]
    fn nulls_take_the_else_and_the_first_raising_member_wins() {
        let int4 = |e: i64| (177, false, [Some(e), Some(e), Some(e), Some(2)], Some(1));
        // zero rows (min / max / sum NULL, count 0), then a NULL literal, then a value
        let empty = (177, false, [None, None, None, Some(0)], Some(1));
        let null_k = (180, true, [Some(1), Some(1), Some(1), Some(1)], None);
        assert_eq!(host(&[empty, null_k, int4(1)]), (vec![None, None, Some(3)], 0));
        for at in 0..3 {
            let mut cases = vec![int4(5); 3];
            cases[at] = int4(i32::MAX as i64);
            let (v, mask) = host(&cases);
            assert_eq!(mask, raise::bit(raise::INT4_RANGE) as i64, "member {at} overflows int4 first");
            assert!(v[..at].iter().all(|x| *x == Some(7)), "the members before it are written");
        }
        // an int2 edge and an int4 edge: whichever comes first in the target list raises
        let int2 = (176, false, [Some(i16::MAX as i64), Some(0), Some(0), Some(1)], Some(1));
        let int4_max = (177, false, [Some(0), Some(i32::MAX as i64), Some(0), Some(1)], Some(1));
        assert_eq!(host(&[int2, int4_max]).1, raise::bit(raise::INT2_RANGE) as i64);
        assert_eq!(host(&[int4_max, int2]).1, raise::bit(raise::INT4_RANGE) as i64);
        // the second edge's overflow after a first edge that fits; k - x at the low edge
        assert_eq!(host(&[int4_max]).1, raise::bit(raise::INT4_RANGE) as i64);
        let k_minus = (181, true, [Some(i32::MIN as i64 + 1), Some(0), Some(0), Some(1)], Some(2));
        assert_eq!(host(&[k_minus]).1, raise::bit(raise::INT4_RANGE) as i64);
        assert_eq!(Straight(HashMap::new()).member(&k_minus), Err(raise::bit(raise::INT4_RANGE) as u64));
    }
}
