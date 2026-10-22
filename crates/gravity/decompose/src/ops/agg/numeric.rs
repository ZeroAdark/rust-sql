//! Numeric scale: the fixed scale and digit bound of a numeric aggregate input (split out of `ops/agg/mod.rs`, v5 agg A2; bound for typesem).
use super::*;

// fmgroids.h's names: the numeric operators' procedures whose scale and digit arithmetic this file implements, and
// numeric's typmod cast (the catalog home's names)
use crate::catalog::numeric::{negation, F_ABS_NUMERIC, F_NUMERIC_ABS, F_NUMERIC_ADD, F_NUMERIC_MUL, F_NUMERIC_NUMERIC_INT4, F_NUMERIC_SUB, F_ROUND_NUMERIC_INT4, F_TRUNC_NUMERIC_INT4};
use crate::ops::shared::agg::{NUM_DIV_ROAD, NUM_SCALE_ROAD};

/// The fixed scale of a numeric expression over typmod-scaled columns and constants (the mantissa arithmetic of
/// numeric's typmod scales: a product adds the scales, a sum / difference takes the larger), with its digit bound; `None`
/// when a leaf is unscaled.
pub fn num_scale<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> Option<(u32, Digits)> {
    let mut d = Digits::START;
    form_at(b, At::of(child), e, true, &mut d).map(|f| (f.0, d))
}

/// D297: the packed register's digit bound over a numeric argument, collected over every node by `form_at`: the most
/// digits (scale + integer) of any node, an int8 counting 19, and whether lowerv4 reads it as a `num.window` tree (no
/// numeric(p,s) cast, no simple CASE, no Var over 18 digits) rather than as one i64 mantissa.
#[derive(Clone, Copy, Debug)]
pub struct Digits {
    pub top: u32,
    pub tree: bool,
    /// PS4-13: a node only its datum gives (NULLIF, abs, round / trunc, GREATEST / LEAST): packed within one word, past
    /// it the Var register as before (agg mod.rs: no spill; the spilled datum's i256 fold ran 1.085x the Var fold at W0).
    pub datum: bool,
}

impl Digits {
    const START: Digits = Digits { top: 0, tree: true, datum: false };

    /// A packed sum / avg: every node within an i128's 38 digits less one reserved (a tree), else one i64 mantissa's 18.
    /// No row headroom: the register's overflow raises at run time.
    pub fn packed(self) -> bool {
        self.top <= if self.tree { 37 } else { 18 }
    }

    /// A packed min / max keeps its value in one i64 mantissa.
    pub fn word(self) -> bool {
        self.top <= 18
    }

    pub(super) fn node(&mut self, s: u32, ib: u32) {
        self.top = self.top.max(s + ib);
    }
}

/// V4R-38 (OID-04: any `+` / `-` / `*` tree, not one product): the scale of a sum / avg argument past the packed
/// register's 37 digits whose every node lowerv4's class.num_wide evaluates exactly in registers (`wide_at`), at most 55
/// digits in all: 10^55 < 2^183, so 2^63 terms stay under 2^246 in the i256 slot. `None` for any other shape (its datum
/// folds instead, `agg.num_spill`) or a scale past `MAX_NUM_SCALE`.
pub fn wide_tree<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> Option<u32> {
    let mut d = Digits::START;
    let f = wide_at(b, At::of(child), e, true, &mut d)?;
    Some(f.0).filter(|&sc| d.top <= WIDE_DIGITS && sc <= MAX_NUM_SCALE)
}

/// The most digits of a class.num_wide term (`wide_tree`).
const WIDE_DIGITS: u32 = 55;

