//! D2605 (modifytable, regress triggers#499 / with#234): each ModifyTable's statement triggers at its own call and end.
//! PostgreSQL fires a ModifyTable's BEFORE STATEMENT triggers at its first call (fireBSTriggers) and queues its AFTER
//! STATEMENT events at its end (fireASTriggers, once its subplan returns no row). A data-modifying CTE's ModifyTable is
//! called when its first reader reads past the stored rows and ends when a reader, or ExecPostprocessPlan, reads past its
//! last row; the statement's own ends when the main plan does. A writer whose plan names its CTE's pulled pipeline
//! (`WriteAt::end`, `Program::write_end`; the lowerer sets it only when every writing CTE of the statement is pulled) binds
//! at its set's first pull (`bind_at`, from the runner's `OnDemand::pull_members`), its bind's statement entries tagged
//! with its slot, and queues its AFTER STATEMENT events once its set ran to its end (`end_at`). The statement's own
//! writers queue theirs when the main plan ends, before the end drain (`end_main`); whatever is left queues at the
//! statement's end, as before. Until bound, a lazy writer takes part in no host walk.
//!
//! D2772 POST / D3449: in a program with post pipelines (an unread data-modifying CTE's, run after the statement's own,
//! lowerv4 ops/modifytable/post.rs) each CTE's writer names its post group's first pipeline (`WriteAt::end`) and binds at
//! the execution's turn before it (entry/post.rs `turn`, `binds_at` then `bind_at`), once the AFTER STATEMENT events so
//! far are queued; a recheck node, which never reaches a post pipeline, never binds it.

use gravity_program::program::END_STATEMENT;
use pgsync::OnceLock;

use super::{Inner, Node};
use crate::binding::BindingRef;
use crate::Result;

/// The hosts' statement-entry hooks (exec `after_query`): the entries made so far (`mark`), the current level's entries
/// made since a mark tagged (`tag`), and the level's entries of one tag queued now (`end`; 0: the untagged ones).
pub struct StmtEnds {
    pub mark: fn() -> usize,
    pub tag: fn(usize, u32),
    pub end: fn(u32) -> Result<()>,
}

pub static STMT_ENDS: OnceLock<StmtEnds> = OnceLock::new();

/// Writer slot `j`'s end: slot 0 the program's `write`, slot `i + 1` its `writes[i]`.
fn end_of(program: &gravity_program::Program, j: usize) -> u16 {
    match j {
        0 => program.write_end,
        _ => program.writes.get(j - 1).map_or(END_STATEMENT, |w| w.end),
    }
}

/// The lazy writer slots of a program (`Inner::lazy`'s first value): none without the hooks or on a worker (no writer).
pub(crate) fn lazy_slots(program: &gravity_program::Program, writer: bool, writers: usize) -> u64 {
    if STMT_ENDS.get().is_none() {
        return 0;
    }
    let mut m = 0u64;
    if writer && program.write_end != END_STATEMENT {
        m |= 1;
    }
    for (i, w) in program.writes.iter().take(writers.min(63)).enumerate() {
        if w.end != END_STATEMENT {
            m |= 1 << (i + 1);
        }
    }
    m
}

/// Writer slot `j` is lazy and not bound yet: every host walk passes it over.
pub(crate) fn unbound(inner: &Inner, j: usize) -> bool {
    j < 64 && inner.lazy & (1 << j) != 0
}

impl Node {
    /// A lazy writer not bound yet names pipeline `p` (program index): a post group's first pipeline before its turn.
    pub(crate) fn binds_at(&self, p: u16) -> bool {
        self.inner.lazy != 0 && (0..64usize).any(|j| unbound(&self.inner, j) && end_of(&self.program, j) == p)
    }

    /// The first pull of the set whose pulled pipeline is `p` (program index): each lazy writer of `p` binds (its BEFORE
    /// STATEMENT triggers), its bind's statement entries tagged with its slot. An error leaves that writer unbound: it
    /// unbinds here (D4602), as the host walks pass over a writer still lazy.
    pub fn bind_at(&mut self, b: &BindingRef<'_>, p: u16) -> Result<()> {
        let Some(h) = STMT_ENDS.get() else { return Ok(()) };
        if self.inner.lazy == 0 {
            return Ok(());
        }
        let program = self.program.clone();
        for j in 0..64usize {
            if !unbound(&self.inner, j) || end_of(&program, j) != p {
                continue;
            }
            let w = match j {
                0 => self.inner.writer.as_mut().map(|w| &mut **w),
                _ => self.inner.writers.get_mut(j - 1).map(|w| &mut *w.1),
            };
            let Some(w) = w else { continue };
            let mark = (h.mark)();
            if let Err(e) = w.bind(b) {
                w.unbind();
                return Err(e);
            }
            (h.tag)(mark, j as u32 + 1);
            self.inner.lazy &= !(1 << j);
        }
        Ok(())
    }

    /// The set whose pulled pipeline is `p` ran to its end: each bound writer of `p` queues its AFTER STATEMENT events.
    pub fn end_at(&mut self, p: u16) -> Result<()> {
        let Some(h) = STMT_ENDS.get() else { return Ok(()) };
        if self.inner.lazy_all == 0 {
            return Ok(());
        }
        let program = self.program.clone();
        for j in 0..64usize {
            if self.inner.lazy_all & (1 << j) != 0 && !unbound(&self.inner, j) && end_of(&program, j) == p {
                (h.end)(j as u32 + 1)?;
            }
        }
        Ok(())
    }

    /// The main plan ended (before the end drain, PostgreSQL's ExecPostprocessPlan): the statement's own writers queue
    /// their AFTER STATEMENT events. Only in a program with lazy writers.
    pub fn end_main(&mut self) -> Result<()> {
        match STMT_ENDS.get() {
            Some(h) if self.inner.lazy_all != 0 => (h.end)(0),
            _ => Ok(()),
        }
    }
}

/// `remove`'s end: the lazy writers unbound again (the unbind walk passed over those still unbound), for the next execution.
pub(crate) fn reset(inner: &mut Inner) {
    inner.lazy = inner.lazy_all;
}
