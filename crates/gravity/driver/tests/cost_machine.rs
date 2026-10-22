//! The cost home's machine facts (`gravity_program::cost::Machine`) agree with the driver's detected profile
//! (`driver/src/machine.rs`): the reference machine is one set of facts, and a detected profile derives the same partition
//! slice and prefetch ring through `Machine::detected` as the driver computes itself. The driver's reference facts and
//! abi's `FOLD_PF` are read from the cost home; the detection is the driver's, so this test keeps the two from drifting.
use gravity_abi::breaker::FOLD_PF;
use gravity_driver::machine::{profile, MachineProfile, REFERENCE_L2, REFERENCE_PREFETCH};
use gravity_program::cost::Machine;

#[test]
fn the_reference_machine_is_one_set_of_facts() {
    let r = MachineProfile::reference();
    let m = Machine::REFERENCE;
    assert_eq!((m.l1d, m.l2, m.prefetch, m.smt), (r.l1d_bytes, REFERENCE_L2, REFERENCE_PREFETCH, r.smt));
    assert_eq!(r.cost(), m, "the reference profile is the cost home's reference machine");
    assert_eq!((m.l2_share(), m.prefetch), (r.partition_bytes, r.prefetch));
    assert_eq!(m.prefetch, FOLD_PF, "the owner fold's ring is the reference prefetch distance (D16)");
}

#[test]
fn a_detected_profile_derives_what_the_driver_derives() {
    let p = profile();
    let m = Machine::detected(Some(p.l1d_bytes), p.l2_bytes, p.smt, p.prefetch);
    assert_eq!((m.l2_share(), m.prefetch), (p.partition_bytes, p.prefetch), "{}", p.describe());
    assert_eq!((p.cost(), m.l1d), (m, p.l1d_bytes), "{}", p.describe());
}

#[test]
fn the_fold_ring_holds_every_detected_prefetch() {
    // D16 (keep): `FOLD_PF` is the reference machine's ring, not a size tuned on its own: a detected machine's ring, the
    // environment's override included, is at most it, so the ring array never needs more room
    assert_eq!(FOLD_PF, Machine::REFERENCE.prefetch);
    let p = profile();
    assert!((1..=FOLD_PF).contains(&p.prefetch), "{}", p.describe());
}
