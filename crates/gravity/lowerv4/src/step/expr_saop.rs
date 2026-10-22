//! Kind `expr.saop` (cards/M2.md L2): a ScalarArrayOpExpr (`x IN (…)`, `x op ANY / ALL (array)`) gravity_expr does not
//! serve itself. gravity_expr emits a constant array of at most `MAX_LIST` (16) elements as unrolled compares
//! (expr/src/emit/scalar.rs `scalar_array`); this extension claims the rest: a longer constant array and an array that
//! is not a constant (`= ANY($1)`, a column, an array expression). It evaluates them in the host `saop_eval` with PG's
//! two forms (execExprInterp.c):
//! - hashed (`ExecEvalHashedScalarArrayOp`), where PG's plan gave the node a `hashfuncid` (a constant array of 9 or more
//!   elements and a hashable operator): literal-free (V4R-102), the array never read here, so one program serves every
//!   list of the statement's shape; its elements' set is built once per execution at bind (V4R-122: exec admit.rs
//!   `WordTpl::Set`, step/raw/saop_set.rs `bound`) and bound as the site's word (`WordPlan::set`), keyed by the hash
//!   function and compared with the equality operator (`negfuncid` under NOT
//!   IN), or keyed by the integer value for the integer / date / time equalities, whose equality is the value's; a NULL
//!   operand is NULL, a miss over an array holding a NULL is NULL, else found (IN) / not found (NOT IN);
//! - linear (`ExecEvalScalarArrayOp`) for the rest: a NULL array is NULL, an empty one false (ANY) / true (ALL) whatever
//!   the operand, a NULL operand of the strict operator NULL, else the elements in order, true (false for ALL) at the
//!   first decided one, NULL when none decided and an element or a compare was NULL.
//! Calls go through the functions' shims (gravity_fn: pure over their arguments, so a pool worker runs them) with the
//! node's collation; an operator without a shim (a function-manager call) is not claimed: gravity_expr walks a non-constant
//! array itself (expr/src/emit/host.rs `saop_walk`, the site's cell per element), a constant of more than 16 stays "a long list". A
//! constant array PG did not hash (no hash function), of more than 16 elements, is an operand like any array (V4R-110):
//! its word bound per execution, walked in the linear form, never read here but for its count (konst.rs `list_len`, the
//! class the keys hash), so every list of the statement's shape is one program. The host half (the `Desc` the program
//! owns, frame/px.rs `owned`) is step/raw/saop_host.rs. A text `= ANY` of a constant array PG did not hash that road
//! text.eq claims (rep/varlena/cmp_text.rs `claims_any`) is declined here: kind rows are tried before road rows. Owner
//! M2-FRAME.
use std::rc::Rc;

use gravity_abi::status::raise;
use gravity_decompose::effects::{held_error, of_call, unobservable, Effects, Licence};
use gravity_program::cost::expr::{EXTREME_PRIM_MIN, MAX_LIST, TEXT_SET_MIN};
use gravity_expr::nodes::func::{ArgConv, How};
use gravity_expr::nodes::var::Var;
use gravity_expr::{Compiled, ExprNode, NodeId, Source as VarSource};
use gravity_fn::{ArgKind, Shim};
use gravity_ir::builder::FunctionBuilder;
use gravity_lower::Place;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, StateMask, Type, Value};
use types_nodes::{Node, NodeTag};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::kinds::KindId;
use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::Reason;

use crate::step::raw::saop_host::{fn2, native_keyed, saop_eval, saop_eval_keyed, Desc, Extreme, Hashed, FAILED, NULL, TRUE};
use crate::rep::varlena::text_set;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ExprSaop, &ExprExtImpl(&SAOP))], ..crate::ops::NONE };

struct Saop;
static SAOP: Saop = Saop;

/// A claimed site's array: an operand (`sub`), or a hashed constant's set word (V4R-122; `None`: the words are full, the
/// statement refused at install) with the licence its build at bind holds (construct C2, `desc_of`): `None` for a
/// user key's set, built at its first probe (PostgreSQL's point).
#[derive(Clone, Copy)]
enum Arr {
    Sub(u32),
    Set(Option<u16>, Option<Licence>),
}

/// One claimed site: its operand's `sub`, its array, its `Desc`'s index in `Px::owned` and its fingerprint (D2248).
struct SitePlan {
    lhs: u32,
    arr: Arr,
    desc: usize,
    fp: u64,
    /// D4197: a user key's hashed site (`Desc::user`): its set and operand are classed in the run's classes, through
    /// `saop_eval_keyed` and the program's `keys.user` handle.
    user: bool,
    /// Road text.set (rep/varlena/text_set.rs): a byte set probed in line, its operand `lhs` the column of a view.
    set: Option<SetSite>,
}

/// Road text.set's form of a byte set site: the operand's view (`lhs` its column), the keyed element count (`None`: past
/// the keyed class), `= ANY` (else `<> ALL`).
#[derive(Clone, Copy)]
struct SetSite {
    view: Option<text_set::SetView>,
    n: Option<usize>,
    use_or: bool,
}

/// A shim of `funcid` a pool worker may call with these argument forms (strict, no resolved types, a by-value result).
fn strict_shim(px: &Px<'_, '_>, funcid: u32, nargs: u8) -> Option<&'static Shim> {
    let s = gravity_fn::shim_of(funcid)?;
    (s.nargs == nargs && s.tyargs.is_empty() && !s.region && px.gx.catalog.func(funcid).ok()?.strict).then_some(s)
}

