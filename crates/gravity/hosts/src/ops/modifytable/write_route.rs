//! The routed INSERT's write host (docs/gravity/ladder/decompose-writes.md need 1; `lower/src/write_route.rs`
//! admits the leaves): ExecInsert through a partitioned root. Per row: the root-form cells into the root's
//! slot, `ExecFindPartition` (the port's `PartitionTupleRouting`, the leaf opened under RowExclusiveLock at
//! its first row as the executor does), then the leaf's own write host (`WriteHost`, the routed form: a routed
//! row skips the leaf's partition constraint — `ri_RootResultRelInfo` set — unless the leaf has a BEFORE ROW
//! INSERT trigger, whose row may have left the bound; its row triggers, CHECK / NOT NULL constraints and
//! btree indexes are the leaf's). The leaf is re-admitted under its lock at that first row from its current
//! relcache entry, as ExecInitPartitionInfo does (V4R-303: a leaf attached or altered since admission is written
//! as PostgreSQL writes it); its indexes are the list at that row, as PG's (D765: `IndexMaint`).

use std::cell::RefCell;
use std::rc::Rc;

use gravity_abi::Datum as Word;
use mcx::GrowIn;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, Outcome, Result, WriteOp, Writer};
use gravity_program::{Cmd, Conflict, RouteLeaf, WritePlan};
use mcx::Mcx;
use tableam_vocab::TableAm;
use types_core::{CommandId, Oid};
use types_error::{PgError, PgResult};
use types_nodes::nodes_enums::CmdType;
use types_nodes::Bitmapset;
use types_rel::{Relation, RELKIND_PARTITIONED_TABLE, RELKIND_RELATION};
use types_tuple::htup::FirstLowInvalidHeapAttributeNumber as FLIHAN;
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::{NoLock, RowExclusiveLock};

use crate::ops::modifytable::index_maint::{unmaintainable_routed, IndexRecipe};
use crate::ops::shared::rel::{open_index, snapshot_of};
use crate::scratch::NodeScratch;
use crate::ops::modifytable::trigger_stmt::{MergeOps, StmtTriggers};
use crate::ops::modifytable::write::{WriteHost, WriteRecipe};
use crate::ops::modifytable::write_check::CheckSpec;
use crate::ops::modifytable::write_constraints::ModifiedCols;

pub(crate) struct Routing {
    pub(crate) root: Relation<'static>,
    pub(crate) router: execpartition::PartitionTupleRouting<'static>,
    pub(crate) pdir: Option<partdesc::PartitionDirectory<'static>>,
    pub(crate) slot: SlotData<'static>,
}

pub struct RouteHost {
    root: Oid,
    cmd: Cmd,
    /// V4R-498: a routed MERGE's subcommands (only INSERT: registry.rs), its root statement triggers' commands.
    merge: MergeOps,
    conflict: Conflict,
    /// The root's arbiter indexes (`WritePlan.arbiters`); each leaf host gets their partitions on the leaf.
    arbiters: Vec<u32>,
    /// The leaf whose row last conflicted (`Outcome::Conflict`): `existing` and the DO UPDATE write are its.
    conflict_leaf: Option<usize>,
    /// The leaf that wrote the last row: `written` (the row as its BEFORE ROW trigger returned it) is its.
    last_leaf: Option<usize>,
    /// D774 (`WritePlan.sys`, `sys_header`): every leaf host keeps its row's system columns for RETURNING; `ins` (V4R-501):
    /// the last row op was an INSERT, whose sys site is past ON CONFLICT DO UPDATE's conflict site.
    sys: (bool, bool),
    ins: bool,
    /// R4c (`WritePlan.old`): RETURNING OLD under ON CONFLICT DO UPDATE: each leaf host fetches the version its update
    /// replaced, which `old` hands on in the root's layout; the old site precedes the sys site (`sys_off`).
    old: bool,
    /// `WritePlan.sys_old`: the replaced version's system columns, the old site's last six cells (the leaf's, PG's oc_Existing).
    sys_old: bool,
    modified: Option<ModifiedCols>,
    /// D636: the root's check options (WITH CHECK OPTION / RLS); each leaf host gets them in its layout (`leaf_checks`).
    checks: Option<Rc<[CheckSpec]>>,
    /// G2(d) (`WritePlan.checks_of`): a MERGE's NOT MATCHED INSERT through the root of its result relations carries one
    /// list per result relation, each in that relation's layout; a routed row is checked by the first one's mapped onto
    /// its leaf by name (ExecInitPartitionInfo: linitial(withCheckOptionLists) over firstResultRel's attnos; a leaf that
    /// is itself a result relation keeps its own list, the same quals translated). 0: the root's own list.
    checks_of: Oid,
    routing: Option<Routing>,
    /// writesubplan (merge#514): the route leg of a MERGE through an INHERITANCE root (`bind_beside` over a plain heap):
    /// ExecInitMerge's rootRelInfo is the root itself, so a NOT MATCHED INSERT goes into the root, no tuple routing, its
    /// own row triggers firing (merge.sql's measurement tree: a BEFORE INSERT trigger redirects the row to a child and
    /// returns NULL), its constraints and indexes its own. The root opened for the statement; its host is `hosts[0]`.
    own: Option<Relation<'static>>,
    /// One leaf host per router leaf index, built at the leaf's first row (the own root's at 0).
    hosts: Vec<Option<WriteHost>>,
    cid: CommandId,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    /// W2: the execution's parameter words, for a leaf's check options' InitPlan params.
    words: crate::ops::modifytable::write_check::ParamWords,
    /// The root's statement triggers (`root_stmt`), bound once per statement, handed to each leaf host.
    stmt: Option<Rc<RefCell<StmtTriggers>>>,
    /// The row in the leaf's layout when the leaf has a root→leaf attribute map (`ri_RootToPartitionMap`). memsig E-pub:
    /// in the host's node scratch (`scratch.mcx()`, declared after them), homed with it; sized to the leaf's columns once.
    conv_values: mcx::PgVec<'static, Word>,
    conv_nulls: mcx::PgVec<'static, u8>,
    scratch: NodeScratch,
    /// PGRCSERVE M1: the plan's `defer`, which each leaf host's bind narrows by the leaf's row triggers.
    defer: bool,
}

impl RouteHost {
    pub fn new(plan: &WritePlan) -> RouteHost {
        let scratch = NodeScratch::new("GravityRouteHost");
        let sm = scratch.mcx();
        RouteHost { root: plan.route.as_ref().map_or(plan.target_rel, |r| r.root), cmd: plan.cmd, merge: MergeOps::NONE, conflict: plan.conflict, arbiters: plan.arbiters.to_vec(), conflict_leaf: None, last_leaf: None, sys: (plan.sys, plan.sys_header), ins: false, old: plan.old, sys_old: plan.sys_old, modified: None, checks: None, checks_of: plan.checks_of, routing: None, own: None, stmt: None, hosts: Vec::new(), cid: 0, snapshot: None, words: crate::ops::modifytable::write_check::ParamWords::NONE, conv_values: mcx::PgVec::new_in(sm), conv_nulls: mcx::PgVec::new_in(sm), scratch, defer: plan.defer } // alloc: Node
    }

    pub fn with_modified(mut self, cols: Option<ModifiedCols>) -> RouteHost {
        self.modified = cols;
        self
    }

    /// V4R-498: a MERGE's subcommands (its routed NOT MATCHED INSERTs).
    pub fn with_merge(mut self, ops: MergeOps) -> RouteHost {
        self.merge = ops;
        self
    }

    /// D636: this statement's check options (the root's `withCheckOptionLists`), per admission as `with_modified`.
    pub fn with_checks(mut self, checks: Option<Rc<[CheckSpec]>>) -> RouteHost {
        self.checks = checks.filter(|c| !c.is_empty());
        self
    }

    /// The root opened under the executor's lock; `false` = not the admitted shape (a refusal before any row).
    fn bind_pg(&mut self, cid: CommandId) -> PgResult<bool> {
        self.bind_root(cid, None)
    }

