//! The one catch (v3 design §3.5, kept): refusal payloads, `NotBuilt`'s seam text for a kind and a road, `Interrupted`,
//! `Bug`, and the PgError-unwind proof (a PgError, bare or boxed, and a thread exit leave the catch unchanged).
use std::panic::{catch_unwind, panic_any, AssertUnwindSafe};

use gravity_lower::admit::{refuse as admit_refuse, Family, Gap, Refuse, D};
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::face::strategy::VERDICT_BLOOM;
use gravity_lowerv4::admit::refusal::{bugs, NumOp};
use gravity_lowerv4::{catch, not_built, refuse, Milestone, Reason, Variant};
use types_error::{PanicExitThread, PgError};

#[test]
fn not_built_is_the_refusal() {
    let r = catch(42, || -> u32 { not_built!(42, Variant::Kind(KindId::SrcPgrc)) }).expect_err("a refusal");
    assert_eq!(r.node, 42);
    assert_eq!(r.reason, Reason::NotBuilt { hook: "source", variant: Variant::Kind(KindId::SrcPgrc), milestone: Milestone::M1 });
    let s = r.to_seam();
    assert_eq!((s.module, s.what, s.node, s.oid), ("lowerv4", "NotBuilt M1: source src.pgrc", 42, 0));
    assert_eq!(s.to_string(), "lowerv4: NotBuilt M1: source src.pgrc (node 42)");
    let r = catch(7, || -> () { not_built!(7, Variant::Strategy(VERDICT_BLOOM)) }).expect_err("a refusal");
    assert_eq!(r.reason, Variant::Strategy(VERDICT_BLOOM).not_built());
    assert_eq!(r.to_seam().what, "NotBuilt M11: strategy verdict.bloom");
}

#[test]
fn a_value_passes() {
    assert_eq!(catch(1, || 5).ok(), Some(5));
}

#[test]
fn refusal_values_stay_out_of_the_text() {
    let r = catch(3, || -> () { refuse!(3, Reason::NoHash { typid: 142 }) }).expect_err("refused");
    let s = r.to_seam();
    assert_eq!((s.what, s.oid, s.detail), ("NoHash", 142, "type"));
    let r = catch(3, || -> () { refuse!(3, Reason::NumericTooWide { precision: 30, op: NumOp::Sum }) }).expect_err("refused");
    assert_eq!(r.to_seam().what, "NumericTooWide: Sum");
    let r = catch(3, || -> () { refuse!(3, Reason::Stopgap { d: 229 }) }).expect_err("refused");
    assert_eq!((r.to_seam().oid, r.to_seam().detail), (229, "D-row"));
    let r = catch(3, || -> () { refuse!(3, Reason::UnsupportedAm { relid: 16390 }) }).expect_err("refused");
    assert_eq!((r.to_seam().what, r.to_seam().oid, r.to_seam().detail), ("UnsupportedAm", 16390, "relation"));
}

/// D231: a pending interrupt refuses `Interrupted`, whose seam text is the one the entry maps to the cancel error.
#[test]
fn interrupted_is_named() {
    let r = catch(4, || -> () { refuse!(4, Reason::Interrupted) }).expect_err("refused");
    assert_eq!(r.to_seam().what, gravity_lower::INTERRUPTED);
    assert_eq!(r.reason.milestone(), Milestone::M0);
}

/// PgError proof, path 1: a bare `PgError` raised inside lowering leaves the catch as the same payload, not a refusal.
#[test]
fn pg_error_is_rethrown() {
    let out = catch_unwind(AssertUnwindSafe(|| catch(9, || -> () { panic_any(PgError::error("relation vanished")) })));
    let e = out.expect_err("the PgError unwinds through the catch").downcast::<PgError>().expect("the same PgError payload");
    assert_eq!(e.message(), "relation vanished");
}

/// PgError proof, path 2: the boxed form (`panic_any` of a `PgResult` error) is rethrown as the box.
#[test]
fn boxed_pg_error_is_rethrown() {
    let out = catch_unwind(AssertUnwindSafe(|| catch(9, || -> () { panic_any(Box::new(PgError::error("canceling statement"))) })));
    let e = out.expect_err("the boxed PgError unwinds through the catch").downcast::<Box<PgError>>().expect("the same boxed PgError");
    assert_eq!(e.message(), "canceling statement");
}

#[test]
fn thread_exit_is_rethrown() {
    let out = catch_unwind(AssertUnwindSafe(|| catch(9, || -> () { panic_any(PanicExitThread) })));
    assert!(out.expect_err("unwinds").is::<PanicExitThread>());
}

