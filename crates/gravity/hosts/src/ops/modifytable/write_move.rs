//! H-e (c): the cross-partition UPDATE's move (PostgreSQL 18.6 ExecCrossPartitionUpdate, nodeModifyTable.c; pgrust
//! nodemodifytable `exec_cross_partition_update`). A row whose new values leave its partition under a partitioned root
//! is deleted from its leaf by the source's write host (`write.rs` `move_row`: the leaf's BEFORE ROW DELETE triggers, a
//! NULL skipping the move; `heap_delete` with `changing_part`; the old-only UPDATE capture and the AFTER ROW DELETE
//! triggers of a moved row) and inserted here through the root: the row in the root's layout
//! (`ExecGetChildToRootMap`), `ExecFindPartition`, then the destination leaf's own write host (the routed form: its
//! BEFORE ROW INSERT triggers, its generated columns, constraints and indexes, the new-only UPDATE capture before its
//! AFTER ROW INSERT triggers, as ExecInsert does for a moved row). Unlike a routed INSERT (`write_route.rs` `RouteHost`)
//! the root's statement triggers are the UPDATE's, bound by the source's host and handed in: no INSERT statement
//! trigger fires. Then the foreign-key leg (`ExecCrossPartitionUpdateForeignKey`): a source leaf with AFTER ROW UPDATE
//! triggers queues the root's UPDATE event over the moved row (its RI triggers see one update, not a delete + insert),
//! or raises PostgreSQL's 0A000 when a non-root ancestor of the source is referenced by a foreign key.
//!
//! The destinations are the partitions `write.route` admitted (`RoutePlan.moves`), each re-admitted under its
//! RowExclusiveLock at its first moved row from its current relcache entry, as ExecInitPartitionInfo does
//! (`write_route.rs` `revalidate`, D833: never compared with the admission, never an ERROR by name). A row the root
//! routes nowhere raises PostgreSQL's "no partition of relation ... found for row". The count and RETURNING are the
//! insert half's (`written`: the row as the destination wrote it, in the source's layout; V4R-349 `sys`: its system
//! columns, the destination's tableoid and new ctid, its header fields where PostgreSQL's inserted slot has them).
//!
//! V4R-329: the UPDATE's check options (WITH CHECK OPTION / RLS) go to each destination as ExecInitPartitionInfo builds
//! the partition's WithCheckOptions: the first result relation's list (`linitial(withCheckOptionLists)`) mapped by name
//! onto the destination (`write_route.rs` `leaf_checks`); the destination's ExecInsert checks them with the UPDATE's
//! kind (WCO_RLS_UPDATE_CHECK, after its BEFORE ROW INSERT triggers) and the view checks after the insert (`write.rs`).

use std::cell::RefCell;
use std::rc::Rc;

use datum::Datum;
use gravity_abi::Datum as Word;
use mcx::GrowIn;
use gravity_abi::ScanPos;
use gravity_driver::{Outcome, WriteOp, Writer};
use gravity_program::{Cmd, Conflict, WritePlan};
use mcx::Mcx;
use trigger::{TriggerWhenCache, TriggerWhenEval};
use types_core::{CommandId, Oid};
use types_error::{PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED};
use types_rel::{Relation, RELKIND_PARTITIONED_TABLE};
use types_snapshot::SnapshotData;
use types_storage::lock::{NoLock, RowExclusiveLock};
use types_tuple::ItemPointerData;

use crate::scratch::NodeScratch;
use crate::ops::modifytable::trigger_stmt::StmtTriggers;
use crate::ops::modifytable::write::{WriteHost, WriteRecipe};
use crate::ops::modifytable::write_check::CheckSpec;
use crate::ops::modifytable::write_constraints::ModifiedCols;
use crate::ops::modifytable::write_route::{changed, leaf_checks, revalidate, root_map, to_leaf, to_root, Routing};

