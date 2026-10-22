use gravity_hosts_rig as common;

use std::cell::RefCell;
use std::rc::Rc;

use common::*;
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};
use gravity_hosts::ops::modifytable::trigger::{trig_ext, Bound, StmtOf, TriggerHost};
use gravity_hosts::ops::modifytable::trigger_stmt::MergeOps;
use gravity_program::Cmd;
use mcx::{Mcx, MemoryContext, PgString, PgVec};
use types_rel::Relation;
use types_trigger::{Trigger, TriggerDesc, TRIGGER_TYPE_BEFORE, TRIGGER_TYPE_DELETE, TRIGGER_TYPE_INSERT, TRIGGER_TYPE_ROW, TRIGGER_TYPE_UPDATE};

fn trig<'m>(mcx: Mcx<'m>, oid: u32, tgtype: i16, internal: bool, deferrable: bool, newtable: bool) -> Trigger<'m> {
    Trigger { tgoid: oid, tgname: PgString::from_str_in("t", mcx).unwrap(), tgfoid: 0, tgtype, tgenabled: b'O' as i8, tgisinternal: internal, tgisclone: false, tgconstrrelid: 0, tgconstrindid: 0, tgconstraint: 0, tgdeferrable: deferrable, tginitdeferred: false, tgnargs: 0, tgnattr: 0, tgattr: PgVec::new_in(mcx), tgargs: PgVec::new_in(mcx), tgqual: None, tgoldtable: None, tgnewtable: newtable.then(|| PgString::from_str_in("n", mcx).unwrap()) }
}

fn desc<'m>(mcx: Mcx<'m>, triggers: Vec<Trigger<'m>>) -> TriggerDesc<'m> {
    let mut v = PgVec::new_in(mcx);
    let (mut br, mut ar, mut st, mut nt) = ((false, false, false), (false, false, false), (false, false, false), false);
    for t in triggers {
        let row = t.tgtype & TRIGGER_TYPE_ROW != 0;
        let before = t.tgtype & TRIGGER_TYPE_BEFORE != 0;
        let (i, u, d) = (t.tgtype & TRIGGER_TYPE_INSERT != 0, t.tgtype & TRIGGER_TYPE_UPDATE != 0, t.tgtype & TRIGGER_TYPE_DELETE != 0);
        if row && before {
            br = (br.0 | i, br.1 | u, br.2 | d);
        } else if row {
            ar = (ar.0 | i, ar.1 | u, ar.2 | d);
        } else if !before {
            st = (st.0 | i, st.1 | u, st.2 | d);
        }
        nt |= t.tgnewtable.is_some() && u;
        v.push(t);
    }
    TriggerDesc { triggers: v, trig_insert_before_row: br.0, trig_insert_after_row: ar.0, trig_insert_instead_row: false, trig_insert_before_statement: false, trig_insert_after_statement: st.0, trig_update_before_row: br.1, trig_update_after_row: ar.1, trig_update_instead_row: false, trig_update_before_statement: false, trig_update_after_statement: st.1, trig_delete_before_row: br.2, trig_delete_after_row: ar.2, trig_delete_instead_row: false, trig_delete_before_statement: false, trig_delete_after_statement: st.2, trig_truncate_before_statement: false, trig_truncate_after_statement: false, trig_insert_new_table: false, trig_update_old_table: false, trig_update_new_table: nt, trig_delete_old_table: false }
}

