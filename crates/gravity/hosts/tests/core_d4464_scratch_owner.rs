//! D4464: a host's scratch (`NodeScratch`: its node, exec and bind roots) lives with its Node, past the statement that
//! built the Node, and a reset keeps each root's first block. The roots follow the Node: the binding statement's from
//! each bind, the slot depot's holder when a worker set parks there and the taking statement at the take
//! (`Source::transfer_held`). Before, they stayed on the statement whose build made them (their posts following its end to
//! its session): a parked set's scratch was never the depot's, and a later statement's bind grew the session's.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_driver::{BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source};
use gravity_hosts::{IndexSource, SiteRecipe};
use mcx::MemoryContext;
use memledger::{charge, Holder, Owner, Release};
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

/// An equality probe of the fixture index over column 1 (`col = $1`): a btree walk, whose scan the unbind parks in the
/// source's scratch.
fn recipe(mcx: mcx::Mcx<'_>) -> SiteRecipe {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4EQ, F_INT4EQ);
    SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap()
}

/// One bind's probes of `keys`, unbound after: the rows found.
fn walk(src: &mut IndexSource, snap: &Rc<SnapshotData<'static>>) -> usize {
    src.bind(&binding_ref(snap)).unwrap();
    let (mut v, mut n, mut rows) = ([0u64; 1], [0u8; 1], 0);
    for key in [7u64, 70, 700] {
        src.seek(&[key], &[0]).unwrap();
        while src.next(&mut v, &mut n).unwrap() {
            assert_eq!(v[0] as i32 as u64, key);
            rows += 1;
        }
    }
    src.unbind();
    rows
}

/// Owner `o`'s context bytes (named and inferred).
fn ctx_bytes(o: Owner) -> i64 {
    let (named, inferred) = charge::owner_ctx(o);
    named + inferred
}

#[test]
fn an_index_sources_scratch_follows_its_node_to_the_binding_statement_and_the_slot_depot() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("d4464-scratch")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..1000).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    memledger::bind_backend(197, 9197);
    let depot = Owner::Cache(Holder::SlotDepot);

    // statement 1 builds the source (its roots are made under it) and runs it
    memledger::statement_begin();
    let stmt1 = charge::context().0;
    let session = Owner::Session(stmt1.acct());
    let mut src = IndexSource::new(recipe(mcx));
    assert_eq!(walk(&mut src, &snap), 3);
    assert!(ctx_bytes(stmt1) > 0, "statement 1 holds the scratch it built and bound");
    memledger::statement_end();

    // a worker set's park: the scratch leaves the backend's session for the depot's holder
    let (s0, d0) = (ctx_bytes(session), ctx_bytes(depot));
    src.transfer_held(depot, Release::Evict);
    let moved = ctx_bytes(depot) - d0;
    assert!(moved > 0, "parked in the depot: the scratch is its holder's");
    assert_eq!(s0 - ctx_bytes(session), moved, "and no longer the session's");

    // statement 2 takes the set: the scratch is the taking statement's, and its bind keeps it there
    memledger::statement_begin();
    let (stmt2, release) = charge::context();
    src.transfer_held(stmt2, release);
    assert_eq!((ctx_bytes(depot) - d0, ctx_bytes(stmt2)), (0, moved), "taken: the taking statement's");
    assert_eq!(walk(&mut src, &snap), 3);
    assert!(ctx_bytes(stmt2) >= moved, "bound by the taking statement: still its");
    memledger::statement_end();

    // statement 3 binds the parked source with no depot between: its bind homes the scratch on it
    memledger::statement_begin();
    let stmt3 = charge::context().0;
    let s3 = ctx_bytes(session);
    assert_eq!(walk(&mut src, &snap), 3);
    assert!(ctx_bytes(stmt3) >= moved, "bound again: the binding statement's");
    assert!(s3 - ctx_bytes(session) >= moved, "and the session's no more");
    memledger::statement_end();

    let s4 = ctx_bytes(session);
    drop(src);
    assert!(s4 - ctx_bytes(session) >= moved, "dropped with the source");
    assert_eq!(ctx_bytes(depot), d0);
    memledger::unbind_backend();
}