/// The claim for plan node `n` (a ScalarArrayOpExpr) compiled at program node `id`: `None` leaves it to gravity_expr.
/// Construct C2: a set's build runs at bind, ahead of PostgreSQL's first evaluation, with the licence beside it: a
/// hashed set whose hash shim is called there holds `effects::held_error`'s (the shim's error is held to the first
/// probe, saop_set `HELD`; `strict_shim` admits built-in shims only, so no user code, and `of_call` reads its acts;
/// denied, the site keeps the linear form), a set of native keys, a byte set or an extreme calls nothing at bind
/// (`effects::unobservable`), a user key's set none (built at its first probe).
fn desc_of(px: &Px<'_, '_>, c: &Compiled<'_>, id: usize, n: Node<'_>) -> Option<(Desc, Option<Licence>)> {
    let s = n.as_scalar_array_op_expr()?;
    let ExprNode::ScalarArrayOp(op) = &c.prog.nodes[id] else { return None };
    let call = &c.prog.sites[op.site.0 as usize];
    if !call.strict || call.nargs != 2 {
        return None;
    }
    let lhs_t = gravity_decompose::catalog::expr_type::result_type(s.args.nth(0));
    let lens = [crate::rep::storage::of(px.gx.catalog, lhs_t).ok()?.len, op.elem.len];
    let arr = &c.prog.nodes[c.prog.kids[call.args.start as usize + 1].0 as usize];
    let How::Shim { shim, .. } = call.how else { return user_set(s, op, call, lhs_t, lens, arr).map(|d| (d, None)) };
    // LIST-FORM: a constant text list under a byte-equal collation is a byte set, PG's hashed one or not
    if let Some(trim) = list_form(&c.prog, c.root, NodeId(id as u32)) {
        let hashed = Some(Hashed { eq: None, hash: None, bytes: true, images: false, extreme: None, trim });
        return Some((Desc { use_or: op.use_or, coll: call.collation, op: Some(fn2(shim, lens)?), elem: op.elem, lens, hashed, user: 0, image_eq: 0, fp: 0 }.sealed(), unobservable(Effects::NONE)));
    }
    // LIST-EXT: an ordering operator's constant list is its extreme element, found once a walk's worth of probes is spent
    if let Some(extreme) = extreme_of(px, c, id, n) {
        let hashed = Some(Hashed { eq: None, hash: None, bytes: false, images: false, extreme: Some(extreme), trim: false });
        return Some((Desc { use_or: op.use_or, coll: call.collation, op: Some(fn2(shim, lens)?), elem: op.elem, lens, hashed, user: 0, image_eq: 0, fp: 0 }.sealed(), unobservable(Effects::NONE)));
    }
    // PG hashed it: the literal-free set over the array's word, never read here (V4R-102); a hashable node whose shims
    // are missing takes the linear form over the word, as PG's would
    let hashed = (s.hashfuncid != 0 && matches!(arr, ExprNode::Const(_))).then(|| {
        let eq_id = if s.useOr { call.funcid } else { s.negfuncid };
        let eq = Some(fn2(strict_shim(px, eq_id, 2)?, lens)?);
        let hash = if native_keyed(eq_id, op.elem.byval, lens) {
            None
        } else {
            let h = strict_shim(px, s.hashfuncid, 1)?;
            let conv = |l: i16| ArgConv { kind: h.args[0], typlen: if matches!(h.args[0], ArgKind::V | ArgKind::D) { 0 } else { l } };
            Some((h, [conv(lens[0]), conv(lens[1])]))
        };
        let licence = if hash.is_some() { held_error(of_call(px.gx.facts, s.hashfuncid), false, None).ok()? } else { unobservable(Effects::NONE)? };
        Some((Hashed { eq, hash, bytes: false, images: false, extreme: None, trim: false }, licence))
    });
    let (hashed, licence) = hashed.flatten().unzip();
    // the type's own `=` (or its negator `<>`) over equal stored bytes, walked linearly: its images are compared, not its
    // shim called
    let own_image = || gravity_fn::typesem::expr::image_eq_len(lhs_t, call.funcid).unwrap_or(0);
    let image_eq = if hashed.is_none() && lhs_t == op.elem.typ && !op.elem.byval && op.elem.len > 0 { own_image() } else { 0 };
    let image_eq = if image_eq.abs() == op.elem.len { image_eq } else { 0 };
    Some((Desc { use_or: op.use_or, coll: call.collation, op: Some(fn2(shim, lens)?), elem: op.elem, lens, hashed, user: 0, image_eq, fp: 0 }.sealed(), licence))
}