    /// SORT-UNION (merge#364 group): the route leg of a multi-target MERGE host (`MultiTargetHost`), its NOT
    /// MATCHED INSERTs through the root beside the result relations' UPDATE / DELETE (ExecInitMerge's rootRelInfo). The
    /// root's statement triggers are the MERGE's, bound once by that host and shared (`stmt`): one BEFORE / AFTER
    /// STATEMENT firing per statement, the routed rows' transition captures the MERGE's.
    pub(crate) fn bind_beside(&mut self, cid: CommandId, snapshot: Option<Rc<SnapshotData<'static>>>, words: crate::ops::modifytable::write_check::ParamWords, stmt: Option<Rc<RefCell<StmtTriggers>>>) -> PgResult<bool> {
        self.snapshot = snapshot;
        self.words = words;
        self.bind_root(cid, Some(stmt))
    }

    /// `bind_pg`; `shared`: the statement triggers another host bound for this statement (`bind_beside`).
    fn bind_root(&mut self, cid: CommandId, shared: Option<Option<Rc<RefCell<StmtTriggers>>>>) -> PgResult<bool> {
        let mcx = self.scratch.mcx();
        // no leaves (a tree without partitions) binds: ExecFindPartition raises PG's error at the first row
        if !matches!(self.cmd, Cmd::Insert | Cmd::Merge) {
            return Ok(false);
        }
        lmgr_seams::lock_relation_oid::call(self.root, RowExclusiveLock)?;
        let root = table::table_open(mcx, self.root, NoLock)?;
        // writesubplan (merge#514): beside a MERGE's result relations (`bind_beside`) an inheritance root is written itself
        let own = shared.is_some() && self.cmd == Cmd::Merge && root.rd_rel.relkind == RELKIND_RELATION;
        if root.rd_rel.relkind != RELKIND_PARTITIONED_TABLE && !own || root.rd_hasrules {
            root.close(NoLock)?;
            return Ok(false);
        }
        // the root's statement triggers are the statement's (its row triggers are the leaves' clones)
        let bound = match shared {
            Some(s) => Ok(Ok(s)),
            None => root_stmt(&root, self.cmd, self.conflict == Conflict::Update, self.merge, self.modified),
        };
        self.stmt = match bound {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => {
                root.close(NoLock)?;
                return Ok(false);
            }
            Err(e) => {
                root.close(NoLock)?;
                return Err(e);
            }
        };
        if own {
            self.own = Some(root);
            self.hosts.clear();
            self.conflict_leaf = None;
            self.cid = cid;
            return Ok(true);
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
        self.routing = Some(Routing { root, router, pdir, slot });
        self.hosts.clear();
        self.conflict_leaf = None;
        self.cid = cid;
        Ok(true)
    }

    pub(crate) fn unbind_pg(&mut self) -> PgResult<()> {
        // bounded("catalog-sized: one write host per leaf partition the statement routed rows to, at most the root's leaf partitions; teardown runs whole")
        for h in self.hosts.drain(..).flatten() {
            drop(h);
        }
        self.stmt = None;
        if let Some(r) = self.routing.take() {
            drop(r.router);
            drop(r.slot);
            if let Some(p) = r.pdir {
                partdesc::DestroyPartitionDirectory(p);
            }
            r.root.close(NoLock)?;
        }
        if let Some(r) = self.own.take() {
            r.close(NoLock)?;
        }
        self.scratch.reset_exec();
        Ok(())
    }

    fn write_pg(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> PgResult<Outcome> {
        let mcx = self.scratch.mcx();
        let exec = self.scratch.exec_mcx();
        if self.own.is_some() {
            return self.write_own(op, pos, values, isnull);
        }
        let RouteHost { routing, hosts, modified, checks, checks_of, cid, snapshot, words, conv_values, conv_nulls, arbiters, conflict, conflict_leaf, last_leaf, stmt, sys, old, sys_old, defer, .. } = self;
        let r = routing.as_mut().expect("gravity: write on an unbound route host");
        *last_leaf = None;
        if op != WriteOp::Insert {
            // ON CONFLICT DO UPDATE's write: the conflicting row's leaf (its layout is the root's: admission)
            let Some(idx) = conflict_leaf.take().filter(|&i| hosts.get(i).is_some_and(|h| h.as_ref().is_some_and(|h| h.heap_oid() == pos.rel))) else {
                return Err(Box::new(PgError::error("gravity: a routed write op that is not an INSERT nor the conflicting row's update".to_string()))); // alloc: Cold
            };
            let host = hosts[idx].as_mut().expect("checked");
            *last_leaf = Some(idx);
            let (values, isnull) = to_leaf(r.router.leaf_attrmap(idx), values, isnull, conv_values, conv_nulls);
            return host.write(op, pos, values, isnull).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: leaf write failed: {e:?}"))))); // alloc: Cold
        }
        // writesubplan (gen mg-0178): the row is the site's first cells, the root's (under RETURNING over a MERGE's union form
        // the site is as wide as the widest result relation)
        let n = values.len().min(r.root.rd_att.attrs.len()).min(isnull.len());
        let (values, isnull) = (&values[..n], &isnull[..n]);
        exectuples::exec_clear_tuple(&mut r.slot, mcx);
        {
            let base = r.slot.base_mut();
            let n = values.len().min(base.tts_values.len());
            // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..n {
                base.tts_values[k] = datum::Datum::from_u64(values[k]);
                base.tts_isnull[k] = isnull[k] != 0;
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
            fires::fire("route.lazy_leaf");
            let leaf = r.router.leaf_rel(idx);
            let recipes = revalidate(mcx, leaf)?;
            let recipe = WriteRecipe { heap: leaf.rd_id, cmd: Cmd::Insert, conflict: *conflict, arbiters: leaf_arbiters(mcx, leaf, arbiters)?, defer: *defer }; // alloc: Node
            // the mapped quals live in the statement's context (reset at unbind, where the leaf hosts drop). G2(d): with
            // `checks_of` set, the leaf host maps the first result relation's list onto the leaf at its bind (WriteHost
            // `checks_onto`, as for an INSERT into an inheritance root)
            let (wco, of) = match *checks_of {
                0 => (leaf_checks(exec, checks, &r.root, leaf)?, 0),
                first => (first_list(checks), first),
            };
            let mut host = WriteHost::for_leaf(recipe, recipes, r.root.alias(), stmt.clone()).with_modified(*modified).with_checks(wco).with_checks_of(of).with_sys(sys.0, sys.1).with_old(*old).with_sys_old(*sys_old);
            if !host.bind_leaf(*cid, snapshot.clone(), *words)? {
                // only a relation the write host cannot write at all (a foreign-table partition attached since admission)
                return Err(changed(leaf, "the leaf write host refused it"));
            }
            hosts[idx] = Some(host);
        }
        let host = hosts[idx].as_mut().expect("just built");
        *last_leaf = Some(idx);
        let (values, isnull) = to_leaf(r.router.leaf_attrmap(idx), values, isnull, conv_values, conv_nulls);
        let o = host.write(WriteOp::Insert, pos, values, isnull).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: leaf write failed: {e:?}")))))?; // alloc: Cold
        *conflict_leaf = (o == Outcome::Conflict).then_some(idx);
        Ok(o)
    }

    /// writesubplan (merge#514): `own`'s INSERT, the inheritance root itself written (ExecMergeNotMatched's ExecInsert
    /// into rootRelInfo, a plain heap): its host built at the first row under the lock the bind took, its indexes the
    /// list at that row (`revalidate`), its check options the first result relation's mapped onto it (`checks_of`, G2(d))
    /// or its own, its row triggers its own (WriteHost's routed form; the statement triggers are the MERGE's, shared). The
    /// row is the root's (the site's first cells), unconverted.
    fn write_own(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> PgResult<Outcome> {
        let mcx = self.scratch.mcx();
        let exec = self.scratch.exec_mcx();
        let RouteHost { own, hosts, modified, checks, checks_of, cid, snapshot, words, last_leaf, stmt, sys, old, sys_old, .. } = self;
        let root = own.as_ref().expect("gravity: write on an unbound route host");
        *last_leaf = None;
        if op != WriteOp::Insert {
            return Err(Box::new(PgError::error("gravity: a write op on an inheritance root's MERGE leg that is not an INSERT".to_string()))); // alloc: Cold
        }
        if hosts.is_empty() {
            hosts.push(None); // alloc: Node
        }
        if hosts[0].is_none() {
            let recipes = revalidate(mcx, root)?;
            let recipe = WriteRecipe { heap: root.rd_id, cmd: Cmd::Insert, conflict: Conflict::None, arbiters: Vec::new(), defer: false }; // alloc: Node
            let (wco, of) = match *checks_of {
                0 => (leaf_checks(exec, checks, root, root)?, 0),
                first => (first_list(checks), first),
            };
            let mut host = WriteHost::for_leaf(recipe, recipes, root.alias(), stmt.clone()).with_modified(*modified).with_checks(wco).with_checks_of(of).with_sys(sys.0, sys.1).with_old(*old).with_sys_old(*sys_old);
            if !host.bind_leaf(*cid, snapshot.clone(), *words)? {
                return Err(changed(root, "its write host refused it"));
            }
            hosts[0] = Some(host);
        }
        let host = hosts[0].as_mut().expect("just built");
        *last_leaf = Some(0);
        let n = values.len().min(root.rd_att.attrs.len()).min(isnull.len());
        host.write(WriteOp::Insert, pos, &values[..n], &isnull[..n]).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: root write failed: {e:?}"))))) // alloc: Cold
    }
}

/// `ri_RootToPartitionMap` over the row's cells: `map[leaf - 1]` = the root column the leaf's takes (0 = none).
pub(crate) fn to_leaf<'a>(map: Option<&[i16]>, values: &'a [Word], isnull: &'a [u8], cv: &'a mut mcx::PgVec<'static, Word>, cn: &'a mut mcx::PgVec<'static, u8>) -> (&'a [Word], &'a [u8]) {
    let Some(map) = map else { return (values, isnull) };
    cv.clear();
    cn.clear();
    // bounded("key-columns: one cell per column of the leaf, at most MaxHeapAttributeNumber (1600) columns")
    for &a in map.iter() {
        let j = a as isize - 1;
        let (v, n) = if j < 0 || j as usize >= values.len() { (0u64, 1u8) } else { (values[j as usize], isnull[j as usize]) };
        cv.push_in(v);
        cn.push_in(n);
    }
    (&cv[..], &cn[..])
}

/// `ExecGetAllUpdatedCols` for a partition: the statement's modified columns (the root's perminfo numbering) as the
/// leaf's triggers read them — renumbered through the root→leaf map by name (`ri_RootToChildMap`), or the caller's when
/// the layouts agree; `root` an open alias, else `under_root` is opened under the lock the write host took.
pub(crate) fn leaf_modified(mcx: Mcx<'static>, modified: Option<ModifiedCols>, leaf: &Relation<'static>, root: Option<&Relation<'static>>, under_root: Option<Oid>) -> PgResult<Option<ModifiedCols>> {
    let Some((ins, upd)) = modified else { return Ok(None) };
    let opened = match (root, under_root) {
        (None, Some(oid)) => Some(table::table_open(mcx, oid, NoLock)?),
        (None, None) => return Ok(modified),
        _ => None,
    };
    let map = root_map(opened.as_ref().or(root).expect("one root"), leaf);
    if let Some(o) = opened {
        o.close(NoLock)?;
    }
    let Some(map) = map else { return Ok(modified) };
    Ok(Some((mcx::alloc_leak_in(mcx, map_cols(mcx, &map, ins)?)?, mcx::alloc_leak_in(mcx, map_cols(mcx, &map, upd)?)?)))
}

/// The leaf's row back in the root's layout (`ri_PartitionToRootMap`'s inverse of `map`: leaf attribute i is root attribute `map[i]`).
pub(crate) fn to_root(map: &[i16], cv: &[Word], cn: &[u8], values: &mut [Word], isnull: &mut [u8]) {
    values.fill(0);
    isnull.fill(1);
    // bounded("key-columns: one cell per column of the leaf, at most MaxHeapAttributeNumber (1600) columns")
    for (i, &a) in map.iter().enumerate() {
        if a > 0 && (a as usize) <= values.len() {
            values[a as usize - 1] = cv[i];
            isnull[a as usize - 1] = cn[i];
        }
    }
}

/// D636: the root's check options for a leaf, as ExecInitPartitionInfo builds the leaf's WithCheckOptions:
/// `map_variable_attnos` through the root→leaf map by name, whole-row Vars to the leaf's row type; the root's own when
/// the layouts agree. A routed row is checked in the leaf's layout (ExecInsert on the leaf's ResultRelInfo).
pub(crate) fn leaf_checks(mcx: Mcx<'static>, checks: &Option<Rc<[CheckSpec]>>, root: &Relation<'static>, leaf: &Relation<'static>) -> PgResult<Option<Rc<[CheckSpec]>>> {
    let Some(c) = checks else { return Ok(None) };
    let Some(map) = root_map(root, leaf) else { return Ok(Some(Rc::clone(c))) };
    // `map` is leaf attno → root attno; map_variable_attnos takes root attno → leaf attno
    let mut attnos: Vec<i16> = vec![0; root.rd_att.attrs.len()]; // alloc: Node
    // bounded("key-columns: one entry per column of the leaf, at most MaxHeapAttributeNumber (1600) columns")
    for (i, &r) in map.iter().enumerate() {
        if r > 0 {
            attnos[r as usize - 1] = i as i16 + 1;
        }
    }
    let mut out = Vec::with_capacity(c.len()); // alloc: Node
    // bounded("other: one per check option of the statement (the plan's WithCheckOption list), its qual's Vars renumbered for the leaf as ExecInitPartitionInfo does")
    for s in c.iter() {
        // the qual's own Vars are the result relation's (a SubPlan or outer reference is write.checks' refusal)
        let mut varno = None;
        let mut find = |q: types_nodes::Node<'static>| {
            gravity_decompose::expr::walk(q, &mut |n| {
                if let Some(v) = n.as_var().filter(|v| v.varlevelsup == 0) {
                    varno.get_or_insert(v.varno);
                }
                varno.is_none()
            });
        };
        match s.node.as_list() {
            Some(l) => l.iter().for_each(&mut find),
            None => find(s.node),
        }
        let node = match varno {
            Some(v) => rewrite_manip::map_variable_attnos(mcx, s.node, v, 0, &attnos, leaf.rd_rel.reltype)?.0,
            None => s.node,
        };
        out.push(CheckSpec { kind: s.kind, relname: s.relname.clone(), polname: s.polname.clone(), node, target: s.target, inits: Rc::clone(&s.inits) }); // alloc: Node
    }
    Ok(Some(out.into()))
}

/// G2(d) (RouteHost `checks_of`): the first result relation's own list (target 0), still in that relation's attnos; the
/// leaf host maps it onto the leaf by name at its bind (WriteHost `checks_onto`: ExecInitPartitionInfo's map from
/// firstResultRel). None when the first result relation has no list.
fn first_list(checks: &Option<Rc<[CheckSpec]>>) -> Option<Rc<[CheckSpec]>> {
    let own: Vec<CheckSpec> = checks.as_ref()?.iter().filter(|s| s.target == 0).map(|s| CheckSpec { kind: s.kind, relname: s.relname.clone(), polname: s.polname.clone(), node: s.node, target: s.target, inits: Rc::clone(&s.inits) }).collect(); // alloc: Node
    if own.is_empty() {
        None
    } else {
        Some(own.into())
    }
}

/// `build_attrmap_by_name_if_req` (root → leaf, dropped columns skipped): `None` when the map is the identity.
pub(crate) fn root_map(root: &Relation<'static>, leaf: &Relation<'static>) -> Option<Vec<i16>> {
    let mut map = Vec::with_capacity(leaf.rd_att.attrs.len()); // alloc: Node
    let mut identity = root.rd_att.attrs.len() == leaf.rd_att.attrs.len();
    // bounded("key-columns: one per column of the leaf, each a name search over the root's columns (build_attrmap_by_name's double loop), at most MaxHeapAttributeNumber (1600) columns each")
    for (i, l) in leaf.rd_att.attrs.iter().enumerate() {
        let at = if l.attisdropped { 0 } else { root.rd_att.attrs.iter().position(|r| !r.attisdropped && r.attname == l.attname).map_or(0, |p| p as i16 + 1) };
        identity &= at as usize == i + 1 || (l.attisdropped && root.rd_att.attrs.get(i).is_some_and(|r| r.attisdropped));
        map.push(at); // alloc: Node
    }
    if identity {
        fires::fire("route.identity_map");
    }
    (!identity).then_some(map)
}

/// `execute_attr_map_cols`: a perminfo column set (attnos offset by `FirstLowInvalidHeapAttributeNumber`) renumbered.
fn map_cols(mcx: Mcx<'static>, map: &[i16], cols: &Bitmapset<'static>) -> PgResult<Bitmapset<'static>> {
    let mut out = Bitmapset::empty();
    // bounded("key-columns: one member test per column of the leaf and the system attributes below it, at most MaxHeapAttributeNumber (1600) columns")
    for leaf in FLIHAN..=map.len() as i32 {
        let root = if leaf < 0 { leaf } else if leaf == 0 { continue } else { map[leaf as usize - 1] as i32 };
        if root != 0 && cols.is_member(root - FLIHAN) {
            out.add_member(mcx, leaf - FLIHAN)?;
        }
    }
    Ok(out)
}