#[test]
fn the_trigger_kinds_classify_by_command_and_the_lattice_names_the_first_unserved_one() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let ar_iu = TRIGGER_TYPE_ROW | TRIGGER_TYPE_INSERT | TRIGGER_TYPE_UPDATE;
    let br_d = TRIGGER_TYPE_ROW | TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_DELETE;
    let stmt_u = TRIGGER_TYPE_UPDATE;
    let e = trig_ext(&desc(mcx, vec![trig(mcx, 1, ar_iu, false, false, false), trig(mcx, 2, br_d, false, false, false), trig(mcx, 3, stmt_u, false, false, true)]));
    assert_eq!((e.after_row, e.before_row, e.statement, e.transition, e.internal, e.deferrable, e.known), (TRIG_INSERT | TRIG_UPDATE, TRIG_DELETE, TRIG_UPDATE, TRIG_UPDATE, 0, 0, true));
    assert_eq!(e.refusal(Cmd::Insert), None);
    assert!(e.fires(Cmd::Insert));
    assert_eq!(e.refusal(Cmd::Update), None);
    assert_eq!(e.refusal(Cmd::Delete), None);
    assert!(e.fires_before(Cmd::Delete) && !e.fires(Cmd::Delete));
    // V4R-313: a MERGE target's BEFORE ROW triggers are served (the actions' prologue)
    assert_eq!(e.refusal(Cmd::Merge), None);
    let st = trig_ext(&desc(mcx, vec![trig(mcx, 6, stmt_u, false, false, true)]));
    // V4R-257: a MERGE target's statement triggers and transition tables are served (H-f fires them).
    assert_eq!(st.refusal(Cmd::Merge), None);
    assert_eq!(st.refusal(Cmd::Update), None);
    assert!(!st.fires(Cmd::Update));
    let ri = trig_ext(&desc(mcx, vec![trig(mcx, 4, TRIGGER_TYPE_ROW | TRIGGER_TYPE_DELETE, true, false, false), trig(mcx, 5, TRIGGER_TYPE_ROW | TRIGGER_TYPE_UPDATE, false, true, false)]));
    assert_eq!(ri.refusal(Cmd::Delete), None);
    assert!(ri.fires(Cmd::Delete) && ri.internal & TRIG_DELETE != 0);
    assert_eq!(ri.refusal(Cmd::Update), None);
    assert!(ri.fires(Cmd::Update) && ri.deferrable & TRIG_UPDATE != 0);
    assert_eq!(ri.refusal(Cmd::Insert), None);
    assert!(!ri.fires(Cmd::Insert));
    assert_eq!(TrigExt::UNKNOWN.refusal(Cmd::Insert), Some("a written relation with triggers (its trigger facts unknown)"));
    assert_eq!(TrigExt::NONE.refusal(Cmd::Merge), None);
    assert_eq!(st.statement, TRIG_UPDATE);
    let bs = trig_ext(&desc(mcx, vec![trig(mcx, 7, TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_INSERT, false, false, false)]));
    assert_eq!((bs.statement, bs.before_row), (TRIG_INSERT, 0));
}

/// The descriptor seam reads this thread's DESC; installed once for the binary (a seam is set once).
fn desc_seam() {
    static SEAM: std::sync::Once = std::sync::Once::new();
    SEAM.call_once(|| relcache_seams::relation_get_trigger_desc::set(|_| Ok(DESC.with(|d| d.borrow().clone()))));
}

thread_local! {
    static DESC: RefCell<Option<Rc<TriggerDesc<'static>>>> = const { RefCell::new(None) };
}

#[test]
fn the_trigger_host_binds_under_after_row_triggers_and_refuses_the_other_kinds_by_name() {
    let _g = serial();
    install();
    desc_seam();
    let ctx = Box::leak(Box::new(MemoryContext::new("t")));
    let mcx: Mcx<'static> = ctx.mcx();
    let kind = |b: Bound| match b {
        Bound::None => "none",
        Bound::Host(_) => "host",
        Bound::Refused(what) => what,
    };
    for (tgtype, deferrable, want) in [(TRIGGER_TYPE_ROW | TRIGGER_TYPE_UPDATE, false, "host"), (TRIGGER_TYPE_ROW | TRIGGER_TYPE_INSERT, false, "none"), (TRIGGER_TYPE_ROW | TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_UPDATE, false, "host"), (TRIGGER_TYPE_UPDATE, false, "host"), (TRIGGER_TYPE_ROW | TRIGGER_TYPE_UPDATE, true, "host")] {
        DESC.with(|d| *d.borrow_mut() = Some(Rc::new(desc(mcx, vec![trig(mcx, 1, tgtype, false, deferrable, false)]))));
        let mut rd = heap_relation_data(mcx);
        rd.rd_hastriggers = true;
        let heap = Relation::open(rd, None);
        assert_eq!(kind(TriggerHost::bind(&heap, Cmd::Update, false, MergeOps::NONE, None, None, StmtOf::Own).unwrap()), want, "tgtype {tgtype}");
        let _ = heap.close(types_storage::lock::NoLock);
    }
    let heap = Relation::open(heap_relation_data(mcx), None);
    assert_eq!(kind(TriggerHost::bind(&heap, Cmd::Update, false, MergeOps::NONE, None, None, StmtOf::Own).unwrap()), "none", "a relation without triggers");
    let _ = heap.close(types_storage::lock::NoLock);
    DESC.with(|d| *d.borrow_mut() = None);
}