/// D2125 (KeySem slice 5's piece, main 2026-10-04): a constant array PG hashed (`hashfuncid`) under an operator with no
/// shim, over a user key (typesem `user_key`: a user DEFAULT hash class) whose equality is the node's (its `negfuncid`
/// under NOT IN) and whose hash proc is the node's `hashfuncid`, under the key's collation: its set keys by the elements'
/// class numbers (`Desc::user`), each element and each row's operand classed through the type's hash proc and then its
/// equality (kernels userfp `class_number`), as PG's `ExecEvalHashedScalarArrayOp` calls them: the hash per element and
/// per row, the equality only on a hash match. typesem decides it (`hashed_user_key`, which also sends the pipeline to
/// the leader: the procs are user code that reaches SPI). Any other shim-less node stays gravity_expr's (its unrolled
/// compares).
fn user_set(s: &types_nodes::primnodes::ScalarArrayOpExpr<'_>, op: &gravity_expr::nodes::op::ScalarArrayOp, call: &gravity_expr::nodes::func::Call, lhs_t: u32, lens: [i16; 2], arr: &ExprNode<'_>) -> Option<Desc> {
    if s.hashfuncid == 0 || !matches!(arr, ExprNode::Const(_)) || lhs_t != op.elem.typ {
        return None;
    }
    let eq_proc = if op.use_or { call.funcid } else { s.negfuncid };
    let spec = gravity_fn::typesem::keysem::user::hashed_user_key(lhs_t, call.collation, eq_proc, s.hashfuncid).ok().flatten()?;
    let hashed = Some(Hashed { eq: None, hash: None, bytes: false, images: false, extreme: None, trim: false });
    Some(Desc { use_or: op.use_or, coll: call.collation, op: None, elem: op.elem, lens, hashed, user: spec, image_eq: 0, fp: 0 }.sealed())
}

/// LIST-FORM (CAP-10 / STEP-14): whether the `= ANY` / `<> ALL` at `id` is a byte set's: texteq under ANY or textne
/// under ALL (PG's two senses of an IN list), a varlena element, a collation whose equality is byte equality (typesem
/// `coll_bytes`; none, two conflicting ones, stays the operator's call, which raises 42P22) and a constant of at least
/// `text_set_min()` elements, its count keyed (konst.rs `list_class`), its values never read here, compared with a column's
/// stored text (a Var, through relabels and COLLATE: the bytes the probe's header guard reads); a computed operand keeps the
/// form its producer's road answers (jsonb.inplace's `->> 'k' IN (..)` decoded once per call, a call's result unrolled).
pub(crate) fn bytes_list(p: &gravity_expr::Program<'_>, root: NodeId, id: NodeId) -> bool {
    list_form(p, root, id).is_some()
}

/// `bytes_list`'s answer with its form: `Some(false)` a text list, `Some(true)` (D4794, road text.set.bpchar, default off)
/// bpchar's: bpchareq under ANY or bpcharne under ALL over a column's stored value, its elements and operand compared
/// without their trailing blanks (bpchareq's bcTruelen on both sides; under a collation whose equality is byte equality
/// what is left compares byte for byte, as bpchareq's memcmp). The road off, a bpchar list keeps gravity_expr's compares.
pub(crate) fn list_form(p: &gravity_expr::Program<'_>, root: NodeId, id: NodeId) -> Option<bool> {
    let ExprNode::ScalarArrayOp(s) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[s.site.0 as usize];
    let ExprNode::Const(k) = &p.nodes[p.kids[call.args.start as usize + 1].0 as usize] else { return None };
    let lhs = p.kids[call.args.start as usize];
    let name = gravity_fn::row_of(call.funcid).map(|r| r.name);
    // D4794: bpchar's `=` / `<>` by catalog name over the column itself (no view: a view's text is text's), in an expression
    // whose comparisons road text.eq does not claim (cmp_text.rs `claims_some`: it would take them all hosted)
    let bpchar = matches!((name, s.use_or), (Some("bpchareq"), true) | (Some("bpcharne"), false)) && stored(p, lhs) && text_set::bpchar_on()
        && !crate::rep::varlena::cmp_text::claims_some(p, root);
    // road text.set: or a view of one (text_set.rs `is_view`: substring / rtrim1 of a column), read in place
    if !bpchar && !stored(p, lhs) && !(text_set::on() && text_set::is_view(p, lhs)) {
        return None;
    }
    // texteq / textne by catalog name: their inputs are text's bytes whatever the array's element type (varchar's, a domain's)
    let sense = bpchar || matches!((name, s.use_or), (Some("texteq"), true) | (Some("textne"), false));
    (sense && call.nargs == 2 && call.strict && s.elem.len == -1 && !s.elem.byval
        && call.collation != 0 && gravity_fn::typesem::claims_coll_bytes(call.collation)
        && k.list_count(s.elem).is_some_and(|n| n >= text_set_min()))
    .then_some(bpchar)
}

/// R5-B: the shortest text list the byte set answers, read from the class table's rows (`cost::forms::text_set_min`;
/// `TEXT_SET_MIN`, the rule of before, only for a table without them).
fn text_set_min() -> usize {
    gravity_program::cost::forms::text_set_min().unwrap_or(TEXT_SET_MIN)
}

