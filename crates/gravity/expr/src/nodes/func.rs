//! `FuncExpr` / `OpExpr` and the call machinery every calling node shares (docs/gravity/
//! expr-compiler.md §1.5): a `Call` site binds once to a `gravity_fn` shim (tiers A and B: the
//! arguments as widened words or as payload / image views formed at the boundary, the result by
//! value or as an image in the region) or to an `FmgrCall` (tier C). Strictness is decided here.

use mcx::GrowIn;
use gravity_fn::{Arg, ArgKind, Datum, ErrSlot, FmgrCall, NullableDatum, Shim};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::{FuncExpr, OpExpr};
use types_nodes::{Node, NodeList};

use crate::eval::{eval, Cx};
use crate::nodes::{unsupported, Builder, ExprNode, Kids, Kind, NodeId, Prog, SiteId, St};
use crate::views::{image_view, no_space, payload_view};
use crate::CompileResult;

pub const MAX_ARGS: usize = gravity_fn::fmgr_call::FUNC_MAX_ARGS;

/// A masked site (the ARRAY constructor's, the row's): one head word, the null mask's words (bit `i % 64` of word
/// `i / 64` says value `i` is NULL), then the values by value.
pub const fn mask_words(n: usize) -> usize {
    if n == 0 { 1 } else { n.div_ceil(64) }
}

/// The most values a masked site carries within a call's `MAX_ARGS` (the inverse of `1 + mask_words(n) + n`).
pub const MAX_MASKED: usize = MAX_ARGS - 1 - (MAX_ARGS - 1).div_ceil(65);

/// A masked site's arguments past the head: its mask words and its values.
pub fn masked(a: &[gravity_fn::Arg]) -> (&[gravity_fn::Arg], &[gravity_fn::Arg]) {
    let k = (a.len() - 1).div_ceil(65).max(1);
    a[1..].split_at(k)
}

/// Whether value `i` of a masked site is NULL.
pub fn masked_null(masks: &[gravity_fn::Arg], i: usize) -> bool {
    masks[i / 64].v() >> (i % 64) & 1 != 0
}

#[derive(Clone, Copy, Debug)]
pub struct FuncNode {
    pub site: SiteId,
}

/// How a shim takes one argument: the kind from the shim catalog and the argument type's typlen.
#[derive(Clone, Copy, Debug)]
pub struct ArgConv {
    pub kind: ArgKind,
    pub typlen: i16,
}

#[derive(Clone, Copy)]
pub enum How {
    /// A direct shim; `convs` is the first of `nargs` entries in `Program::convs`; `byref` = an image in the region.
    Shim { shim: &'static Shim, convs: u32, byref: bool },
    /// The function manager: an index into `State::calls`.
    Fmgr(u32),
    /// An interpreter island (`Program::islands`): the emitter's host evaluates `root` over the site's column `Var`s.
    Island { root: NodeId },
}

pub struct Call {
    pub funcid: Oid,
    pub args: Kids,
    pub nargs: u8,
    pub strict: bool,
    pub collation: Oid,
    pub how: How,
    pub rettype: Oid,
    pub volatile: bool,
}

/// Bind `funcid` over `args` (already compiled; `arg_nodes` are the trees, for their types).
pub fn bind<'mcx>(b: &mut Builder<'_, 'mcx>, funcid: Oid, args: Kids, arg_nodes: &NodeList<'mcx>, collation: Oid) -> CompileResult<Call> {
    let mut types = Vec::with_capacity(arg_nodes.len()); // alloc: Program
    for n in arg_nodes.iter() {
        types.push(nodes_core::expr_type(n));
    }
    bind_typed(b, funcid, args, &types, collation)
}

