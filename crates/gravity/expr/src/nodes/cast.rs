//! Casts: `RelabelType` (no code), `CoerceViaIO` (the output function then the input function as
//! two call sites), `CoerceToDomain` (the constraints from the catalog: NOT NULL, then each CHECK
//! with the value in `Cx::domainval`; violations raise `23502` / `23514`).

use mcx::GrowIn;
use gravity_fn::{Addr, Arg, ArgKind, Datum, ErrSlot, Family, NullableDatum, Region, Ret, Shim, ERROR};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{CoerceToDomain as CoerceToDomainTree, CoerceViaIO as CoerceViaIOTree, RelabelType};

use crate::catalog::DomainConstraint;
use crate::eval::{eval, Cx};
use crate::nodes::func::{bind_typed, invoke, ArgConv, Call, How, MAX_ARGS};
use crate::nodes::{Builder, ExprNode, Kids, NodeId, Prog, SiteId, St};
use crate::CompileResult;

const CSTRINGOID: Oid = 2275;
const OIDOID: Oid = 26;
const INT4OID: Oid = 23;
const TEXTOID: Oid = 25;
const VARCHAROID: Oid = 1043;
const TEXTOUT: Oid = 47;
const VARCHAROUT: Oid = 1047;

#[derive(Clone, Copy, Debug)]
pub struct Relabel {
    pub arg: NodeId,
    /// D858: the word's Datum convention across the relabel (`relabel_canon`).
    pub canon: Canon,
}

/// D858: how a binary-coercible relabel re-forms its word. A `RelabelType` emits no code, but the word conventions of
/// its two types can differ: `ObjectIdGetDatum` / `TransactionIdGetDatum` / `CommandIdGetDatum` zero-extend a 4-byte
/// value, `Int32GetDatum` sign-extends. The open-coded int4 forms read the whole word (a compare, the range check of
/// the narrow arithmetic, the widening to int8), so `o::int4` over o = 4294967295 must read -1, as C's `DatumGetInt32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Canon {
    /// Same convention: the word as is.
    Keep,
    /// A zero-extended 4-byte type to int4: truncate to 32 bits, sign-extend.
    Sext32,
    /// int4 to a zero-extended 4-byte type: truncate to 32 bits, zero-extend.
    Zext32,
}

impl Relabel {
    /// D858: the relabel leaves the word as it is (`Canon::Keep`): a form that reads the argument's word (a lane, a bare
    /// column) may look through it; any other relabel re-forms the word and is looked through by none.
    pub fn transparent(&self) -> bool {
        self.canon == Canon::Keep
    }
}