/// LIST-EXT: the extreme form of plan node `n` (a ScalarArrayOpExpr) compiled at program node `id`: its operator is the `<`,
/// `<=`, `>=` or `>` member of a btree family (typesem `list_order`: built-in or user, the family's own order, so its
/// collation and NaN), strict, with a shim a pool worker may call, as is the family's `<` over the element type; the
/// array a constant (a parameter's keeps the walk) of at least two elements, its count keyed (konst.rs `list_count`), its
/// values never read here. A list gravity_expr compares in line (a prim operator, at most `MAX_LIST` elements) stays
/// gravity_expr's below `cost::forms::extreme_prim_min` (R5-B: the class table's rows by element class, judged at the leader
/// and the server's widest width, `Facts::width`; `EXTREME_PRIM_MIN`, the rule of before, only for a table without them).
fn extreme_of(px: &Px<'_, '_>, c: &Compiled<'_>, id: usize, n: Node<'_>) -> Option<Extreme> {
    let s = n.as_scalar_array_op_expr()?;
    let ExprNode::ScalarArrayOp(op) = &c.prog.nodes[id] else { return None };
    let call = &c.prog.sites[op.site.0 as usize];
    let ExprNode::Const(k) = &c.prog.nodes[c.prog.kids[call.args.start as usize + 1].0 as usize] else { return None };
    let count = k.list_count(op.elem)?;
    let prim = gravity_expr::emit::prim::is_prim(&c.prog, call, false);
    let min = || gravity_program::cost::forms::extreme_prim_min(op.elem.typ, op.elem.len, px.gx.facts.width().unwrap_or(0)).unwrap_or(EXTREME_PRIM_MIN);
    if !call.strict || call.nargs != 2 || count < 2 || (prim && count <= MAX_LIST && count < min()) {
        return None;
    }
    strict_shim(px, call.funcid, 2)?;
    let order = gravity_fn::typesem::expr::list_order(s.opno).ok()??;
    let less = fn2(strict_shim(px, order.less, 2)?, [op.elem.len, op.elem.len])?;
    Some(Extreme { less, greatest: order.greatest == op.use_or })
}

/// Node `id` reads a column's stored value: a Var of a user column (not a whole row, a system column or another claim's
/// placeholder), through RelabelTypes.
fn stored(p: &gravity_expr::Program<'_>, id: NodeId) -> bool {
    crate::stack_check();
    match &p.nodes[id.0 as usize] {
        ExprNode::Var(v) => (1..EXT_AT).contains(&v.attno),
        ExprNode::Relabel(r) => stored(p, r.arg),
        _ => false,
    }
}

/// Whether expr.saop takes the `= ANY` / `ALL` at `id` (`hashed`: PG's plan hashed it): an array gravity_expr would walk
/// (emit/scalar.rs `walked`: a column, a parameter, a call's result, a multi-dimension or longer ARRAY[...]), a constant
/// PG hashed (not read: V4R-102), one of more than `MAX_LIST` elements or a text list its byte set answers (`bytes_list`).
/// gravity_expr unrolls the rest, a constant's elements from their words and a one-dimension ARRAY[...] of at most
/// `MAX_LIST` elements in line (PS4-08 / PS4-16: the generic `t IN ($1, $2)`, `k = ANY (ARRAY[g, g + 1])`, built per row
/// by the host walk before), and road text.eq a text constant it claims.
pub(crate) fn wants(p: &gravity_expr::Program<'_>, root: NodeId, id: NodeId, hashed: bool) -> bool {
    let ExprNode::ScalarArrayOp(s) = &p.nodes[id.0 as usize] else { return false };
    if bytes_list(p, root, id) {
        return true;
    }
    match &p.nodes[p.kids[p.sites[s.site.0 as usize].args.start as usize + 1].0 as usize] {
        ExprNode::Const(_) if hashed => true,
        // its count is its class, keyed (konst.rs `list_count`, V4R-110), not a bake
        ExprNode::Const(k) => k.list_count(s.elem).is_some_and(|n| n > MAX_LIST),
        _ => gravity_expr::emit::scalar::walked(p, s),
    }
}

/// Road text.eq keeps a text `= ANY` of a constant it claims (rep/varlena/cmp_text.rs `claims_any`) that PG did not hash;
/// a hashed one is this kind's set, as PG's plan hashes it (main 09-24, after V4R-72's per-row walk).
fn text_eq_keeps(p: &gravity_expr::Program<'_>, root: NodeId, id: NodeId, hashed: bool) -> bool {
    crate::rep::varlena::cmp_text::claims_any(p, root, id) && !wants(p, root, id, hashed)
}

/// Whether PG's plan hashed ScalarArrayOpExpr `n`.
fn pg_hashed(n: Node<'_>) -> bool {
    n.as_scalar_array_op_expr().is_some_and(|s| s.hashfuncid != 0)
}

/// The plan's ScalarArrayOpExprs in walk order; `None` when one sits under another (the program's order would differ).
fn plan_saops<'p, 'g>(node: Node<'p>, mcx: Mcx<'g>) -> Option<PgVec<'g, Node<'p>>> {
    let mut out = PgVec::new_in(mcx);
    gravity_decompose::expr::walk(node, &mut |x| {
        if x.node_tag() == NodeTag::T_ScalarArrayOpExpr {
            out.push_in(x);
        }
        true
    });
    let nested = out.iter().any(|&o| {
        let mut n = 0;
        gravity_decompose::expr::walk(o, &mut |x| {
            n += (x.node_tag() == NodeTag::T_ScalarArrayOpExpr) as usize;
            true
        });
        n > 1
    });
    (!nested).then_some(out)
}

