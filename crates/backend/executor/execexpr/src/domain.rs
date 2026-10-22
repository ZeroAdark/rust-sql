//! domains.c domain_check_input engine: adt_domains sits under fmgr_core and
//! reaches this through typcache_seams::domain_check_input. The per-domain
//! memo is this backend's fn_extra analog (rule 5), keyed by domain OID and
//! revalidated against the dcc identity per call (UpdateDomainConstraintRef).

use core::cell::RefCell;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use ::datum::{Datum, NullableDatum};
use ::mcx::{Mcx, MemoryContext, PgBox, PgHashMap, PgVec};
use ::types_core::Oid;
use ::types_error::PgResult;
use ::types_nodes::Node;
use ::types_portal::params::ParamBind;

use crate::interp::{domain_check_violation, domain_not_null_violation, exec_eval_expr, EvalSlots};
use crate::steps::{ExprState, OutRef, Step};

struct CompiledCheck {
    name: &'static str,
    slot: NonNull<NullableDatum>,
    state: PgBox<'static, ExprState<'static>>,
}

struct DomainMemo {
    cref: typcache::DomainConstraintRef,
    dcc_addr: usize,
    typlen: i16,
    checks: Vec<CompiledCheck>,
}

struct EngineState {
    mcx: Mcx<'static>,
    scratch: PgVec<'static, NonNull<MemoryContext>>,
    depth: usize,
    memos: PgHashMap<'static, Oid, DomainMemo>,
}

thread_local! {
    static STATE: RefCell<Option<ManuallyDrop<EngineState>>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut EngineState) -> R) -> R {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let st = slot.get_or_insert_with(|| {
            let mcx = ::mcx::session_root("DomainCheckEngine").mcx();
            // D4682: the teardown retires this root; clear the slot first, so a reused thread's next
            // session builds a new one.
            ::mcx::register_session_cleanup(Box::new(|| {
                let _ = STATE.try_with(|c| drop(c.borrow_mut().take()));
            }));
            ManuallyDrop::new(EngineState {
                mcx,
                scratch: PgVec::new_in(mcx),
                depth: 0,
                memos: PgHashMap::with_capacity_in(4, mcx),
            })
        });
        f(st)
    })
}

// The standalone check program: CoerceToDomainValue reads a dedicated slot
// (the EXT step's econtext channel collapsed to compile-time wiring).
fn compile_check(
    mcx: Mcx<'static>,
    expr: Node<'static>,
    name: &'static str,
) -> PgResult<CompiledCheck> {
    let slot = crate::compile::alloc_nullable_datum(mcx)?;
    let mut state = ExprState::new_boxed_in(mcx)?;
    crate::compile::create_expr_setup_steps(&mut state, mcx, &[expr], None, ParamBind::NONE, None)?;
    state.innermost_domain = Some(OutRef(slot));
    let rout = state.result_out();
    crate::compile::init_expr_rec(expr, &mut state, mcx, rout, None, ParamBind::NONE, None)?;
    crate::compile::push_step(&mut state, mcx, Step::DoneReturn)?;
    crate::compile::ready_expr(&mut state);
    Ok(CompiledCheck { name, slot, state })
}

// ALTER DOMAIN validation programs (typecmds.c validateDomainCheckConstraint's
// ExecPrepareExpr): same wiring as compile_check, caller-lifetime.
pub struct DomainCheckExpr<'mcx> {
    slot: NonNull<NullableDatum>,
    state: PgBox<'mcx, ExprState<'mcx>>,
}

impl<'mcx> DomainCheckExpr<'mcx> {
    /// Re-arm by-ref call results at a per-tuple context the caller resets
    /// between rows (C: the validation scan's ecxt_per_tuple_memory).
    ///
    /// # Safety
    /// The armed context must outlive every later `eval`.
    pub unsafe fn arm_result_mcx_raw(&mut self, mcx: Mcx<'_>) {
        unsafe { self.state.arm_result_mcx_raw(mcx) };
    }