/// ExecInitPartitionInfo's arbiter list: the leaf's indexes whose ancestry (`get_partition_ancestors`, every level —
/// a multi-level partition's index descends from the root's through the intermediate's) holds a root arbiter.
fn leaf_arbiters(mcx: Mcx<'static>, leaf: &Relation<'static>, arbiters: &[u32]) -> PgResult<Vec<u32>> {
    let mut out = Vec::with_capacity(arbiters.len()); // alloc: Node
    let idxlist = relcache_seams::relation_get_index_list::call(mcx, leaf.rd_id)?;
    // bounded("catalog-sized: one per index of the leaf (its pg_index rows), an ancestor lookup each, as ExecInitPartitionInfo matches the arbiters")
    for &child in idxlist.iter() {
        let ancestors = pg_inherits::get_partition_ancestors(mcx, child)?;
        // bounded("catalog-sized: one per arbiter index of the root the statement's ON CONFLICT names (the plan's arbiterIndexes)")
        for &root_index in arbiters {
            if ancestors.iter().any(|&a| a == root_index) {
                out.push(child); // alloc: Node
            }
        }
    }
    if out.len() != arbiters.len() {
        return Err(changed(leaf, "an arbiter index of the root has no partition on it"));
    }
    Ok(out)
}

/// The partitioned root's statement triggers under a routed INSERT or a pruned UPDATE / DELETE (`fireBSTriggers` /
/// `fireASTriggers` run on the result relation, relkind 'p': the BS twins now, the AS twins at the statement's
/// end, once per statement whether or not a row reaches a leaf; its transition tables take every leaf's rows
/// through the leaf host's AR twin, converted to the root's row type). `Err(name)` = no descriptor seam — nothing
/// has fired.
pub(crate) fn root_stmt(root: &Relation<'static>, cmd: Cmd, conflict_update: bool, merge: MergeOps, modified: Option<ModifiedCols>) -> PgResult<core::result::Result<Option<Rc<RefCell<StmtTriggers>>>, &'static str>> {
    if !root.rd_hastriggers {
        return Ok(Ok(None));
    }
    if !relcache_seams::relation_get_trigger_desc::is_installed() {
        return Ok(Err("a partitioned root with triggers (no trigger descriptor seam)"));
    }
    let desc = relcache_seams::relation_get_trigger_desc::call(root.rd_id)?;
    // D2830: the root is the statement's result relation: its AFTER events keep this copy (trigger_stmt `target`)
    crate::ops::modifytable::trigger_stmt::target(root.rd_id, desc.as_ref());
    let Some(desc) = desc else { return Ok(Ok(None)) };
    Ok(Ok(StmtTriggers::bind(root, &desc, cmd, conflict_update, merge, modified)?))
}

#[cold]
pub(crate) fn changed(leaf: &Relation<'static>, what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: partition \"{}\" changed since the routed INSERT was admitted: {what} (the lowered write cannot continue)", String::from_utf8_lossy(leaf.rd_rel.relname.name_str())))) // alloc: Cold
}

/// The leaf at its first routed (or moved) row, under its RowExclusiveLock (whose acquisition processed the invalidations of any DDL
/// that committed since admission): re-admitted as ExecInitPartitionInfo admits it, from its CURRENT relcache entry, never
/// compared with the admission (V4R-303: a leaf attached, given a trigger, a rule or another table AM between admission and
/// its first row is written as PostgreSQL writes it; an ERROR there would be error-vs-value). The leaf host binds its
/// row triggers, generated columns, constraints and check options from that entry; a leaf's rules never apply to a routed
/// row (the rewriter applies the root's); its indexes are the list at that row (D765: `IndexMaint`).
pub(crate) fn revalidate(mcx: Mcx<'static>, leaf: &Relation<'static>) -> PgResult<Vec<IndexRecipe>> {
    index_recipes(mcx, leaf)
}

/// Every index of `leaf` as the write host maintains it (ExecOpenIndices under the executor's RowExclusiveLock). D765: the
/// list as it is at the relation's first row (ExecInitPartitionInfo's / ExecInitModifyTable's ExecOpenIndices), not as
/// admitted: an index dropped or created since is PG's to skip or maintain. An index the recipes cannot carry gets none,
/// so the host's IndexMaint finds the list is not its recipes' and maintains every index by the executor's own path.
fn index_recipes(mcx: Mcx<'static>, leaf: &Relation<'static>) -> PgResult<Vec<IndexRecipe>> {
    let list = relcache_seams::relation_get_index_list::call(mcx, leaf.rd_id)?;
    let mut recipes = Vec::with_capacity(list.len()); // alloc: Node
    // bounded("catalog-sized: one per index of the leaf (its pg_index rows), opened as ExecOpenIndices does")
    for &oid in list.iter() {
        let index = open_index(mcx, &None, oid, RowExclusiveLock)?;
        if let (None, Some(form)) = (unmaintainable_routed(&index), index.rd_index.as_ref()) {
            // Decision 18b: every index column as the AM receives it (INCLUDE too); an expression key's 0 as in indkey
            let nkeys = form.indnatts as usize;
            recipes.push(IndexRecipe { oid, nkeys, key_attnos: form.indkey.iter().take(nkeys).copied().collect(), unique: form.indisunique }); // alloc: Node
        }
        index.close(NoLock)?;
    }
    Ok(recipes)
}

impl Writer for RouteHost {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.snapshot = Some(snapshot_of(b));
        self.words = crate::ops::modifytable::write_check::ParamWords::of(b.params);
        match self.bind_pg(b.command_id.0) {
            Ok(true) => Ok(()),
            Ok(false) => Err(crate::unsupported()),
            Err(e) => Err(crate::stash(e)),
        }
    }

    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        self.ins = op == WriteOp::Insert;
        let r = self.write_pg(op, pos, values, isnull).map_err(crate::stash);
        self.scratch.reset_exec();
        r
    }

    /// updatable_views#978 (writesubplan D1112 row B): the routed row's leaf parked at its check option's SubPlan read (its
    /// WriteChecks are the root's, mapped onto the leaf by `leaf_checks`); the parked call's words are the leaf's.
    fn sub_call(&mut self) -> Result<*mut u64> {
        let Some(h) = self.last_leaf.and_then(|idx| self.hosts.get_mut(idx)).and_then(|h| h.as_mut()) else { return Err(gravity_driver::Error::host("gravity: a SubPlan call with no routed leaf")) };
        h.sub_call()
    }

    /// updatable_views#978: the parked leaf's row op resumed (the row converted to the leaf's layout as `write_pg` hands it).
    fn resume(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        let RouteHost { last_leaf, hosts, routing, conv_values, conv_nulls, .. } = self;
        let (Some(idx), Some(r)) = (*last_leaf, routing.as_ref()) else { return Err(gravity_driver::Error::host("gravity: a SubPlan resume with no routed leaf")) };
        let Some(h) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else { return Err(gravity_driver::Error::host("gravity: a SubPlan resume with no routed leaf")) };
        let (values, isnull) = to_leaf(r.router.leaf_attrmap(idx), values, isnull, conv_values, conv_nulls);
        h.resume(op, pos, values, isnull)
    }

    /// The conflicting row's leaf checks it against its own policies.
    fn conflict_check(&mut self) -> Result<()> {
        let Some(h) = self.conflict_leaf.and_then(|idx| self.hosts.get_mut(idx)).and_then(|h| h.as_mut()) else { return Err(gravity_driver::Error::host("gravity: a conflict outcome without its leaf")) };
        h.conflict_check()
    }

    fn existing(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<ScanPos> {
        let RouteHost { conflict_leaf, hosts, routing, conv_values, conv_nulls, .. } = self;
        let (Some(idx), Some(r)) = (*conflict_leaf, routing.as_ref()) else { return Err(gravity_driver::Error::host("gravity: a conflict outcome without its leaf")) };
        let Some(h) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else { return Err(gravity_driver::Error::host("gravity: a conflict outcome without its leaf")) };
        let Some(map) = r.router.leaf_attrmap(idx) else { return h.existing(values, isnull) };
        conv_values.clear();
        conv_values.resize_in(map.len(), 0);
        conv_nulls.clear();
        conv_nulls.resize_in(map.len(), 1);
        let pos = h.existing(conv_values, conv_nulls)?;
        to_root(map, conv_values, conv_nulls, values, isnull);
        Ok(pos)
    }

    /// D774: the leaf's row; a leaf of the root's own layout (no attribute map) is inserted from the root's virtual slot in
    /// PostgreSQL (ExecPrepareTupleRouting converts nothing), whose header fields raise.
    fn sys(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        let RouteHost { last_leaf, hosts, routing, own, .. } = self;
        // writesubplan (merge#514): the own root's row, inserted from the root's own slot (ri_newTupleSlot, a heap one)
        if own.is_some() {
            let Some(h) = last_leaf.and_then(|i| hosts.get(i)).and_then(|h| h.as_ref()) else { return Ok(false) };
            return h.sys_fill(values, isnull, false).map_err(crate::stash);
        }
        let (Some(idx), Some(r)) = (*last_leaf, routing.as_ref()) else { return Ok(false) };
        let Some(h) = hosts.get(idx).and_then(|h| h.as_ref()) else { return Ok(false) };
        h.sys_fill(values, isnull, r.router.leaf_attrmap(idx).is_none()).map_err(crate::stash)
    }

    fn sys_off(&self) -> usize {
        self.sys.0 as usize * (1 + self.old as usize + (self.ins && self.conflict == Conflict::Update) as usize)
    }

    /// R4c: ON CONFLICT DO UPDATE's replaced version, the leaf's whose update wrote the last row (`last_leaf`), in the
    /// root's layout as `existing` maps the conflicting row; an inserted row's leaf fetched none (`false`).
    fn old(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        let RouteHost { last_leaf, hosts, routing, conv_values, conv_nulls, sys_old, .. } = self;
        let (Some(idx), Some(r)) = (*last_leaf, routing.as_ref()) else { return Ok(false) };
        let Some(h) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else { return Ok(false) };
        let Some(map) = r.router.leaf_attrmap(idx) else { return h.old(values, isnull) };
        // V4R-501: the leaf's six system cells follow its row; they stay the leaf version's, unmapped
        let (n, sys) = (map.len(), if *sys_old { 6 } else { 0 });
        conv_values.clear();
        conv_values.resize_in(n + sys, 0);
        conv_nulls.clear();
        conv_nulls.resize_in(n + sys, 1);
        if !h.old(conv_values, conv_nulls)? {
            return Ok(false);
        }
        let k = values.len().saturating_sub(sys);
        to_root(map, &conv_values[..n], &conv_nulls[..n], &mut values[..k], &mut isnull[..k]);
        values[k..].copy_from_slice(&conv_values[n..]);
        isnull[k..].copy_from_slice(&conv_nulls[n..]);
        Ok(true)
    }

    fn written(&mut self, values: &mut [Word], isnull: &mut [u8]) -> bool {
        let RouteHost { last_leaf, hosts, routing, own, conv_values, conv_nulls, .. } = self;
        // writesubplan (merge#514): the own root's row, in its layout (the root's)
        if own.is_some() {
            let Some(h) = last_leaf.and_then(|i| hosts.get_mut(i)).and_then(|h| h.as_mut()) else { return false };
            return h.written(values, isnull);
        }
        let (Some(idx), Some(r)) = (*last_leaf, routing.as_ref()) else { return false };
        let Some(h) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else { return false };
        let Some(map) = r.router.leaf_attrmap(idx) else { return h.written(values, isnull) };
        conv_values.clear();
        conv_values.resize_in(map.len(), 0);
        conv_nulls.clear();
        conv_nulls.resize_in(map.len(), 1);
        if !h.written(conv_values, conv_nulls) {
            return false;
        }
        to_root(map, conv_values, conv_nulls, values, isnull);
        true
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: route host unbind");
    }
}

