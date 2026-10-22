//! Road `expr.fusion` (H35, R1-L12 R13-L09; applies `datetime_expr`; cards/M2.md L2): `date_part` / `date_trunc` over a
//! timestamptz as integer arithmetic under a fixed session offset. gravity_expr already serves them over a timestamp in tier
//! A (expr/src/emit/prim/datetime.rs: the quotient forms, the BC years, the infinities) and hosts the timestamptz forms
//! (timestamp2tm under the session zone). This extension claims one `timestamptz_part(unit, ts)` / `timestamptz_trunc(unit,
//! ts)` per expression (a constant unit the timestamp forms serve; under a filter's comparison too: its unit keeps the
//! qual's word, D390) and emits it over `local = ts + offset`: the part is the
//! timestamp's part of the local time, the trunc the local trunc minus the offset (a fixed zone has no transition, so
//! `tm2timestamp`'s zone is the same offset). The offset is `tz_fixed_offset` (kernels host_abi v4/m2.rs, seconds east,
//! PG's gmtoff), read per row, never cached in the program (a `SET TimeZone` between two executions is seen); under a
//! zone with DST (`i64::MIN`) the row takes the hosted call, the claimed call compiled as an operand of its own. An
//! infinite timestamptz is not shifted: its part is NULL or ±Infinity and its trunc itself, as PG's. The calendar truncs
//! raise PG's "timestamp out of range" below the range's first day. Owner M2-FRAME.
use std::rc::Rc;

use gravity_expr::emit::prim::datetime::{self, DtForm, Kind};
use gravity_expr::nodes::var::Var;
use gravity_expr::{Compiled, ExprNode, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, StateMask, Type, Value};
use gravity_kernels::ops::agg::trans::entries::m6_extract_word;
use gravity_kernels::ops::scan::entries::{tz_fixed_offset, tz_offset_at};
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_nodes::{Node, NodeTag};

use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, SubEmit, EXT_AT};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};

crate::strategies! {
    /// Date_trunc / extract as integer arithmetic, Umulh, strict key words.
    EXPR_FUSION = 62, "expr.fusion", DatetimeExpr;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(EXPR_FUSION, &ExprExtImpl(&FUSE))], ..crate::ops::NONE };

struct Fuse;
static FUSE: Fuse = Fuse;

/// The claimed call: its form over the local time; `word`: an `extract` claimed at a group key's word root
/// (`compile_word`), whose value is the field's integer word (the hosted side's numeric converted, `m6_extract_word`).
struct Plan {
    form: DtForm,
    word: bool,
}

/// The timestamp form whose unit table a timestamptz call takes (`datetime::form_of` reads the unit by that name).
fn tz_form(funcid: u32) -> Option<&'static str> {
    match gravity_fn::row_of(funcid)?.name {
        "timestamptz_part" => Some("timestamp_part"),
        "timestamptz_trunc" => Some("timestamp_trunc"),
        "extract_timestamptz" => Some("extract_timestamp"),
        _ => None,
    }
}

/// Whether `x` is a `FuncExpr` over a fused form.
fn is_plan_call(x: Node<'_>) -> bool {
    x.node_tag() == NodeTag::T_FuncExpr && x.as_func_expr().is_some_and(|f| tz_form(f.funcid).is_some())
}

/// The plan's `FuncExpr` nodes over a fused form, in walk order.
fn plan_calls<'p, 'g>(node: Node<'p>, mcx: Mcx<'g>) -> PgVec<'g, Node<'p>> {
    let mut out = PgVec::new_in(mcx);
    gravity_decompose::expr::walk(node, &mut |x| {
        if is_plan_call(x) {
            out.push_in(x);
        }
        true
    });
    out
}

/// `plan_calls(node).len()`, counted without the vector.
fn plan_call_count(node: Node<'_>) -> usize {
    let mut n = 0;
    gravity_decompose::expr::walk(node, &mut |x| {
        n += usize::from(is_plan_call(x));
        true
    });
    n
}

