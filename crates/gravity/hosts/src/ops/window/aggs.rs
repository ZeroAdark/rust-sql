//! The window object's aggregates (a `#[path]` child of hosts v4/m10.rs): nodeWindowAgg.c's `initialize_peragg`
//! (the moving-aggregate choice, the component functions' EXECUTE checks, the transition type, the initcond),
//! `initialize_windowaggregate`, `advance_windowaggregate` / `_base` (the inverse transition), `finalize_windowaggregate`
//! and `eval_windowaggregates` (the restart, the inverse transitions of rows leaving the frame, the saved result of a
//! frame that did not change). Each aggregate's state lives in its own aggregate context (PG shares one among the
//! aggregates without an inverse and resets it when any restarts; each of those restarts then, so the values are PG's).
//! An argument or FILTER is read from its stored cell, its error raised there as PG evaluates it. Created by M10-WINDOW.
use std::ffi::CString;

use datum::NullableDatum;
use mcx::MemoryContext;
use types_core::fmgr::AggFnArgTypes;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_fmgr::{AggStateNode, FmgrInfo, LocalFcinfo};
use types_nodes::rawnodes::*;

use crate::ops::window::object::{bad, Rt, WinSpec};
use crate::ops::window::raw::{bind, datum_copy, read_only, result_into};

const ACL_EXECUTE: u64 = 1 << 7;
const PROCEDURE_RELATION_ID: Oid = 1255;
pub(super) const AGGMODIFY_READ_ONLY: i8 = b'r' as i8;
const OBJECT_FUNCTION: i32 = types_nodes::parsenodes::ObjectType::OBJECT_FUNCTION as i32;
pub(super) type Frame = LocalFcinfo<{ types_core::FUNC_MAX_ARGS }>;

/// One aggregate (WindowStatePerAggData).
struct Agg {
    /// Its function's index (`wfuncno`).
    fno: usize,
    coll: Oid,
    transfn: FmgrInfo,
    invtransfn: Option<FmgrInfo>,
    finalfn: Option<FmgrInfo>,
    nfinal: usize,
    frame: Box<Frame>, // alloc: Exec
    translen: i16,
    transbyval: bool,
    reslen: i16,
    resbyval: bool,
    init: NullableDatum,
    node: Box<AggStateNode>, // alloc: Exec
    trans: NullableDatum,
    count: i64,
    result: NullableDatum,
    restart: bool,
    /// The argument types the bound functions' `fn_expr` points at (build_aggregate_*fn_expr's carriers).
    _carriers: Vec<(Box<AggFnArgTypes>, Box<[Oid]>)>, // alloc: Exec
}

pub(crate) struct Aggs {
    v: Vec<Agg>, // alloc: Exec
    /// PG's `aggregatedbase` / `aggregatedupto` (partition positions).
    base: i64,
    upto: i64,
    /// The initconds (the execution's), and each transition call's result context (reset after the call).
    _keep: MemoryContext,
    call_cx: MemoryContext,
}

/// ExecInitWindowAgg / initialize_peragg's EXECUTE check of `f` as `user`, then the function-execute hook.
fn acl(mcx: mcx::Mcx<'_>, f: Oid, user: Oid) -> PgResult<()> {
    let r = aclchk_seams::object_aclcheck::call(PROCEDURE_RELATION_ID, f, user, ACL_EXECUTE)?;
    if r != 0 {
        let name = lsyscache::get_func_name(mcx, f)?;
        aclchk_seams::aclcheck_error::call(r, OBJECT_FUNCTION, name.as_ref().map(|n| n.as_str()).unwrap_or(""))?;
    }
    objectaccess::InvokeFunctionExecuteHook(f)
}

fn lookup_failed(what: &str, oid: Oid) -> Box<PgError> {
    Box::new(PgError::error(format!("cache lookup failed for {what} {oid}"))) // alloc: Cold
}

