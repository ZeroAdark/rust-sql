//! The hosted aggregate bracket's binding (a `#[path]` child of hosts v4/m6_agg.rs, split by responsibility): each
//! aggregate's catalog shape and ExecInitAgg's EXECUTE checks on the leader, once; per slot, its transfn / finalfn /
//! combinefn bound with build_aggregate_*fn_expr's carriers and their frames. Created by M6-HOSTS.
use core::ptr::NonNull;
use core::sync::atomic::AtomicU64;
use std::ffi::CString;

use datum::NullableDatum;
use gravity_decompose::effects::Parallel;
use mcx::{GrowIn, Mcx, MemoryContext};
use syscache_seams::PgAggregateShape;
use types_core::fmgr::AggFnArgTypes;
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_INSUFFICIENT_PRIVILEGE};
use types_fmgr::{AggStateNode, FmgrInfo, LocalFcinfo};
use types_nodes::primnodes::{Aggref, AGGKIND_NORMAL, AGGSPLIT_SIMPLE};

mod raw;

use crate::ops::agg::trans::hosted::Slot;

const ACL_EXECUTE: u64 = 1 << 7;
const PROCEDURE_RELATION_ID: Oid = 1255;

const ENUM_SMALLER: Oid = 3524;
const ENUM_LARGER: Oid = 3525;

pub(crate) type Frame = LocalFcinfo<100>;
type Carriers = Vec<Box<AggFnArgTypes>>;

/// One hosted aggregate: a plain (`AGGSPLIT_SIMPLE`, no unsorted ORDER BY but a single-input DISTINCT's, D1210, or an
/// ordered-set / hypothetical aggregate's WITHIN GROUP, which its transfn sorts, V4R-425) aggregate's catalog shape, the
/// transition type's shape, the initcond and the result's length (v1 `aggstate.rs`'s `GenericAgg`), an ordered-set
/// aggregate's direct arguments, and a DISTINCT aggregate's sort (m6_agg_distinct.rs).
pub(crate) struct Spec {
    pub(crate) a: &'static Aggref<'static>,
    pub(crate) shape: PgAggregateShape,
    /// The transfn's inputs: the aggregated arguments (an ordered-set aggregate's direct ones are its finalfn's).
    pub(crate) ninputs: usize,
    /// V4R-425: an ordered-set / hypothetical aggregate's direct arguments (constants, copied into `keep`), the finalfn's
    /// arguments after the state (ExecInitAgg's numFinalArgs; finalextra's aggregated ones NULL after them). D1520: `None`
    /// for one that is no constant, which each final is given (`m6_agg_final_direct`, the group's value).
    pub(crate) direct: Vec<Option<NullableDatum>>, // alloc: Exec
    pub(crate) translen: i16,
    pub(crate) transbyval: bool,
    pub(crate) initval: NullableDatum,
    pub(crate) reslen: i16,
    /// D2337 / D2336: the result's word type (the aggregate's result type, a domain's base type): an int2 / int4 result
    /// word is sign-extended at the final (`hosted::result_word`).
    pub(crate) resword: Oid,
    pub(crate) aggref: NonNull<()>,
    pub(crate) distinct: Option<crate::ops::group::hosted_distinct::Sorted>,
    pub(crate) shared: bool, // D1463: ExecInitAgg's aggshared (another aggregate on this aggtransno, V4R-424): AggStateIsShared
}

/// One aggregate's calls in one slot: its transfn, finalfn and combinefn (when it has one) bound with their frames.
pub(crate) struct Calls {
    pub(crate) transfn: FmgrInfo,
    pub(crate) trans: Box<Frame>, // alloc: Exec
    pub(crate) finalfn: Option<(FmgrInfo, Box<Frame>, usize)>,
    pub(crate) combine: Option<(FmgrInfo, Box<Frame>)>,
    /// A DISTINCT aggregate's equality functions (each DISTINCT column's eqop).
    pub(crate) eq: Vec<FmgrInfo>, // alloc: Exec
    pub(crate) _carriers: Carriers,
}