/// B2-31: a group key's integral `extract(unit FROM timestamptz)` whose word the fused form computes: the expression is
/// the call itself (its only fused call), a constant unit the timestamp form serves as an integer, not monotonic (an
/// infinite timestamptz's field is NULL, the word's validity). `extract_word_key` keys such a projection by its word only
/// when this holds, and `compile_word` then claims it (`FUSE_WORD`): the two read the same predicate.
pub(crate) fn word_claimable(node: Node<'_>) -> bool {
    let node = node.as_target_entry().map_or(node, |te| te.expr);
    // under an integer cast root (`extract(hour FROM ts)::int`) the word is the extract's (gravity_expr `word_node`)
    let node = crate::table::emit_group::word_cast_arg(node).0;
    let Some(f) = node.as_func_expr().filter(|f| f.args.len() == 2) else { return false };
    let is_extract = gravity_fn::row_of(f.funcid).is_some_and(|r| r.name == "extract_timestamptz");
    let unit = f.args.nth(0).as_const().filter(|c| !c.constisnull).and_then(gravity_decompose::expr::const_bytes);
    is_extract && unit.and_then(datetime::extract_timestamp_unit).is_some_and(|u| !u.monotonic()) && plan_call_count(node) == 1
}

/// One call per expression: the program's only fused call, matched to the plan's only one (two calls of one expression
/// stay hosted). `word`: the root is a group key's word (an `extract` is claimed only there).
fn claim<'p>(node: Node<'p>, c: &mut Compiled<'p>, sub: &mut dyn FnMut(Node<'p>) -> u32, mcx: Mcx<'_>, word: bool) -> Option<Claim> {
    let p = &c.prog;
    let at: PgVec<'_, (usize, DtForm)> = p.nodes.iter().enumerate().filter_map(|(i, n)| {
        let ExprNode::Func(f) = n else { return None };
        let call = &p.sites[f.site.0 as usize];
        tz_form(call.funcid).map(|name| (i, datetime::form_of(p, call, name, word)))
    }).filter_map(|(i, f)| Some((i, f?))).collect_in(mcx);
    let calls = plan_calls(node, mcx);
    let [(i, form)] = at[..] else { return None };
    let [call] = calls[..] else { return None };
    if word != (form.kind == Kind::Extract) {
        return None;
    }
    let f = call.as_func_expr()?;
    let ts = f.args.iter().nth(1)?;
    let typ = match form.kind {
        Kind::Trunc => 1184,
        Kind::Extract => 1700,
        _ => 701,
    };
    c.prog.nodes[i] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT, typ, desc: 0 });
    let (a, whole) = (sub(ts), sub(call));
    debug_assert_eq!((a, whole), (0, 1));
    Some(Claim { plan: Rc::new(Plan { form, word }), skip: vec![call.as_raw().as_ptr() as usize] }) // alloc: Program
}

/// B2-31: `ex` holds the word claim: its value is the field's word, so a root integer cast over it is that word too
/// (step/expr.rs sets `RowRefs::word_root`, gravity_expr `word_node`).
pub(crate) fn word_claimed(e: &crate::hir::Ex<'_>) -> bool {
    crate::face::step::ExtPlan::of(e).and_then(|x| x.plan.downcast_ref::<Plan>()).is_some_and(|p| p.word)
}

/// B2-31: the word-root claim `compile_word` tries before `WordRoot` (`word_claimable`).
struct FuseWord;
pub(crate) static FUSE_WORD: &dyn ExprExtMech = &FuseWord;

impl ExprExtMech for FuseWord {
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, _at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        let mcx = px.mcx();
        let claimed = word_claimable(node).then(|| claim(node, c, sub, mcx, true)).flatten();
        if claimed.is_some() {
            fires::fire("expr.fusion.word"); // B2-31 witness signal: a timestamptz extract group key claimed by its word
        }
        claimed
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, site: u32, sub: SubEmit<'_>) -> [Value; 3] {
        FUSE.emit(b, env, row, ex, site, sub)
    }
}