impl ExprExtMech for Saop {
    /// Every long constant or non-constant `= ANY` / `ALL` of the expression, when none of its nodes is text.eq's.
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, _at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        let mcx = px.mcx();
        let ids: PgVec<'_, usize> = (0..c.prog.nodes.len()).filter(|&i| matches!(c.prog.nodes[i], ExprNode::ScalarArrayOp(_))).collect_in(mcx);
        let plans = plan_saops(node, mcx)?;
        if ids.is_empty() || ids.len() != plans.len() || ids.iter().zip(plans.iter()).any(|(&i, &n)| text_eq_keeps(&c.prog, c.root, NodeId(i as u32), pg_hashed(n))) {
            return None;
        }
        let mut claimed = PgVec::new_in(mcx);
        for (&id, &n) in ids.iter().zip(plans.iter()) {
            if !wants(&c.prog, c.root, NodeId(id as u32), pg_hashed(n)) && extreme_of(px, c, id, n).is_none() {
                continue;
            }
            if let Some((d, licence)) = desc_of(px, c, id, n) {
                claimed.push_in((id, n, d, licence));
            }
        }
        if claimed.is_empty() {
            return None;
        }
        let (mut sites, mut skip) = (Vec::new(), Vec::new()); // alloc: Program
        for (k, (id, n, mut d, licence)) in claimed.into_iter().enumerate() {
            let s = n.as_scalar_array_op_expr()?;
            // road text.set: a byte set's operand read in line, a view's column its operand (its start / count operands
            // of their own, after it); the keyed count, read before the node is replaced
            let call = &c.prog.sites[match &c.prog.nodes[id] { ExprNode::ScalarArrayOp(o) => o.site.0 as usize, _ => return None }];
            let (lhs_id, arr_id) = (c.prog.kids[call.args.start as usize], c.prog.kids[call.args.start as usize + 1]);
            let count = match &c.prog.nodes[arr_id.0 as usize] { ExprNode::Const(k) => k.list_count(d.elem).filter(|&n| n <= MAX_LIST), _ => None };
            // D4794: a bpchar byte set's operand is its column, trimmed in line (text_set.rs `probe`) and in the view host
            let bpchar = d.hashed.as_ref().is_some_and(|h| h.trim);
            let view = match bpchar {
                true => Some((s.args.nth(0), true, None)),
                false => text_set::is_view(&c.prog, lhs_id).then(|| text_set::view_plan(s.args.nth(0))).flatten(),
            };
            // a view whose plan side reads otherwise keeps the byte set over gravity_expr's operand
            let set = d.hashed.as_ref().is_some_and(|h| h.bytes) && text_set::on() && (view.is_some() || !text_set::is_view(&c.prog, lhs_id));
            let view = view.filter(|_| set);
            if let Some(h) = d.hashed.as_mut().filter(|_| set) {
                // the bind lays the images out for this site alone (saop_bytes.rs `build_bytes`)
                h.images = true;
                d = d.sealed();
            }
            c.prog.nodes[id] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT + k as i16, typ: 16, desc: 0 });
            let (lhs, desc) = (sub(view.map_or(s.args.nth(0), |v| v.0)), px.owned.len());
            let set = set.then(|| {
                let substr = view.and_then(|v| v.2).map(|(st, n)| (sub(st), n.map(&mut *sub)));
                fires::fire("text.set"); // textkeys witness signal: a byte set probed in line
                SetSite { view: view.map(|v| text_set::SetView { trim: v.1, substr, bpchar }), n: count, use_or: d.use_or }
            });
            if bpchar {
                fires::fire("text.set.bpchar"); // D4794 witness signal: a bpchar list's trimmed byte set (in line or hosted)
            }
            // a hashed constant's set is built once per execution at bind, its address a word (V4R-122); any other array
            // an operand, a constant's too: its word bound per execution (V4R-110)
            let arr = match d.hashed {
                Some(_) => Arr::Set(
                    match px.plan.set(s.args.nth(1), desc as u32) {
                        Place::Word(w) => Some(w),
                        _ => None,
                    },
                    licence,
                ),
                None => Arr::Sub(sub(s.args.nth(1))),
            };
            sites.push(SitePlan { lhs, arr, desc, fp: d.fp, user: d.user != 0, set }); // alloc: Program
            // D3438: shared, so a process-wide template holds the same descriptor
            px.owned.push(Box::new(std::sync::Arc::new(d))); // alloc: Program
            skip.push(n.as_raw().as_ptr() as usize); // alloc: Program
        }
        Some(Claim { plan: Rc::new(sites), skip })
    }

    /// A hashed site's set, built once per execution at bind (V4R-122).
    fn bind_set(&self, owner: Option<&dyn core::any::Any>, desc: u32, a: u64) -> Option<types_error::PgResult<gravity_driver::RegionBuf>> {
        Some(crate::step::raw::saop_set::bound(owner, desc, a))
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _row: &RowVals, ex: ExId, site: u32, sub: SubEmit<'_>) -> [Value; 3] {
        let plan = ExtPlan::of(env.placed(ex)).and_then(|x| x.plan.downcast_ref::<Vec<SitePlan>>()).and_then(|v| v.get(site as usize)).map(|s| (s.lhs, s.arr, s.desc, s.fp, s.user, s.set));
        let Some((lhs, arr, desc, fp, user, set)) = plan else { crate::refuse!(env.node, Reason::Bug { payload: "an expr.saop site without its plan" }) };
        let zero = env.zero;
        let [v, v_ok, v_err] = sub(b, env, lhs);
        let [a, a_ok, a_err] = match arr {
            Arr::Sub(i) if crate::effects::visible::expr::volatile_sub(env, ex, i) && b.const_of(v_err).is_none_or(|(_, c)| c != 0) => after(b, env, v_err, &mut |b, env| sub(b, env, i)),
            Arr::Sub(i) => sub(b, env, i),
            // the set's word (a NULL array's: a null word)
            Arr::Set(Some(w), _) => {
                let nb = b.load_zext(Type::I8, env.params_null, w as i32);
                let ok = b.icmp(IntCond::Eq, nb, zero);
                [b.load(Type::I64, env.params, w as i32 * 8), ok, zero]
            }
            Arr::Set(None, _) => crate::refuse!(env.node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("an IN list's set past the word budget") }),
        };
        let (v, a) = (word(b, v), word(b, a));
        // GV-D1: the scalar's raise before the array's (PG's order), never the lower code of the two
        let v_raised = b.icmp(IntCond::Ne, v_err, zero);
        let err = b.select(v_raised, v_err, a_err);
        // PG evaluates both arguments first; an argument's raise leaves before the compare
        let raised = b.icmp(IntCond::Ne, err, zero);
        let (l_call, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        let skipped = b.iconst(Type::I64, NULL as i64);
        b.cond_br(raised, (l_join, &[skipped]), (l_call, &[]));
        b.switch_to(l_call);
        let host = |b: &mut FunctionBuilder<'_>, env: &mut Env| -> Value {
            let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: true, state: StateMask::NONE };
            let h = (!user).then(|| b.declare_host_once("expr_saop::saop_eval", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), saop_eval as *const () as usize, flags));
            // the Desc through the program's owner (V4R-101: no per-lowering address in the IR), which the host reads from
            // the ctx, whose owner a value's copy is charged to (CLASS E)
            let ctx = env.ctx;
            let (d, fp) = (b.iconst(Type::I64, desc as i64), b.iconst(Type::I64, fp as i64));
            let (v_null, a_null) = (null_word(b, v_ok), null_word(b, a_ok));
            let a2 = b.int_bin(IntBinOp::Add, a_null, a_null);
            let nulls = b.int_bin(IntBinOp::Or, v_null, a2);
            match h {
                Some(h) => b.call_host(h, &[ctx, d, v, a, nulls, fp], Some(Type::I64)).unwrap_or(zero),
                // D4197: a user key's set and operand are classed in the run's classes (the leader's: no worker calls it)
                None => {
                    let Some(keys) = crate::frame::env::user_keys(b, env.ctx) else { crate::refuse!(env.node, Reason::Bug { payload: crate::frame::env::NO_USER_KEYS }) };
                    let flags = HostFlags { worker: false, ..flags };
                    let h = b.declare_host_once("expr_saop::saop_eval_keyed", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), saop_eval_keyed as *const () as usize, flags);
                    b.call_host(h, &[ctx, d, v, a, nulls, fp, keys], Some(Type::I64)).unwrap_or(zero)
                }
            }
        };
        let st = match set.filter(|_| !user) {
            // road text.set: the byte set in line (text_set.rs `probe`); a NULL set word (never a byte set's: its list is a
            // non-NULL constant) is NULL, as the host answers it
            Some(ss) => {
                let (l_probe, l_none) = (b.create_block(&[]), b.create_block(&[]));
                b.cond_br(a_ok, (l_probe, &[]), (l_none, &[]));
                b.switch_to(l_none);
                let null = b.iconst(Type::I64, NULL as i64);
                b.br(l_join, &[null]);
                b.switch_to(l_probe);
                let mut int = |b: &mut FunctionBuilder<'_>, env: &mut Env, i: u32| { let [w, _, _] = sub(b, env, i); word(b, w) };
                let words = ss.view.and_then(|vw| vw.substr).map(|(st, n)| (int(b, env, st), n.map(|n| int(b, env, n))));
                let view = ss.view.map(|vw| (vw.trim, words));
                let form = text_set::form_word(ss.view, ss.use_or);
                let mut cold = |b: &mut FunctionBuilder<'_>, env: &mut Env| match ss.view {
                    Some(_) => text_set::view_cold(b, env, v, v_ok, a, form, words),
                    None => host(b, env),
                };
                text_set::probe(b, env, a, v, v_ok, view, ss.view.is_some_and(|vw| vw.bpchar), ss.n, ss.use_or, &mut cold)
            }
            None => host(b, env),
        };
        b.br(l_join, &[st]);
        b.switch_to(l_join);
        let st = b.block_params(l_join)[0];
        let one = b.iconst(Type::I64, TRUE as i64);
        let is_true = b.icmp(IntCond::Eq, st, one);
        let value = b.conv(ConvOp::Zext, Type::I64, is_true);
        let two = b.iconst(Type::I64, NULL as i64);
        let valid = b.icmp(IntCond::Ult, st, two);
        let three = b.iconst(Type::I64, FAILED as i64);
        let failed = b.icmp(IntCond::Eq, st, three);
        let bit = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        let host = b.select(failed, bit, zero);
        let err = b.int_bin(IntBinOp::Or, err, host);
        [value, valid, err]
    }
}