    pub fn eval(&mut self, value: Datum, isnull: bool) -> PgResult<NullableDatum> {
        // SAFETY: slot lives in the program's mcx alongside the state.
        unsafe { self.slot.as_ptr().write(NullableDatum { value, isnull }) };
        let mut slots = EvalSlots {
            scan: None,
            inner: None,
            outer: None,
        };
        exec_eval_expr(&mut self.state, &mut slots)
    }
}

pub fn prepare_domain_check_expr<'mcx>(
    mcx: Mcx<'mcx>,
    expr: Node<'mcx>,
) -> PgResult<DomainCheckExpr<'mcx>> {
    let slot = crate::compile::alloc_nullable_datum(mcx)?;
    let mut state = ExprState::new_boxed_in(mcx)?;
    crate::compile::create_expr_setup_steps(&mut state, mcx, &[expr], None, ParamBind::NONE, None)?;
    state.innermost_domain = Some(OutRef(slot));
    let rout = state.result_out();
    crate::compile::init_expr_rec(expr, &mut state, mcx, rout, None, ParamBind::NONE, None)?;
    crate::compile::push_step(&mut state, mcx, Step::DoneReturn)?;
    crate::compile::ready_expr(&mut state);
    // By-ref call results (record detoast, coercions) land in the caller's
    // validation-scan mcx (C: per-tuple econtext).
    state.arm_result_mcx(mcx);
    Ok(DomainCheckExpr { slot, state })
}

fn rebuild_memo(memo: &mut DomainMemo, mcx: Mcx<'static>) -> PgResult<()> {
    // Old programs leak into the engine mcx (constraint changes are DDL-rare;
    // typcache's dcc takes the same stance).
    memo.checks.clear();
    for con in memo.cref.constraints() {
        if con.constrainttype == typcache::DomConstraintType::Check {
            let expr = con
                .check_expr
                .expect("CHECK DomainConstraintState carries check_expr");
            memo.checks.push(compile_check(mcx, expr, con.name)?);
        }
    }
    memo.dcc_addr = memo.cref.dcc_addr();
    memo.typlen = memo.cref.typlen();
    Ok(())
}

/// typcache_seams::domain_prepare_constraints target.
pub fn domain_prepare_constraints(domain_type: Oid) -> PgResult<()> {
    ensure_memo(domain_type)
}

fn ensure_memo(domain_type: Oid) -> PgResult<()> {
    // The memo leaves the map while its constraints refresh: update() plans
    // the CHECK expressions, whose constant folding may run a function that
    // re-enters this engine (domains.c:145 holds no lock across the reload).
    let mcx = with_state(|st| st.mcx);
    let memo = match with_state(|st| st.memos.remove(&domain_type)) {
        None => {
            let cref = typcache::DomainConstraintRef::init(domain_type)?;
            let mut memo = DomainMemo {
                cref,
                dcc_addr: usize::MAX,
                typlen: 0,
                checks: Vec::new(),
            };
            rebuild_memo(&mut memo, mcx)?;
            memo
        }
        Some(mut memo) => {
            if memo.cref.update()? {
                rebuild_memo(&mut memo, mcx)?;
            }
            memo
        }
    };
    with_state(|st| st.memos.insert(domain_type, memo));
    Ok(())
}

/// typcache_seams::domain_check_input target (domains.c domain_check_input).
pub fn domain_check_input(
    value: Datum,
    isnull: bool,
    domain_type: Oid,
    escontext: Option<&mut ::types_error::SoftErrorContext>,
) -> PgResult<()> {
    ensure_memo(domain_type)?;

    // Evaluate with the memo temporarily out of the map: a CHECK expression
    // may re-enter this engine for a different domain (map growth would move
    // entries). Ownership round-trips instead of a raw pointer.
    let mut memo = with_state(|st| st.memos.remove(&domain_type)).unwrap();
    let mut scratch = match acquire_scratch() {
        Ok(scratch) => scratch,
        Err(error) => {
            with_state(|st| st.memos.insert(domain_type, memo));
            return Err(error);
        }
    };
    // SAFETY: each active depth owns a stable context until its checks return.
    let scratch_mcx = unsafe { scratch.as_ref() }.mcx();
    let result = run_checks(
        &mut memo,
        scratch_mcx,
        value,
        isnull,
        domain_type,
        escontext,
    );
    // SAFETY: run_checks has returned; enclosing calls use separate contexts.
    unsafe { scratch.as_mut().reset() };
    with_state(|st| {
        st.depth -= 1;
        st.memos.insert(domain_type, memo)
    });
    result
}

