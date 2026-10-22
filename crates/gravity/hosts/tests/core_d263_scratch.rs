//! D263: a host's node context (`NodeScratch::mcx`, a bump context that lives as long as the host) takes nothing per bind.
//! A parked Node binds its hosts once per execution, so a per-bind allocation there grows the Node for as long as its
//! prepared statement runs. The statement-life facts are armed once per Node (and again when the relcache epoch moves);
//! every bind after the first leaves the node context where it was.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use datum::Datum;
use gravity_driver::{BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Writer};
use gravity_hosts::ops::modifytable::write_check::CheckSpec;
use gravity_hosts::{IndexSource, SiteRecipe, WriteHost};
use gravity_program::{Cmd, IndexPlan, WritePlan};
use mcx::MemoryContext;
use types_nodes::parsenodes::WCOKind;
use gravity_hosts::ops::modifytable::write_constraints::ModifiedCols;
use types_nodes::{Bitmapset, Node, NodeList};
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

static FLAG: AtomicU8 = AtomicU8::new(0);

const BINDS: usize = 1000;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn plan(cmd: Cmd) -> WritePlan {
    let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
    WritePlan::write(a, cmd, HEAP_OID, &[IndexPlan::new(a, IDX_OID, &[1], true)])
}

/// One RLS check option whose qual is `true` (the implicit-AND list `ExecInitQual` takes), in a context the specs outlive.
fn true_check() -> Rc<[CheckSpec]> {
    let qctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("d263 quals")));
    let qm = qctx.mcx();
    let t = Node::mk_const(qm, 16, -1, 0, 1, Datum::from_bool(true), false, true).unwrap();
    let node = Node::mk_list(qm, NodeList::make1(qm, t).unwrap()).unwrap();
    Rc::from(vec![CheckSpec { kind: WCOKind::WCO_RLS_UPDATE_CHECK, relname: Arc::from("t"), polname: Some(Arc::from("p")), node, target: 0, inits: Rc::from([]) }])
}

#[test]
fn a_write_host_with_check_options_binds_a_thousand_times_in_flat_memory() {
    let _g = serial();
    install();
    set_unique_index(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10, 20]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&plan(Cmd::Update)).with_checks(Some(true_check()));
    w.bind(&b).unwrap();
    w.unbind();
    let used = w.scratch_used();
    for _ in 0..BINDS {
        w.bind(&b).unwrap();
        w.unbind();
    }
    assert_eq!(w.scratch_used(), used, "{BINDS} binds of a parked write host grew its node context");
    drop(w);
    quiesced();
}

/// H16 (b), the simple protocol's parked writer: the modified columns a write host is derived with point into the deriving
/// statement's plan, which is gone when a later statement binds the parked host; from its first bind on the host reads its
/// own copies (node context, taken once: the binds after it stay flat).
#[test]
fn a_write_host_owns_its_modified_columns_from_its_first_bind() {
    let _g = serial();
    install();
    set_unique_index(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10, 20]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let stmt = MemoryContext::new("d263 statement plan");
    let sm = stmt.mcx();
    let ins = mcx::alloc_leak_in(sm, Bitmapset::empty()).unwrap();
    let upd = mcx::alloc_leak_in(sm, Bitmapset::make_singleton(sm, 2).unwrap()).unwrap();
    // SAFETY: the plan's bitmaps as the admission hands them in (`admit::modified_cols`: 'static over the statement's plan)
    let cols: ModifiedCols = unsafe { (&*(ins as *const Bitmapset<'_> as *const Bitmapset<'static>), &*(upd as *const Bitmapset<'_> as *const Bitmapset<'static>)) };
    let plan_at = (cols.0 as *const Bitmapset<'static> as usize, cols.1 as *const Bitmapset<'static> as usize);

    let mut w = WriteHost::new(&plan(Cmd::Update)).with_modified(Some(cols));
    w.bind(&b).unwrap();
    w.unbind();
    drop(stmt); // the deriving statement ends: its plan's memory goes
    let used = w.scratch_used();
    for _ in 0..BINDS {
        w.bind(&b).unwrap();
        w.unbind();
    }
    let (i, u) = w.modified_cols().expect("the host keeps its modified columns");
    assert_ne!((i as *const Bitmapset<'static> as usize, u as *const Bitmapset<'static> as usize), plan_at, "the host still reads the deriving plan's bitmaps");
    assert!(i.is_empty() && u.is_member(2) && u.num_members() == 1, "the host's copies are not the statement's columns");
    assert_eq!(w.scratch_used(), used, "{BINDS} binds after the first grew the write host's node context");
    drop(w);
    quiesced();
}

#[test]
fn an_index_source_binds_a_thousand_times_in_flat_memory() {
    let _g = serial();
    install();
    set_unique_index(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    load_rows(mcx, &[10, 20]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, true), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4EQ, F_INT4EQ);
    let mut src = IndexSource::new(SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap());
    src.bind(&b).unwrap();
    src.unbind();
    let used = src.scratch_used();
    for _ in 0..BINDS {
        src.bind(&b).unwrap();
        src.unbind();
    }
    assert_eq!(src.scratch_used(), used, "{BINDS} binds of a parked index source grew its node context");
    drop(src);
    quiesced();
}