/// D946 (V4R-342): PG builds the array only after the scalar and never past its raise (`err`), so an array holding a
/// VOLATILE call runs in its own block, entered where `err` is 0 (a NULL word with no raise past it; its memo its own).
fn after(b: &mut FunctionBuilder<'_>, env: &mut Env, err: Value, sub: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> [Value; 3]) -> [Value; 3] {
    let (zero, f) = (env.zero, b.bconst(false));
    let (l_do, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::Bool, Type::I64]));
    let raised = b.icmp(IntCond::Ne, err, zero);
    b.cond_br(raised, (l_join, &[zero, f, zero]), (l_do, &[]));
    b.switch_to(l_do);
    let memo = env.memo.set_aside();
    let [a, ok, e] = sub(b, env);
    env.memo.restore(memo);
    let a = word(b, a);
    b.br(l_join, &[a, ok, e]);
    b.switch_to(l_join);
    let p = b.block_params(l_join);
    [p[0], p[1], p[2]]
}

/// `v` as the datum word the host reads.
fn word(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    match b.value_type(v) {
        Type::I64 => v,
        Type::F64 => b.conv(ConvOp::Bitcast, Type::I64, v),
        Type::F32 => {
            let i = b.conv(ConvOp::Bitcast, Type::I32, v);
            b.conv(ConvOp::Zext, Type::I64, i)
        }
        Type::Ptr => b.conv(ConvOp::PtrToInt, Type::I64, v),
        Type::Bool => b.conv(ConvOp::Zext, Type::I64, v),
        _ => b.conv(ConvOp::Sext, Type::I64, v),
    }
}