pub(crate) struct Mover {
    root: Oid,
    modified: Option<ModifiedCols>,
    /// The UPDATE's root statement triggers (its transition capture), the source host's.
    stmt: Option<Rc<RefCell<StmtTriggers>>>,
    routing: Option<Routing>,
    /// One destination host per router leaf index, built at the leaf's first moved row.
    hosts: Vec<Option<WriteHost>>,
    /// The destination of the last moved row: `written` and the foreign-key leg are its.
    last: Option<usize>,
    cid: CommandId,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    /// W2: the execution's parameter words, for a destination's check options' InitPlan params.
    words: crate::ops::modifytable::write_check::ParamWords,
    /// The moved row in the root's layout, and in the destination's (`ri_RootToPartitionMap`). memsig E-pub: in the
    /// host's node scratch (`scratch.mcx()`, declared after them), homed with it on the binding statement or the depot's
    /// holder; sized to the row's columns once, reused by every row.
    root_values: mcx::PgVec<'static, Word>,
    root_nulls: mcx::PgVec<'static, u8>,
    conv_values: mcx::PgVec<'static, Word>,
    conv_nulls: mcx::PgVec<'static, u8>,
    /// The root's WHEN states for the foreign-key leg's UPDATE event.
    root_when: TriggerWhenCache<'static>,
    /// V4R-329: the first result relation's check options and that relation (open while bound), mapped per destination.
    checks: Option<Rc<[CheckSpec]>>,
    first: Oid,
    first_rel: Option<Relation<'static>>,
    /// V4R-349: RETURNING's system columns (WritePlan.sys, sys_header), each destination's host capturing its insert's;
    /// `virt`: the last moved row went in from PostgreSQL's virtual root slot (`sys`).
    sys: (bool, bool),
    virt: bool,
    scratch: NodeScratch,
}

impl Mover {
    /// The move of a cross-partition UPDATE's plan (`RoutePlan.moves`), `None` for any other plan.
    pub(crate) fn of(plan: &WritePlan) -> Option<Rc<RefCell<Mover>>> {
        // MOVE: a MERGE's UPDATE action moves as an UPDATE's row does (ExecMergeMatched's ExecUpdateAct)
        let r = plan.route.as_ref().filter(|r| matches!(plan.cmd, Cmd::Update | Cmd::Merge) && !r.moves.is_empty())?;
        let scratch = NodeScratch::new("GravityMoveHost");
        let sm = scratch.mcx();
        let m = Mover { root: r.root, modified: None, stmt: None, routing: None, hosts: Vec::new(), last: None, cid: 0, snapshot: None, words: crate::ops::modifytable::write_check::ParamWords::NONE, root_values: mcx::PgVec::new_in(sm), root_nulls: mcx::PgVec::new_in(sm), conv_values: mcx::PgVec::new_in(sm), conv_nulls: mcx::PgVec::new_in(sm), root_when: TriggerWhenCache::default(), checks: None, first: 0, first_rel: None, sys: (plan.sys, plan.sys_header), virt: false, scratch }; // alloc: Node
        Some(Rc::new(RefCell::new(m))) // alloc: Node
    }

    /// V4R-329: the UPDATE's check options (the source's write host's or the multi-target host's) and its first result
    /// relation `first`: the list of target 0 (`CheckSpec.target`) is the one every destination takes.
    pub(crate) fn with_checks(&mut self, first: Oid, checks: Option<Rc<[CheckSpec]>>) {
        let own: Option<Vec<CheckSpec>> = checks.map(|c| c.iter().filter(|s| s.target == 0).map(|s| CheckSpec { kind: s.kind, relname: s.relname.clone(), polname: s.polname.clone(), node: s.node, target: 0, inits: Rc::clone(&s.inits) }).collect()); // alloc: Node
        self.checks = own.filter(|c| !c.is_empty()).map(Into::into);
        self.first = first;
    }

    /// The router over the root at the statement's first moved row (the source's host holds the root's lock): the
    /// statement's command id, snapshot, modified columns and the root's UPDATE statement triggers.
    pub(crate) fn bind(&mut self, cid: CommandId, snapshot: Option<Rc<SnapshotData<'static>>>, modified: Option<ModifiedCols>, stmt: Option<Rc<RefCell<StmtTriggers>>>, words: crate::ops::modifytable::write_check::ParamWords) -> PgResult<()> {
        if self.routing.is_some() {
            return Ok(());
        }
        let mcx = self.scratch.mcx();
        let root = table::table_open(mcx, self.root, NoLock)?;
        if root.rd_rel.relkind != RELKIND_PARTITIONED_TABLE {
            let e = changed(&root, "the moved row's root is not a partitioned table");
            root.close(NoLock)?;
            return Err(e);
        }
        let mut pdir = None;
        let router = match execpartition::PartitionTupleRouting::new(mcx, &root, &mut pdir) {
            Ok(r) => r,
            Err(e) => {
                root.close(NoLock)?;
                return Err(e);
            }
        };
        let slot = tableam::table_slot_create(mcx, &root)?;
        if self.checks.is_some() && self.first_rel.is_none() {
            // the first result relation is the plan's (locked by the executor as a result relation; taken again, as a no-op)
            let first = lmgr_seams::lock_relation_oid::call(self.first, RowExclusiveLock).and_then(|_| table::table_open(mcx, self.first, NoLock));
            match first {
                Ok(f) => self.first_rel = Some(f),
                Err(e) => {
                    drop(router);
                    drop(slot);
                    if let Some(p) = pdir {
                        partdesc::DestroyPartitionDirectory(p);
                    }
                    root.close(NoLock)?;
                    return Err(e);
                }
            }
        }
        self.routing = Some(Routing { root, router, pdir, slot });
        self.hosts.clear();
        self.last = None;
        self.cid = cid;
        self.snapshot = snapshot;
        self.words = words;
        self.modified = modified;
        self.stmt = stmt;
        Ok(())
    }