impl Drop for RouteHost {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
    }
}

/// H-e (b): a write over several result relations (an unpruned partitioned DELETE, or an inheritance tree; the
/// lowerer's `write.route` sets `RoutePlan { root, leaves }` with every result relation in `resultRelations` order,
/// an inheritance parent as `leaves[0]`). ExecInitModifyTable opens a ResultRelInfo per target and ExecModifyTable
/// picks the row's by its tableoid (`ExecLookupResultRelByOid`): here one `WriteHost` per target, built at the
/// target's first row under its RowExclusiveLock and re-checked against its frozen `RouteLeaf`, the row's target the
/// relation of its position (each arm scans one result relation, V4R-214; the arms share leaves[0]'s write site).
/// The named relation's statement triggers fire once per statement (`fireBSTriggers` / `fireASTriggers` on the root,
/// `root_stmt`); every target's row triggers are its own, their transition rows converted to the root's row type.
/// A DELETE carries no cells; RETURNING's cells (M4-RET R5) are the target's own layout, `written` hands them back
/// unconverted. An UPDATE's cells are the SET entries alone, in the plan's `updateColnos` order (the root's attnos
/// ascending: the lowerer's write/update.rs checks every target's list against it): per row, ExecUpdate's new tuple over
/// the old version (ExecGetUpdateNewTuple): the version at the position fetched under SnapshotAny
/// (`table_tuple_fetch_row_version`, as ExecModifyTable fetches `oldSlot`), every attribute copied (a dropped one NULL),
/// then each SET cell at the target attribute its root column names (ri_RootToChildMap, by name), handed to the target's
/// write host in the target's layout; its maintained indexes are the target's own, re-read at its first row.
/// W6-MERGE-ROUTE-MATCHED: a MERGE whose actions update or delete (never insert: lowerv4 write/merge_route.rs) is the same
/// host over its `MergeOps`: each row an UPDATE (its cells the statement's updated columns, as an UPDATE's) or a DELETE,
/// through the target's MERGE host (TM_Deleted is NOT MATCHED, TM_SelfModified 21000); every target's old-version reader
/// and indexes come from the statement's having UPDATE actions, never from its first row's action.
pub struct MultiTargetHost {
    root: Oid,
    cmd: Cmd,
    /// W6-MERGE-ROUTE-MATCHED: a MERGE's subcommands (the registry's, per admission), the root's statement triggers' too.
    merge: MergeOps,
    leaves: Vec<RouteLeaf>,
    /// The statement's insertedCols / updatedCols: the plan's at the derive, the host's own copies (node context) from its
    /// first bind on, as `WriteHost` keeps them (a parked Node's later binds never read a plan that is gone, H16 (b)).
    modified: Option<ModifiedCols>,
    cols_owned: bool,
    root_rel: Option<Relation<'static>>,
    stmt: Option<Rc<RefCell<StmtTriggers>>>,
    /// One host per leaf position, built at the leaf's first row.
    hosts: Vec<Option<WriteHost>>,
    /// The leaf of the last row: the next row's first guess (an arm streams one relation), `written`'s and
    /// `recheck_pos`'s host.
    last: Option<usize>,
    /// UPDATE: the root's SET attnos in the cells' order (its updatedCols less the generated columns the rewriter leaves
    /// out of `updateColnos`, ascending), per bind; and per leaf position its old-version reader, built at its first row.
    set: Vec<i16>,
    olds: Vec<Option<OldRow>>,
    /// UPDATE: the new row in the target's layout. R5 (D764): read back by `written` (RETURNING), so the old version it
    /// was formed over (its unchanged by-reference cells point into its page) stays pinned until the next row
    /// (`release_last`).
    row_values: Vec<Word>,
    row_nulls: Vec<u8>,
    /// The leaf whose old version the last row fetched, released at the next row and at unbind.
    held: Option<usize>,
    /// D774 x R5: RETURNING's system columns (WritePlan.sys, sys_header), each target's host capturing its row's; R4c
    /// (V4R-350) `old`: RETURNING OLD's replaced version (WritePlan.old), each target's host fetching its row's; V4R-501
    /// `sys_old`: its system columns too (WritePlan.sys_old).
    sys: (bool, bool),
    old: bool,
    sys_old: bool,
    cid: CommandId,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    /// W2: the execution's parameter words, for a leaf's check options' InitPlan params.
    words: crate::ops::modifytable::write_check::ParamWords,
    /// H-e (c): a cross-partition UPDATE's move host, shared by every target's write host.
    mover: Option<Rc<RefCell<crate::ops::modifytable::write_move::Mover>>>,
    /// The statement's check options, every target's (withCheckOptionLists, each in its target's own layout as the planner
    /// translated it); each target's host gets its own list (ExecInitModifyTable's ri_WithCheckOptions per result relation).
    checks: Option<Rc<[CheckSpec]>>,
    /// SORT-UNION (merge#364 group): a MERGE's NOT MATCHED INSERTs beside its UPDATE / DELETE go through the root
    /// (ExecInitMerge's rootRelInfo: the root's layout and tuple routing), on this route leg sharing the statement
    /// triggers; `ins_last`: the last row was one of them (`written` / `sys` are its).
    ins: Option<Box<RouteHost>>,
    ins_last: bool,
    scratch: NodeScratch,
}