/// `form_at` of a wide node: a product of a packed tree (`Digits::packed`) and a one-word one (at most 18 digits, a tree),
/// past 37 digits: one packed term times one word, its exact 192 bits; or a sum / difference whose two sides are each a
/// wide node or a packed tree, the larger scale and a carry digit (the sides rescaled to it, then added in 256 bits);
/// `-x` of a wide node as `0 - x`. `d` collects every node's digits (`Digits::node`). `None` for any other node.
fn wide_at<'p>(b: &Builder<'p, '_>, child: At<'_, 'p>, e: Node<'p>, agg: bool, d: &mut Digits) -> Option<(u32, u32, u32)> {
    crate::walk::stack_check(); // D3748
    let (child, e, agg) = resolved(b, child, e, agg);
    if let Some(op) = e.as_op_expr().filter(|o| negation(o.opfuncid) && o.args.len() == 1) {
        // -x of a wide node: lowerv4's 0 - x, a carry digit more
        let f = wide_at(b, child, op.args.nth(0), agg, d)?;
        d.node(f.0, f.2 + 1);
        return Some((f.0, f.1 + 1, f.2 + 1));
    }
    let op = e.as_op_expr().filter(|o| o.args.len() == 2)?;
    let (x, y) = (op.args.nth(0), op.args.nth(1));
    // a packed tree under a wide node: its own digit bound, a tree (lowerv4 num_tree's strict `tree`)
    let packed = |e: Node<'p>, d: &mut Digits| {
        let mut p = Digits::START;
        let f = form_at(b, child, e, agg, &mut p).filter(|_| p.tree && p.packed())?;
        d.top = d.top.max(p.top);
        Some((f, p))
    };
    let f = match op.opfuncid {
        F_NUMERIC_MUL => {
            let ((fx, dx), (fy, dy)) = (packed(x, d)?, packed(y, d)?);
            if dx.top > 18 && dy.top > 18 {
                return None;
            }
            (fx.0 + fy.0, fx.1 + fy.1, fx.2 + fy.2)
        }
        F_NUMERIC_ADD | F_NUMERIC_SUB => {
            let side = |e: Node<'p>, d: &mut Digits| wide_at(b, child, e, agg, d).or_else(|| packed(e, d).map(|p| p.0));
            let (fx, fy) = (side(x, d)?, side(y, d)?);
            (fx.0.max(fy.0), fx.1.max(fy.1) + 1, fx.2.max(fy.2) + 1)
        }
        _ => return None,
    };
    d.node(f.0, f.2);
    Some(f).filter(|f| f.0 + f.2 > 37)
}

/// `(scale, integer digits)` of a numeric expression (a product adds both, a sum takes the larger and a carry): D197's word bound.
pub(super) fn num_form_at<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>, agg: bool) -> Option<(u32, u32)> {
    form_at(b, At::of(child), e, agg, &mut Digits::START).map(|f| (f.0, f.1))
}