leader_tls::leader_tls! {
    thread_local! {
        static SUBSTR_ASCII: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// D4822 (lowerv4 road `text.substr.ascii`, default off): while the guard lives (a statement's lowering, lowerv4
/// frame/program.rs) a text_substr / text_substr_no_len site binds the shim's ASCII twin (gravity_fn shims/text.rs
/// `ascii_twin`: the result by bytes when the text the result reaches is ASCII, else the same walk). The drop restores the
/// outer setting, on an unwind too.
#[must_use = "the setting ends when the guard drops"]
pub struct SubstrAscii(bool);

impl SubstrAscii {
    pub fn scope(on: bool) -> SubstrAscii {
        SubstrAscii(SUBSTR_ASCII.with(|c| c.replace(on)))
    }

    /// `shim`, or its ASCII twin under the guard (fire `lower.text.substr.ascii`).
    fn twin(shim: &'static Shim) -> &'static Shim {
        if !SUBSTR_ASCII.with(|c| c.get()) {
            return shim;
        }
        let t = gravity_fn::shims::text::ascii_twin(shim);
        if !core::ptr::eq(t, shim) {
            fires::fire("lower.text.substr.ascii");
        }
        t
    }
}

impl Drop for SubstrAscii {
    fn drop(&mut self) {
        SUBSTR_ASCII.with(|c| c.set(self.0));
    }
}

/// Bind `funcid` with the argument types given: `args` may be empty when the caller supplies the
/// argument values itself (`MinMaxExpr`, `RowCompareExpr`, `CoerceViaIO`).
pub fn bind_typed(b: &mut Builder<'_, '_>, funcid: Oid, args: Kids, types: &[Oid], collation: Oid) -> CompileResult<Call> {
    execute_gate(funcid)?;
    let facts = b.catalog.func(funcid)?;
    if facts.retset {
        return unsupported(Kind::FuncExpr);
    }
    let nargs = types.len();
    let mut how = None;
    if let Some(shim) = b.catalog.shim(funcid) {
        if shim.nargs as usize == nargs && shim.tyargs.is_empty() && facts.strict {
            let shim = SubstrAscii::twin(shim);
            let convs = b.prog.convs.len() as u32;
            for (i, &t) in types.iter().enumerate() {
                let kind = shim.args[i];
                let typlen = match kind {
                    ArgKind::V | ArgKind::D => 0,
                    // cstring (a CoerceViaIO input function's text) is a pseudo-type of fixed length -2
                    _ if gravity_fn::typesem::expr::cstring(t) => -2,
                    _ => b.catalog.type_facts(t)?.len,
                };
                b.prog.convs.push_in(ArgConv { kind, typlen });
            }
            how = Some(How::Shim { shim, convs, byref: shim.region });
        }
    }
    let how = match how {
        Some(h) => h,
        None => How::Fmgr(b.fmgr(FmgrCall::bind(funcid, nargs, collation)?, types, facts.node_arg)),
    };
    let volatile = facts.call_acts(matches!(how, How::Fmgr(_)));
    // D316: a named collation is resolved here, on the leader, so the section's stamp arms its pool workers with it (a
    // worker has no catcache); a failure is the call's own at run time, as PG raises it
    if !matches!(collation, 0 | gravity_fn::typesem::collation::DEFAULT_COLLATION_OID | gravity_fn::typesem::collation::C_COLLATION_OID | gravity_fn::typesem::collation::POSIX_COLLATION_OID) {
        let _ = b.catalog.text(collation);
    }
    Ok(Call { funcid, args, nargs: nargs as u8, strict: facts.strict, collation, how, rettype: 0, volatile })
}

pub fn compile_func<'mcx>(b: &mut Builder<'_, 'mcx>, f: &FuncExpr<'mcx>, node: Node<'mcx>) -> CompileResult<NodeId> {
    if f.funcretset {
        return crate::nodes::srf::compile_srf(b, f);
    }
    let args = b.kids(&f.args)?;
    let mut call = bind(b, f.funcid, args, &f.args, f.inputcollid)?;
    call.rettype = f.funcresulttype;
    set_fn_expr(b, &call, node);
    let site = b.site(call);
    Ok(b.push(ExprNode::Func(FuncNode { site })))
}

/// C `fmgr_info_set_expr`: a function-manager site carries its calling node for the polymorphic readers.
pub fn set_fn_expr<'mcx>(b: &mut Builder<'_, 'mcx>, call: &Call, node: Node<'mcx>) {
    if let How::Fmgr(ix) = call.how {
        let f = &mut b.state.call_facts[ix as usize];
        f.rettype = call.rettype;
        f.variadic = node.as_func_expr().is_some_and(|e| e.funcvariadic);
        f.stable = 0;
        let args = node.as_func_expr().map(|e| &e.args).or_else(|| node.as_op_expr().map(|e| &e.args));
        for (i, a) in args.into_iter().flat_map(|l| l.iter()).enumerate().take(128) {
            f.stable |= (a.as_const().is_some_and(|c| gravity_fn::typesem::expr::unknown_type(c.consttype)) as u128) << i;
        }
        // SAFETY: a `Node` is a pointer into the plan arena, which outlives the program.
        let node = unsafe { core::mem::transmute::<Node<'mcx>, Node<'static>>(node) };
        b.state.calls[ix as usize].set_fn_expr(node);
    }
}

pub fn compile_op<'mcx>(b: &mut Builder<'_, 'mcx>, o: &OpExpr<'mcx>, node: Node<'mcx>) -> CompileResult<NodeId> {
    let funcid = if o.opfuncid != 0 { o.opfuncid } else { b.catalog.opcode(o.opno)? };
    if o.opretset {
        return Ok(crate::nodes::srf::srf_cell(b, funcid, o.opresulttype));
    }
    let args = b.kids(&o.args)?;
    let mut call = bind(b, funcid, args, &o.args, o.inputcollid)?;
    call.rettype = o.opresulttype;
    set_fn_expr(b, &call, node);
    let site = b.site(call);
    Ok(b.push(ExprNode::Op(FuncNode { site })))
}