fn denied(what: &str, f: Oid) -> Box<PgError> {
    let name = syscache_seams::pg_proc_proname::call(f).ok().flatten().map_or(f.to_string(), |n| String::from_utf8_lossy(n.name_str()).to_string()); // alloc: Cold
    Box::new(PgError::error(format!("permission denied for {what} {name}")).with_sqlstate(ERRCODE_INSUFFICIENT_PRIVILEGE)) // alloc: Cold
}

/// ExecInitAgg's EXECUTE check (`what` names the object) as `user`, then the function-execute hook.
fn acl(what: &str, f: Oid, user: Oid) -> PgResult<()> {
    if aclchk_seams::object_aclcheck::call(PROCEDURE_RELATION_ID, f, user, ACL_EXECUTE)? != 0 {
        return Err(denied(what, f));
    }
    objectaccess::InvokeFunctionExecuteHook(f)
}

/// memsig E-pub t3: a carrier's argument types, `first` then `rest`, in the arena's init context `keep` (`raw::static_oids`'s
/// contract: it outlives the slots).
fn arg_types(keep: Mcx<'_>, first: Oid, rest: impl Iterator<Item = Oid>) -> &'static [Oid] {
    let mut v = mcx::PgVec::new_in(keep);
    v.push_in(first);
    v.extend_in(rest);
    raw::static_oids(v.leak())
}

/// `f` bound with build_aggregate_transfn_expr / _finalfn_expr's carrier (`get_fn_expr_argtype`; `variadic`: the
/// transfn's funcvariadic, the Aggref's aggvariadic, `get_fn_expr_variadic`), its frame of `n`.
fn bind(f: Oid, rettype: Oid, argtypes: &'static [Oid], coll: Oid, variadic: bool, carriers: &mut Carriers) -> PgResult<(FmgrInfo, Box<Frame>)> {
    let mut info = fmgr_core::fmgr_info(f)?;
    let carrier = Box::new(AggFnArgTypes { rettype, argtypes, variadic, stable: 0, inputcollid: coll }); // alloc: Exec
    // as above, the carrier's box outlives the FmgrInfo
    info.fn_expr = Some(raw::fn_expr(&carrier));
    carriers.push(carrier); // alloc: Exec
    let mut frame = Box::new(Frame::new(coll)); // alloc: Exec
    frame.rearm(coll);
    Ok((info, frame))
}

fn lookup_failed(what: &str, oid: Oid) -> Box<PgError> {
    Box::new(PgError::error(format!("cache lookup failed for {what} {oid}"))) // alloc: Cold
}

impl Calls {
    /// `s`'s transfn / finalfn (and combinefn when it has one: a parallel merge, or two partials of one group in a keyed
    /// fold at any width, D798) bound for one slot.
    /// memsig E-pub t3: `keep`, the arena's init context, holds the carriers' argument types.
    fn bind(s: &Spec, keep: Mcx<'_>) -> PgResult<Calls> {
        let (a, tt) = (s.a, s.a.aggtranstype);
        let nd = s.direct.len(); // V4R-425: the direct arguments lead aggargtypes
        let mut carriers = Vec::new(); // alloc: Exec
        let finalfn = match s.shape.aggfinalfn {
            0 => None,
            f => {
                let n = if s.shape.aggfinalextra { 1 + a.aggargtypes.iter().count() } else { 1 + nd };
                let types = arg_types(keep, tt, a.aggargtypes.iter().take(n - 1));
                let (info, frame) = bind(f, a.aggtype, types, a.inputcollid, false, &mut carriers)?;
                Some((info, frame, n))
            }
        };
        let combine = match s.shape.aggcombinefn {
            0 => None,
            f => Some(bind(f, tt, arg_types(keep, tt, core::iter::once(tt)), a.inputcollid, false, &mut carriers)?),
        };
        let types = arg_types(keep, tt, a.aggargtypes.iter().skip(nd)); // build_aggregate_transfn_expr: the aggregated arguments
        let (mut transfn, trans) = bind(s.shape.aggtransfn, tt, types, a.inputcollid, a.aggvariadic, &mut carriers)?;
        // V4R-453 (D1409): enum_larger / enum_smaller find their enum type by the value's pg_enum row (ENUMOID) on first
        // call and keep it in fn_extra (adt_enum enum_cmp_internal's carrier, an Oid): set here on the leader, from the
        // argument type, so a pool claim never makes that lookup
        if matches!(s.shape.aggtransfn, ENUM_SMALLER | ENUM_LARGER) {
            transfn.set_fn_extra(tt);
        }
        let combine = combine.map(|(mut f, fr)| {
            if matches!(f.fn_oid, ENUM_SMALLER | ENUM_LARGER) {
                f.set_fn_extra(tt);
            }
            (f, fr)
        });
        let eq = s.distinct.as_ref().map_or(&[][..], |d| &d.eqfns).iter().map(|&f| fmgr_core::fmgr_info(f)).collect::<PgResult<_>>()?;
        Ok(Calls { transfn, trans, finalfn, combine, eq, _carriers: carriers })
    }
}

/// The aggregates' specs and whether the arena is parallel at `width` (every aggregate with a combinefn and `PARALLEL
/// SAFE`, PG's rule, Decision 12); the initconds in `keep`.
pub(crate) fn specs(aggrefs: &[&'static Aggref<'static>], width: u16, keep: &MemoryContext) -> PgResult<(Vec<Spec>, bool)> {
    let user = miscinit_seams::get_user_id::call();
    let mut specs = Vec::with_capacity(aggrefs.len()); // alloc: Exec
    let mut parallel = width > 1;
    // bounded("other: the Agg node's aggregate calls, one bind each (catalog rows, EXECUTE checks, the initcond): the query text bounds them, as PG's ExecInitAgg walks the same aggregates with no poll")
    for &a in aggrefs {
        let distinct = crate::ops::group::hosted_distinct::Sorted::of(a)?; // D1210: its ORDER BY is the DISTINCT's own sort
        let ordered_set = a.aggkind != AGGKIND_NORMAL; // V4R-425: the transfn sorts its WITHIN GROUP input itself
        if a.aggsplit != AGGSPLIT_SIMPLE || !ordered_set && distinct.is_none() && !a.aggorder.is_nil() && !a.aggpresorted {
            return Err(Box::new(PgError::error(format!("gravity: aggregate {} is no plain hosted aggregate", a.aggfnoid)))); // alloc: Cold
        }
        acl("aggregate", a.aggfnoid, user)?;
        let shape = syscache_seams::lookup_pg_aggregate_shape::call(a.aggfnoid)?.ok_or_else(|| lookup_failed("aggregate", a.aggfnoid))?;
        let owner = syscache_seams::lookup_pg_proc_secdef::call(a.aggfnoid)?.ok_or_else(|| lookup_failed("function", a.aggfnoid))?.proowner;
        let safe = crate::facts::proc_parallel(a.aggfnoid)?.ok_or_else(|| lookup_failed("function", a.aggfnoid))? == Parallel::Safe;
        // PG's partial-aggregate rule (planner.c's get_agg_clause_costs hasNonPartial): no DISTINCT and no ORDER BY in the call
        parallel &= safe && shape.aggcombinefn != 0 && distinct.is_none() && a.aggorder.is_nil() && a.aggdistinct.is_nil();
        if !gravity_fn::typesem::collation::armed_by_default(a.inputcollid) {
            // V4R-452 (D1409a): resolved here, before the section's StatementArm::capture, so a pool claim finds it armed
            gravity_fn::typesem::collation::Facts::of(a.inputcollid)?;
        }
        if shape.aggfinalfn != 0 {
            acl("function", shape.aggfinalfn, owner)?;
        }
        acl("function", shape.aggtransfn, owner)?;
        let tt = a.aggtranstype;
        let (translen, transbyval) = gravity_fn::typesem::storage::len_byval(tt)?;
        let initval = match syscache_seams::pg_aggregate_agginitval::call(keep.mcx(), a.aggfnoid)?.ok_or_else(|| lookup_failed("aggregate", a.aggfnoid))? {
            None => NullableDatum::null(),
            Some(text) => {
                let (input, ioparam) = lsyscache::getTypeInputInfo(tt)?;
                let cstr = CString::new(text.as_bytes()).map_err(|_| PgError::error("agginitval contains a NUL"))?;
                // D1280: the input function's by-reference result lives only as long as its FmgrInfo: copied into `keep`
                // before the FmgrInfo drops (GetAggInitVal's datumCopy; WINDOW's m10_aggs.rs)
                let mut flinfo = fmgr_core::fmgr_info(input)?;
                let v = types_fmgr::input_function_call(&mut flinfo, Some(&cstr), ioparam, -1, keep.mcx())?;
                // a non-NULL by-reference datum the input function made, alive while `flinfo` is
                let v = if transbyval { v } else { raw::datum_copy(keep.mcx(), v, translen)? };
                drop(flinfo);
                NullableDatum { value: v, isnull: false }
            }
        };
        let mut direct = Vec::new(); // alloc: Exec
        // bounded("small-fixed: one direct argument of an ordered-set aggregate, at most FUNC_MAX_ARGS (100)")
        for n in a.aggdirectargs.iter() {
            let Some(c) = n.as_const() else {
                // D1520: evaluated per group over the group's row, given to each final
                direct.push(None); // alloc: Exec
                continue;
            };
            // a non-NULL by-reference constant of the live plan (its image read, copied into `keep`)
            let v = gravity_decompose::bake::read(c);
            let v = if c.constisnull || c.constbyval { v } else { raw::datum_copy(keep.mcx(), v, c.constlen as i16)? };
            direct.push(Some(NullableDatum { value: v, isnull: c.constisnull })); // alloc: Exec
        }
        let reslen = match gravity_fn::typesem::storage::len_byval(a.aggtype)? { (_, true) => 0, (len, false) => len };
        let resword = lsyscache::typ::getBaseType(a.aggtype)?;
        specs.push(Spec { a, shape, ninputs: a.aggargtypes.iter().count() - direct.len(), direct, translen, transbyval, initval, reslen, resword, aggref: NonNull::from(a).cast(), distinct, shared: a.aggtransno >= 0 && aggrefs.iter().filter(|o| o.aggtransno == a.aggtransno).count() > 1 }); // alloc: Exec
    }
    if parallel {
        // ExecInitAgg's combine phase checks the combinefn as the aggregate's owner too
        // bounded("other: the Agg node's aggregate calls, one EXECUTE check of its combine function each: the query text bounds them, as PG's ExecInitAgg walks the same aggregates with no poll")
        for s in specs.iter() {
            let owner = syscache_seams::lookup_pg_proc_secdef::call(s.a.aggfnoid)?.ok_or_else(|| lookup_failed("function", s.a.aggfnoid))?.proowner;
            acl("function", s.shape.aggcombinefn, owner)?;
        }
    }
    Ok((specs, parallel))
}

impl Slot {
    /// memsig E-pub t3: `keep`, the arena's init context (`AggArena.keep`), which outlives the slot.
    pub(crate) fn new(specs: &[Spec], parallel: bool, keep: &MemoryContext) -> PgResult<Slot> {
        let aggs = specs.iter().map(|s| Calls::bind(s, keep.mcx())).collect::<PgResult<Vec<_>>>()?; // alloc: Exec
        // D2942: the aggcontext resets wholesale (C's AllocSetReset): a streamed group's start (`next_group`) and a re-open
        // (`reset`) reset it with the group's transition states still in it, as nodeAgg.c's ReScanExprContext does
        let node = Box::new(AggStateNode::new(gravity_driver::mem::root::context_wholesale("gravity agg arena", !parallel))); // alloc: Exec
        // CAND-M10: C ExecAggCopyTransValue keeps a transition's read-write expanded result made in the aggcontext, so
        // array_append grows its state in place (AggArena::advance); finals and combines read the state read-only
        node.set_expanded_trans(true);
        // D2170: a final's result image lives in `tmp` until the slot's next final, which resets it: wholesale, as PG's
        // per-tuple context reset (the bump context of the serial slot is wholesale already)
        let tmp = if parallel { MemoryContext::new_unobserved_wholesale("gravity agg arena call") } else { MemoryContext::new_bump("gravity agg arena call") };
        Ok(Slot { aggs, node, tmp, calls: 0, bytes: AtomicU64::new(0), spill: Default::default() })
    }
}
