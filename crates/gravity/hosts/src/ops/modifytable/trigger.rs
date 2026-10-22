use std::cell::RefCell;
use std::rc::Rc;

use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};
use gravity_program::Cmd;
use mcx::{Mcx, PgVec};
use types_snapshot::SnapshotData;
use trigger::{TriggerWhenCache, TriggerWhenEval};
use types_core::Oid;
use types_error::PgResult;
use types_rel::{Relation, RELKIND_VIEW};
use types_trigger::{TriggerDesc, TRIGGER_FOR_BEFORE, TRIGGER_FOR_DELETE, TRIGGER_FOR_INSERT, TRIGGER_FOR_INSTEAD, TRIGGER_FOR_ROW, TRIGGER_FOR_UPDATE};
use types_tuple::ItemPointerData;

use crate::scratch::NodeScratch;
use crate::ops::modifytable::trigger_before;
use crate::ops::modifytable::trigger_before::BeforeRow;
use crate::ops::modifytable::trigger_stmt::{MergeOps, StmtTriggers};
use crate::ops::modifytable::write_constraints::ModifiedCols;

pub fn trig_ext(d: &TriggerDesc<'_>) -> TrigExt {
    let mut e = TrigExt::NONE;
    // bounded("catalog-sized: one per trigger of the relation (its pg_trigger rows), a few flag tests each")
    for t in d.triggers.iter() {
        let ty = t.tgtype;
        let c = if TRIGGER_FOR_INSERT(ty) { TRIG_INSERT } else { 0 } | if TRIGGER_FOR_UPDATE(ty) { TRIG_UPDATE } else { 0 } | if TRIGGER_FOR_DELETE(ty) { TRIG_DELETE } else { 0 };
        if !TRIGGER_FOR_ROW(ty) {
            e.statement |= c;
        } else if TRIGGER_FOR_INSTEAD(ty) {
            e.instead_row |= c;
        } else if TRIGGER_FOR_BEFORE(ty) {
            e.before_row |= c;
        } else {
            e.after_row |= c;
            if t.tgisinternal {
                e.internal |= c;
            }
            if t.tgdeferrable {
                e.deferrable |= c;
            }
        }
        if t.tgoldtable.is_some() || t.tgnewtable.is_some() {
            e.transition |= c;
        }
    }
    e
}

/// The relation's trigger kinds through the relcache (`RelationGetTriggerDesc`); a relation without triggers
/// has none, an uninstalled seam (the rig) answers unknown.
pub fn trig_ext_of(relid: Oid) -> TrigExt {
    if !relcache_seams::relation_get_trigger_desc::is_installed() {
        return TrigExt::UNKNOWN;
    }
    match relcache_seams::relation_get_trigger_desc::call(relid) {
        Ok(Some(d)) => trig_ext(&d),
        Ok(None) => TrigExt::NONE,
        Err(_) => TrigExt::UNKNOWN,
    }
}

/// H-f: a MERGE target's trigger refusals are its subcommands' (each action's command as its own statement would
/// refuse). Statement triggers and transition tables are served (`StmtTriggers::bind` over the subcommands, CMD_MERGE's
/// capture); V4R-313: BEFORE ROW triggers too, the actions' prologue (ExecUpdatePrologue / ExecDeletePrologue under
/// ExecMergeMatched: GetTupleForTrigger without its own recheck, `BeforeRow::merge`).
fn merge_refusal(ext: &TrigExt, merge: MergeOps, view: bool) -> Option<&'static str> {
    let ops = [(merge.insert, Cmd::Insert), (merge.update, Cmd::Update), (merge.delete, Cmd::Delete)];
    ops.iter().filter(|o| o.0).find_map(|o| ext.refusal_on(o.1, view))
}

pub enum Bound {
    None,
    Host(TriggerHost),
    Refused(&'static str),
}

/// The statement half a row-trigger bind takes (`TriggerHost::bind`): the written relation's own, or the
/// partitioned root's under a routed / pruned write (`None` = the root fires none) with the root itself.
pub enum StmtOf<'a> {
    Own,
    Root(Option<Rc<RefCell<StmtTriggers>>>, &'a Relation<'static>),
}