/// Evaluate the arguments of `call` into `args` / `nulls`; `true` when a null reached a strict call.
#[inline(always)]
pub fn eval_args(p: &Prog<'_, '_>, st: &mut St<'_>, call: &Call, cx: &mut Cx<'_, '_>, args: &mut [Datum; MAX_ARGS], nulls: &mut [bool; MAX_ARGS]) -> PgResult<bool> {
    let n = call.nargs as usize;
    let mut any_null = false;
    for i in 0..n {
        let v = eval(p, st, p.kids[call.args.start as usize + i], cx)?;
        args[i] = v.value;
        nulls[i] = v.isnull;
        any_null |= v.isnull;
    }
    Ok(any_null)
}

pub fn eval_call(p: &Prog<'_, '_>, st: &mut St<'_>, call: &Call, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let mut args = [Datum::null(); MAX_ARGS];
    let mut nulls = [false; MAX_ARGS];
    let any_null = eval_args(p, st, call, cx, &mut args, &mut nulls)?;
    invoke(p, st, call, cx, &args, &nulls, any_null)
}

/// Call the bound function over evaluated arguments.
#[inline]
pub fn invoke(p: &Prog<'_, '_>, st: &mut St<'_>, call: &Call, cx: &mut Cx<'_, '_>, args: &[Datum; MAX_ARGS], nulls: &[bool; MAX_ARGS], any_null: bool) -> PgResult<NullableDatum> {
    let n = call.nargs as usize;
    if any_null && call.strict {
        return Ok(NullableDatum::null());
    }
    match call.how {
        How::Island { root } => eval(p, st, root, cx),
        How::Fmgr(ix) => st.calls[ix as usize].call_site(&args[..n], &nulls[..n]).map(|v| int_result(call.rettype, v)),
        How::Shim { shim, convs, byref } => {
            let mut a = [Arg::V(0); MAX_ARGS];
            for i in 0..n {
                let c = p.convs[convs as usize + i];
                a[i] = match c.kind {
                    ArgKind::V | ArgKind::D => Arg::V(args[i].as_i64()),
                    ArgKind::P => Arg::R(payload_view(args[i], c.typlen, cx.region)?),
                    ArgKind::I => Arg::R(image_view(args[i], c.typlen, cx.region)?),
                };
            }
            let mut e = ErrSlot::NONE;
            let r = (shim.call)(&mut e, cx.region, call.collation, &[], &a[..n]);
            match r.status {
                gravity_fn::OK => Ok(NullableDatum::value(if byref { Datum::from_usize(r.val as usize) } else { Datum::from_i64(r.val) })),
                gravity_fn::NULL => Ok(NullableDatum::null()),
                gravity_fn::ERROR => Err(e.take().expect("a shim's ERROR carries its error")),
                _ => Err(no_space()),
            }
        }
    }
}

/// D2311: an int2 / int4 result's word in the convention the emitted code reads it (sign-extended: prim.rs open-codes
/// int2 / int4 arithmetic and comparisons over the whole word, as `Int32GetDatum` / `Int16GetDatum` form it). A function-
/// manager callee (or an island's interpreter) hands back the word as the body left it, and a hash body's
/// `UInt32GetDatum` / `hash_any` word is zero-extended, which C reads through DatumGetInt32 but a native `< 0`, `max` or
/// `sum` would read as a large positive value. Every other result type keeps the callee's word (an oid, xid or float4
/// word stays zero-extended). A shim does this itself (scripts/gen-gravity-function-tiers.py `enc`; gravity_fn's
/// differential pins every int2 / int4 shim's word sign-extended).
#[inline(always)]
pub fn int_word(rettype: Oid, w: i64) -> i64 {
    match rettype {
        INT2OID => w as i16 as i64,
        INT4OID => w as i32 as i64,
        _ => w,
    }
}

/// D2336: the type a site's result word is canonized (`int_word`) and read (percode's word types) as. A domain's word
/// is its base type's (a domain over int4 is an int4 word, so a hash body's zero-extended word is sign-extended as for
/// int4); a built-in type (below `FirstGenbkiObjectId`) is its own base and takes no lookup.
pub fn word_type(catalog: &dyn crate::catalog::Catalog<'_>, rettype: Oid) -> Oid {
    if rettype < FIRST_GENBKI_OBJECT_ID {
        return rettype;
    }
    let mut typmod = -1;
    catalog.base_type(rettype, &mut typmod).unwrap_or(rettype)
}