/// `num_form_at`'s `(scale, integer digits)` plus the node's integer digits for `d`'s bound (an int8 is 19 there; the
/// form keeps 18, which the key forms read: an int8 is exact in one mantissa word).
pub(super) fn form_at<'p>(b: &Builder<'p, '_>, child: At<'_, 'p>, e: Node<'p>, agg: bool, d: &mut Digits) -> Option<(u32, u32, u32)> {
    crate::walk::stack_check(); // D3748
    let (child, e, agg) = resolved(b, child, e, agg);
    let int = |t: u32| types_core::cast::int_digits(t).map(|d| (0, d.min(18), d));
    let f = match e.node_tag() {
        NodeTag::T_Var => {
            let v = e.as_var()?;
            // D2700: numeric or a domain over it (typefacts keys a domain as its base); a domain column's facts carry the
            // domain's typmod
            let numeric = |t: u32| crate::catalog::typefacts::facts_of(b.facts, t).is_some_and(|f| f.image.is_numeric());
            match v.vartype {
                t if numeric(t) => {
                    let form: core::cell::Cell<Option<Option<(u32, u32)>>> = core::cell::Cell::new(None);
                    let put = |f: Option<(u32, u32)>| form.set(Some(if form.get().is_none_or(|x| x == f) { f } else { None }));
                    let mut tree = true;
                    let seen = origin(b, child.pipe, child.proj, e, 8, &mut |relid, attno| {
                        put(b.facts.att(relid, attno).and_then(|a| {
                            let f = a.num_facts();
                            let s = u32::try_from(f.scale().filter(|&s| s >= 0)?).ok()?;
                            Some((s, (f.precision as u32).saturating_sub(s)))
                        }));
                    }, &mut |p, j, x| {
                        // D3062: a column an earlier projection computes (an aggregate input precomputed into a column of a
                        // grouping-sets spool's run or of form T's fanned-out walk): the expression's own form, its value's
                        // dscale, where the column's type alone (a product's typmod -1) has none
                        let mut dd = Digits::START;
                        let f = form_at(b, At { pipe: p, proj: j }, x, false, &mut dd);
                        tree &= dd.tree && !dd.datum;
                        put(f.map(|f| (f.0, f.1)));
                        f.is_some()
                    });
                    let (s, i) = form.get().filter(|_| seen && tree).flatten()?;
                    d.tree &= s + i <= 18;
                    Some((s, i, i))
                }
                t => int(t),
            }
        }
        NodeTag::T_Const => {
            let c = e.as_const()?;
            match c.consttype {
                crate::catalog::NUMERICOID => const_num_form(expr::const_bytes(c)?).map(|(s, i)| (s, i, i)),
                t => int(t),
            }
        }
        NodeTag::T_OpExpr => {
            let op = e.as_op_expr()?;
            // D830: a negated numeric (numeric_uminus) is its argument's form (lowerv4's tree: 0 - x)
            if op.args.len() == 1 && negation(op.opfuncid) {
                let f = form_at(b, child, op.args.nth(0), agg, d);
                if f.is_some() {
                    fires::fire("agg.num_neg"); // shapecheck fire signal (REGISTRY OID-05): negated numeric keeps its form
                }
                return f;
            }
            if op.args.len() != 2 {
                return None;
            }
            if crate::catalog::numeric::sum_or_difference(op.opfuncid) {
                // D4567: one side may be a CASE / COALESCE / GREATEST / LEAST whose arms differ in scale when the other
                // side's fixed scale is at least every arm's: PG's sum / difference takes the larger dscale of its two
                // values (add_var), so every row's is that side's (e.g. `x - COALESCE(y, 0.0)` over numeric(7,2)); its
                // road is on by default (off when the option names `agg.num_scale` plainly)
                let mixed = b.facts.default_on_road_on(NUM_SCALE_ROAD);
                let side = |e: Node<'p>, d: &mut Digits| form_at(b, child, e, agg, d).map(|f| (f, false)).or_else(|| mixed.then(|| mixed_at(b, child, e, agg, d)).flatten().map(|f| (f, true)));
                let ((a, ma), (c, mc)) = (side(op.args.nth(0), d)?, side(op.args.nth(1), d)?);
                let fixed = match (ma, mc) {
                    (true, true) => false,
                    (true, false) => c.0 >= a.0,
                    (false, true) => a.0 >= c.0,
                    (false, false) => true,
                };
                fixed.then(|| (a.0.max(c.0), a.1.max(c.1) + 1, a.2.max(c.2) + 1))
            } else {
                let (a, c) = (form_at(b, child, op.args.nth(0), agg, d)?, form_at(b, child, op.args.nth(1), agg, d)?);
                match op.opfuncid {
                    F_NUMERIC_MUL => Some((a.0 + c.0, a.1 + c.1, a.2 + c.2)),
                    _ => None,
                }
            }
        }
        NodeTag::T_FuncExpr => {
            let f = e.as_func_expr()?;
            match f.funcid {
                F_NUMERIC_NUMERIC_INT4 if f.args.len() == 2 => {
                    let t = f.args.nth(1).as_const().filter(|c| !c.constisnull && c.constbyval)?.constvalue.as_i32();
                    let (p, s) = ((((t - 4) >> 16) & 0xffff) as u32, ((t - 4) & 0xffff) as u32);
                    d.tree = false;
                    (t >= 4).then(|| (s, p.saturating_sub(s), p.saturating_sub(s)))
                }
                // PS4-13 (none a num.window tree: the datum folds): abs keeps its argument's dscale and digits (an int8's
                // 19: abs(-2^63) passes one word); round / trunc (x, s) take dscale max(s, 0) (round_var / trunc_var), a
                // round that drops a digit of x a carry digit
                F_NUMERIC_ABS | F_ABS_NUMERIC if f.args.len() == 1 => {
                    let x = form_at(b, child, f.args.nth(0), agg, d)?;
                    (d.tree, d.datum) = (false, true);
                    Some((x.0, x.2, x.2))
                }
                F_ROUND_NUMERIC_INT4 | F_TRUNC_NUMERIC_INT4 if f.args.len() == 2 => {
                    let s = f.args.nth(1).as_const().filter(|c| !c.constisnull && c.constbyval)?.constvalue.as_i32();
                    let x = form_at(b, child, f.args.nth(0), agg, d)?;
                    (d.tree, d.datum) = (false, true);
                    let carry = (crate::catalog::numeric::rounds(f.funcid) && s < x.0 as i32) as u32;
                    u32::try_from(s.max(0)).ok().filter(|&s| s <= MAX_NUM_SCALE).map(|s| (s, x.1 + carry, x.2 + carry))
                }
                id => types_core::cast::exact_to_numeric(id).and_then(int),
            }
        }
        // D256: a CASE has a fixed scale only when every non-NULL result arm has the same one (its value's dscale is the
        // arm's); arms of different scales are `case_scales`' (a sum tracks the dscale), never a fixed form. PS4-17: a
        // COALESCE is the CASE `WHEN a1 IS NOT NULL THEN a1 .. ELSE an` (its value one argument's, that dscale its own)
        // PS4-13: GREATEST / LEAST is one of its arguments with that one's dscale (ExecEvalMinMax), as a CASE's arm; NULLIF(x,
        // y) is x or NULL, x's form
        NodeTag::T_NullIfExpr => {
            let x = e.as_null_if_expr()?.args.iter().next()?;
            (d.tree, d.datum) = (false, true);
            return form_at(b, child, x, agg, d);
        }
        NodeTag::T_CaseExpr | NodeTag::T_CoalesceExpr | NodeTag::T_MinMaxExpr => {
            let arms = case_arms(b, child, e, agg, d)?;
            let mut forms = arms.iter().flatten();
            let first = *forms.next()?;
            let max = |k: fn(&(u32, u32, u32)) -> u32| arms.iter().flatten().map(k).max().unwrap_or(0);
            forms.all(|f| f.0 == first.0).then(|| (first.0, max(|f| f.1), max(|f| f.2)))
        }
        _ => None,
    }?;
    d.node(f.0, f.2);
    Some(f)
}

