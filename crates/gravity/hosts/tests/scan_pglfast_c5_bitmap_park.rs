//! pglfast C5: a bitmap source derived at a registry entry (`at_entry`) serves a parked Node. Each bind takes the
//! executing statement's recheck qual from its `Resolved` (`cells_from` files it), never the qual it was built with (that
//! one was compiled in the building execution's context), a worker slot's binding as the leader's (pglfast#3520); the
//! unbind drops the qual, and a parked rechecking source bound without one is refused. Its kept buffers are charged: the binding statement's, the session's once parked, the slot
//! depot's when `transfer_held` sends them there, the taking statement's at the take; they leave with the source.
use gravity_hosts_rig as common;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source};
use gravity_hosts::ops::scan::pgrc::granule_source::Resolved;
use gravity_hosts::{BitmapSource, FilterFn, HostRegistry, SiteRecipe};
use mcx::MemoryContext;
use memledger::{charge, Holder, Owner, Release};
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref<'a>(snap: &'a Rc<SnapshotData<'static>>, resolved: &'a Resolved) -> BindingRef<'a> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

/// Worker slot `ordinal`'s binding (driver `WorkerBinding::as_ref`), bound on the leader's thread as `bind_hosts` binds it.
fn worker_ref<'a>(snap: &'a Rc<SnapshotData<'static>>, resolved: &'a Resolved, ordinal: u16) -> BindingRef<'a> {
    BindingRef { ordinal: Some(ordinal), ..binding_ref(snap, resolved) }
}

/// The seams a bitmap heap scan asks beyond the rig's (`common::install`).
fn install_bitmap() {
    install();
    if !xact_seams::isolation_is_serializable::is_installed() {
        xact_seams::isolation_is_serializable::set(|| false);
    }
}

/// A one-key probe of the fixture index over column 1 (`col > $1`), as a bitmap walk's recipe.
fn leaf(mcx: mcx::Mcx<'static>) -> SiteRecipe {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4GT, F_INT4GT);
    SiteRecipe::from_plan_any_am(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap()
}

/// A recheck qual that passes every row and counts its calls.
fn counting(calls: &Rc<Cell<u32>>) -> FilterFn {
    let c = Rc::clone(calls);
    Rc::new(move |_| {
        c.set(c.get() + 1);
        Ok(true)
    })
}

/// An EvalPlanQual row at `pos` under key `col > key`: the source re-checks the keys, then runs its recheck qual.
fn substituted(src: &mut BitmapSource, pos: ScanPos, key: u64) -> bool {
    src.substitute(pos);
    src.seek(&[key], &[0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    src.next(&mut v, &mut n).unwrap()
}

#[test]
fn a_parked_bitmap_source_rechecks_with_each_executions_qual_and_charges_its_buffers() {
    let _g = serial();
    install_bitmap();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("c5-bitmap-park")));
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &(0..1000).collect::<Vec<i32>>());
    let pos = ScanPos::new(HEAP_OID, 0, tids[2].ip_posid);
    let snap = static_mvcc_snapshot();
    let (a, b) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    memledger::bind_backend(196, 9196);
    let c = |o| charge::owner_row(o).committed;

    // statement 1: its registry's entry 0 rechecks with qual A, and the source is derived from it
    let mut reg = HostRegistry::new();
    reg.push_bitmap(leaf(mcx), Some(counting(&a)));
    let mut src = BitmapSource::new(leaf(mcx), reg.bitmap_recheck(0).cloned()).at_entry(0);
    memledger::statement_begin();
    let stmt = charge::context().0;
    let (session, depot) = (Owner::Session(stmt.acct()), Owner::Cache(Holder::SlotDepot));
    let (s0, d0) = (c(session), c(depot));
    let mut res = Resolved::new();
    res.cells_from(&reg);
    src.bind(&binding_ref(&snap, &res)).unwrap();
    let kept = src.kept_bytes() as i64;
    assert!(kept > 0, "a bound source keeps its key and row buffers");
    assert_eq!(c(stmt), kept, "bound: the statement's");
    assert!(substituted(&mut src, pos, 1), "the row passes the key and the recheck");
    assert_eq!((a.get(), b.get()), (1, 0), "statement 1's qual rechecks");
    src.unbind();
    drop(res);
    drop(reg);
    memledger::statement_end();
    assert_eq!((c(session) - s0, Rc::strong_count(&a)), (kept, 1), "parked: the session's bytes, and no qual of statement 1 held");

    // a worker set's park and take move the bytes through the slot depot
    src.transfer_held(depot, Release::Evict);
    assert_eq!((c(session) - s0, c(depot) - d0), (0, kept), "parked in the depot: its holder's");
    src.transfer_held(session, Release::Session);
    assert_eq!((c(session) - s0, c(depot) - d0), (kept, 0), "back on the session");

    // statement 2, another admission: its own registry's qual B
    let mut reg = HostRegistry::new();
    reg.push_bitmap(leaf(mcx), Some(counting(&b)));
    memledger::statement_begin();
    let stmt2 = charge::context().0;
    let mut res = Resolved::new();
    res.cells_from(&reg);
    src.bind(&binding_ref(&snap, &res)).unwrap();
    assert_eq!((c(stmt2), c(session) - s0), (kept, 0), "bound again: statement 2's");
    assert!(substituted(&mut src, pos, 1));
    assert_eq!((a.get(), b.get()), (1, 1), "statement 2's qual rechecks, never statement 1's");
    src.unbind();
    drop(res);
    drop(reg);
    memledger::statement_end();
    assert_eq!(Rc::strong_count(&b), 1, "parked again: no qual held");

    // pglfast#3520: statement 3 at W8, the source a parked worker set's: a worker slot's binding takes its statement's
    // qual C as the leader's does (it took none, and the bind was refused: the W8 rerun of every such statement failed)
    let cq = Rc::new(Cell::new(0));
    let mut reg = HostRegistry::new();
    reg.push_bitmap(leaf(mcx), Some(counting(&cq)));
    memledger::statement_begin();
    let mut res = Resolved::new();
    res.cells_from(&reg);
    src.bind(&worker_ref(&snap, &res, 3)).unwrap();
    assert!(substituted(&mut src, pos, 1));
    assert_eq!((a.get(), b.get(), cq.get()), (1, 1, 1), "statement 3's qual rechecks on the worker slot");
    src.unbind();
    drop(res);
    drop(reg);
    memledger::statement_end();
    assert_eq!(Rc::strong_count(&cq), 1, "unbound: the worker slot's source holds no qual of statement 3");

    // a parked rechecking source bound without its statement's qual is refused, never run without one
    memledger::statement_begin();
    let none = Resolved::new();
    assert!(src.bind(&binding_ref(&snap, &none)).is_err(), "no recheck qual for a rechecking entry");
    drop(none);
    memledger::statement_end();

    drop(src);
    assert_eq!(c(session) - s0, 0, "dropped with the source");
    memledger::unbind_backend();
}