/// H-f: a MERGE target's statement triggers and transition tables are its subcommands' (fireBSTriggers /
/// fireASTriggers over `mt_merge_subcommands`; the CMD_MERGE capture needs a query, so it is the e2e's): an AFTER
/// STATEMENT UPDATE trigger binds the statement half when an action updates, fires nothing for a MERGE whose actions
/// never update, and a BEFORE ROW trigger on an action's command stays refused by name (it was refused whatever the
/// actions were).
#[test]
fn a_merge_binds_the_statement_triggers_of_its_actions() {
    let _g = serial();
    install();
    desc_seam();
    let ctx = Box::leak(Box::new(MemoryContext::new("t")));
    let mcx: Mcx<'static> = ctx.mcx();
    let kind = |b: Bound| match b {
        Bound::None => "none",
        Bound::Host(_) => "host",
        Bound::Refused(what) => what,
    };
    let upd = MergeOps { insert: false, update: true, delete: false };
    let ins = MergeOps { insert: true, update: false, delete: false };
    let br = "host";
    for (tgtype, newtable, ops, want) in [(TRIGGER_TYPE_UPDATE, false, upd, "host"), (TRIGGER_TYPE_UPDATE, false, ins, "none"), (TRIGGER_TYPE_UPDATE, false, MergeOps::NONE, "none"), (TRIGGER_TYPE_ROW | TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_UPDATE, false, upd, br)] {
        DESC.with(|d| *d.borrow_mut() = Some(Rc::new(desc(mcx, vec![trig(mcx, 1, tgtype, false, false, newtable)]))));
        let mut rd = heap_relation_data(mcx);
        rd.rd_hastriggers = true;
        let heap = Relation::open(rd, None);
        assert_eq!(kind(TriggerHost::bind(&heap, Cmd::Merge, false, ops, None, None, StmtOf::Own).unwrap_or_else(|e| panic!("tgtype {tgtype} newtable {newtable} {ops:?}: {}", e.message))), want, "tgtype {tgtype} newtable {newtable} {ops:?}");
        let _ = heap.close(types_storage::lock::NoLock);
    }
    DESC.with(|d| *d.borrow_mut() = None);
}

/// D2830 (B39, C ExecGetTriggerResultRel): a statement's own target (`StmtOf::Own`) is filed under the query level with
/// the descriptor its bind read; a second bind of the relation at the level keeps the first copy; the level's targets are
/// taken once; a relation without triggers is not filed; `drop_targets_from` forgets the level.
#[test]
fn a_served_target_files_its_bind_time_descriptor_per_level() {
    use gravity_hosts::ops::modifytable::trigger_stmt::{drop_targets_from, take_targets, target};
    let _g = serial();
    install();
    desc_seam();
    let ctx = Box::leak(Box::new(MemoryContext::new("t")));
    let mcx: Mcx<'static> = ctx.mcx();
    let depth = trigger::query_depth();
    drop_targets_from(i32::MIN);
    let first = Rc::new(desc(mcx, vec![trig(mcx, 1, TRIGGER_TYPE_ROW | TRIGGER_TYPE_UPDATE, false, false, false)]));
    let second = Rc::new(desc(mcx, vec![trig(mcx, 2, TRIGGER_TYPE_ROW | TRIGGER_TYPE_UPDATE, false, false, false)]));
    let mut rd = heap_relation_data(mcx);
    rd.rd_hastriggers = true;
    let heap = Relation::open(rd, None);
    for d in [&first, &second] {
        DESC.with(|x| *x.borrow_mut() = Some(Rc::clone(d)));
        assert!(matches!(TriggerHost::bind(&heap, Cmd::Update, false, MergeOps::NONE, None, None, StmtOf::Own).unwrap(), Bound::Host(_)));
    }
    let taken = take_targets();
    assert_eq!(taken.len(), 1, "one entry per relation and level");
    assert_eq!(taken[0].0, heap.rd_id);
    assert!(taken[0].1.as_ref().is_some_and(|d| Rc::ptr_eq(d, &first)), "the first bind's copy");
    assert!(take_targets().is_empty(), "taken once");
    let _ = heap.close(types_storage::lock::NoLock);
    let plain = Relation::open(heap_relation_data(mcx), None);
    assert!(matches!(TriggerHost::bind(&plain, Cmd::Update, false, MergeOps::NONE, None, None, StmtOf::Own).unwrap(), Bound::None));
    assert!(take_targets().is_empty(), "a relation without triggers is not filed");
    let _ = plain.close(types_storage::lock::NoLock);
    target(42, None);
    drop_targets_from(depth);
    assert!(take_targets().is_empty(), "the level forgotten");
    DESC.with(|d| *d.borrow_mut() = None);
}