impl Aggs {
    /// ExecInitWindowAgg's per-function loop: each function's EXECUTE check, and each aggregate's initialize_peragg.
    pub(crate) fn new(s: &WinSpec, root: &MemoryContext) -> PgResult<Aggs> {
        let keep = root.new_child("gravity window aggregates");
        let user = miscinit_seams::get_user_id::call();
        let mut v = Vec::new(); // alloc: Exec
        // bounded("other: the WindowAgg node's window functions, one EXECUTE check and one bind each: the query text bounds them, as PG's ExecInitWindowAgg walks the same functions with no poll")
        for (i, w) in s.funcs.iter().enumerate() {
            acl(keep.mcx(), w.winfnoid, user)?;
            if w.winagg {
                v.push(Agg::new(s, i, &keep, root)?); // alloc: Exec
            }
        }
        Ok(Aggs { v, base: 0, upto: 0, _keep: keep, call_cx: root.new_child_wholesale("gravity window aggregate call") })
    }

    pub(crate) fn any(&self) -> bool {
        !self.v.is_empty()
    }

    pub(crate) fn begin_partition(&mut self) {
        (self.base, self.upto) = (0, 0);
    }
}

impl Agg {
    /// initialize_peragg.
    fn new(s: &WinSpec, fno: usize, keep: &MemoryContext, root: &MemoryContext) -> PgResult<Agg> {
        let w = s.funcs[fno];
        let inputs = &s.argtypes[fno];
        let nargs = inputs.len();
        if nargs > types_core::FUNC_MAX_ARGS - 1 {
            return Err(Box::new(PgError::error(format!("aggregates cannot have more than {} arguments", types_core::FUNC_MAX_ARGS - 1)).with_sqlstate(types_error::ERRCODE_TOO_MANY_ARGUMENTS))); // alloc: Cold
        }
        let a = syscache_seams::lookup_pg_aggregate_shape::call(w.winfnoid)?.ok_or_else(|| lookup_failed("aggregate", w.winfnoid))?;
        let ma = if a.aggminvtransfn == 0 {
            false
        } else if a.aggmfinalmodify == AGGMODIFY_READ_ONLY && a.aggfinalmodify != AGGMODIFY_READ_ONLY {
            true
        } else {
            s.plan.frameOptions & FRAMEOPTION_START_UNBOUNDED_PRECEDING == 0 && s.moving_ok[fno]
        };
        let (transfn, invfn, finalfn, extra, modify, transtype) = match ma {
            true => (a.aggmtransfn, a.aggminvtransfn, a.aggmfinalfn, a.aggmfinalextra, a.aggmfinalmodify, a.aggmtranstype),
            false => (a.aggtransfn, 0, a.aggfinalfn, a.aggfinalextra, a.aggfinalmodify, a.aggtranstype),
        };
        let owner = syscache_seams::lookup_pg_proc_secdef::call(w.winfnoid)?.ok_or_else(|| lookup_failed("function", w.winfnoid))?.proowner;
        for f in [transfn, invfn, finalfn] {
            if f != 0 {
                acl(keep.mcx(), f, owner)?;
            }
        }
        if modify != AGGMODIFY_READ_ONLY {
            let name = adt_regproc::format_procedure(keep.mcx(), w.winfnoid)?;
            return Err(Box::new(PgError::error(format!("aggregate function {name} does not support use as a window function")).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))); // alloc: Cold
        }
        let nfinal = if extra { nargs + 1 } else { 1 };
        let transtype = if coerce::IsPolymorphicType(transtype) {
            let (_ret, mut declared) = lsyscache::get_func_signature(keep.mcx(), w.winfnoid)?;
            let n = declared.len().min(nargs);
            coerce::enforce_generic_type_consistency(&inputs[..n], &mut declared[..n], transtype, false)?
        } else {
            transtype
        };
        let mut carriers = Vec::new(); // alloc: Exec
        let mut types = vec![transtype]; // alloc: Exec
        types.extend(inputs.iter().copied());
        let tf = bind(transfn, transtype, types.clone(), w.inputcollid, &mut carriers)?;
        let inv = if invfn != 0 { Some(bind(invfn, transtype, types.clone(), w.inputcollid, &mut carriers)?) } else { None };
        let fin = if finalfn != 0 { Some(bind(finalfn, w.wintype, types[..nfinal].to_vec(), w.inputcollid, &mut carriers)?) } else { None }; // alloc: Exec
        let (reslen, resbyval) = gravity_fn::typesem::storage::len_byval(w.wintype)?;
        let (translen, transbyval) = gravity_fn::typesem::storage::len_byval(transtype)?;
        let text = match ma {
            true => syscache_seams::pg_aggregate_aggminitval::call(keep.mcx(), w.winfnoid)?,
            false => syscache_seams::pg_aggregate_agginitval::call(keep.mcx(), w.winfnoid)?,
        }
        .ok_or_else(|| lookup_failed("aggregate", w.winfnoid))?;
        let init = match text {
            None => NullableDatum::null(),
            Some(t) => {
                let (input, ioparam) = lsyscache::getTypeInputInfo(transtype)?;
                let cstr = CString::new(t.as_bytes()).map_err(|_| bad("window.agg: an initcond with a NUL"))?;
                // the input function's by-reference result lives only as long as its FmgrInfo: copied into the
                // aggregates' context before the FmgrInfo drops (GetAggInitVal's datumCopy)
                let mut flinfo = fmgr_core::fmgr_info(input)?;
                let v = types_fmgr::input_function_call(&mut flinfo, Some(&cstr), ioparam, -1, keep.mcx())?;
                let v = match transbyval {
                    true => v,
                    // a non-NULL by-reference datum the input function made, alive while `flinfo` is
                    false => datum_copy(keep.mcx(), v, translen)?,
                };
                drop(flinfo);
                NullableDatum { value: v, isnull: false }
            }
        };
        if tf.fn_strict && init.isnull && (nargs < 1 || !coerce::IsBinaryCoercible(inputs[0], transtype)?) {
            return Err(Box::new(PgError::error(format!("aggregate {} needs to have compatible input type and transition type", w.winfnoid)).with_sqlstate(types_error::ERRCODE_INVALID_FUNCTION_DEFINITION))); // alloc: Cold
        }
        if inv.as_ref().is_some_and(|f| f.fn_strict != tf.fn_strict) {
            return Err(Box::new(PgError::error("strictness of aggregate's forward and inverse transition functions must match".to_string()).with_sqlstate(types_error::ERRCODE_INVALID_FUNCTION_DEFINITION))); // alloc: Cold
        }
        let node = Box::new(AggStateNode::new(root.new_child_wholesale("gravity window aggregate"))); // alloc: Exec
        let frame = Box::new(Frame::new(w.inputcollid)); // alloc: Exec
        Ok(Agg {
            fno,
            coll: w.inputcollid,
            transfn: tf,
            invtransfn: inv,
            finalfn: fin,
            nfinal,
            frame,
            translen,
            transbyval,
            reslen,
            resbyval,
            init,
            node,
            trans: NullableDatum::null(),
            count: 0,
            result: NullableDatum::null(),
            restart: false,
            _carriers: carriers,
        })
    }

    /// initialize_windowaggregate.
    fn initialize(&mut self) -> PgResult<()> {
        self.node.reset();
        self.trans = match self.init.isnull || self.transbyval {
            true => self.init,
            // a by-reference initcond made by its type's input function, alive for the execution
            false => NullableDatum { value: datum_copy(self.node.aggcontext(), self.init.value, self.translen)?, isnull: false },
        };
        (self.count, self.result) = (0, NullableDatum::null());
        Ok(())
    }

    /// The transition (or, `inverse`, the inverse transition) call over the state and `args`; `None` when the call
    /// returned NULL.
    fn call(&mut self, inverse: bool, args: &[NullableDatum], call_cx: &MemoryContext) -> PgResult<NullableDatum> {
        let f = if inverse { self.invtransfn.as_mut().ok_or_else(|| bad("window.agg: an inverse transition without its function"))? } else { &mut self.transfn };
        let fc = &mut *self.frame;
        fc.rearm(self.coll);
        fc.nargs = (args.len() + 1) as i16;
        fc.context = self.node.fm_node_ptr();
        // the call context lives across the call; the result is copied out below before it resets
        result_into(fc, call_cx.mcx());
        if self.trans.isnull { fc.set_arg_null(0) } else { fc.set_arg(0, self.trans.value) }
        // bounded("small-fixed: one argument of the transition call, at most FUNC_MAX_ARGS - 1 (99: `Agg::new` refuses more)")
        for (k, a) in args.iter().enumerate() {
            if a.isnull { fc.set_arg_null(k + 1) } else { fc.set_arg(k + 1, a.value) }
        }
        let v = f.invoke(fc)?;
        Ok(NullableDatum { value: v, isnull: fc.isnull })
    }

    /// The new state `v` kept: a by-reference value other than the old state copied into the aggregate context.
    fn keep_state(&mut self, v: NullableDatum) -> PgResult<()> {
        let mut v = v;
        if !self.transbyval && v.value.as_u64() != self.trans.value.as_u64() && !v.isnull {
            // a non-NULL by-reference transition result, alive in the call context
            v.value = datum_copy(self.node.aggcontext(), v.value, self.translen)?;
        }
        self.trans = v;
        Ok(())
    }

    /// advance_windowaggregate over the row's arguments (the FILTER already passed).
    fn advance(&mut self, args: &[NullableDatum], call_cx: &MemoryContext) -> PgResult<()> {
        if self.transfn.fn_strict {
            if args.iter().any(|a| a.isnull) {
                return Ok(());
            }
            if self.count == 0 && self.trans.isnull {
                self.trans = match self.transbyval {
                    true => args[0],
                    // a non-NULL by-reference argument cell, alive for the execution
                    false => NullableDatum { value: datum_copy(self.node.aggcontext(), args[0].value, self.translen)?, isnull: false },
                };
                self.count = 1;
                return Ok(());
            }
            if self.trans.isnull {
                return Ok(());
            }
        }
        let v = self.call(false, args, call_cx)?;
        if v.isnull && self.invtransfn.is_some() {
            return Err(Box::new(PgError::error("moving-aggregate transition function must not return null".to_string()).with_sqlstate(types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED))); // alloc: Cold
        }
        self.count += 1;
        self.keep_state(v)
    }

    /// advance_windowaggregate_base: false when the inverse transition could not remove the row (a restart).
    fn advance_base(&mut self, args: &[NullableDatum], call_cx: &MemoryContext) -> PgResult<bool> {
        if self.invtransfn.as_ref().is_some_and(|f| f.fn_strict) && args.iter().any(|a| a.isnull) {
            return Ok(true);
        }
        if self.trans.isnull {
            return Err(bad("aggregate transition value is NULL before inverse transition"));
        }
        if self.count == 1 {
            self.initialize()?;
            return Ok(true);
        }
        let v = self.call(true, args, call_cx)?;
        if v.isnull {
            return Ok(false);
        }
        self.count -= 1;
        self.keep_state(v).map(|()| true)
    }

    /// finalize_windowaggregate into `row_cx`.
    fn finalize(&mut self, row_cx: &MemoryContext) -> PgResult<NullableDatum> {
        let t = NullableDatum { value: read_only(self.trans, self.translen), isnull: self.trans.isnull };
        let Some(f) = self.finalfn.as_mut() else { return Ok(t) };
        let fc = &mut *self.frame;
        fc.rearm(self.coll);
        fc.nargs = self.nfinal as i16;
        fc.context = self.node.fm_node_ptr();
        // the row context lives until the object's next row, as the result must
        result_into(fc, row_cx.mcx());
        if t.isnull { fc.set_arg_null(0) } else { fc.set_arg(0, t.value) }
        (1..self.nfinal).for_each(|k| fc.set_arg_null(k));
        if f.fn_strict && (t.isnull || self.nfinal > 1) {
            return Ok(NullableDatum::null());
        }
        let v = f.invoke(fc)?;
        let r = NullableDatum { value: v, isnull: fc.isnull };
        Ok(NullableDatum { value: read_only(r, self.reslen), isnull: r.isnull })
    }
}