/// A target's old version (ExecModifyTable's `oldSlot`): the relation opened for the statement, the slot the fetch fills,
/// and each root SET column's attno in the target (`set[i]` for cell `i`), a dropped attribute's flag.
struct OldRow {
    rel: Relation<'static>,
    slot: SlotData<'static>,
    attnos: Vec<i16>,
    set: Vec<i16>,
    dropped: Vec<bool>,
}

impl MultiTargetHost {
    pub fn new(plan: &WritePlan) -> MultiTargetHost {
        let (root, leaves) = plan.route.as_ref().map_or((plan.target_rel, Vec::new()), |r| (r.root, r.leaves.to_vec())); // alloc: Node
        MultiTargetHost { root, cmd: plan.cmd, merge: MergeOps::NONE, leaves, modified: None, cols_owned: false, root_rel: None, stmt: None, hosts: Vec::new(), last: None, set: Vec::new(), olds: Vec::new(), row_values: Vec::new(), row_nulls: Vec::new(), held: None, sys: (plan.sys, plan.sys_header), old: plan.old, sys_old: plan.sys_old, cid: 0, snapshot: None, words: crate::ops::modifytable::write_check::ParamWords::NONE, mover: crate::ops::modifytable::write_move::Mover::of(plan), checks: None, ins: (plan.cmd == Cmd::Merge).then(|| Box::new(RouteHost::new(plan))), ins_last: false, scratch: NodeScratch::new("GravityMultiTargetHost") } // alloc: Node
    }

