//! The only divergence class is a concurrent mid-planning-cycle catalog change
//! (ALTER OPERATOR / GRANT / REVOKE committing while we plan) that C's
//! per-call re-searches could observe on a later call within the same
//! cycle — nondeterministic interleaving in C, estimates-only here: the
//! ACL memo feeds vardata.acl_ok (whether stats are CONSULTED, never
//! whether access is ALLOWED — ExecCheckPermissions runs unmemoized at
//! executor startup), and the shape/amop memos feed selectivity/cost
//! machinery.
use mcx::{PgMemoMap, PgVec};
use syscache_seams::{PgAmopMemberShape, PgAmopShape, PgOperatorShape};
use types_core::{InvalidOid, Oid, RegProcedure};
use types_error::PgResult;
use types_pathnodes::run::PlannerRun;

use crate::amop::{AMOP_ORDER, AMOP_SEARCH};

fn env_disabled(name: &'static str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("0") | Ok("off"))
}

fn opshape_memo_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| env_disabled("PGRUST_PLANNER_OPSHAPE_MEMO"))
}

fn aclmask_memo_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| env_disabled("PGRUST_PLANNER_ACLMASK_MEMO"))
}

fn amop_memo_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| env_disabled("PGRUST_PLANNER_AMOP_MEMO"))
}

fn amoplist_memo_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| env_disabled("PGRUST_PLANNER_AMOPLIST_MEMO"))
}

struct MemoBlock<'mcx> {
    op_shape: core::cell::RefCell<PgMemoMap<'mcx, Oid, Option<PgOperatorShape>>>,
    aclmask: core::cell::RefCell<PgMemoMap<'mcx, (Oid, Oid, u64, bool), u64>>,
    amop_by_op: core::cell::RefCell<PgMemoMap<'mcx, (Oid, u8, Oid), Option<PgAmopShape>>>,
    amop_member: core::cell::RefCell<PgMemoMap<'mcx, (Oid, Oid, Oid, i16), Oid>>,
    amop_members: core::cell::RefCell<
        PgMemoMap<'mcx, Oid, &'mcx PgVec<'mcx, PgAmopMemberShape>>,
    >,
    mergejoin_opfamilies: core::cell::RefCell<PgMemoMap<'mcx, Oid, &'mcx [Oid]>>,
}

// Arena-leaked via forget_box_in: drop glue never runs; every member is
// arena-backed or Copy, so the run's arena reset reclaims everything.
mcx::forget_safe_struct!(
    MemoBlock<'_> { op_shape, aclmask, amop_by_op, amop_member, amop_members, mergejoin_opfamilies },
);