/// Every other panic is a `Bug` and is counted; a PgError never is. The only test that raises a Bug counts here.
#[test]
fn any_other_panic_is_a_bug() {
    let before = bugs();
    let _ = catch_unwind(AssertUnwindSafe(|| catch(9, || -> () { panic_any(PgError::error("not a bug")) })));
    assert_eq!(bugs(), before, "a PgError is never counted as a Bug");
    let r = catch(5, || -> () { panic!("checked at compile") }).expect_err("a Bug refusal");
    assert_eq!(r.reason, Reason::Bug { payload: "checked at compile" });
    let rows = std::hint::black_box(3); // a runtime value: rustc folds `panic!("{} rows", 3)` into a static str
    let r = catch(5, || -> () { panic!("{rows} rows") }).expect_err("a Bug refusal");
    assert!(matches!(r.reason, Reason::Bug { .. }));
    assert_eq!(r.to_seam().what, "Bug: a panic with a runtime message (logged)");
    // Admit landing 8a: a facade invariant is a counted Bug too (here, not in another test: the counter is shared).
    let r = catch(5, || -> () { gravity_lower::admit::invariant(5, gravity_lower::admit::Family::Agg, "a broken invariant") })
        .expect_err("refused");
    assert_eq!((r.node, r.reason), (5, Reason::Bug { payload: "a broken invariant" }));
    // A filed bug's gap is a decision, never counted.
    let filed = Gap::Bug(D(2194), "a known-broken shape (D2194)");
    let _ = catch(6, || -> () { admit_refuse(Refuse { node: 6, family: Family::Sort, gap: filed }) }).expect_err("refused");
    assert_eq!(bugs(), before + 3);
}

/// A filed bug's gap refuses exactly as the `refuse!` of its text: the same reason and seam text, and (above) uncounted.
#[test]
fn bug_gap_is_the_shape_refusal_it_replaces() {
    use gravity_lowerv4::admit::refusal::ShapeWhy;
    let why = "W8-EQUIVCLASS-SORTOP no-widen: a no-order-proc raise in a statement holding a set operation (D2197)";
    let old = catch(3, || -> () { refuse!(3, Reason::Shape { why: ShapeWhy(why) }) }).expect_err("refused");
    let new = catch(3, || -> () { admit_refuse(Refuse { node: 3, family: Family::Sort, gap: Gap::Bug(D(2197), why) }) }).expect_err("refused");
    assert_eq!((new.node, new.reason), (old.node, old.reason));
    let (n, o) = (new.to_seam(), old.to_seam());
    assert_eq!((n.module, n.what, n.node, n.oid), (o.module, o.what, o.node, o.oid));
    assert_eq!(n.to_string(), o.to_string());
}

/// Admit landing 8a: a facade refusal (`gravity_lower::admit::refuse`) is caught as the `refuse!` it replaces.
#[test]
fn facade_refusal_is_the_refuse_it_replaces() {
    use gravity_lowerv4::admit::refusal::ShapeWhy;
    let why = "a shape the family does not lower yet";
    let old = catch(9, || -> () { refuse!(9, Reason::Shape { why: ShapeWhy(why) }) }).expect_err("refused");
    let new = catch(9, || -> () { admit_refuse(Refuse { node: 9, family: Family::Memoize, gap: Gap::Shape(why) }) }).expect_err("refused");
    assert_eq!((new.node, new.reason), (old.node, old.reason));
    assert_eq!(new.to_seam().to_string(), old.to_seam().to_string());
    let r = catch(4, || -> () { admit_refuse(Refuse { node: 4, family: Family::SeqScan, gap: Gap::Am(16390) }) }).expect_err("refused");
    assert_eq!(r.reason, Reason::UnsupportedAm { relid: 16390 });
}

/// D2962: a designed exit another crate registers (ipc's ProcExitThread / KilledBySignal, parallel's PoolRetireRaw; here a
/// test-local payload registered the same way) leaves the catch unchanged, never a `Bug` refusal.
#[test]
fn d2962_registered_exit_is_rethrown() {
    struct ThreadExit(i32);
    static EXITS: types_error::ExitPayloadTest = types_error::ExitPayloadTest(|p| p.is::<ThreadExit>());
    types_error::register_exit_payload_test(&EXITS);
    let out = catch_unwind(AssertUnwindSafe(|| catch(11, || -> () { panic_any(ThreadExit(1)) })));
    let e = out.expect_err("the exit unwinds through the catch").downcast::<ThreadExit>().expect("the same exit payload");
    assert_eq!(e.0, 1);
}