    pub fn with_modified(mut self, cols: Option<ModifiedCols>) -> MultiTargetHost {
        self.modified = cols;
        if let Some(h) = self.ins.as_mut() {
            h.modified = cols;
        }
        self
    }

    /// W6-MERGE-ROUTE-MATCHED: THIS statement's MERGE subcommands; mergecmd SORT-UNION: an INSERT among them keeps the
    /// route leg.
    pub fn with_merge(mut self, ops: MergeOps) -> MultiTargetHost {
        self.merge = ops;
        self.ins = self.ins.take().filter(|_| ops.insert);
        if let Some(h) = self.ins.as_mut() {
            h.merge = ops;
        }
        self
    }

    /// The statement updates rows: an UPDATE, or a MERGE with an UPDATE action (its old-version readers and indexes).
    fn updates(&self) -> bool {
        self.cmd == Cmd::Update || self.cmd == Cmd::Merge && self.merge.update
    }

    pub fn with_checks(mut self, checks: Option<Rc<[CheckSpec]>>) -> MultiTargetHost {
        self.checks = checks.filter(|c| !c.is_empty());
        // SORT-UNION: the route leg checks a routed row by the first result relation's list (WritePlan.checks_of)
        if let Some(h) = self.ins.as_mut() {
            h.checks = self.checks.clone(); // alloc: Node
        }
        // V4R-329: a moved row's destination takes the first target's list (ExecInitPartitionInfo's linitial)
        if let (Some(m), Some(first)) = (self.mover.as_ref(), self.leaves.first()) {
            m.borrow_mut().with_checks(first.relid, self.checks.clone());
        }
        self
    }

    /// Target `idx`'s own check options (its ResultRelInfo's ri_WithCheckOptions), `None` when it has none.
    fn target_checks(&self, idx: usize) -> Option<Rc<[CheckSpec]>> {
        let c = self.checks.as_ref()?;
        let own: Vec<CheckSpec> = c.iter().filter(|s| usize::from(s.target) == idx).map(|s| CheckSpec { kind: s.kind, relname: s.relname.clone(), polname: s.polname.clone(), node: s.node, target: s.target, inits: Rc::clone(&s.inits) }).collect(); // alloc: Node
        (!own.is_empty()).then(|| own.into())
    }

    /// The named relation under the executor's lock and its statement triggers; `false` = not a shape this host serves
    /// (an UPDATE without its updated columns), a refusal before any row.
    fn bind_pg(&mut self, cid: CommandId) -> PgResult<bool> {
        let merge = self.cmd == Cmd::Merge && (!self.merge.insert || self.ins.is_some()) && (self.merge.update || self.merge.delete);
        if !(matches!(self.cmd, Cmd::Delete | Cmd::Update) || merge) || self.leaves.is_empty() || (self.updates() && self.modified.is_none()) {
            return Ok(false);
        }
        let mcx = self.scratch.mcx();
        if !self.cols_owned {
            if let Some((ins, upd)) = self.modified {
                self.modified = Some((mcx::alloc_leak_in(mcx, ins.clone_in(mcx)?)?, mcx::alloc_leak_in(mcx, upd.clone_in(mcx)?)?)); // alloc: Node
            }
            self.cols_owned = true;
        }
        lmgr_seams::lock_relation_oid::call(self.root, RowExclusiveLock)?;
        let root = table::table_open(mcx, self.root, NoLock)?;
        self.stmt = match root_stmt(&root, self.cmd, false, self.merge, self.modified) {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => {
                root.close(NoLock)?;
                return Ok(false);
            }
            Err(e) => {
                root.close(NoLock)?;
                return Err(e);
            }
        };
        self.set.clear();
        if let Some((_, upd)) = self.modified.filter(|_| self.updates()) {
            // updateColnos: the updated columns rewriteTargetListIU keeps, in attno order (a generated column's SET DEFAULT
            // leaves no target entry: the executor computes it)
            // bounded("key-columns: one per column of the root, at most MaxHeapAttributeNumber (1600) columns")
            for (i, a) in root.rd_att.attrs.iter().enumerate() {
                if !a.attisdropped && a.attgenerated == 0 && upd.is_member(i as i32 + 1 - FLIHAN) {
                    self.set.push(i as i16 + 1); // alloc: Node
                }
            }
        }
        self.root_rel = Some(root);
        self.hosts.clear();
        self.hosts.resize_with(self.leaves.len(), || None); // alloc: Node
        self.olds.clear();
        self.olds.resize_with(self.leaves.len(), || None); // alloc: Node
        self.last = None;
        self.cid = cid;
        self.ins_last = false;
        // SORT-UNION: the route leg over the same root, the statement triggers this bind fired shared
        if let Some(h) = self.ins.as_mut() {
            return h.bind_beside(cid, self.snapshot.clone(), self.words, self.stmt.clone());
        }
        Ok(true)
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        let mut closed = self.mover.as_ref().map_or(Ok(()), |m| m.borrow_mut().unbind());
        if let Some(h) = self.ins.as_mut() {
            closed = closed.and(h.unbind_pg());
        }
        self.ins_last = false;
        // bounded("catalog-sized: one write host per target the statement wrote (the plan's result relations, a partitioned table's leaves); teardown runs whole")
        for h in self.hosts.drain(..).flatten() {
            drop(h);
        }
        // bounded("catalog-sized: one old-version reader per target the statement updated (the plan's result relations), its slot cleared and its relation closed; teardown runs whole")
        for mut o in self.olds.drain(..).flatten() {
            exectuples::exec_clear_tuple(&mut o.slot, self.scratch.mcx());
            drop(o.slot);
            closed = closed.and(o.rel.close(NoLock));
        }
        self.stmt = None;
        self.last = None;
        self.held = None;
        if let Some(r) = self.root_rel.take() {
            r.close(NoLock)?;
        }
        self.scratch.reset_exec();
        closed
    }