/// D3062: where an expression's columns are read: pipeline `pipe`'s `proj`-th projection (an Agg-level `OUTER_VAR` names an
/// entry of it; a Var inside that entry an earlier column, `origin`). `At::of` is a child's last projection, the Agg's input.
#[derive(Clone, Copy)]
pub(super) struct At<'a, 'p> {
    pipe: &'a Open<'p>,
    proj: usize,
}

impl<'a, 'p> At<'a, 'p> {
    pub(super) fn of(child: &'a Open<'p>) -> At<'a, 'p> {
        At { pipe: child, proj: projections(child).saturating_sub(1) }
    }
}

/// An Agg-level `OUTER_VAR` read as the child's projection entry it names (`agg` false from there on). D3062: an entry that
/// only passes an earlier column on is read as the one expression that computes it (`defining`), at that expression's
/// place, so a sum over a precomputed CASE / COALESCE reads the arms (`case_scales`) as over the expression itself.
pub(super) fn resolved<'a, 'p>(b: &'a Builder<'p, '_>, child: At<'a, 'p>, e: Node<'p>, agg: bool) -> (At<'a, 'p>, Node<'p>, bool) {
    match relabels(e).as_var().filter(|v| agg && v.varno == OUTER_VAR && v.varlevelsup == 0).and_then(|v| entry(projection(child.pipe, child.proj)?, v.varattno)) {
        Some(o) => match defining(b, child, o) {
            Some((at, x)) => (at, relabels(x), false),
            None => (child, relabels(o), false),
        },
        None => (child, relabels(e), agg),
    }
}

/// D3062: the one expression a column of `at` (`e`, a Var passing an earlier column on) is computed by, with its place:
/// through projections, a breaker's producer and a union's arms (a NULL arm neutral), every path reaching that same node and
/// none a scan column. `None` otherwise (a scan column, two expressions, no path).
fn defining<'a, 'p>(b: &'a Builder<'p, '_>, at: At<'a, 'p>, e: Node<'p>) -> Option<(At<'a, 'p>, Node<'p>)> {
    if relabels(e).as_var().is_none_or(|v| v.varno != OUTER_VAR) {
        return None;
    }
    let scan = core::cell::Cell::new(false);
    let mut found: Option<(At<'a, 'p>, Node<'p>)> = None;
    let mut same = true;
    let seen = origin(b, at.pipe, at.proj, e, 8, &mut |_, _| scan.set(true), &mut |p, j, x| {
        match found {
            Some((_, y)) if !y.ptr_eq(x) => same = false,
            Some(_) => {}
            None => found = Some((At { pipe: p, proj: j }, x)),
        }
        true
    });
    found.filter(|_| seen && same && !scan.get())
}

/// Each result arm's `form_at` of a CASE (the WHENs', then the ELSE) and their count, `None` for a NULL arm (a missing
/// ELSE is the parser's NULL constant), one per arm, however many; `None` when an arm has no fixed form. A simple CASE
/// (`CASE x WHEN ..`) is no `num.window` tree. A COALESCE's arms are its arguments; a GREATEST / LEAST's too (no tree).
pub(super) fn case_arms<'p>(b: &Builder<'p, '_>, child: At<'_, 'p>, e: Node<'p>, agg: bool, d: &mut Digits) -> Option<&'p [Option<(u32, u32, u32)>]> {
    let (c, co, mm) = (e.as_case_expr(), e.as_coalesce_expr(), e.as_min_max_expr());
    let list = c.map(|c| &c.args).or(co.map(|c| &c.args)).or(mm.map(|m| &m.args))?;
    d.tree &= c.is_none_or(|c| c.arg.is_none()) && mm.is_none();
    d.datum |= mm.is_some();
    let mut arms = ArenaVec::new_in(b.arena); // alloc: Cold
    for r in list.iter().map(|w| if c.is_some() { w.as_case_when().and_then(|w| w.result) } else { Some(w) }).chain(c.map(|c| c.defresult)) {
        let form = match r.map(relabels) {
            Some(r) if !r.as_const().is_some_and(|k| k.constisnull) => Some(form_at(b, child, r, agg, d)?),
            _ => None,
        };
        arms.push(form);
    }
    if co.is_some() {
        fires::fire("agg.num_coalesce"); // shapecheck fire signal (REGISTRY PS4-17): a COALESCE read as its CASE
    }
    Some(arms.leak())
}