fn memos<'mcx>(run: &PlannerRun<'mcx>) -> PgResult<&'mcx MemoBlock<'mcx>> {
    if let Some(p) = run.syscache_memos.get() {
        // SAFETY: the pointer was created below from &'mcx MemoBlock<'mcx>
        // leaked into run.mcx (this function is the only writer); the arena
        // outlives the run, and planning is single-threaded.
        return Ok(unsafe { p.cast::<MemoBlock<'mcx>>().as_ref() });
    }
    let block = &*mcx::forget_box_in(
        run.mcx,
        MemoBlock {
            op_shape: core::cell::RefCell::new(PgMemoMap::new_in(run.mcx)),
            aclmask: core::cell::RefCell::new(PgMemoMap::new_in(run.mcx)),
            amop_by_op: core::cell::RefCell::new(PgMemoMap::new_in(run.mcx)),
            amop_member: core::cell::RefCell::new(PgMemoMap::new_in(run.mcx)),
            amop_members: core::cell::RefCell::new(PgMemoMap::new_in(run.mcx)),
            mergejoin_opfamilies: core::cell::RefCell::new(PgMemoMap::new_in(run.mcx)),
        },
    )?;
    run.syscache_memos
        .set(Some(core::ptr::NonNull::from(block).cast()));
    Ok(block)
}

fn operator_shape(run: &PlannerRun<'_>, opno: Oid) -> PgResult<Option<PgOperatorShape>> {
    let block = memos(run)?;
    let hit = block
        .op_shape
        .borrow()
        .get(&opno)
        .copied();
    if let Some(v) = hit {
        return Ok(v);
    }
    let fetched = syscache_seams::lookup_pg_operator_shape::call(opno)?;
    block.op_shape.borrow_mut().insert(opno, fetched);
    Ok(fetched)
}

pub fn get_oprrest(run: &PlannerRun<'_>, opno: Oid) -> PgResult<RegProcedure> {
    if opshape_memo_disabled() {
        return crate::get_oprrest(opno);
    }
    Ok(match operator_shape(run, opno)? {
        Some(shape) => shape.oprrest,
        None => InvalidOid,
    })
}

pub fn get_opcode(run: &PlannerRun<'_>, opno: Oid) -> PgResult<RegProcedure> {
    if opshape_memo_disabled() {
        return crate::get_opcode(opno);
    }
    Ok(match operator_shape(run, opno)? {
        Some(shape) => shape.oprcode,
        None => InvalidOid,
    })
}

pub fn get_commutator(run: &PlannerRun<'_>, opno: Oid) -> PgResult<Oid> {
    if opshape_memo_disabled() {
        return crate::get_commutator(opno);
    }
    Ok(match operator_shape(run, opno)? {
        Some(shape) => shape.oprcom,
        None => InvalidOid,
    })
}

/// The record/array-eq arms are typcache-keyed (by inputtype, not opno) —
/// those delegate to the incumbent untouched.
pub fn op_mergejoinable(
    run: &PlannerRun<'_>,
    opno: Oid,
    inputtype: Oid,
) -> PgResult<bool> {
    if opshape_memo_disabled()
        || opno == crate::RECORD_EQ_OP
        || opno == crate::ARRAY_EQ_OP
    {
        return crate::op_mergejoinable(opno, inputtype);
    }
    Ok(match operator_shape(run, opno)? {
        Some(shape) => shape.oprcanmerge,
        None => false,
    })
}

/// Feeds vardata.acl_ok (statistics gating) only; never permission enforcement.
pub fn class_aclmask(
    run: &PlannerRun<'_>,
    table_oid: Oid,
    roleid: Oid,
    mask: u64,
    how_all: bool,
) -> PgResult<u64> {
    if aclmask_memo_disabled() {
        return aclchk_seams::pg_class_aclmask::call(table_oid, roleid, mask, how_all);
    }
    let block = memos(run)?;
    let key = (table_oid, roleid, mask, how_all);
    let hit = block
        .aclmask
        .borrow()
        .get(&key)
        .copied();
    if let Some(v) = hit {
        return Ok(v);
    }
    let fetched = aclchk_seams::pg_class_aclmask::call(table_oid, roleid, mask, how_all)?;
    block.aclmask.borrow_mut().insert(key, fetched);
    Ok(fetched)
}

fn amop_by_operator(
    run: &PlannerRun<'_>,
    opno: Oid,
    purpose: u8,
    opfamily: Oid,
) -> PgResult<Option<PgAmopShape>> {
    let block = memos(run)?;
    let key = (opno, purpose, opfamily);
    let hit = block
        .amop_by_op
        .borrow()
        .get(&key)
        .copied();
    if let Some(v) = hit {
        return Ok(v);
    }
    let fetched = syscache_seams::lookup_pg_amop_by_operator::call(opno, purpose, opfamily)?;
    block.amop_by_op.borrow_mut().insert(key, fetched);
    Ok(fetched)
}

pub fn get_op_opfamily_strategy(
    run: &PlannerRun<'_>,
    opno: Oid,
    opfamily: Oid,
) -> PgResult<i32> {
    if amop_memo_disabled() {
        return crate::get_op_opfamily_strategy(opno, opfamily);
    }
    Ok(match amop_by_operator(run, opno, AMOP_SEARCH, opfamily)? {
        Some(amop) => amop.amopstrategy as i32,
        None => 0,
    })
}

pub fn get_opfamily_member(
    run: &PlannerRun<'_>,
    opfamily: Oid,
    lefttype: Oid,
    righttype: Oid,
    strategy: i16,
) -> PgResult<Oid> {
    if amop_memo_disabled() {
        return syscache_seams::lookup_pg_amop_by_strategy::call(
            opfamily, lefttype, righttype, strategy,
        );
    }
    let block = memos(run)?;
    let key = (opfamily, lefttype, righttype, strategy);
    let hit = block
        .amop_member
        .borrow()
        .get(&key)
        .copied();
    if let Some(v) = hit {
        return Ok(v);
    }
    let fetched =
        syscache_seams::lookup_pg_amop_by_strategy::call(opfamily, lefttype, righttype, strategy)?;
    block.amop_member.borrow_mut().insert(key, fetched);
    Ok(fetched)
}

pub fn op_in_opfamily(run: &PlannerRun<'_>, opno: Oid, opfamily: Oid) -> PgResult<bool> {
    if amop_memo_disabled() {
        return crate::op_in_opfamily(opno, opfamily);
    }
    Ok(amop_by_operator(run, opno, AMOP_SEARCH, opfamily)?.is_some())
}

pub fn get_op_opfamily_properties(
    run: &PlannerRun<'_>,
    opno: Oid,
    opfamily: Oid,
    ordering_op: bool,
) -> PgResult<(i32, Oid, Oid)> {
    if amop_memo_disabled() {
        return crate::get_op_opfamily_properties(opno, opfamily, ordering_op);
    }
    let purpose = if ordering_op { AMOP_ORDER } else { AMOP_SEARCH };
    let amop = amop_by_operator(run, opno, purpose, opfamily)?.ok_or_else(|| {
        crate::cache_lookup_error(format!(
            "operator {opno} is not a member of opfamily {opfamily}"
        ))
    })?;
    Ok((amop.amopstrategy as i32, amop.amoplefttype, amop.amoprighttype))
}

pub fn get_op_opfamily_sortfamily(
    run: &PlannerRun<'_>,
    opno: Oid,
    opfamily: Oid,
) -> PgResult<Oid> {
    if amop_memo_disabled() {
        return crate::get_op_opfamily_sortfamily(opno, opfamily);
    }
    Ok(match amop_by_operator(run, opno, AMOP_ORDER, opfamily)? {
        Some(amop) => amop.amopsortfamily,
        None => InvalidOid,
    })
}

fn amop_members<'mcx>(
    run: &PlannerRun<'mcx>,
    opno: Oid,
) -> PgResult<&'mcx PgVec<'mcx, PgAmopMemberShape>> {
    let block = memos(run)?;
    let hit = block
        .amop_members
        .borrow()
        .get(&opno)
        .copied();
    if let Some(v) = hit {
        return Ok(v);
    }
    let fetched = syscache_seams::lookup_pg_amop_members_by_operator::call(run.mcx, opno)?;
    let leaked: &'mcx PgVec<'mcx, PgAmopMemberShape> =
        &*mcx::forget_box_in(run.mcx, fetched)?;
    block.amop_members.borrow_mut().insert(opno, leaked);
    Ok(leaked)
}

/// get_mergejoin_opfamilies over the run's decoded pg_amop member list for `opno` (decoded once per run, not once per
/// call: the planner asks for each mergejoinable clause and each sort pathkey it builds). The list is built once per
/// operator and frozen into the run's context (D4561 tranche 3): every RestrictInfo.mergeopfamilies and
/// EquivalenceClass.ec_opfamilies made from it, join-search tours' included, is a pointer copy.
pub fn get_mergejoin_opfamilies<'mcx>(run: &PlannerRun<'mcx>, opno: Oid) -> PgResult<&'mcx [Oid]> {
    if amoplist_memo_disabled() {
        return Ok(mcx::freeze_fit(crate::amop::get_mergejoin_opfamilies(run.mcx, opno)?));
    }
    let block = memos(run)?;
    if let Some(fams) = block.mergejoin_opfamilies.borrow().get(&opno).copied() {
        return Ok(fams);
    }
    let mut result = PgVec::new_in(run.mcx);
    for aform in amop_members(run, opno)?.iter() {
        if crate::amop::is_mergejoin_equality(aform) {
            result.push(aform.amopfamily);
        }
    }
    let fams = mcx::freeze_fit(result);
    block.mergejoin_opfamilies.borrow_mut().insert(opno, fams);
    Ok(fams)
}

