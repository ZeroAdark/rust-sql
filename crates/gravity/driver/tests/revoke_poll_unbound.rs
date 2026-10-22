//! C5: the kernels' poll on a set revoke bit (exec glue `poll_kernel` -> `mem::grow::revoke_poll`) may come from a ctx no
//! program block binds (a host loop's own) or from a block with no participant (`ProgramBlock::NONE`): there is no region
//! to cut, and the poll goes on. Before: it read the participant through the null block (SIGSEGV, fault address 0x8: a
//! pool worker under nestfast_c5_revoke at W8 once the revoke bit stayed set for every participant).

#[test]
fn a_poll_from_a_ctx_with_no_participant_cuts_nothing() {
    let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(0) / 8];
    let c = ctx.as_mut_ptr() as *mut gravity_abi::Ctx;
    // SAFETY: a zeroed ctx of its size: no program block bound
    assert!(!unsafe { gravity_driver::mem::grow::revoke_poll(c) }, "no program block: nothing to cut");
    let mut block = Box::new(gravity_abi::ProgramBlock::NONE);
    // SAFETY: as above, now bound to a block whose participant (`env`) is null
    unsafe { (*c).program = &mut *block };
    assert!(!unsafe { gravity_driver::mem::grow::revoke_poll(c) }, "a block with no participant: nothing to cut");
}