/// D4567: the largest arm form of a CASE / COALESCE / GREATEST / LEAST whose non-NULL arms all have fixed forms (their
/// scales differing: the node has none of its own, `form_at`): the largest scale, integer digits and node digits. `None`
/// for any other node or an arm without one.
fn mixed_at<'p>(b: &Builder<'p, '_>, child: At<'_, 'p>, e: Node<'p>, agg: bool, d: &mut Digits) -> Option<(u32, u32, u32)> {
    let (child, e, agg) = resolved(b, child, e, agg);
    if !matches!(e.node_tag(), NodeTag::T_CaseExpr | NodeTag::T_CoalesceExpr | NodeTag::T_MinMaxExpr) {
        return None;
    }
    let arms = case_arms(b, child, e, agg, d)?;
    let max = |k: fn(&(u32, u32, u32)) -> u32| arms.iter().flatten().map(k).max();
    Some((max(|f| f.0)?, max(|f| f.1)?, max(|f| f.2)?))
}

/// D4567 (a): a sum / avg argument `x / k`, x of a fixed form (`form_at`, a num.window tree) whose mantissa fits one word
/// and k a positive constant whose reciprocal is a finite decimal (catalog numeric `exact_reciprocal`: 1 / k = c / 10^e):
/// numeric_div's quotient is x x c / 10^e exactly, at scale s + e, while its display scale is select_div_scale's, which
/// reads x's first base-10000 digit and weight (catalog numeric `div_scales`), so it varies by row (e.g.
/// `sum(((a - b - c) + d) / 2)` prints 0.00000000000000000000 for a zero row and 617.2800000000000000 for 1234.56).
/// Every row's display scale is at least s + e within x's digit bound (else PostgreSQL would round the quotient there and
/// the argument stays hosted), so the register sums exactly at s + e and its dscale word keeps the largest display scale
/// (D256's, past the register's scale here: the final pads with zeros). Returns the register's scale, the display scales
/// (`div_arms`) and the digit bound. `None` for any other argument, or road `agg.num_div` off (its default).
pub(super) fn div_form<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> Option<(u32, &'p [u8], Digits)> {
    if !b.facts.road_on(NUM_DIV_ROAD) {
        return None;
    }
    let (child, e, agg) = resolved(b, At::of(child), e, true);
    let op = e.as_op_expr().filter(|o| crate::catalog::numeric::is_div(o.opfuncid) && o.args.len() == 2)?;
    let (km, ks) = div_const(op.args.nth(1))?;
    let mut digits = Digits::START;
    let x = form_at(b, child, op.args.nth(0), agg, &mut digits).filter(|_| digits.tree && !digits.datum && digits.word())?;
    let (c, e) = crate::catalog::numeric::exact_reciprocal(km, ks)?;
    let d = crate::catalog::numeric::div_scales(x.0, x.0 + x.2, km, ks)?;
    let at = x.0 + e;
    let arms = div_arms(b, &d)?;
    if at > MAX_NUM_SCALE || d.min() < at {
        return None;
    }
    // the term x x c: x's mantissa digits and c's, at scale s + e
    let top = x.0 + x.2 + c.unsigned_abs().checked_ilog10().unwrap_or(0) + 1;
    digits.node(at, top.saturating_sub(at));
    digits.word().then_some((at, arms, digits))
}

/// D4567 (a): `div_form`'s display scales as the register's dscales (x = 0's, then each step's, in order), as lowerv4's
/// num_tree.rs `Div` node lists them; `None` past a byte.
fn div_arms<'p>(b: &Builder<'p, '_>, d: &crate::catalog::numeric::DivScales) -> Option<&'p [u8]> {
    let mut out = ArenaVec::new_in(b.arena); // alloc: Cold
    for s in core::iter::once(d.zero).chain(d.steps().iter().map(|s| s.1)) {
        out.push(u8::try_from(s).ok().filter(|&s| s != NULL_ARM)?);
    }
    Some(out.leak())
}