fn ops_are_compatible_run(
    run: &PlannerRun<'_>,
    opno1: Oid,
    opno2: Oid,
    check_ordering: bool,
) -> PgResult<bool> {
    if opno1 == opno2 {
        return Ok(true);
    }
    let members = amop_members(run, opno1)?;
    for op_form in members.iter() {
        if op_in_opfamily(run, opno2, op_form.amopfamily)? {
            let (consistent_eq, consistent_ord) =
                crate::amop::index_am_consistent_flags(op_form.amopmethod);
            if if check_ordering { consistent_ord } else { consistent_eq } {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub fn comparison_ops_are_compatible(
    run: &PlannerRun<'_>,
    opno1: Oid,
    opno2: Oid,
) -> PgResult<bool> {
    if amoplist_memo_disabled() {
        return crate::comparison_ops_are_compatible(opno1, opno2);
    }
    ops_are_compatible_run(run, opno1, opno2, true)
}

pub fn equality_ops_are_compatible(
    run: &PlannerRun<'_>,
    opno1: Oid,
    opno2: Oid,
) -> PgResult<bool> {
    if amoplist_memo_disabled() {
        return crate::equality_ops_are_compatible(opno1, opno2);
    }
    ops_are_compatible_run(run, opno1, opno2, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    // wio P288: a partitioned-FK plan probes one ACL mask per leaf, so the
    // memo holds thousands of keys; each is fetched once and hits after.
    #[test]
    fn aclmask_memo_fetches_each_key_once_at_scale() {
        aclchk_seams::pg_class_aclmask::set(|table_oid, _roleid, mask, _how_all| {
            CALLS.fetch_add(1, Ordering::Relaxed);
            Ok(mask ^ u64::from(table_oid))
        });
        let cx = mcx::MemoryContext::new("run_memo test");
        let run = PlannerRun::new(cx.mcx());
        const N: u32 = 20_000;
        for round in 0..2 {
            for oid in 16384..16384 + N {
                assert_eq!(class_aclmask(&run, oid, 10, 2, false).unwrap(), 2 ^ u64::from(oid));
            }
            assert_eq!(CALLS.load(Ordering::Relaxed), N as usize, "round {round}");
        }
    }
}
