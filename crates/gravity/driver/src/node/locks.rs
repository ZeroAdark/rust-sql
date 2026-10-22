//! The Node's lock hosts and the rows they locked (split out of `node/mod.rs`, contract-runtime landing 3).

use super::*;

/// A lock host of the node: a program's one lock (`rel` None, `Program.write`'s), or one of a LockRows' several
/// (`Program.locks`, in its order: `lock_row`'s argument names the index).
pub(crate) struct LockSlot {
    pub(crate) rel: Option<u32>,
    pub(crate) host: ArenaBox<dyn Locker>,
}

pub(crate) fn nlocks(program: &Program) -> usize {
    if program.locks.is_empty() { usize::from(program.write.lock != LockKind::None) } else { program.locks.len() }
}

/// WHERE CURRENT OF's curCtid of relation `rel` (execCurrentOf picks its one ExecRowMark first): a program's one lock
/// answers for any relation (a family's leaf is checked by the caller), else the lock of `rel`, else the lock whose last
/// locked row is in `rel` (a family lock's later leaf: its host's one row is PostgreSQL's one active child's curCtid).
pub fn locked_in(locks: &[(Option<u32>, ScanPos)], rel: u32) -> ScanPos {
    locks.iter().find(|l| l.0.is_none_or(|r| r == rel)).or_else(|| locks.iter().find(|l| l.1.rel == rel)).map_or(ScanPos::NONE, |l| l.1)
}

impl Node {
    /// After a `Recheck` outcome: the newest version of the row the write or
    /// lock host found updated (the EPQ test tuple), or `NONE`.
    pub fn recheck_pos(&self) -> ScanPos {
        self.inner.recheck_pos
    }

    /// C5b: the row the lock host last left locked (ExecLockRows' erm->curCtid; `Locker::current`), `NONE` without a lock;
    /// of several locks (`Program.locks`), relation `rel`'s (`locked_in`).
    pub fn locked(&self, rel: u32) -> ScanPos {
        locked_in(&self.lockeds(), rel)
    }

    /// Each lock's (relation, `Locker::current`); the relation `None` for a program's one lock.
    pub fn lockeds(&self) -> Vec<(Option<u32>, ScanPos)> {
        self.inner.lockers.iter().map(|l| (l.rel, l.host.current())).collect() // alloc: Cold
    }
}