    /// The last row's old version released (its buffer pin) and its per-row context reset: at the next row, as its new
    /// row (`row_values`) stays readable for `written` until then.
    fn release_last(&mut self) {
        if let Some(o) = self.held.take().and_then(|i| self.olds.get_mut(i)).and_then(|o| o.as_mut()) {
            exectuples::exec_clear_tuple(&mut o.slot, self.scratch.mcx());
        }
        self.scratch.reset_exec();
    }

    /// `ExecLookupResultRelByOid`: the leaf whose relation is the row's (the last row's first).
    fn leaf_of(&self, rel: Oid) -> Option<usize> {
        match self.last {
            Some(i) if self.leaves[i].relid == rel => {
                fires::fire("route.last_leaf");
                Some(i)
            }
            _ => self.leaves.iter().position(|l| l.relid == rel),
        }
    }

    fn write_pg(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> PgResult<Outcome> {
        let want = match self.cmd {
            Cmd::Delete => op == WriteOp::Delete,
            Cmd::Merge => op == WriteOp::Delete || op == WriteOp::Update && self.merge.update,
            _ => op == WriteOp::Update,
        };
        let Some(idx) = self.leaf_of(pos.rel).filter(|_| want) else {
            return Err(Box::new(PgError::error(format!("gravity: Reason::Bug: a {op:?} of a row of relation {} outside the write's result relations", pos.rel)))); // alloc: Cold
        };
        if self.hosts[idx].is_none() {
            let mcx = self.scratch.mcx();
            let root = self.root_rel.as_ref().expect("gravity: write on an unbound multi-target host");
            let admitted = &self.leaves[idx];
            let leaf = table::table_open(mcx, admitted.relid, RowExclusiveLock)?;
            let checked = revalidate_target(root, admitted, &leaf);
            let alias = root.alias();
            leaf.close(NoLock)?;
            checked?;
            // an UPDATE maintains the target's own indexes (ExecUpdate's ExecInsertIndexTuples on a non-HOT update), read
            // under its lock at its first row as admitted; a DELETE maintains none. Its old-version reader with them
            let indexes = match self.updates() {
                true => {
                    let (old, recipes) = old_row(mcx, root, table::table_open(mcx, admitted.relid, NoLock)?, &self.set)?;
                    self.olds[idx] = Some(old);
                    recipes
                }
                false => Vec::new(), // alloc: Node
            };
            let recipe = WriteRecipe { heap: admitted.relid, cmd: self.cmd, conflict: Conflict::None, arbiters: Vec::new(), defer: false }; // alloc: Node
            let wco = self.target_checks(idx);
            let mut host = WriteHost::for_leaf(recipe, indexes, alias, self.stmt.clone()).with_merge(self.merge).with_modified(self.modified).with_mover(self.mover.clone()).with_checks(wco).with_sys(self.sys.0, self.sys.1).with_old(self.old).with_sys_old(self.sys_old); // alloc: Node
            if !host.bind_leaf(self.cid, self.snapshot.clone(), self.words)? {
                let leaf = table::table_open(mcx, admitted.relid, NoLock)?;
                let e = changed_target(&leaf, "its write host refused it (its triggers or form)");
                leaf.close(NoLock)?;
                return Err(e);
            }
            self.hosts[idx] = Some(host);
        }
        self.last = Some(idx);
        let host = self.hosts[idx].as_mut().expect("just built");
        // the row's own action decides (a MERGE's DELETE beside its UPDATEs deletes); an UPDATE row always has its reader
        let Some(old) = self.olds[idx].as_mut().filter(|_| op == WriteOp::Update) else {
            // R5 (D764): a DELETE's site under RETURNING is as wide as the widest target: this target's own cells, which
            // its host fills with the deleted version
            let n = values.len().min(usize::from(self.leaves[idx].natts));
            let (values, isnull) = (&values[..n], &isnull[..n]);
            return host.write(op, pos, values, isnull).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: target write failed: {e:?}"))))); // alloc: Cold
        };
        // UPDATE: the new row over the old version (ExecModifyTable's fetch under SnapshotAny, ExecGetUpdateNewTuple). R5
        // (D764): under RETURNING the site is as wide as the widest target, the SET cells first
        // (SORT-UNION: beside the route leg the site is the root's row, the SET cells first)
        // (gen mg-0178: a MERGE's site under RETURNING over the union form is as wide as the widest target)
        let wide = self.leaves.iter().map(|l| usize::from(l.natts)).max().unwrap_or(0);
        if values.len() < old.set.len() || isnull.len() != values.len() || self.cmd == Cmd::Merge && self.ins.is_none() && values.len() != old.set.len() && values.len() != wide {
            return Err(Box::new(PgError::error(format!("gravity: Reason::Bug: an UPDATE row of {} cells where the statement updates {} columns", values.len(), old.set.len())))); // alloc: Cold
        }
        let exec = self.scratch.exec_mcx();
        if !crate::ops::shared::heap_fetch::fetch_any(exec, &old.rel, &crate::ops::shared::rel::tid_of(pos), &mut old.slot)? {
            return Err(Box::new(PgError::error("failed to fetch tuple being updated".to_string()))); // alloc: Cold
        }
        self.held = Some(idx);
        let n = old.attnos.len();
        self.row_values.resize(n, 0); // alloc: Node
        self.row_nulls.resize(n, 1); // alloc: Node
        crate::ops::shared::heap_fetch::project(&mut old.slot, &old.attnos, &mut self.row_values, &mut self.row_nulls)?;
        // bounded("key-columns: one cell per dropped column of the target, at most MaxHeapAttributeNumber (1600) columns")
        for (k, _) in old.dropped.iter().enumerate().filter(|(_, d)| **d) {
            (self.row_values[k], self.row_nulls[k]) = (0, 1);
        }
        // bounded("key-columns: one cell per SET column of the statement, at most MaxHeapAttributeNumber (1600) columns")
        for (i, &a) in old.set.iter().enumerate() {
            (self.row_values[a as usize - 1], self.row_nulls[a as usize - 1]) = (values[i], isnull[i]);
        }
        // the old version's pin held through the write and until the next row (its unchanged by-reference cells point into
        // its page; `written` hands them back)
        host.write(op, pos, &self.row_values, &self.row_nulls).map_err(|e| crate::take_error().unwrap_or_else(|| Box::new(PgError::error(format!("gravity: target write failed: {e:?}"))))) // alloc: Cold
    }
}

/// An UPDATE target's old-version reader at its first row (`leaf` opened by the caller under the lock the target's first
/// row took, kept open for the statement): its slot, every attribute (dropped ones flagged NULL), each root SET column's
/// attno in the target by name (`root_map`, ri_RootToChildMap: a column the target does not have is a changed relation),
/// and its indexes at that row (`index_recipes`).
fn old_row(mcx: Mcx<'static>, root: &Relation<'static>, leaf: Relation<'static>, set: &[i16]) -> PgResult<(OldRow, Vec<IndexRecipe>)> {
    let built = (|| {
        let map = root_map(root, &leaf);
        let mut to = Vec::with_capacity(set.len()); // alloc: Node
        // bounded("key-columns: one per SET column of the statement, each a search of the root-to-leaf column map, at most MaxHeapAttributeNumber (1600) columns each")
        for &r in set {
            let a = match &map {
                None => Some(r),
                Some(m) => m.iter().position(|&x| x == r).map(|i| i as i16 + 1),
            };
            let Some(a) = a.filter(|&a| a > 0 && (a as usize) <= leaf.rd_att.attrs.len()) else { return Err(changed_target(&leaf, "an updated column of the named relation it does not have")) };
            to.push(a); // alloc: Node
        }
        let recipes = index_recipes(mcx, &leaf)?;
        let slot = tableam::table_slot_create(mcx, &leaf)?;
        let natts = leaf.rd_att.attrs.len();
        Ok((slot, to, recipes, (1..=natts as i16).collect::<Vec<_>>(), leaf.rd_att.attrs.iter().map(|a| a.attisdropped).collect::<Vec<_>>())) // alloc: Node
    })();
    match built {
        Ok((slot, set, recipes, attnos, dropped)) => Ok((OldRow { rel: leaf, slot, attnos, set, dropped }, recipes)),
        Err(e) => {
            leaf.close(NoLock)?;
            Err(e)
        }
    }
}

/// D978: CheckValidResultRel's publication check of one result relation (CheckCmdReplicaIdentity, execReplication.c), as
/// ExecInitModifyTable runs it before the first row and the BEFORE STATEMENT triggers over each result relation initial
/// pruning keeps (the exec runner's `held::take` walks them; a target's first row is too late: a target with no row never
/// reaches it). CheckValidResultRel calls it for a table or a partitioned table only (the latter returns at once there).
pub fn replica_identity(mcx: Mcx<'static>, rel: &Relation<'static>, cmd: CmdType) -> PgResult<()> {
    if !matches!(rel.rd_rel.relkind, RELKIND_RELATION | RELKIND_PARTITIONED_TABLE) || !execreplication_seams::check_cmd_replica_identity::is_installed() {
        return Ok(());
    }
    execreplication_seams::check_cmd_replica_identity::call(mcx, rel, cmd)
}

/// A result relation under its lock against its frozen `RouteLeaf` (the lowerer's admission): still a table of a carried
/// table AM, the same column count, and under a partitioned root still a partition. An inheritance child's link is not re-read (a
/// pg_inherits scan per target per execution): the plan's range table holds every target locked, so NO INHERIT (its
/// AccessExclusiveLock on the child) waits for the statement, and a changed tree replans the next one.
fn revalidate_target(root: &Relation<'static>, admitted: &RouteLeaf, leaf: &Relation<'static>) -> PgResult<()> {
    if leaf.rd_rel.relkind != RELKIND_RELATION || TableAm::of(leaf).is_none() {
        return Err(changed_target(leaf, "not a table of a table access method this server carries"));
    }
    if leaf.rd_att.natts as u16 != admitted.natts {
        return Err(changed_target(leaf, "its column count changed"));
    }
    let in_tree = leaf.rd_id == root.rd_id || root.rd_rel.relkind != RELKIND_PARTITIONED_TABLE || leaf.rd_rel.relispartition;
    if !in_tree {
        return Err(changed_target(leaf, "no longer in the tree of the named relation"));
    }
    Ok(())
}

#[cold]
fn changed_target(leaf: &Relation<'static>, what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: result relation \"{}\" changed since the write was admitted: {what} (the lowered write cannot continue)", String::from_utf8_lossy(leaf.rd_rel.relname.name_str())))) // alloc: Cold
}

impl Writer for MultiTargetHost {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.snapshot = Some(snapshot_of(b));
        self.words = crate::ops::modifytable::write_check::ParamWords::of(b.params);
        match self.bind_pg(b.command_id.0) {
            Ok(true) => Ok(()),
            Ok(false) => Err(crate::unsupported()),
            Err(e) => Err(crate::stash(e)),
        }
    }

    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        self.release_last();
        // SORT-UNION: a NOT MATCHED INSERT goes through the root on the route leg (ExecMergeNotMatched's
        // ExecInsert into rootRelInfo)
        self.ins_last = op == WriteOp::Insert && self.ins.is_some();
        if let (true, Some(h)) = (self.ins_last, self.ins.as_mut()) {
            return h.write(op, pos, values, isnull);
        }
        self.write_pg(op, pos, values, isnull).map_err(crate::stash)
    }

    fn recheck_pos(&self) -> ScanPos {
        self.last.and_then(|i| self.hosts.get(i)).and_then(|h| h.as_ref()).map_or(ScanPos::NONE, |h| h.recheck_pos())
    }

    /// D1112 row B (update#164 / #165 / #171 / #176): the last row's target parked at its check option's SubPlan read
    /// (its own WriteChecks, or a moved row's destination's through the move host): the parked call's words are its.
    fn sub_call(&mut self) -> Result<*mut u64> {
        if self.ins_last {
            return Err(gravity_driver::Error::host("gravity: a SubPlan call on a MERGE's route leg"));
        }
        let Some(h) = self.last.and_then(|i| self.hosts.get_mut(i)).and_then(|h| h.as_mut()) else { return Err(gravity_driver::Error::host("gravity: a SubPlan call with no parked target")) };
        h.sub_call()
    }

    /// D1112 row B: the parked target's row op resumed over the same row: an UPDATE's new row over the old version
    /// (`row_values`, its pin still held: a resume releases nothing), else the cells `write_pg` cut to the target.
    fn resume(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        let MultiTargetHost { last, hosts, olds, leaves, row_values, row_nulls, ins_last, .. } = self;
        let Some(idx) = (*last).filter(|_| !*ins_last) else { return Err(gravity_driver::Error::host("gravity: a SubPlan resume with no parked target")) };
        let Some(h) = hosts.get_mut(idx).and_then(|h| h.as_mut()) else { return Err(gravity_driver::Error::host("gravity: a SubPlan resume with no parked target")) };
        if op == WriteOp::Update && olds.get(idx).is_some_and(|o| o.is_some()) {
            return h.resume(op, pos, row_values, row_nulls);
        }
        let n = values.len().min(usize::from(leaves[idx].natts));
        h.resume(op, pos, &values[..n], &isnull[..n])
    }

    /// D652-S2 (lowerv4 write/rowop_epq.rs): the newest version the last row's Recheck locked, its target's host's, in that
    /// target's own layout (the arm that wrote the row reads its own leaf's attributes from the site); the retry's write
    /// then finds that target by the position's relation, as ExecUpdate / ExecDelete retry on their own result relation.
    fn epq_row(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<ScanPos> {
        let Some(h) = self.last.and_then(|i| self.hosts.get_mut(i)).and_then(|h| h.as_mut()) else {
            return Err(gravity_driver::Error::host("gravity: an EvalPlanQual row without a locked newest version"));
        };
        h.epq_row(values, isnull)
    }

    /// D774 x R5: the last row's system columns, its target's host's (mergecmd SORT-UNION: a routed INSERT's, the leg's).
    fn sys(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if let (true, Some(h)) = (self.ins_last, self.ins.as_mut()) {
            return h.sys(values, isnull);
        }
        let Some(h) = self.last.and_then(|i| self.hosts.get(i)).and_then(|h| h.as_ref()) else { return Ok(false) };
        h.sys_fill(values, isnull, false).map_err(crate::stash)
    }

    fn sys_off(&self) -> usize {
        self.sys.0 as usize * (1 + self.old as usize)
    }

    /// R4c (V4R-350): the last row's replaced version, its target's host's (a moved row's: the version its move deleted).
    fn old(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        let Some(h) = self.last.and_then(|i| self.hosts.get_mut(i)).and_then(|h| h.as_mut()) else { return Ok(false) };
        h.old(values, isnull)
    }

    fn written(&mut self, values: &mut [Word], isnull: &mut [u8]) -> bool {
        // SORT-UNION: a routed INSERT's row, in the root's layout (the leg's)
        if let (true, Some(h)) = (self.ins_last, self.ins.as_mut()) {
            return h.written(values, isnull);
        }
        let Some(h) = self.last.and_then(|i| self.hosts.get_mut(i)).and_then(|h| h.as_mut()) else { return false };
        // the target's row as written: a DELETE's deleted version, a BEFORE ROW trigger's row, the computed stored
        // generated columns
        if h.written(values, isnull) {
            return true;
        }
        // (writesubplan, gen mg-0178: a MERGE UPDATE action's row too, the last row an UPDATE while its old version is held)
        if !self.updates() || self.held.is_none() {
            return false;
        }
        // R5 (D764): else an UPDATE's new row over the old version (write_pg's), the site's first cells in the target's
        // layout (the kernel's cells are the SET cells: RETURNING projects the target's row)
        let n = self.row_values.len().min(values.len()).min(isnull.len());
        values[..n].copy_from_slice(&self.row_values[..n]);
        isnull[..n].copy_from_slice(&self.row_nulls[..n]);
        true
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: multi-target host unbind");
    }
}

impl Drop for MultiTargetHost {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
    }
}
