//! The write plan: `Cmd` through `RoutePlan` (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmd {
    Select,
    Insert,
    Update,
    Delete,
    /// MERGE: the per-row action dispatch (decompose.md §2.12); the host's per-call op names the action's command.
    Merge,
}

impl Cmd {
    pub const fn tag(self) -> &'static str {
        match self {
            Cmd::Select => "SELECT",
            Cmd::Insert => "INSERT",
            Cmd::Update => "UPDATE",
            Cmd::Delete => "DELETE",
            Cmd::Merge => "MERGE",
        }
    }

    pub const fn writes(self) -> bool {
        !matches!(self, Cmd::Select)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LockKind {
    None,
    KeyShare,
    Share,
    NoKeyExclusive,
    Exclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaitPolicy {
    Block,
    Skip,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IndexPlan {
    pub index_oid: u32,
    pub key_attnos: ArenaVec<i16>,
    pub unique: bool,
}

impl IndexPlan {
    pub fn new(a: ArenaRef, index_oid: u32, key_attnos: &[i16], unique: bool) -> IndexPlan {
        assert!(!key_attnos.is_empty(), "index key count");
        IndexPlan { index_oid, key_attnos: copy_vec(key_attnos, a), unique }
    }

    pub fn copy_in(&self, a: ArenaRef) -> IndexPlan {
        IndexPlan { index_oid: self.index_oid, key_attnos: copy_vec(&self.key_attnos, a), unique: self.unique }
    }

    pub fn keys(&self) -> &[i16] {
        &self.key_attnos
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WritePlan {
    pub target_rel: u32,
    pub indexes: ArenaVec<IndexPlan>,
    pub cmd: Cmd,
    pub lock: LockKind,
    pub wait: WaitPolicy,
    /// An INSERT's ON CONFLICT action; `arbiters` the arbiter indexes (empty = every unique index).
    pub conflict: Conflict,
    pub arbiters: ArenaVec<u32>,
    /// A write routed through a partitioned parent (`hosts/src/write_route.rs`): the leaves as admitted.
    pub route: Option<RoutePlan>,
    /// M4-RET R4a (V4R-286): an UPDATE whose RETURNING reads OLD: after Done the host fetches the version it replaced into
    /// the write site's successor (driver dml.rs); off, no fetch.
    pub old: bool,
    /// D774 (M4-CLOSE B3): a RETURNING that reads the written row's system columns: after Done the host hands `ctid`,
    /// `xmin`, `cmin`, `xmax`, `cmax` and `tableoid` into the site after the old site (driver dml.rs `Writer::sys`);
    /// `sys_header` when it reads xmin .. cmax (a row PostgreSQL returns from a virtual slot then errors); off, nothing.
    pub sys: bool,
    pub sys_header: bool,
    /// M8-WRITE (the CTE-write seam): a data-modifying CTE's write (its ModifyTable's `canSetTag` false): the statement's
    /// completion tag and row count are the main statement's (SELECT n over the rows sent), not this write's Done rows.
    pub cte: bool,
    /// V4R-501: a RETURNING that reads OLD's system columns: the old site (`old`) carries six more cells after the row's,
    /// the replaced version's `ctid`, `xmin`, `cmin`, `xmax`, `cmax` and `tableoid` (hosts write_sys.rs `SysCols::fill_old`).
    pub sys_old: bool,
    /// D655 Row C (V4R-534): the CTE write's EvalPlanQual runs in the IR (lowerv4 write/rowop_epq.rs).
    pub epq_ir: bool,
    /// INHERIT-ROOT (ExecInitMerge's rootRelInfo lists): the relation whose attnos the check options' Vars carry
    /// when it is not `target_rel`, a MERGE's first result relation when its NOT MATCHED INSERT writes the inheritance
    /// root itself; the host maps that list onto the target's attnos by name (hosts write.rs). 0: the target's own.
    pub checks_of: u32,
    /// PGRCSERVE M1: an INSERT whose rows a table AM may place at statement end (decompose `insert_defer`, host `defer_rows`).
    pub defer: bool,
}

/// One admitted leaf of a routed write: the host re-reads it under the leaf's lock at the first row routed there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RouteLeaf {
    pub relid: u32,
    pub natts: u16,
    pub nindexes: u16,
    /// The leaf has a BEFORE ROW INSERT trigger: the routed row's partition check runs after it (ExecInsert's condition).
    pub before_row: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePlan {
    /// The partitioned root: a routed INSERT's target, or the parent a pruned single-leaf UPDATE / DELETE goes through.
    pub root: u32,
    pub leaves: ArenaVec<RouteLeaf>,
    /// H-e (c): a cross-partition UPDATE's destinations, every partition of the root (a moved row's routed INSERT); empty = no move.
    pub moves: ArenaVec<RouteLeaf>,
}

/// `ON CONFLICT`'s action on an INSERT (ModifyTable's onConflictAction).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Conflict {
    None,
    Nothing,
    Update,
}

impl WritePlan {
    pub const SELECT: WritePlan = WritePlan { target_rel: 0, indexes: ArenaVec::new_in(ArenaRef::EMPTY), cmd: Cmd::Select, lock: LockKind::None, wait: WaitPolicy::Block, conflict: Conflict::None, arbiters: ArenaVec::new_in(ArenaRef::EMPTY), route: None, old: false, sys: false, sys_header: false, cte: false, sys_old: false, epq_ir: false, checks_of: 0, defer: false };

    pub fn write(a: ArenaRef, cmd: Cmd, target_rel: u32, indexes: &[IndexPlan]) -> WritePlan {
        let mut ix = ArenaVec::with_capacity_in(indexes.len(), a);
        for i in indexes {
            ix.push(i.copy_in(a));
        }
        WritePlan { target_rel, indexes: ix, cmd, ..WritePlan::SELECT }
    }

    pub const fn locking(target_rel: u32, lock: LockKind, wait: WaitPolicy) -> WritePlan {
        WritePlan { target_rel, indexes: ArenaVec::new_in(ArenaRef::EMPTY), cmd: Cmd::Select, lock, wait, conflict: Conflict::None, arbiters: ArenaVec::new_in(ArenaRef::EMPTY), route: None, old: false, sys: false, sys_header: false, cte: false, sys_old: false, epq_ir: false, checks_of: 0, defer: false }
    }

    /// The same plan with an ON CONFLICT action over `arbiters`.
    pub fn with_conflict(self, a: ArenaRef, conflict: Conflict, arbiters: &[u32]) -> WritePlan {
        WritePlan { conflict, arbiters: copy_vec(arbiters, a), ..self }
    }

    /// The same plan with the replaced version fetched after each UPDATE (RETURNING OLD, M4-RET R4a).
    pub fn with_old(self) -> WritePlan {
        WritePlan { old: true, ..self }
    }

    /// D774: the same plan with the written row's system columns handed back (`header`: xmin .. cmax read too).
    pub fn with_sys(self, header: bool) -> WritePlan {
        WritePlan { sys: true, sys_header: header, ..self }
    }

    /// V4R-501: the same plan with the replaced version's system columns after its cells in the old site.
    pub fn with_sys_old(self) -> WritePlan {
        WritePlan { sys_old: true, ..self }
    }

    /// The same plan routed through `root` over `leaves` (none: the target is one leaf written under the root).
    pub fn with_route(self, a: ArenaRef, root: u32, leaves: &[RouteLeaf]) -> WritePlan {
        WritePlan { route: Some(RoutePlan { root, leaves: copy_vec(leaves, a), moves: ArenaVec::new_in(a) }), ..self }
    }

    /// H-e (c): a routed UPDATE that moves rows into `moves` (the root's partitions); `with_route` first.
    pub fn with_moves(mut self, a: ArenaRef, moves: &[RouteLeaf]) -> WritePlan {
        if let Some(r) = self.route.as_mut() {
            r.moves = copy_vec(moves, a);
        }
        self
    }

    pub fn copy_in(&self, a: ArenaRef) -> WritePlan {
        let mut ix = ArenaVec::with_capacity_in(self.indexes.len(), a);
        for i in self.indexes.iter() {
            ix.push(i.copy_in(a));
        }
        WritePlan { target_rel: self.target_rel, indexes: ix, cmd: self.cmd, lock: self.lock, wait: self.wait, conflict: self.conflict, arbiters: copy_vec(&self.arbiters, a), route: self.route.as_ref().map(|r| RoutePlan { root: r.root, leaves: copy_vec(&r.leaves, a), moves: copy_vec(&r.moves, a) }), old: self.old, sys: self.sys, sys_header: self.sys_header, cte: self.cte, sys_old: self.sys_old, epq_ir: self.epq_ir, checks_of: self.checks_of, defer: self.defer }
    }

    pub fn arena_bytes(&self) -> usize {
        round8(self.indexes.len() * core::mem::size_of::<IndexPlan>()) + self.indexes.iter().map(|i| round8(i.key_attnos.len() * 2)).sum::<usize>() + round8(self.arbiters.len() * 4) + self.route.as_ref().map_or(0, |r| round8(r.leaves.len() * core::mem::size_of::<RouteLeaf>()) + round8(r.moves.len() * core::mem::size_of::<RouteLeaf>()))
    }

    pub fn indexes(&self) -> &[IndexPlan] {
        &self.indexes
    }
}