impl ExprExtMech for Fuse {
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, _at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        claim(node, c, sub, px.mcx(), false)
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _row: &RowVals, ex: ExId, _site: u32, sub: SubEmit<'_>) -> [Value; 3] {
        let plan = crate::face::step::ExtPlan::of(env.placed(ex)).and_then(|x| x.plan.downcast_ref::<Plan>()).map(|p| (p.form, p.word));
        let Some((form, word)) = plan else { crate::refuse!(env.node, Reason::Bug { payload: "expr.fusion emitted a site it never claimed" }) };
        // session state: not a worker host (a program calling it runs on the leader)
        let flags = HostFlags { may_error: false, pure: false, touches_memory: false, worker: false, state: StateMask::NONE };
        let h = b.declare_host_once("tz_fixed_offset", &[Type::Ptr], Some(Type::I64), tz_fixed_offset as *const () as usize, flags);
        let off = b.call_host(h, &[env.ctx], Some(Type::I64)).unwrap_or(env.zero);
        let min = b.iconst(Type::I64, i64::MIN);
        let dst = b.icmp(IntCond::Eq, off, min);
        let (l_host, l_fused) = (b.create_block(&[]), b.create_block(&[]));
        if !word {
            b.mark_cold(l_host);
        }
        b.cond_br(dst, (l_host, &[]), (l_fused, &[]));
        // a zone with DST: the hosted call; each side's column reads are memoised in its own block only
        b.switch_to(l_host);
        let memo = env.memo.set_aside();
        let host = if word { dst_word(b, env, form, flags, &mut *sub) } else { sub(b, env, 1) };
        env.memo.restore(memo);
        let types = host.map(|v| b.value_type(v));
        let l_join = b.create_block(&types);
        b.br(l_join, &host);
        b.switch_to(l_fused);
        let memo = env.memo.set_aside();
        let [ts, ok, err] = sub(b, env, 0);
        env.memo.restore(memo);
        let fused = fused(b, form, off, ts, ok, err);
        if fused.iter().zip(types.iter()).any(|(&v, &t)| b.value_type(v) != t) {
            crate::refuse!(env.node, Reason::Shape { why: ShapeWhy("a fused datetime value whose form differs from the hosted one") });
        }
        b.br(l_join, &fused);
        b.switch_to(l_join);
        let p = b.block_params(l_join);
        [p[0], p[1], p[2]]
    }
}

/// B2-31: a word root's value under a zone with DST. Each row reads the zone's offset at its own instant (`tz_offset_at`,
/// `timestamp2tm`'s `pg_localtime`) and open-codes the field over `ts + offset`, as a fixed zone's does; a row whose offset
/// it cannot read that way (out of `pg_localtime`'s range, a leap second, an infinity) takes the hosted call, its numeric
/// converted to the integer word (`extract_word`: rep.hosted.hash's conversion, PG's int4 range and its raise).
fn dst_word(b: &mut FunctionBuilder<'_>, env: &mut Env, form: DtForm, flags: HostFlags, sub: SubEmit<'_>) -> [Value; 3] {
    let [ts, ok, err] = sub(b, env, 0);
    let h = b.declare_host_once("tz_offset_at", &[Type::Ptr, Type::I64], Some(Type::I64), tz_offset_at as *const () as usize, flags);
    let min = b.iconst(Type::I64, i64::MIN);
    let off = b.call_host(h, &[env.ctx, ts], Some(Type::I64)).unwrap_or(min);
    let unread = b.icmp(IntCond::Eq, off, min);
    let (l_host, l_fused) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_host);
    b.cond_br(unread, (l_host, &[]), (l_fused, &[]));
    b.switch_to(l_host);
    let host = sub(b, env, 1);
    let host = extract_word(b, host);
    let types = host.map(|v| b.value_type(v));
    let l_join = b.create_block(&types);
    b.br(l_join, &host);
    b.switch_to(l_fused);
    let fused = fused(b, form, off, ts, ok, err);
    if fused.iter().zip(types.iter()).any(|(&v, &t)| b.value_type(v) != t) {
        crate::refuse!(env.node, Reason::Shape { why: ShapeWhy("a fused datetime value whose form differs from the hosted one") });
    }
    b.br(l_join, &fused);
    b.switch_to(l_join);
    let p = b.block_params(l_join);
    [p[0], p[1], p[2]]
}