/// The AFTER ROW half of the write's trigger calls (`ExecAR{Insert,Update,Delete}Triggers`): each written row's
/// event queued under this query level with the row's ctid(s), the WHEN states compiled once per bind in the
/// host's node context; `TrigExt::refusal` names what the bind does not serve. The statement half (`stmt`) is
/// `trigger_stmt.rs`'s: bound here, its capture handed to each row's twin.
pub struct TriggerHost {
    desc: Option<Rc<TriggerDesc<'static>>>,
    when: TriggerWhenCache<'static>,
    modified: Option<ModifiedCols>,
    after: bool,
    pub before: Option<BeforeRow>,
    stmt: Option<Rc<RefCell<StmtTriggers>>>,
    /// The leaf's attmap to the root's row type when the root's transition table takes its rows (`None` = same layout).
    to_root: Option<PgVec<'static, i16>>,
    scratch: NodeScratch,
}

fn child_to_root(mcx: Mcx<'static>, heap: &Relation<'static>, root: Option<&Relation<'static>>, captures: bool) -> PgResult<Option<PgVec<'static, i16>>> {
    match root {
        Some(r) if captures => tupdesc::build_attrmap_by_name_if_req(mcx, &heap.rd_att, &r.rd_att, false),
        _ => Ok(None),
    }
}

impl TriggerHost {
    /// `stmt_of`: whose statement-level triggers this bind runs — the relation's own, or (a partition written
    /// under its root) the root's, bound by the root's host (ExecInitModifyTable binds only the result relation's;
    /// the partition's own never fire under it; `write_route.rs`).
    /// `merge`: a MERGE's subcommands (H-f; `MergeOps::NONE` for any other command).
    pub fn bind(heap: &Relation<'static>, cmd: Cmd, conflict_update: bool, merge: MergeOps, modified: Option<ModifiedCols>, snapshot: Option<Rc<SnapshotData<'static>>>, stmt_of: StmtOf<'_>) -> PgResult<Bound> {
        Self::bind_moving(heap, cmd, conflict_update, merge, false, modified, snapshot, stmt_of)
    }

    /// `moving` (H-e (c)): an UPDATE whose rows may move out of this leaf (`write_move.rs`): its DELETE row triggers fire
    /// too (ExecDelete with changingPart: BEFORE ROW DELETE, AFTER ROW DELETE of a moved row).
    pub fn bind_moving(heap: &Relation<'static>, cmd: Cmd, conflict_update: bool, merge: MergeOps, moving: bool, modified: Option<ModifiedCols>, snapshot: Option<Rc<SnapshotData<'static>>>, stmt_of: StmtOf<'_>) -> PgResult<Bound> {
        let (root_stmt, root) = match stmt_of {
            StmtOf::Own => (None, None),
            StmtOf::Root(s, r) => (Some(s), Some(r)),
        };
        let root_captures = root_stmt.as_ref().is_some_and(|s| s.as_ref().is_some_and(|s| s.borrow().captures()));
        // relhastriggers outlives the last DROP TRIGGER (RemoveTriggerById): a relation without a descriptor has none
        let desc = if !heap.rd_hastriggers {
            None
        } else if !relcache_seams::relation_get_trigger_desc::is_installed() {
            return Ok(Bound::Refused("a written relation with triggers (no trigger descriptor seam)"));
        } else {
            relcache_seams::relation_get_trigger_desc::call(heap.rd_id)?
        };
        // D2830: the statement's own target keeps this copy for its AFTER events (a leaf under a root resolves at firing)
        if root.is_none() && heap.rd_hastriggers {
            crate::ops::modifytable::trigger_stmt::target(heap.rd_id, desc.as_ref());
        }
        let Some(desc) = desc else {
            if !root_captures {
                return Ok(Bound::None);
            }
            let scratch = NodeScratch::new("GravityTriggerHost");
            let to_root = child_to_root(scratch.mcx(), heap, root, true)?;
            return Ok(Bound::Host(TriggerHost { desc: None, when: TriggerWhenCache::default(), modified, after: true, before: None, stmt: root_stmt.flatten(), to_root, scratch }));
        };
        let ext = trig_ext(&desc);
        let refusal = if cmd == Cmd::Merge { merge_refusal(&ext, merge, heap.rd_rel.relkind == RELKIND_VIEW) } else { ext.refusal_on(cmd, heap.rd_rel.relkind == RELKIND_VIEW) };
        if let Some(what) = refusal.or_else(|| ext.refusal_on(Cmd::Delete, false).filter(|_| moving)) {
            return Ok(Bound::Refused(what));
        }
        // ON CONFLICT DO UPDATE's arm is an ExecUpdate: the UPDATE row kinds fire on the conflict row too
        let before = trigger_before::fires(&desc, cmd) || (conflict_update && desc.trig_update_before_row) || (moving && desc.trig_delete_before_row);
        if modified.is_none() && (cmd != Cmd::Insert || conflict_update) && desc.triggers.iter().any(|t| t.tgnattr > 0) {
            return Ok(Bound::Refused("an UPDATE OF column trigger on a write without its modified columns"));
        }
        let scratch = NodeScratch::new("GravityTriggerHost");
        // ExecGetAllUpdatedCols: `UPDATE OF b` on a generated `b` fires when a column its expression reads is updated
        let modified = match modified {
            Some((ins, upd)) if (cmd == Cmd::Update || conflict_update || merge.update) && heap.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored || c.has_generated_virtual) => {
                let mcx = scratch.mcx();
                let mut all = upd.union(&types_nodes::Bitmapset::empty(), mcx)?;
                nodemodifytable::add_generated_extra_updated_cols(mcx, heap, desc.trig_update_before_row, &mut all)?;
                Some((ins, mcx::alloc_leak_in(mcx, all)?))
            }
            m => m,
        };
        let stmt = match root_stmt {
            None => StmtTriggers::bind(heap, &desc, cmd, conflict_update, merge, modified)?,
            Some(s) => s,
        };
        let captures = stmt.as_ref().is_some_and(|s| s.borrow().captures());
        let after = ext.fires(cmd) || (conflict_update && ext.fires(Cmd::Update)) || (moving && ext.fires(Cmd::Delete)) || captures;
        if !after && !before && stmt.is_none() {
            return Ok(Bound::None);
        }
        let before = if before { Some(BeforeRow::bind(heap, &desc, modified, snapshot)?.merge(cmd == Cmd::Merge)) } else { None };
        let to_root = child_to_root(scratch.mcx(), heap, root, captures)?;
        Ok(Bound::Host(TriggerHost { desc: Some(desc), when: TriggerWhenCache::default(), modified, after, before, stmt, to_root, scratch }))
    }

    fn conv<'a>(to_root: &'a Option<PgVec<'static, i16>>, heap: &'a Relation<'static>, root: Option<&'a Relation<'static>>) -> Option<trigger::ChildToRoot<'a, 'static>> {
        let map = to_root.as_deref()?;
        root.map(|r| trigger::ChildToRoot { map, child_desc: heap.rd_att.as_ref(), root_desc: r.rd_att.as_ref() })
    }

    /// `recheck`: ExecInsertIndexTuples' recheckIndexes (H-a2: the deferrable unique indexes with a possible conflict), whose
    /// unique_key_recheck events AfterTriggerSaveEvent queues.
    pub fn after_insert(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, root: Option<&Relation<'static>>, tid: ItemPointerData, recheck: &[Oid]) -> PgResult<()> {
        if !self.after {
            return Ok(());
        }
        let desc = self.desc.clone();
        let stmt = self.stmt.as_ref().map(|s| s.borrow());
        let capture = stmt.as_deref().and_then(|s| s.capture(false));
        let conv = Self::conv(&self.to_root, heap, root);
        let mut when = TriggerWhenEval { mcx: self.scratch.mcx(), cache: &mut self.when, modified_cols: None };
        trigger::ExecARInsertTriggers(mcx, heap, desc.as_ref(), tid, recheck, capture, Some(&mut when), conv.as_ref())
    }

    /// `conflict`: the row is ON CONFLICT DO UPDATE's (the INSERT statement's update arm, its own capture).
    pub fn after_update(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, root: Option<&Relation<'static>>, old: ItemPointerData, new: ItemPointerData, conflict: bool, recheck: &[Oid]) -> PgResult<()> {
        if !self.after {
            return Ok(());
        }
        let desc = self.desc.clone();
        let modified = self.modified.map(|m| m.1);
        let stmt = self.stmt.as_ref().map(|s| s.borrow());
        let capture = stmt.as_deref().and_then(|s| s.capture(conflict));
        let conv = Self::conv(&self.to_root, heap, root);
        let mut when = TriggerWhenEval { mcx: self.scratch.mcx(), cache: &mut self.when, modified_cols: modified };
        trigger::ExecARUpdateTriggers(mcx, heap, desc.as_ref(), None, None, Some(old), Some(new), recheck, capture, Some(&mut when), false, conv.as_ref(), conv.as_ref(), modified)
    }

    pub fn after_delete(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, root: Option<&Relation<'static>>, old: ItemPointerData) -> PgResult<()> {
        if !self.after {
            return Ok(());
        }
        let desc = self.desc.clone();
        let stmt = self.stmt.as_ref().map(|s| s.borrow());
        let capture = stmt.as_deref().and_then(|s| s.capture(false));
        let conv = Self::conv(&self.to_root, heap, root);
        let mut when = TriggerWhenEval { mcx: self.scratch.mcx(), cache: &mut self.when, modified_cols: None };
        trigger::ExecARDeleteTriggers(mcx, heap, desc.as_ref(), old, capture, Some(&mut when), false, conv.as_ref())
    }

    /// ExecDeleteEpilogue for a moved row (changingPart): the old row into the UPDATE's OLD transition table (old-only
    /// ExecARUpdateTriggers), then the AFTER ROW DELETE triggers as the source half of a move (cloned PK RI triggers
    /// skipped: the root's event is the foreign-key leg's), without the capture when it filed the row.
    pub fn after_delete_moved(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, root: Option<&Relation<'static>>, old: ItemPointerData) -> PgResult<()> {
        if !self.after {
            return Ok(());
        }
        let desc = self.desc.clone();
        let stmt = self.stmt.as_ref().map(|s| s.borrow());
        let capture = stmt.as_deref().and_then(|s| s.capture(false));
        // MOVE: under a MERGE the old row is the DELETE's (ExecDelete's UPDATE capture is CMD_UPDATE's alone)
        let moved = capture.filter(|c| c.tcs_update_old_table && !stmt.as_deref().is_some_and(StmtTriggers::merge));
        let conv = Self::conv(&self.to_root, heap, root);
        let mut when = TriggerWhenEval { mcx: self.scratch.mcx(), cache: &mut self.when, modified_cols: None };
        if moved.is_some() {
            trigger::ExecARUpdateTriggers(mcx, heap, desc.as_ref(), None, None, Some(old), None, &[], moved, Some(&mut when), false, conv.as_ref(), None, None)?;
        }
        trigger::ExecARDeleteTriggers(mcx, heap, desc.as_ref(), old, if moved.is_some() { None } else { capture }, Some(&mut when), true, conv.as_ref())
    }

    /// ExecInsert's AFTER ROW half for a moved row (the INSERT half of a cross-partition UPDATE): the new row into the
    /// UPDATE's NEW transition table (new-only ExecARUpdateTriggers), then the AFTER ROW INSERT triggers without it.
    pub fn after_insert_moved(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, root: Option<&Relation<'static>>, tid: ItemPointerData, recheck: &[Oid]) -> PgResult<()> {
        if !self.after {
            return Ok(());
        }
        let desc = self.desc.clone();
        let stmt = self.stmt.as_ref().map(|s| s.borrow());
        let capture = stmt.as_deref().and_then(|s| s.capture(false));
        // MOVE: under a MERGE the new row is the INSERT's (ExecInsert's UPDATE capture is CMD_UPDATE's alone)
        let moved = capture.filter(|c| c.tcs_update_new_table && !stmt.as_deref().is_some_and(StmtTriggers::merge));
        let conv = Self::conv(&self.to_root, heap, root);
        let mut when = TriggerWhenEval { mcx: self.scratch.mcx(), cache: &mut self.when, modified_cols: None };
        if moved.is_some() {
            trigger::ExecARUpdateTriggers(mcx, heap, desc.as_ref(), None, None, None, Some(tid), &[], moved, Some(&mut when), false, None, conv.as_ref(), None)?;
        }
        trigger::ExecARInsertTriggers(mcx, heap, desc.as_ref(), tid, recheck, if moved.is_some() { None } else { capture }, Some(&mut when), conv.as_ref())
    }

    pub fn reads_rows(&self) -> bool {
        self.after || self.before.is_some()
    }

    /// The statement half this bind took (the root's under a routed or pruned write): the move host's capture.
    pub fn stmt(&self) -> Option<Rc<RefCell<StmtTriggers>>> {
        self.stmt.clone()
    }

    /// `trig_update_after_row` (ExecCrossPartitionUpdate's foreign-key leg runs when the source leaf has one).
    pub fn update_after_row(&self) -> bool {
        self.desc.as_ref().is_some_and(|d| d.trig_update_after_row)
    }

    /// The row's end: the BEFORE ROW slots released (their pins, the trigger's image).
    /// ExecInsert's `ri_TrigDesc->trig_insert_before_row`: a routed row rechecks its partition after the trigger.
    pub fn before_insert_row(&self) -> bool {
        self.desc.as_ref().is_some_and(|d| d.trig_insert_before_row)
    }

    pub fn row_done(&mut self) {
        if let Some(b) = self.before.as_mut() {
            b.clear();
        }
    }
}