/// D858: the canon of a relabel from `from` to `to` (base types).
pub fn relabel_canon(from: Oid, to: Oid) -> Canon {
    match (from, to) {
        (f, INT4OID) if gravity_fn::typesem::expr::zext4(f) => Canon::Sext32,
        (INT4OID, t) if gravity_fn::typesem::expr::zext4(t) => Canon::Zext32,
        _ => Canon::Keep,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CoerceViaIO {
    pub arg: NodeId,
    pub out: SiteId,
    pub inp: SiteId,
    pub ioparam: Oid,
    /// The argument is text or varchar, whose output function hands back its own characters, and the input function is a
    /// shim taking a view: the input reads the argument's payload directly (no output call, no cstring copy).
    pub direct: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CoerceToDomain {
    pub arg: NodeId,
    pub typ: Oid,
    pub notnull: bool,
    /// The CHECK expressions; `names + i` indexes `Program::names` for the i-th.
    pub checks: Kids,
    pub names: u32,
    /// The emitted raise (`DOMAIN_RAISE`): a host site whose cell carries the built error (typ, the check's index).
    pub site: SiteId,
}

pub fn compile_relabel<'mcx>(b: &mut Builder<'_, 'mcx>, r: &RelabelType<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(r.arg)?;
    // a relabel to its argument's own type keeps the word (no catalog lookup: a composite's or a typmod-only relabel)
    let from = nodes_core::expr_type(r.arg);
    let canon = if from == r.resulttype {
        Canon::Keep
    } else {
        let (mut m1, mut m2) = (-1, -1);
        relabel_canon(b.catalog.base_type(from, &mut m1)?, b.catalog.base_type(r.resulttype, &mut m2)?)
    };
    Ok(b.push(ExprNode::Relabel(Relabel { arg, canon })))
}

pub fn compile_coerce_via_io<'mcx>(b: &mut Builder<'_, 'mcx>, c: &CoerceViaIOTree<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(c.arg)?;
    let argtype = nodes_core::expr_type(c.arg);
    let mut typmod = -1;
    let argbase = b.catalog.base_type(argtype, &mut typmod)?;
    let out_io = b.catalog.io(argbase)?;
    let in_io = b.catalog.io(c.resulttype)?;
    let out = bind_typed(b, out_io.output, Kids::NONE, &[argbase], 0)?;
    // textout / varcharout give back the value's own characters (a text holds no NUL), so a shim input function that reads
    // a view takes the text's payload as the cstring it would have read; a three-argument one (numeric_in, timestamp_in)
    // takes the type's I/O parameter and typmod -1 as words, as the cstring road passes them
    let direct_nargs = b.catalog.shim(in_io.input).and_then(|s| match (s.nargs, s.args) {
        _ if !s.tyargs.is_empty() => None,
        (1, &[ArgKind::P]) => Some(1),
        (3, &[ArgKind::P, ArgKind::V, ArgKind::V]) => Some(3),
        _ => None,
    });
    let direct = matches!((argbase, out_io.output), (TEXTOID, TEXTOUT) | (VARCHAROID, VARCHAROUT)) && direct_nargs.is_some();
    // C's InputFunctionCall passes three arguments, which a one-argument input function (int4in, uuid_in, textin) never
    // reads; binding its own arity lets such a function's shim serve the cast instead of the fmgr round trip
    // (D3043: only through an output function whose text reads no GUC, `types_core::quiet::GUC_FREE_OUT`; any other type's
    // text may, and a function's own `SET` clause changes it inside the call, so such a cast keeps InputFunctionCall's three
    // arguments, the hosted road: heldout dt-0063 `ts::text LIKE` under `SET datestyle = 'SQL, DMY'` at W8, D3029)
    let in_nargs = if types_core::quiet::guc_free_out(out_io.output) { b.catalog.func(in_io.input)?.nargs.clamp(1, 3) as usize } else { 3 };
    let inp = match direct {
        true => bind_typed(b, in_io.input, Kids::NONE, &[argbase, OIDOID, INT4OID][..direct_nargs.unwrap_or(1)], 0)?,
        false => bind_typed(b, in_io.input, Kids::NONE, &[CSTRINGOID, OIDOID, INT4OID][..in_nargs], 0)?,
    };
    let direct = direct && matches!(inp.how, How::Shim { .. });
    let out = b.site(out);
    let inp = b.site(inp);
    Ok(b.push(ExprNode::CoerceViaIO(CoerceViaIO { arg, out, inp, ioparam: in_io.ioparam, direct })))
}

pub fn compile_coerce_to_domain<'mcx>(b: &mut Builder<'_, 'mcx>, c: &CoerceToDomainTree<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(c.arg)?;
    let facts = b.catalog.domain(c.resulttype)?;
    let mut notnull = false;
    // the CHECKs' range first, each compiled into its slot (`Builder::kid_slots`)
    let checks = b.kid_slots(facts.constraints.iter().filter(|c| matches!(c, DomainConstraint::Check { .. })).count());
    let mut k = checks.start as usize;
    // D2019: the error's names, read here on the backend (a pool thread has no catalog): `DOMAIN_NAMES` entries before the
    // CHECKs' own, which the raise reads from its cell (`EmitState`'s aux) and the interpreter from `names - DOMAIN_NAMES`
    let (shown, typname, nspname) = domain_names(c.resulttype);
    b.name(&shown);
    b.name(&typname);
    b.name(&nspname);
    let names = b.prog.names.len() as u32;
    for con in &facts.constraints {
        match con {
            DomainConstraint::NotNull => notnull = true,
            DomainConstraint::Check { name, expr } => {
                let id = b.compile(*expr)?;
                b.prog.kids[k] = id;
                k += 1;
                b.name(name);
            }
        }
    }
    // the emitted raise is a host site (a shim with no address in the IR: the type and the check's index are its
    // words, the message is built in the cold call); the interpreter builds the error itself
    let convs = b.prog.convs.len() as u32;
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    let site = b.site(Call { funcid: 0, args: Kids::NONE, nargs: 3, strict: true, collation: 0, how: How::Shim { shim: &DOMAIN_RAISE, convs, byref: false }, rettype: 0, volatile: false });
    Ok(b.push(ExprNode::CoerceToDomain(CoerceToDomain { arg, typ: c.resulttype, notnull, checks, names, site })))
}

gravity_abi::host! {
    #[unwind]
    unsafe fn no_addr() {}
}

/// The names a domain's error reads, baked at compile time ahead of its CHECKs' names (`Program::names`): the domain as
/// `format_type_be` shows it, its `typname` and its schema (C `errdatatype`).
pub const DOMAIN_NAMES: u32 = 3;