impl Rt {
    /// Aggregate `a`'s FILTER and arguments on partition position `pos`, read in PG's order: `None` when the FILTER
    /// rejects the row.
    fn agg_inputs(&mut self, a: usize, pos: i64) -> PgResult<Option<Vec<NullableDatum>>> {
        let (base, n, filter) = self.spec.args[self.aggs.v[a].fno];
        let r = self.f.pstart + pos as u64;
        if let Some(c) = filter {
            let f = self.cell(r, c)?;
            if f.isnull || !f.value.as_bool() {
                return Ok(None);
            }
        }
        (0..n).map(|k| self.cell(r, base + k)).collect::<PgResult<Vec<_>>>().map(Some) // alloc: Exec
    }

    /// PG `eval_windowaggregates`.
    pub(crate) fn eval_windowaggregates(&mut self) -> PgResult<()> {
        let fo = self.f.fo;
        let cur = self.f.cur;
        self.update_frameheadpos()?;
        let fh = self.f.fh;
        if fh < self.aggs.base {
            return Err(bad("window frame head moved backward"));
        }
        let na = self.aggs.v.len();
        if self.aggs.base == fh && fo & (FRAMEOPTION_END_UNBOUNDED_FOLLOWING | FRAMEOPTION_END_CURRENT_ROW) != 0 && fo & FRAMEOPTION_EXCLUSION == 0 && self.aggs.base <= cur && self.aggs.upto > cur {
            // bounded("other: the WindowAgg node's aggregates, one step each: the query text bounds them, as PG's eval_windowaggregates walks the same aggregates per row with no poll")
            for a in 0..na {
                let (fno, r) = (self.aggs.v[a].fno, self.aggs.v[a].result);
                self.set_res(fno, r);
            }
            return Ok(());
        }
        let mut nrestart = 0;
        // bounded("other: the WindowAgg node's aggregates, one step each: the query text bounds them, as PG's eval_windowaggregates walks the same aggregates per row with no poll")
        for a in self.aggs.v.iter_mut() {
            a.restart = cur == 0 || self.aggs.base != fh && a.invtransfn.is_none() || fo & FRAMEOPTION_EXCLUSION != 0 || self.aggs.upto <= fh;
            nrestart += a.restart as usize;
        }
        while nrestart < na && self.aggs.base < fh {
            cancelpoll::interrupts::check()?;
            let pos = self.aggs.base;
            // bounded("other: the WindowAgg node's aggregates, one step each: the query text bounds them, as PG's eval_windowaggregates walks the same aggregates per row with no poll")
            for a in 0..na {
                if self.aggs.v[a].restart {
                    continue;
                }
                let ok = match self.agg_inputs(a, pos)? {
                    None => true,
                    Some(args) => {
                        let (v, cx) = (&mut self.aggs.v, &self.aggs.call_cx);
                        let ok = v[a].advance_base(&args, cx);
                        self.aggs.call_cx.reset();
                        ok?
                    }
                };
                if !ok {
                    self.aggs.v[a].restart = true;
                    nrestart += 1;
                }
            }
            self.aggs.base += 1;
        }
        self.aggs.base = fh;
        // bounded("other: the WindowAgg node's aggregates, one step each: the query text bounds them, as PG's eval_windowaggregates walks the same aggregates per row with no poll")
        for a in self.aggs.v.iter_mut() {
            if a.restart {
                a.initialize()?;
            } else {
                a.result = NullableDatum::null();
            }
        }
        let upto_kept = self.aggs.upto;
        if nrestart > 0 && self.aggs.upto != fh {
            self.aggs.upto = fh;
        }
        while self.has(self.aggs.upto)? {
            let pos = self.aggs.upto;
            let ret = self.row_is_in_frame(pos)?;
            if ret < 0 {
                break;
            }
            if ret > 0 {
                // bounded("other: the WindowAgg node's aggregates, one step each: the query text bounds them, as PG's eval_windowaggregates walks the same aggregates per row with no poll")
                for a in 0..na {
                    if !self.aggs.v[a].restart && pos < upto_kept {
                        continue;
                    }
                    if let Some(args) = self.agg_inputs(a, pos)? {
                        let (v, cx) = (&mut self.aggs.v, &self.aggs.call_cx);
                        let r = v[a].advance(&args, cx);
                        self.aggs.call_cx.reset();
                        r?;
                    }
                }
            }
            self.aggs.upto += 1;
        }
        // bounded("other: the WindowAgg node's aggregates, one step each: the query text bounds them, as PG's eval_windowaggregates walks the same aggregates per row with no poll")
        for a in 0..na {
            let r = self.aggs.v[a].finalize(&self.row_cx)?;
            let ag = &mut self.aggs.v[a];
            ag.result = match !ag.resbyval && !r.isnull {
                // a non-NULL by-reference final result, alive in the row context
                true => NullableDatum { value: datum_copy(ag.node.aggcontext(), r.value, ag.reslen)?, isnull: false },
                false => r,
            };
            let fno = ag.fno;
            self.set_res(fno, r);
        }
        Ok(())
    }
}