fn null_word(b: &mut FunctionBuilder<'_>, ok: Value) -> Value {
    let t = b.bconst(true);
    let n = b.int_bin(IntBinOp::Xor, ok, t);
    b.conv(ConvOp::Zext, Type::I64, n)
}
#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_expr::nodes::func::{Call, How};
    use gravity_fn::Datum;
    use gravity_expr::nodes::array::ElemFacts;
    use gravity_expr::nodes::konst::ConstNode;
    use gravity_expr::nodes::op::ScalarArrayOp;
    use gravity_expr::{Kids, Program, SiteId};

    use super::*;

    /// A one-dimensional array image of `n` elements `v<i>` of varlena type `typ` (4-byte headers, 'i' alignment).
    fn text_array(typ: u32, n: usize) -> Vec<u64> {
        let mut data = Vec::new(); // alloc: Cold
        for i in 0..n {
            let b = format!("v{i}"); // alloc: Cold
            data.extend_from_slice(&(((4 + b.len()) as u32) << 2).to_le_bytes());
            data.extend_from_slice(b.as_bytes());
            data.resize(data.len().div_ceil(4) * 4, 0);
        }
        let mut img = Vec::new(); // alloc: Cold
        let total = 24 + data.len();
        img.extend_from_slice(&((total as u32) << 2).to_le_bytes());
        img.extend_from_slice(&1i32.to_le_bytes());
        img.extend_from_slice(&0i32.to_le_bytes());
        img.extend_from_slice(&typ.to_le_bytes());
        img.extend_from_slice(&(n as i32).to_le_bytes());
        img.extend_from_slice(&1i32.to_le_bytes());
        img.extend_from_slice(&data);
        let mut w = vec![0u64; img.len().div_ceil(8)]; // alloc: Cold
        unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), w.as_mut_ptr().cast::<u8>(), img.len()) };
        w
    }

    /// `col = ANY (konst)` over column 2 of type `typ` with operator `funcid`.
    fn any<'a>(funcid: u32, typ: u32, konst: &'a [u64]) -> Program<'a> {
        let k = ConstNode::new(gravity_fn::NullableDatum::value(Datum::from_usize(konst.as_ptr() as usize)), 1009, -1, false);
        let call = Call { funcid, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: 100, how: How::Fmgr(0), rettype: 16, volatile: false };
        let op = ExprNode::ScalarArrayOp(ScalarArrayOp { site: SiteId(0), use_or: true, elem: ElemFacts { typ, len: -1, byval: false, align: b'i' as i8 } });
        let v = ExprNode::Var(Var { src: VarSource::Scan, attno: 2, typ, desc: 0 });
        { let mut p = Program::empty_in(crate::step::expr::test_lists()); p.nodes.extend_in([v, ExprNode::Const(k), op]); p.kids.extend_in([NodeId(0), NodeId(1)]); p.sites.extend_in([call]); p } // alloc: Cold
    }

    /// Main 09-24: of text.eq and expr.saop exactly one takes a text / bpchar `= ANY` of a constant array; gravity_expr's
    /// unrolled form keeps a list of at most 16 elements PG did not hash that text.eq does not take (bpchar). A list PG
    /// hashed (9 or more elements of a hashable type; also as if it had not, a hash function missing) is expr.saop's, and
    /// so is a text list of `text_set_min()` or more under a byte-equal collation (LIST-FORM's byte set).
    #[test]
    fn exactly_one_of_text_eq_and_saop_takes_a_text_any() {
        for (funcid, typ) in [(67u32, 25u32), (1048, 1042)] {
            for n in [1usize, 2, 3, 8, 9, 16, 17, 64, 65, 1000] {
                for hashed in [n >= 9, false] {
                    let img = text_array(typ, n);
                    let p = any(funcid, typ, &img);
                    let (text, saop) = (text_eq_keeps(&p, NodeId(2), NodeId(2), hashed), wants(&p, NodeId(2), NodeId(2), hashed));
                    let want = match (typ, n) {
                        _ if hashed => (false, true),
                        // LIST-FORM: a text list of text_set_min() or more is the byte set's
                        (25, n) if n >= text_set_min() => (false, true),
                        (25, n) if n <= MAX_LIST => (true, false),
                        (_, n) if n <= MAX_LIST => (false, false),
                        _ => (false, true),
                    };
                    assert_eq!((text, saop), want, "funcid {funcid} over {n} elements, hashed {hashed}: (text.eq, expr.saop)");
                }
            }
        }
    }

    /// LIST-FORM: the byte set compares a column's stored text; a computed operand (here a whole-row reference and another
    /// claim's placeholder stand for one) is not a byte set's, so the road that produced it keeps it.
    #[test]
    fn a_byte_set_reads_a_column() {
        let img = text_array(25, 3);
        let mut p = any(67, 25, &img);
        assert!(bytes_list(&p, NodeId(2), NodeId(2)), "a column");
        for attno in [0, EXT_AT] {
            p.nodes[0] = ExprNode::Var(Var { src: VarSource::Scan, attno, typ: 25, desc: 0 });
            assert!(!bytes_list(&p, NodeId(2), NodeId(2)), "attno {attno}");
        }
    }

    /// V4R-102: a list PG hashed is not read (no bake logged), so one program serves every list of the statement's shape.
    #[test]
    fn a_hashed_list_is_not_read() {
        let img = text_array(25, 20);
        let p = any(67, 25, &img);
        let d = gravity_expr::bake::Scope::open();
        let claimed = wants(&p, NodeId(2), NodeId(2), true) && !text_eq_keeps(&p, NodeId(2), NodeId(2), true);
        let reads = d.reads();
        drop(d);
        assert!(claimed && reads.is_empty(), "claimed {claimed}, reads {reads:?}");
    }

    /// PS4-08 / PS4-16: an `= ANY (ARRAY[..])` of at most `MAX_LIST` elements in one dimension is gravity_expr's unrolled
    /// form (every element after the scalar, before any compare), not the host walk; a longer or multi-dimension one, and a
    /// parameter array, stay expr.saop's.
    #[test]
    fn a_short_array_expression_is_unrolled() {
        use gravity_expr::nodes::array::ArrayExpr;
        use gravity_expr::nodes::param::Param;
        use types_nodes::primnodes::ParamKind;
        let int4 = ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 };
        let param = |id: i32, typ: u32| ExprNode::Param(Param { kind: ParamKind::PARAM_EXTERN, id, typ });
        // `col = ANY (ARRAY[$1, .., $n])` (or `= ANY ($1)` of an int4[] parameter, `param_array`)
        let prog = |n: u32, multidims: bool, param_array: bool| {
            let mut nodes = vec![ExprNode::Var(Var { src: VarSource::Scan, attno: 1, typ: 23, desc: 0 })]; // alloc: Cold
            let mut kids = vec![NodeId(0)]; // alloc: Cold
            if param_array {
                nodes.push(param(1, 1007));
                kids.push(NodeId(1));
            } else {
                nodes.extend((1..=n).map(|i| param(i as i32, 23)));
                nodes.push(ExprNode::Array(ArrayExpr { elements: Kids { start: 2, len: n }, array_type: 1007, elem: int4, multidims, site: SiteId(1) }));
                kids.push(NodeId(n + 1));
                kids.extend((1..=n).map(NodeId));
            }
            let call = Call { funcid: 65, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: 0, how: How::Fmgr(0), rettype: 16, volatile: false };
            nodes.push(ExprNode::ScalarArrayOp(ScalarArrayOp { site: SiteId(0), use_or: true, elem: int4 }));
            { let mut p = Program::empty_in(crate::step::expr::test_lists()); p.nodes.extend_in(nodes); p.kids.extend_in(kids); p.sites.push_in(call); p } // alloc: Cold
        };
        for (n, multidims, param_array, want) in [(1u32, false, false, false), (3, false, false, false), (16, false, false, false), (17, false, false, true), (3, true, false, true), (0, false, true, true)] {
            let p = prog(n, multidims, param_array);
            let at = NodeId(p.nodes.len() as u32 - 1);
            assert_eq!(wants(&p, at, at, false), want, "{n} elements, multidims {multidims}, a parameter array {param_array}");
        }
    }

    /// G4 (does not apply): an int `= ANY` of a constant of at most 16 elements is gravity_expr's unrolled compare.
    #[test]
    fn a_short_constant_list_is_not_claimed() {
        let mut img = vec![0u64; 5];
        let b = unsafe { core::slice::from_raw_parts_mut(img.as_mut_ptr().cast::<u8>(), 40) };
        let hdr: [i32; 6] = [(36 << 2) as i32, 1, 0, 23, 3, 1];
        hdr.iter().enumerate().for_each(|(i, x)| b[i * 4..i * 4 + 4].copy_from_slice(&x.to_le_bytes()));
        [7i32, 8, 9].iter().enumerate().for_each(|(i, x)| b[24 + i * 4..28 + i * 4].copy_from_slice(&x.to_le_bytes()));
        let k = ConstNode::new(gravity_fn::NullableDatum::value(Datum::from_usize(img.as_ptr() as usize)), 1007, -1, false);
        let call = Call { funcid: 65, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: 0, how: How::Fmgr(0), rettype: 16, volatile: false };
        let op = ExprNode::ScalarArrayOp(ScalarArrayOp { site: SiteId(0), use_or: true, elem: ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 } });
        let v = ExprNode::Var(Var { src: VarSource::Scan, attno: 1, typ: 23, desc: 0 });
        let p = { let mut p = Program::empty_in(crate::step::expr::test_lists()); p.nodes.extend_in([v, ExprNode::Const(k), op]); p.kids.extend_in([NodeId(0), NodeId(1)]); p.sites.extend_in([call]); p }; // alloc: Cold
        assert!(!wants(&p, NodeId(2), NodeId(2), false));
    }
}