/// D4567 (a): a positive numeric divisor constant's mantissa and display scale (its image). `None` for any other node,
/// NULL, NaN, an infinity, zero, a negative or a mantissa past one word.
pub fn div_const(e: Node<'_>) -> Option<(i64, u32)> {
    // a numeric Const only: the planner folds an integer literal's cast to numeric into one (its image is the bake log's)
    let c = relabels(e).as_const().filter(|c| !c.constisnull && c.consttype == crate::catalog::NUMERICOID)?;
    let (m, s) = const_mantissa(expr::const_bytes(c)?)?;
    (m > 0).then_some((m, s))
}

/// D4567 (a): a finite numeric datum's mantissa at its own display scale (value x 10^dscale) with that scale, when it fits
/// one i64; `None` for NaN / an infinity, a digit below the display scale (an image numeric.c never writes) or past i64.
pub(super) fn const_mantissa(data: &[u8]) -> Option<(i64, u32)> {
    let s = const_num_scale(data)?;
    let h = u16::from_le_bytes([data[0], data[1]]);
    let (weight, first) = if h & 0xC000 == 0x8000 {
        let w = (h & 0x3F) as i32;
        (if h & 0x40 != 0 { w - 64 } else { w }, 2)
    } else {
        (i16::from_le_bytes([*data.get(2)?, *data.get(3)?]) as i32, 4)
    };
    let neg = if h & 0xC000 == 0x8000 { h & 0x2000 != 0 } else { h & 0xC000 == 0x4000 };
    let mut m: i128 = 0;
    for (i, d) in data.get(first..)?.chunks_exact(2).enumerate() {
        let d = u16::from_le_bytes([d[0], d[1]]) as i128;
        if d > 9999 {
            return None;
        }
        // the digit's place at the display scale: 4 x (weight - i) + s decimal places
        let e = 4 * (weight - i as i32) + s as i32;
        m = if e >= 0 {
            m.checked_add(d.checked_mul(10i128.checked_pow(e as u32)?)?)?
        } else {
            let p = 10i128.pow(e.unsigned_abs().min(4));
            if d % p != 0 {
                return None;
            }
            m.checked_add(d / p)?
        };
    }
    Some((i64::try_from(if neg { -m } else { m }).ok()?, s))
}

/// D256: a sum / avg argument that is a CASE (or a COALESCE, a GREATEST / LEAST) whose non-NULL result arms all have fixed forms of different scales: the
/// register's scale (the largest) and each arm's dscale (`AggDecl::dscales`). Numeric sum's result dscale is the largest
/// dscale of the values it added, so the register tracks the arm each row took (a group whose rows all took `ELSE 0`
/// prints `0`, not `0.0000`).
/// With the CASE's digit bound: each arm at the register's scale.
pub(super) fn case_scales<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> Option<(u32, &'p [u8], Digits)> {
    let (child, e, agg) = resolved(b, At::of(child), e, true);
    let mut digits = Digits::START;
    let arms = case_arms(b, child, e, agg, &mut digits)?;
    if arms.iter().flatten().any(|f| f.0 > MAX_NUM_SCALE) {
        return None;
    }
    let scale = arms.iter().flatten().map(|f| f.0).max()?;
    digits.node(scale, arms.iter().flatten().map(|f| f.2).max()?);
    let mut dscales = ArenaVec::new_in(b.arena); // alloc: Cold
    for f in arms {
        dscales.push(f.map_or(NULL_ARM, |f| f.0 as u8));
    }
    Some((scale, dscales.leak(), digits))
}

pub(super) fn projections(pipe: &Open<'_>) -> usize {
    pipe.ops.iter().filter(|op| matches!(op, Op::Project(_))).count()
}

pub(super) fn projection<'p>(pipe: &Open<'p>, i: usize) -> Option<&'p NodeList<'p>> {
    pipe.ops.iter().filter_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).nth(i)
}

pub(super) fn entry<'p>(tl: &'p NodeList<'p>, resno: i16) -> Option<Node<'p>> {
    // a column past the list (a window function's column over the window's input projection, V4R-C) has no entry
    let t = tl.nth((resno as usize).checked_sub(1).filter(|&n| n < tl.len())?).as_target_entry()?;
    (t.resno == resno).then_some(t.expr)
}