/// `int_word` over a call's non-null result.
#[inline(always)]
pub fn int_result(rettype: Oid, v: NullableDatum) -> NullableDatum {
    if v.isnull || !matches!(rettype, INT2OID | INT4OID) {
        return v;
    }
    NullableDatum::value(Datum::from_i64(int_word(rettype, v.value.as_i64())))
}

/// transam.h `FirstGenbkiObjectId`: every type below it is a built-in pg_type.dat row (never a domain).
const FIRST_GENBKI_OBJECT_ID: Oid = 10000;
const INT2OID: Oid = 21;
const INT4OID: Oid = 23;
const PROCEDURE_RELATION_ID: Oid = 1255;
const ACL_EXECUTE: u64 = 1 << 7;

leader_tls::leader_tls! {
    thread_local! {
        static ACL_DEFERRED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// For the guard's life `execute_gate` fires the hook and checks nothing: the graph admission
/// rechecks the plan's functions at every execution instead (lower.md §14.13 item 4). D4112: its
/// drop restores the flag on an ERROR that unwinds out of the lowering too.
#[must_use = "the deferral ends when the guard drops"]
pub struct AclDefer(bool);

pub fn acl_defer() -> AclDefer {
    AclDefer(ACL_DEFERRED.with(|d| d.replace(true)))
}

impl Drop for AclDefer {
    fn drop(&mut self) {
        ACL_DEFERRED.with(|d| d.set(self.0));
    }
}

/// The in-process test rigs' bare pair; production lowering holds an `AclDefer` (D4112).
pub fn acl_defer_begin() {
    ACL_DEFERRED.with(|d| d.set(true));
}

pub fn acl_defer_end() {
    ACL_DEFERRED.with(|d| d.set(false));
}

/// C `ExecInitFunc`'s gate: `object_aclcheck(PROCEDURE, funcid, GetUserId(), ACL_EXECUTE)`
/// with C's text on denial, then the OAT_FUNCTION_EXECUTE object-access hook.
pub fn execute_gate(funcid: Oid) -> PgResult<()> {
    if ACL_DEFERRED.with(|d| d.get()) {
        return objectaccess::InvokeFunctionExecuteHook(funcid);
    }
    let userid = miscinit_seams::get_user_id::call();
    if aclchk_seams::object_aclcheck::call(PROCEDURE_RELATION_ID, funcid, userid, ACL_EXECUTE)? != 0 {
        return Err(permission_denied("function", funcid)?);
    }
    objectaccess::InvokeFunctionExecuteHook(funcid)
}

/// C `ExecInitAgg`'s gate on an Aggref's `aggfnoid` (nodeAgg.c): `object_aclcheck(PROCEDURE, aggfnoid, GetUserId(),
/// ACL_EXECUTE)`, denied as OBJECT_AGGREGATE ("permission denied for aggregate X", privileges#562), then the
/// OAT_FUNCTION_EXECUTE hook. Never deferred: the graph admission's per-execution recheck calls it.
pub fn aggregate_gate(aggfnoid: Oid) -> PgResult<()> {
    let userid = miscinit_seams::get_user_id::call();
    if aclchk_seams::object_aclcheck::call(PROCEDURE_RELATION_ID, aggfnoid, userid, ACL_EXECUTE)? != 0 {
        return Err(permission_denied("aggregate", aggfnoid)?);
    }
    objectaccess::InvokeFunctionExecuteHook(aggfnoid)
}

#[cold]
#[inline(never)]
fn permission_denied(what: &str, funcid: Oid) -> PgResult<Box<types_error::PgError>> {
    let cx = gravity_fn::session::root_context("gravity_expr permission_denied"); // alloc: Cold
    let name = lsyscache::get_func_name(cx.mcx(), funcid)?;
    let name = name.as_ref().map(|n| n.as_str()).unwrap_or("(unknown)");
    let e = Box::new(types_error::PgError::error(format!("permission denied for {what} {name}")).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)); // alloc: Cold
    drop(name);
    Ok(e)
}

/// Item 4: an argument tree's type, collation and typmod are nodes_core's complete ports of C `exprType`,
/// `exprCollation` and `exprTypmod` (-1 where it names none), with no private copy here; lowering types a record
/// descriptor's columns through them (D1887).
pub use nodes_core::{expr_collation, expr_type, expr_typmod};