/// The domain raise as a shim (`emit/scalar.rs` calls it only where a constraint failed): `args[0]` the domain's
/// type, `args[1]` the failing CHECK's index in the domain's constraint order, or -1 for NOT NULL, `args[2]` the address
/// of its baked names (`DOMAIN_NAMES`, then the CHECKs'), the site cell's aux word: the raise runs on any thread (D2019).
pub static DOMAIN_RAISE: Shim = Shim { oid: 0, name: "domain check", family: Family::None, nargs: 3, coll: false, region: false, args: &[ArgKind::V, ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: domain_raise };

fn domain_raise(e: *mut ErrSlot, _region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let (Arg::V(typ), Arg::V(ix), Arg::V(names)) = (a[0], a[1], a[2]) else { return Ret { val: 0, status: ERROR } };
    // SAFETY: `names` is the program's own `names` table at the domain's entries (`EmitState::new` sets the cell's aux
    // word), live for the program's life; the CHECK `ix` is one of the domain's (the emitter's index).
    let baked = |n: usize| unsafe { *(names as *const &'static str).add(n) };
    let err = match (names, ix) {
        (0, ix) if ix < 0 => domain_null(typ as Oid),
        (0, ix) => domain_check_at(typ as Oid, ix as usize),
        (_, ix) if ix < 0 => domain_null_named(typ as Oid, [baked(0), baked(1), baked(2)]),
        (_, ix) => domain_check_named(typ as Oid, [baked(0), baked(1), baked(2)], baked(DOMAIN_NAMES as usize + ix as usize)),
    };
    // SAFETY: the host passes the cell's live slot; a previous error in it is dropped through `take`.
    unsafe {
        drop((*e).take());
        (*e).err = gravity_abi::word(Box::into_raw(err));
    }
    Ret { val: 0, status: ERROR }
}

/// The error of the domain's `ix`-th CHECK: its name read again from the type cache (the cold path, as the
/// message's type name is) — the program carries no pointer to it.
#[cold]
#[inline(never)]
fn domain_check_at(typ: Oid, ix: usize) -> Box<PgError> {
    let Ok(cref) = typcache::DomainConstraintRef::init(typ) else { return domain_check(typ, "?") };
    match cref.constraints().iter().filter(|c| matches!(c.constrainttype, typcache::DomConstraintType::Check)).nth(ix) {
        Some(c) => domain_check(typ, c.name),
        None => domain_check(typ, "?"),
    }
}

pub fn eval_coerce_via_io(p: &Prog<'_, '_>, st: &mut St<'_>, n: &CoerceViaIO, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    let text = via_io_out(p, st, n, v, cx)?;
    via_io_in(p, st, n, text, cx)
}

/// The input half of `CoerceViaIO` over the output function's text (C's `InputFunctionCallSafe` step).
pub fn via_io_in(p: &Prog<'_, '_>, st: &mut St<'_>, n: &CoerceViaIO, text: NullableDatum, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let mut args = [Datum::null(); MAX_ARGS];
    let mut nulls = [false; MAX_ARGS];
    args[0] = text.value;
    nulls[0] = text.isnull;
    args[1] = Datum::from_u32(n.ioparam);
    args[2] = Datum::from_i32(-1);
    invoke(p, st, &p.sites[n.inp.0 as usize], cx, &args, &nulls, text.isnull)
}

/// The output half of `CoerceViaIO`: the argument's text, NULL through.
pub fn via_io_out(p: &Prog<'_, '_>, st: &mut St<'_>, n: &CoerceViaIO, v: NullableDatum, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    if v.isnull || n.direct {
        return Ok(v);
    }
    let mut args = [Datum::null(); MAX_ARGS];
    args[0] = v.value;
    invoke(p, st, &p.sites[n.out.0 as usize], cx, &args, &[false; MAX_ARGS], false)
}

pub fn eval_coerce_to_domain(p: &Prog<'_, '_>, st: &mut St<'_>, n: &CoerceToDomain, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    domain_checks(p, st, n, v, cx).map(|()| v)
}

/// The domain's NOT NULL and CHECKs over an evaluated value (`23502` / `23514`).
pub fn domain_checks(p: &Prog<'_, '_>, st: &mut St<'_>, n: &CoerceToDomain, v: NullableDatum, cx: &mut Cx<'_, '_>) -> PgResult<()> {
    if n.notnull && v.isnull {
        return Err(domain_null_named(n.typ, baked(p, n)));
    }
    if n.checks.len == 0 {
        return Ok(());
    }
    let saved = cx.domainval;
    cx.domainval = v;
    let r = eval_checks(p, st, n, cx);
    cx.domainval = saved;
    r
}

fn eval_checks(p: &Prog<'_, '_>, st: &mut St<'_>, n: &CoerceToDomain, cx: &mut Cx<'_, '_>) -> PgResult<()> {
    let kids = &p.kids[n.checks.start as usize..n.checks.start as usize + n.checks.len as usize];
    for (i, &k) in kids.iter().enumerate() {
        let c = eval(p, st, k, cx)?;
        if !c.isnull && !c.value.as_bool() {
            return Err(domain_check_named(n.typ, baked(p, n), p.names[n.names as usize + i]));
        }
    }
    Ok(())
}

/// The domain's baked names (`DOMAIN_NAMES`, just before its CHECKs').
fn baked(p: &Prog<'_, '_>, n: &CoerceToDomain) -> [&'static str; 3] {
    let at = n.names as usize - DOMAIN_NAMES as usize;
    [p.names[at], p.names[at + 1], p.names[at + 2]]
}

/// The domain's names for its errors, read on the backend at compile time: as `format_type_be` shows it (the OID's text
/// if the lookup fails, as before), its `typname` and its schema (empty when unknown: no field set).
fn domain_names(typ: Oid) -> (String, String, String) {
    let shown = format_type::format_type_be(typ).unwrap_or_else(|_| typ.to_string()); // alloc: Program
    let mut e = PgError::error(String::new()); // alloc: Program
    errdatatype(&mut e, typ);
    (shown, e.datatype_name.unwrap_or_default(), e.schema_name.unwrap_or_default())
}

/// D3551: the names `domain_names` bakes, for the content and front keys (lower inputs/key.rs, exec inputs/front.rs): a
/// domain renamed, moved to another schema or shown otherwise under another search_path is another program, not one baked
/// with its old name. None where no catalog is installed (a test rig's key walk).
pub fn domain_key_names(typ: Oid) -> Option<(String, String, String)> {
    syscache_seams::pg_type_domain_shape::is_installed().then(|| domain_names(typ))
}

#[cold]
#[inline(never)]
fn domain_null_named(typ: Oid, names: [&str; 3]) -> Box<PgError> {
    let mut e = PgError::error(format!("domain {} does not allow null values", names[0])).with_sqlstate(types_error::ERRCODE_NOT_NULL_VIOLATION); // alloc: Cold
    datatype_named(&mut e, typ, names);
    Box::new(e) // alloc: Cold
}

#[cold]
#[inline(never)]
fn domain_check_named(typ: Oid, names: [&str; 3], con: &str) -> Box<PgError> {
    let mut e = PgError::error(format!("value for domain {} violates check constraint \"{con}\"", names[0])).with_sqlstate(types_error::ERRCODE_CHECK_VIOLATION); // alloc: Cold
    datatype_named(&mut e, typ, names);
    e.constraint_name = Some(con.to_string()); // alloc: Cold
    Box::new(e) // alloc: Cold
}

/// `errdatatype` from the baked names; a name the compile could not read (empty) is read now (the backend's thread).
fn datatype_named(e: &mut PgError, typ: Oid, names: [&str; 3]) {
    if names[1].is_empty() {
        errdatatype(e, typ);
        return;
    }
    e.datatype_name = Some(names[1].to_string()); // alloc: Cold
    e.schema_name = (!names[2].is_empty()).then(|| names[2].to_string()); // alloc: Cold
}

#[cold]
#[inline(never)]
pub fn domain_null(typ: Oid) -> Box<PgError> {
    let name = format_type::format_type_be(typ).unwrap_or_else(|_| typ.to_string()); // alloc: Cold
    let mut e = PgError::error(format!("domain {name} does not allow null values")).with_sqlstate(types_error::ERRCODE_NOT_NULL_VIOLATION); // alloc: Cold
    errdatatype(&mut e, typ);
    Box::new(e) // alloc: Cold
}

#[cold]
#[inline(never)]
pub fn domain_check(typ: Oid, con: &str) -> Box<PgError> {
    let name = format_type::format_type_be(typ).unwrap_or_else(|_| typ.to_string()); // alloc: Cold
    let mut e = PgError::error(format!("value for domain {name} violates check constraint \"{con}\"")).with_sqlstate(types_error::ERRCODE_CHECK_VIOLATION); // alloc: Cold
    errdatatype(&mut e, typ);
    e.constraint_name = Some(con.to_string()); // alloc: Cold
    Box::new(e) // alloc: Cold
}

/// C `errdatatype`: the domain's name and schema on the error.
fn errdatatype(e: &mut PgError, typ: Oid) {
    if let Ok(Some(t)) = syscache_seams::pg_type_domain_shape::call(typ) {
        e.datatype_name = core::str::from_utf8(t.typname.name_str()).ok().map(|s| s.to_string()); // alloc: Cold
        let cx = gravity_fn::session::root_context("gravity_expr errdatatype"); // alloc: Cold
        let nsp = lsyscache::get_namespace_name(cx.mcx(), t.typnamespace);
        if let Ok(Some(nsp)) = &nsp {
            e.schema_name = Some(nsp.as_str().to_string()); // alloc: Cold
        }
        drop(nsp);
    }
}