    pub(crate) fn unbind(&mut self) -> PgResult<()> {
        // bounded("catalog-sized: one destination write host per leaf partition the statement's moved rows reached, at most the root's leaf partitions; teardown runs whole")
        for h in self.hosts.drain(..).flatten() {
            drop(h);
        }
        self.stmt = None;
        self.snapshot = None;
        self.last = None;
        self.root_when = TriggerWhenCache::default();
        if let Some(r) = self.routing.take() {
            drop(r.router);
            drop(r.slot);
            if let Some(p) = r.pdir {
                partdesc::DestroyPartitionDirectory(p);
            }
            r.root.close(NoLock)?;
        }
        if let Some(f) = self.first_rel.take() {
            f.close(NoLock)?;
        }
        self.scratch.reset_exec();
        Ok(())
    }

    /// ExecCrossPartitionUpdate's part 2: the new row `values` / `nulls` of source leaf `src` (its layout), deleted there,
    /// converted to the root's layout and inserted through the root. `Skipped`: the destination's BEFORE ROW INSERT
    /// trigger returned NULL (the row is gone, uncounted, as in PostgreSQL).
    pub(crate) fn insert(&mut self, src: &Relation<'static>, values: &[Datum], nulls: &[bool]) -> PgResult<Outcome> {
        let mcx = self.scratch.mcx();
        let exec = self.scratch.exec_mcx();
        let Mover { routing, hosts, modified, cid, snapshot, words, stmt, root_values, root_nulls, conv_values, conv_nulls, last, checks, first_rel, sys, virt, .. } = self;
        let r = routing.as_mut().expect("gravity: a move on an unbound move host");
        *last = None;
        // ExecGetChildToRootMap (by name): the source's row in the root's layout
        let natts = r.root.rd_att.attrs.len();
        root_values.clear();
        root_values.resize_in(natts, 0);
        root_nulls.clear();
        root_nulls.resize_in(natts, 1);
        conv_values.clear();
        conv_nulls.clear();
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..values.len() {
            conv_values.push_in(values[k].as_u64());
            conv_nulls.push_in(nulls[k] as u8);
        }
        let src_map = root_map(&r.root, src);
        match &src_map {
            Some(map) => to_root(map, conv_values, conv_nulls, root_values, root_nulls),
            None => {
                let n = natts.min(conv_values.len());
                root_values[..n].copy_from_slice(&conv_values[..n]);
                root_nulls[..n].copy_from_slice(&conv_nulls[..n]);
            }
        }
        exectuples::exec_clear_tuple(&mut r.slot, mcx);
        {
            let base = r.slot.base_mut();
            let n = natts.min(base.tts_values.len());
            // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..n {
                base.tts_values[k] = Datum::from_u64(root_values[k]);
                base.tts_isnull[k] = root_nulls[k] != 0;
            }
        }
        exectuples::exec_store_virtual_tuple(&mut r.slot);
        r.slot.base_mut().tts_tableOid = r.root.rd_id;
        let both;
        let cols = match *modified {
            None => None,
            Some((ins, upd)) if upd.is_empty() => Some(ins),
            Some((ins, upd)) if ins.is_empty() => Some(upd),
            Some((ins, upd)) => {
                both = ins.union(upd, mcx)?;
                Some(&both)
            }
        };
        let idx = r.router.find_partition(&mut r.slot, exec, &mut r.pdir, cols)?;
        exectuples::exec_clear_tuple(&mut r.slot, mcx);
        if hosts.len() <= idx {
            hosts.resize_with(idx + 1, || None); // alloc: Node
        }
        if hosts[idx].is_none() {
            let leaf = r.router.leaf_rel(idx);
            let recipes = revalidate(mcx, leaf)?;
            let recipe = WriteRecipe { heap: leaf.rd_id, cmd: Cmd::Insert, conflict: Conflict::None, arbiters: Vec::new(), defer: false }; // alloc: Node
            // V4R-329: the first result relation's check options in the destination's layout (the statement's context, reset
            // at unbind where the destination hosts drop)
            let wco = match first_rel.as_ref() {
                Some(f) => leaf_checks(exec, checks, f, leaf)?,
                None => None,
            };
            let mut host = WriteHost::for_leaf(recipe, recipes, r.root.alias(), stmt.clone()).with_modified(*modified).with_checks(wco).with_sys(sys.0, sys.1).moved_in();
            if !host.bind_leaf(*cid, snapshot.clone(), *words)? {
                return Err(changed(leaf, "the moved row's destination write host refused it"));
            }
            hosts[idx] = Some(host);
        }
        let host = hosts[idx].as_mut().expect("just built");
        *last = Some(idx);
        // ExecCrossPartitionUpdate converts a source row of another layout into mt_root_tuple_slot (a partitioned root's
        // slot is virtual); ExecPrepareTupleRouting passes it to table_tuple_insert unconverted when the destination has
        // the root's layout, and a virtual slot has no tuple header (`sys`)
        *virt = src_map.is_some() && r.router.leaf_attrmap(idx).is_none();
        let (values, isnull) = to_leaf(r.router.leaf_attrmap(idx), root_values, root_nulls, conv_values, conv_nulls);
        host.write(WriteOp::Insert, ScanPos::NONE, values, isnull).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: moved row's insert failed: {e:?}"))))) // alloc: Cold
    }

    /// D1112 row B (update#164 / #165 / #171 / #176): the last destination parked at its check option's SubPlan read
    /// (its WriteChecks are the first result relation's list mapped onto it, `leaf_checks`): the parked call's words are its.
    pub(crate) fn sub_call(&mut self) -> gravity_driver::Result<*mut u64> {
        match self.last.and_then(|i| self.hosts.get_mut(i)).and_then(|h| h.as_mut()) {
            Some(h) => h.sub_call(),
            None => Err(gravity_driver::Error::host("gravity: a SubPlan call with no moved row's destination")),
        }
    }

    /// D1112 row B: the parked destination's insert resumed over the same row (`root_values` still holds it in the
    /// root's layout, converted again as `insert` hands it; `written` rewrites them only after a written row).
    pub(crate) fn resume(&mut self) -> PgResult<Outcome> {
        let Mover { routing, hosts, last, root_values, root_nulls, conv_values, conv_nulls, .. } = self;
        let (Some(idx), Some(r)) = (*last, routing.as_ref()) else {
            return Err(Box::new(PgError::error("gravity: a moved row's resume with no destination".to_string()))); // alloc: Cold
        };
        let Some(host) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else {
            return Err(Box::new(PgError::error("gravity: a moved row's resume with no destination host".to_string()))); // alloc: Cold
        };
        let (values, isnull) = to_leaf(r.router.leaf_attrmap(idx), root_values, root_nulls, conv_values, conv_nulls);
        host.resume(WriteOp::Insert, ScanPos::NONE, values, isnull).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: moved row's insert failed: {e:?}"))))) // alloc: Cold
    }

    /// ExecCrossPartitionUpdateForeignKey, after the insert half wrote the row: a non-root ancestor of `src` referenced
    /// by a foreign key is PostgreSQL's 0A000; else the root's AFTER ROW UPDATE triggers (RI's, the source and
    /// destination leaves named) take the move as one UPDATE of the row at `old` (now at the destination's new tid).
    pub(crate) fn foreign_key(&mut self, mcx: Mcx<'static>, src: &Relation<'static>, old: ItemPointerData) -> PgResult<()> {
        let Mover { routing, hosts, last, root_when, scratch, .. } = self;
        let r = routing.as_ref().expect("gravity: a move on an unbound move host");
        let root_oid = r.root.rd_id;
        // bounded("catalog-sized: one per partition ancestor of the source leaf below the root (its pg_inherits chain), as ExecCrossPartitionUpdateForeignKey walks them")
        for anc in pg_inherits::get_partition_ancestors(mcx, src.rd_id)?.iter() {
            if *anc == root_oid {
                break;
            }
            let Some(td) = relcache_seams::relation_get_trigger_desc::call(*anc)? else { continue };
            if td.trig_update_after_row && td.triggers.iter().any(|t| !t.tgisclone && trigger::ri_trigger_kind(t.tgfoid) == types_trigger::RI_TRIGGER_PK) {
                let name = |oid: Oid| lsyscache::relation::get_rel_name(mcx, oid).map(|n| n.map(|s| s.as_str().to_string()).unwrap_or_default()); // alloc: Cold
                let (anc_name, root_name) = (name(*anc)?, name(root_oid)?);
                return Err(Box::new(PgError::error("cannot move tuple across partitions when a non-root ancestor of the source partition is directly referenced in a foreign key".to_string()) // alloc: Cold
                        .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED)
                        .with_detail(format!("A foreign key points to ancestor \"{anc_name}\" but not the root ancestor \"{root_name}\".")) // alloc: Cold
                        .with_hint(format!("Consider defining the foreign key on table \"{root_name}\".")), // alloc: Cold
                ));
            }
        }
        let Some(idx) = *last else { return Ok(()) };
        let Some(new) = hosts.get(idx).and_then(|h| h.as_ref()).map(|h| h.inserted_tid()) else { return Ok(()) };
        let Some(root_td) = (if r.root.rd_hastriggers { relcache_seams::relation_get_trigger_desc::call(root_oid)? } else { None }) else { return Ok(()) };
        let dst = r.router.leaf_rel(idx);
        let src_map = tupdesc::build_attrmap_by_name_if_req(mcx, &src.rd_att, &r.root.rd_att, false)?;
        let dst_map = tupdesc::build_attrmap_by_name_if_req(mcx, &dst.rd_att, &r.root.rd_att, false)?;
        let src_conv = src_map.as_deref().map(|map| trigger::ChildToRoot { map, child_desc: src.rd_att.as_ref(), root_desc: r.root.rd_att.as_ref() });
        let dst_conv = dst_map.as_deref().map(|map| trigger::ChildToRoot { map, child_desc: dst.rd_att.as_ref(), root_desc: r.root.rd_att.as_ref() });
        let mut when = TriggerWhenEval { mcx: scratch.mcx(), cache: root_when, modified_cols: None };
        trigger::ExecARUpdateTriggers(mcx, &r.root, Some(&root_td), Some(src), Some(dst), Some(old), Some(new), &[], None, Some(&mut when), true, src_conv.as_ref(), dst_conv.as_ref(), None)
    }

    /// V4R-349: the last moved row's system columns, its destination's host's (`WriteHost::sys_fill`): ctid and tableoid
    /// always (heapam_tuple_insert sets both on any slot), xmin .. cmax PostgreSQL's "cannot retrieve a system column in
    /// this context" when its slot was the root's virtual one (`virt`, tts_virtual_getsysattr), else the stamped header.
    pub(crate) fn sys(&self, values: &mut [Word], isnull: &mut [u8]) -> PgResult<bool> {
        match self.last.and_then(|i| self.hosts.get(i)).and_then(|h| h.as_ref()) {
            Some(h) => h.sys_fill(values, isnull, self.virt),
            None => Ok(false),
        }
    }

    /// The moved row as its destination wrote it (a BEFORE ROW INSERT trigger's row, the destination's generated
    /// columns), in source `src`'s layout: RETURNING's row after a move (ExecInsert's slot, the insert half's).
    pub(crate) fn written(&mut self, src: &Relation<'static>, values: &mut [Word], isnull: &mut [u8]) -> bool {
        let Mover { routing, hosts, last, root_values, root_nulls, conv_values, conv_nulls, .. } = self;
        let (Some(idx), Some(r)) = (*last, routing.as_ref()) else { return false };
        let Some(h) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else { return false };
        // the destination's row in the root's layout: its own (a trigger's, the generated columns) when it has one
        let map = r.router.leaf_attrmap(idx);
        let n = map.map_or(root_values.len(), |m| m.len());
        conv_values.clear();
        conv_values.resize_in(n, 0);
        conv_nulls.clear();
        conv_nulls.resize_in(n, 1);
        if h.written(conv_values, conv_nulls) {
            match map {
                Some(m) => to_root(m, conv_values, conv_nulls, root_values, root_nulls),
                None => {
                    let k = n.min(root_values.len());
                    root_values[..k].copy_from_slice(&conv_values[..k]);
                    root_nulls[..k].copy_from_slice(&conv_nulls[..k]);
                }
            }
        }
        // root → source (by name): the source's attribute i is the root's `map[i]`
        let map = root_map(&r.root, src);
        values.fill(0);
        isnull.fill(1);
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for i in 0..values.len() {
            let at = map.as_ref().map_or(i as i16 + 1, |m| m.get(i).copied().unwrap_or(0));
            if at > 0 && (at as usize) <= root_values.len() {
                values[i] = root_values[at as usize - 1];
                isnull[i] = root_nulls[at as usize - 1];
            }
        }
        true
    }
}

impl Drop for Mover {
    fn drop(&mut self) {
        let _ = self.unbind();
    }
}
