//! The statement's key words (`eval_keys`: parameters and stable expressions) and its InitPlans (`run_inits`).
//! Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;

/// The InitPlan programs of a composed statement (rung 12; `ExecSetParamPlan`'s protocol): each run to
/// completion serially under the statement's snapshot, its cell into the exec word the main program reads —
/// EXISTS a row's presence, a scalar link the first row's column (NULL without a row, an error past one row).
pub fn run_inits(mcx: Mcx<'static>, entry: &mut gravity_driver::Entry, a: &mut Admitted, snap: &Rc<SnapshotData<'static>>, cid: u32, kparams: &mut [u64], knulls: &mut [u8]) -> PgResult<()> {
    let (pstmt, params) = (a.pstmt, crate::ops::append::proto::Initial::params_of(a));
    let Some(g) = a.graph.as_mut() else { return Ok(()) };
    for init in g.inits.iter_mut() {
        // the init's own lease: its reservation ends with it (the outer statement's is its own value, untouched)
        let lease = admit_grant(&init.program, &init.program.pipelines, 1, None)?;
        let mut resolved = resolve_serial(mcx, &init.registry, snap, lease.grant(), PartRelease::Off)?;
        // D3663: a relation set its Append prunes at the executor start reads its survivors
        if init.program.sites.iter().any(|d| d.set_prune.is_some()) {
            super::drive::file_pruned_sets(mcx, &crate::ops::append::proto::Initial::decide_in(&init.program, pstmt, params)?, &init.registry, &mut resolved)?;
        }
        init.registry.rewind();
        let mut sink = Receiver::Cell(CellSink { exists: init.exists, ..CellSink::default() });
        {
            let parts = glue::binding_parts(snap, cid, kparams, knulls, Some(&resolved));
            let mut binding = Binding::new(parts, &mut sink);
            let mut exec = entry.start(None, &init.program, &mut init.registry, &mut binding, lease.reservation()).map_err(glue::pg_error)?;
            let r = entry.run_to_end(&mut exec, &mut binding).map_err(glue::pg_error); // D374: the error, then the end
            let ended = drive::end_entry(entry, exec, r.is_ok());
            r?;
            ended?;
        }
        let Receiver::Cell(c) = sink else { unreachable!() };
        if !init.exists && c.rows > 1 {
            return Err(Box::new(types_error::PgError::error("more than one row returned by a subquery used as an expression").with_sqlstate(types_error::ERRCODE_CARDINALITY_VIOLATION))); // alloc: Cold
        }
        let w = init.word;
        if w >= kparams.len() {
            return Err(glue::internal("gravity: an InitPlan's word outside the binding"));
        }
        (kparams[w], knulls[w]) = if init.exists { ((c.rows > 0) as u64, 0) } else if c.rows == 0 { (0, 1) } else { (c.value, c.null) };
    }
    Ok(())
}

/// The words `eval_keys` writes for `a`, the length of every binding's key words (the statement's own:
/// `limits::PARAM_WORDS` bounds a statement by name at its lowering and never sizes a binding): a graph admission's words,
/// a shape table's key slots.
pub fn key_words(a: &Admitted) -> usize {
    match a.graph.as_ref() {
        Some(g) => g.words.len().max(a.nkparams),
        None => {
            let slots = key_slots(&a.program);
            (0..a.nkparams).map(|k| key_slot(slots, k) + 1).max().unwrap_or(0)
        }
    }
}

/// A shape table's key slots: `keys[k]` is the value for the scan's key `k` (index-column order); the kernel reads
/// `ctx.params` through the site's `param_slots` (identity for OL1, `[1,0]` for OL5).
fn key_slots(program: &Program) -> &[i16] {
    program.sites.iter().find(|d| d.is_source()).map_or(&[0, 1, 2, 3], |d| &d.param_slots)
}

fn key_slot(slots: &[i16], k: usize) -> usize {
    if k < slots.len() && slots[k] >= 0 { slots[k] as usize } else { k }
}