pub(super) fn producer<'a, 'p>(b: &'a Builder<'p, '_>, pipe: &Open<'p>, slot: u8) -> Option<&'a Open<'p>> {
    let life = crate::breakers::life_read(b, &pipe.desc, slot)?;
    b.graph.pipelines.get(life.0 as usize)
}

/// Every scan column an expression of `pipe`'s `proj`-th projection descends from (`each`), false when a path has none:
/// through earlier projections (`OUTER_VAR`), a breaker's producer, a nested inner, and past a union to every arm.
/// D3062: a path that ends at an expression an earlier projection computes calls `expr` with its pipeline, projection and
/// node (false: the path has no scan column, as before).
pub(super) fn origin<'a, 'p>(b: &'a Builder<'p, '_>, pipe: &'a Open<'p>, proj: usize, e: Node<'p>, depth: u8, each: &mut dyn FnMut(u32, i16), expr: &mut dyn FnMut(&'a Open<'p>, usize, Node<'p>) -> bool) -> bool {
    crate::walk::stack_check(); // D3748
    // a NULL arm (a grouping set's absent column) carries no scale: neutral, it visits nothing
    if relabels(e).as_const().is_some_and(|c| c.constisnull) {
        return true;
    }
    let Some(v) = relabels(e).as_var() else { return depth < 8 && expr(pipe, proj, e) };
    if depth == 0 || v.varlevelsup != 0 || v.varattno <= 0 {
        return false;
    }
    if let Some((rti, relid, _)) = pipe.source.relation() {
        if v.varno == rti as i32 {
            each(relid, v.varattno);
            return true;
        }
    }
    if let Source::Index { relid, indexid, only: true, .. } = pipe.source {
        if v.varno == INDEX_VAR {
            let Some(a) = b.facts.index(indexid).and_then(|i| i.keys().get(v.varattno as usize - 1).copied()) else { return false };
            each(relid, a);
            return true;
        }
    }
    let through = v.varno == OUTER_VAR || (v.varno > 0 && b.rte(v.varno as u32).is_some_and(|r| r.relid == 0));
    let at = pipe.ops.iter().enumerate().filter(|(_, op)| matches!(op, Op::Project(_))).nth(proj).map_or(pipe.ops.len(), |(i, _)| i);
    // D3086: a recursive union's rows (its generations' iteration, `SetDesc::Iter`) are its seed's and every term
    // generation's; the walk below reaches the seed only, so no column of them has one origin (a term `x * 2.25` changes
    // its scale each generation)
    if pipe.ops[..at].iter().any(|op| matches!(op, Op::SetOp(crate::ops::shared::setop::SetDesc::Iter(_)))) {
        return false;
    }
    let from = pipe.ops[..at].iter().rposition(|op| matches!(op, Op::Append(AppendDesc::Arm { .. }))).map_or(0, |m| m + 1);
    let ops = &pipe.ops[from..at];
    let join = ops.iter().filter_map(|op| if let Op::Join(j) = op { Some(j) } else { None }).last();
    let mut go = |p: &'a Open<'p>, j: Option<usize>| match j.and_then(|j| Some((j, entry(projection(p, j)?, v.varattno)?))) {
        Some((j, x)) => origin(b, p, j, x, depth - 1, each, expr),
        None => false,
    };
    let last = |p: &Open<'p>| projections(p).checked_sub(1);
    // a projection past the pipeline's last join (a Sort's or a Result's over the join's own) reads that join's output
    // columns: through it first, before a drain's outer side (D3062: a sum read under a Sort over a right-anti drain)
    let projected = ops.iter().rposition(|op| matches!(op, Op::Join(_))).is_some_and(|j| ops[j + 1..].iter().any(|op| matches!(op, Op::Project(_))));
    match (v.varno, join) {
        _ if through && projected => go(pipe, Some(proj - 1)),
        (_, Some(crate::join::JoinDesc::Drain { matches, .. })) if through => producer(b, pipe, *matches).is_some_and(|p| go(p, last(p))),
        (INNER_VAR, Some(crate::join::JoinDesc::Drain { .. })) => match pipe.source {
            Source::Breaker { slot } => producer(b, pipe, slot).is_some_and(|p| go(p, last(p))),
            _ => false,
        },
        _ if through && ops.iter().any(|op| matches!(op, Op::Project(_))) => go(pipe, Some(proj - 1)),
        _ if through && from > 0 => arm_rows(b, pipe, at).all(|(p, j)| go(p, j)),
        _ if through => match (pipe.union, &pipe.source) {
            (Some(u), _) => b.graph.unions[u as usize].arms.iter().all(|a| go(a, last(a))),
            (None, Source::Breaker { slot }) => feeders(b, pipe, *slot).all(|p| go(p, last(p))),
            _ => false,
        },
        (INNER_VAR, Some(crate::join::JoinDesc::Probe { slot, .. } | crate::join::JoinDesc::Merge { slot, .. })) => producer(b, pipe, *slot).is_some_and(|p| go(p, last(p))),
        (INNER_VAR, Some(crate::join::JoinDesc::Nest { inner, .. })) => go(&**inner, last(inner)),
        _ => false,
    }
}

/// The arms sharing `pipe`'s union tail (its projection at `at`, copied per arm): each arm and its projection before the marker.
pub(super) fn arm_rows<'a, 'p>(b: &'a Builder<'p, '_>, pipe: &'a Open<'p>, at: usize) -> impl Iterator<Item = (&'a Open<'p>, Option<usize>)> + 'a {
    let tl = match pipe.ops.get(at) {
        Some(Op::Project(l)) => Some(*l),
        _ => None,
    };
    b.graph.pipelines.iter().filter_map(move |q| {
        let aq = q.ops.iter().position(|op| matches!(op, Op::Project(l) if tl.is_some_and(|t| core::ptr::eq(*l, t))))?;
        let before = q.ops[..aq].iter().filter(|op| matches!(op, Op::Project(_))).count();
        Some((q, before.checked_sub(1)))
    }).chain(tl.is_none().then_some((pipe, None)))
}

/// The feeds of the settled union `pipe` reads from `slot` (the producer alone when the slot is not a union's). D4782: a
/// union or MergeAppend op names its children's count, and its feeds are the last that many declared on the slot: a
/// MergeAppend child's own Sort declares its run between two feeds, so they are no unbroken run of the slot's.
pub(super) fn feeders<'a, 'p>(b: &'a Builder<'p, '_>, pipe: &Open<'p>, slot: u8) -> impl Iterator<Item = &'a Open<'p>> + 'a {
    let life = crate::breakers::life_read(b, &pipe.desc, slot).map(|l| l.0 as usize);
    let feeds = move |p: usize| matches!(b.graph.pipelines[p].ops.last(), Some(Op::Append(AppendDesc::Feed { slot: s, .. })) if *s == slot);
    let fed = move |k: usize| b.graph.breakers[k].breaker.slot == slot && feeds(b.graph.breakers[k].producer as usize);
    let children = pipe.ops.iter().find_map(|op| match op {
        Op::Append(AppendDesc::Union { slot: s, children, .. } | AppendDesc::Merge { slot: s, children, .. }) if *s == slot => Some(*children as usize),
        _ => None,
    });
    let d = life.and_then(|l| b.graph.breakers.iter().rposition(|d| d.producer as usize == l && d.breaker.slot == slot));
    let lo = match (d, life, children) {
        (Some(d), Some(l), Some(n)) if feeds(l) => (0..=d).rev().filter(|&k| fed(k)).take(n).last().unwrap_or(d),
        (Some(d), Some(l), None) if feeds(l) => (0..d).rev().take_while(|&k| fed(k)).last().unwrap_or(d),
        _ => d.unwrap_or(usize::MAX),
    };
    let hi = d.map_or(0, |d| d + 1);
    (lo..hi).filter(move |&k| Some(k) == d || fed(k)).map(move |k| &b.graph.pipelines[b.graph.breakers[k].producer as usize])
}

/// The display scale in a numeric datum's header (`NUMERIC_SHORT`: bits 7–13; the long form: the low 14 bits; NaN / infinity: none).
pub(super) fn const_num_scale(data: &[u8]) -> Option<u32> {
    let h = u16::from_le_bytes([*data.first()?, *data.get(1)?]);
    match h & 0xC000 {
        0x8000 => Some(((h & 0x1F80) >> 7) as u32),
        0xC000 => None,
        _ => Some((h & 0x3FFF) as u32),
    }
}

/// `(display scale, integer digits)` of a numeric datum: `(weight + 1) × 4` from the header's weight (short: bits 0–6; long: word 2).
pub(super) fn const_num_form(data: &[u8]) -> Option<(u32, u32)> {
    let s = const_num_scale(data)?;
    let h = u16::from_le_bytes([data[0], data[1]]);
    let weight = if h & 0xC000 == 0x8000 {
        let w = (h & 0x3F) as i32;
        if h & 0x40 != 0 { w - 64 } else { w }
    } else {
        i16::from_le_bytes([*data.get(2)?, *data.get(3)?]) as i32
    };
    Some((s, ((weight + 1).max(0) * 4) as u32))
}