fn acquire_scratch() -> PgResult<NonNull<MemoryContext>> {
    with_state(|st| {
        if st.depth == st.scratch.len() {
            ::mcx::pgvec_reserve(&mut st.scratch, 1)?;
            st.scratch.push(NonNull::from(::mcx::session_root_mut(
                MemoryContext::new_wholesale("DomainCheckScratch"),
            )));
        }
        let scratch = st.scratch[st.depth];
        st.depth += 1;
        Ok(scratch)
    })
}

#[cfg(test)]
pub(super) fn assert_scratch_released() {
    with_state(|st| {
        assert_eq!(st.depth, 0);
        for scratch in &st.scratch {
            // SAFETY: the context is session-owned and no checks are active.
            assert_eq!(unsafe { scratch.as_ref() }.used(), 0);
        }
    });
}

fn run_checks(
    memo: &mut DomainMemo,
    scratch_mcx: Mcx<'_>,
    value: Datum,
    isnull: bool,
    domain_type: Oid,
    mut escontext: Option<&mut ::types_error::SoftErrorContext>,
) -> PgResult<()> {
    let mut check_ix = 0;
    for con in memo.cref.constraints() {
        match con.constrainttype {
            typcache::DomConstraintType::NotNull => {
                if isnull {
                    return ::types_error::ereturn(
                        escontext.as_deref_mut(),
                        (),
                        *domain_not_null_violation(domain_type),
                    );
                }
            }
            typcache::DomConstraintType::Check => {
                let check = &mut memo.checks[check_ix];
                check_ix += 1;
                let ro = if isnull {
                    value
                } else {
                    // SAFETY: non-null datum of the domain's own typlen class.
                    unsafe {
                        ::datum::expandeddatum::make_expanded_object_read_only(
                            value,
                            isnull,
                            memo.typlen,
                        )
                    }
                };
                // SAFETY: slot is an engine-mcx allocation owned by this memo.
                unsafe { check.slot.write(NullableDatum { value: ro, isnull }) };
                // SAFETY: scratch outlives this eval; re-armed every call.
                unsafe { check.state.arm_result_mcx_raw(scratch_mcx) };
                let r = exec_eval_expr(&mut check.state, &mut EvalSlots::default())?;
                // C ExecCheck: NULL is not a failure.
                if !r.isnull && !r.value.as_bool() {
                    return ::types_error::ereturn(
                        escontext.as_deref_mut(),
                        (),
                        *domain_check_violation(domain_type, check.name),
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_scratch_preserves_outer_allocations() {
        let mut outer = acquire_scratch().unwrap();
        // SAFETY: outer remains active until the end of this test.
        let outer_mcx = unsafe { outer.as_ref() }.mcx();
        let value = ::mcx::alloc_leak_in(outer_mcx, [42u8; 1024]).unwrap();
        for _ in 0..16 {
            let mut inner = acquire_scratch().unwrap();
            assert_ne!(outer, inner);
            // SAFETY: inner has a distinct context and no surviving allocations.
            unsafe {
                ::mcx::alloc_leak_in(inner.as_ref().mcx(), [7u8; 1024]).unwrap();
                inner.as_mut().reset();
                assert_eq!(inner.as_ref().used(), 0);
            }
            with_state(|st| st.depth -= 1);
            assert_eq!(*value, [42u8; 1024]);
            // SAFETY: outer is still active and its allocation is live.
            assert!(unsafe { outer.as_ref() }.used() >= 1024);
        }
        // SAFETY: no outer allocations are accessed after this reset.
        unsafe { outer.as_mut().reset() };
        with_state(|st| st.depth -= 1);
        assert_scratch_released();
    }
}