/// `form` over timestamptz word `ts` (valid `ok`, raise word `err`) under the fixed offset `off` (seconds east).
fn fused(b: &mut FunctionBuilder<'_>, form: DtForm, off: Value, ts: Value, ok: Value, err: Value) -> [Value; 3] {
    let (lo, hi) = (b.iconst(Type::I64, i64::MIN), b.iconst(Type::I64, i64::MAX));
    let (nlo, nhi) = (b.icmp(IntCond::Ne, ts, lo), b.icmp(IntCond::Ne, ts, hi));
    let finite = b.int_bin(IntBinOp::And, nlo, nhi);
    let usec = b.iconst(Type::I64, 1_000_000);
    let off_us = b.int_bin(IntBinOp::Mul, off, usec);
    let shifted = b.int_bin(IntBinOp::Add, ts, off_us);
    let local = b.select(finite, shifted, ts);
    let (out, valid) = datetime::emit(b, form, local);
    let zero = b.iconst(Type::I64, 0);
    let (value, out_err) = match form.kind {
        Kind::Trunc => {
            let back = b.int_bin(IntBinOp::Sub, out.value, off_us);
            // D3519: tm2timestamp's IS_VALID_TIMESTAMP on the trunc's instant (the local trunc minus the offset): a
            // local midnight / hour before the range's first instant raises PG's "timestamp out of range"
            let min = b.iconst(Type::I64, datetime::MIN_TS);
            let low = b.icmp(IntCond::Slt, back, min);
            let bad = b.int_bin(IntBinOp::And, finite, low);
            let range = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::TS_RANGE) as i64);
            let range = b.select(bad, range, zero);
            (b.select(finite, back, out.value), b.int_bin(IntBinOp::Or, out.err, range))
        }
        _ => (out.value, out.err),
    };
    // a NULL argument raises nothing: the strict call is not evaluated
    let raised = b.select(ok, out_err, zero);
    // GV-D1: the argument's raise before the call's own (PG never reaches the call)
    let arg_raised = b.icmp(IntCond::Ne, err, zero);
    let err = b.select(arg_raised, err, raised);
    let ok = match valid {
        Some(v) => b.int_bin(IntBinOp::And, ok, v),
        None => ok,
    };
    [value, ok, err]
}

/// B2-31: the hosted `extract`'s numeric `[datum, ok, err]` as its integer word (`m6_extract_word`, canon.rs
/// `integral_word`: the int4, ±Infinity as the packed sentinels, PG's "integer out of range" past int4, raised with the
/// HOST bit as rep.hosted.hash's). Not called for a NULL or a raised argument: the word is 0 and the err the argument's.
fn extract_word(b: &mut FunctionBuilder<'_>, [v, ok, err]: [Value; 3]) -> [Value; 3] {
    let zero = b.iconst(Type::I64, 0);
    let clean = b.icmp(IntCond::Eq, err, zero);
    let live = b.int_bin(IntBinOp::And, ok, clean);
    let (l_conv, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(live, (l_conv, &[]), (l_out, &[zero, err]));
    b.switch_to(l_conv);
    let p = if b.value_type(v) == Type::Ptr { v } else { b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, v) };
    let slot = b.declare_stack_slot(16, 8);
    let out = b.stack_addr(slot);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let h = b.declare_host_once("m6_extract_word", &[Type::Ptr, Type::Ptr], Some(Type::I64), m6_extract_word as *const () as usize, flags);
    let st = b.call_host(h, &[p, out], Some(Type::I64)).expect("m6_extract_word returns its status");
    let w = b.load(Type::I64, out, 0);
    let bad = b.icmp(IntCond::Ne, st, zero);
    let host_bit = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let e = b.select(bad, host_bit, zero);
    b.br(l_out, &[w, e]);
    b.switch_to(l_out);
    let o = b.block_params(l_out);
    [o[0], ok, o[1]]
}