/// The program's EXECUTE gates, each as its C site checks it at ExecutorStart: an Aggref's aggregate as nodeAgg.c
/// (OBJECT_AGGREGATE), every other gate as execExpr.c. A VALUES row's visit-time gates are the program's own, run at the
/// row's turn (D2775: lowerv4 ops/values `gate_row`), never here.
pub fn check_gates(a: &Admitted, mcx: Mcx<'_>) -> PgResult<()> {
    for &(f, object) in &a.key_exprs.acl_funcs {
        match object {
            crate::shapes::AclObject::Function => execexpr::recheck_execute_acls(mcx, core::slice::from_ref(&f))?,
            crate::shapes::AclObject::Aggregate => gravity_expr::nodes::func::aggregate_gate(f)?,
        }
    }
    Ok(())
}

/// The key words of this execution into `kparams` / `knulls` (at least `key_words(a)` long); `false` when the binding is
/// short (a parameter the execution's list does not hold is ExecEvalParamExtern's error, `admit::no_param`). `gates`: check the program's EXECUTE gates first (`check_gates`); the runner's
/// crossings check them at every execution, the entry once at its start (a DECLARE raises there: D2219).
pub fn eval_keys(a: &mut Admitted, params: ParamListHandle, kparams: &mut [u64], knulls: &mut [u8], mcx: Mcx<'static>, gates: bool) -> PgResult<bool> {
    if gates {
        check_gates(a, mcx)?;
    }
    // exec pruning (`prune::exec_skip`, after this call's `run_inits`) resolves this execution's extern parameters, not the admitting portal's (freed after its execution: a keyed program's next EXECUTE)
    if let Some(p) = a.graph.as_mut().and_then(|g| g.prune.as_mut()) {
        p.params = params;
    }
    a.params = params; // V4R-246: the incumbent FunctionScan's (exec v4/m5.rs), this execution's
    if (!a.key_exprs.exprs.is_empty() || a.key_exprs.filters + a.key_exprs.rechecks > 0) && !params.is_null() {
        types_portal::params::with(params, |ps| {
            let n = ps.len().min(a.key_exprs.stable.len());
            a.key_exprs.stable[..n].copy_from_slice(&ps[..n]);
        });
    }
    // the extern parameters read in place, from the ParamList's own array (no copy, no bound on their count)
    if params.is_null() {
        return eval_words(a, params, &[], kparams, knulls, mcx);
    }
    types_portal::params::with(params, |ext| eval_words(a, params, ext, kparams, knulls, mcx))
}

fn eval_words(a: &mut Admitted, params: ParamListHandle, ext: &[types_portal::params::ParamExternData], kparams: &mut [u64], knulls: &mut [u8], mcx: Mcx<'static>) -> PgResult<bool> {
    if a.graph.is_some() {
        return admit::eval(a, params, ext, kparams, knulls, mcx);
    }
    if kparams.len().min(knulls.len()) < key_words(a) {
        return Ok(false);
    }
    let slots = key_slots(&a.program);
    for k in 0..a.nkparams {
        let j = key_slot(slots, k);
        match a.keys[k] {
            KeySrc::Extern(i) => {
                let Some(p) = ext.get(i) else { return Err(admit::no_param(i)) };
                kparams[j] = admit::extern_word(p, mcx)?; // D3576: an on-disk toast pointer fetched on the leader
                knulls[j] = p.isnull as u8;
            }
            KeySrc::Expr(e) => {
                let st = &mut a.key_exprs.exprs[e];
                st.arm_result_mcx(mcx);
                let nd = execexpr::exec_eval_expr(st, &mut execexpr::EvalSlots { scan: None, inner: None, outer: None })?;
                kparams[j] = nd.value.as_u64();
                knulls[j] = nd.isnull as u8;
            }
            KeySrc::Const(v) => {
                kparams[j] = v;
                knulls[j] = 0;
            }
        }
    }
    Ok(true)
}
