//! A root memory context, made here (only `mem` creates one, rule C14) and boxed so its address holds while a
//! context handle into it is out. `held` is the one place a root's handle is widened to `Mcx<'static>` for PG's
//! `'static`-typed APIs (rule R7); every caller states what it drops first.
#![allow(unsafe_code)]
use mcx::{Mcx, MemoryContext};

pub struct Root(Box<MemoryContext>);

/// Every root constructor's one box: the context's address holds while a handle into it is out.
fn boxed(c: MemoryContext) -> Root {
    Root(Box::new(c)) // alloc: Exec
}

impl Root {
    pub fn new(name: &'static str) -> Root {
        boxed(MemoryContext::new(name))
    }

    /// D2942 / D2944: a root whose reset is wholesale (C's AllocSetReset): a per-row or per-group context reset as
    /// PostgreSQL resets its per-tuple context, with the row's or group's values still in it.
    pub fn wholesale(name: &'static str) -> Root {
        boxed(MemoryContext::new_wholesale(name))
    }

    /// D4494: a root whose first block is small (mcx new_start_small) for a long-lived root that keeps little between uses;
    /// a kept 8 kB first block shared mimalloc's 8 kB page class with the planner's per-statement blocks, and the planner's
    /// page was freed and fetched again every statement.
    pub fn start_small(name: &'static str) -> Root {
        boxed(MemoryContext::new_start_small(name))
    }

    /// `start_small` whose reset is wholesale (mcx new_small_wholesale: a 1 KiB first block, 8 KiB blocks, live chunks
    /// dropped at reset as a bump root's are): a host's root kept between statements that holds little while parked
    /// (gravity_hosts `scratch::small_roots`).
    pub fn small_wholesale(name: &'static str) -> Root {
        boxed(MemoryContext::new_small_wholesale(name))
    }

    /// A bump root: allocation is a pointer bump, nothing is freed before `reset` or drop.
    pub fn bump(name: &'static str) -> Root {
        boxed(MemoryContext::new_bump(name))
    }

    pub fn mcx(&self) -> Mcx<'_> {
        self.0.mcx()
    }

    /// A child context whose reset frees every chunk, live or not (`MemoryContext::new_child_wholesale`), charged to this
    /// root's accounting. The child drops before its root.
    pub fn new_child_wholesale(&self, name: &'static str) -> MemoryContext {
        self.0.new_child_wholesale(name)
    }

    /// A child context (an aset, `MemoryContext::new_child`) under this root, taking its owner until it is given its own
    /// (`MemoryContext::set_owner`); it drops before its root.
    pub fn new_child(&self, name: &'static str) -> MemoryContext {
        self.0.new_child(name)
    }

    /// LEDGER2: this root's bytes are the calling backend's session's (`MemoryContext::set_owner_session`): a root that
    /// lives with the backend's state; nothing off a backend thread.
    pub fn set_owner_session(&self) {
        self.0.set_owner_session();
    }

    /// D4464: this root and every live context under it are `o`'s from now, their bytes moving with them in one step
    /// (`MemoryContext::set_owner_tree`): a root a parked Node keeps past the statement that built it (a host's scratch)
    /// follows the Node to the owner that holds it next.
    pub fn set_owner_tree(&self, o: memledger::Owner) {
        self.0.set_owner_tree(o);
    }

    /// Frees everything placed in it; `&mut` so no handle borrowed through `mcx` is out.
    pub fn reset(&mut self) {
        self.0.reset();
    }

    /// Bytes the context holds now.
    pub fn used(&self) -> usize {
        self.0.used()
    }

    /// The context's blocks' bytes (`MemoryContext::mem_allocated`): a bump root's are what `used` says, an aset root's
    /// add the free room in its blocks.
    pub fn footprint(&self) -> usize {
        self.0.mem_allocated()
    }

    /// The context as `Mcx<'static>`.
    ///
    /// # Safety
    /// Every object placed through the returned handle is dropped, and the handle is no longer used, before this
    /// `Root` drops.
    pub unsafe fn held(&self) -> Mcx<'static> {
        // SAFETY: the box keeps the context's address for the root's life; the caller keeps the uses inside it.
        unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(self.0.mcx()) }
    }
}

/// A root context handed by value to an owner that keeps it itself (a hosted aggregate's state node, PG's per-Agg
/// `aggcontext`, and its arena's init context). `observed` false makes it unobserved (mcx `new_unobserved`): no entry in
/// the creating thread's context forest, so a parallel owner's root may be used and dropped on another thread in turn.
pub fn context(name: &'static str, observed: bool) -> MemoryContext {
    if observed { MemoryContext::new(name) } else { MemoryContext::new_unobserved(name) }
}

/// `context` whose reset is wholesale (C's AllocSetReset; D2942: a hosted aggregate's aggcontext, reset at each streamed
/// group's start and at a re-open with the group's transition states still in it, as nodeAgg.c's ReScanExprContext).
pub fn context_wholesale(name: &'static str, observed: bool) -> MemoryContext {
    if observed { MemoryContext::new_wholesale(name) } else { MemoryContext::new_unobserved_wholesale(name) }
}

#[cfg(test)]
mod tests {
    use super::Root;

    #[test]
    fn a_held_handle_is_the_roots_context_and_stays_put_when_the_root_moves() {
        let root = Root::new("GravityRootTest");
        let before = root.mcx().context() as *const _;
        let moved = vec![root]; // alloc: Cold
        // SAFETY: `h` is used only while `moved` is alive.
        let h = unsafe { moved[0].held() };
        assert_eq!(h.context() as *const _, before);
        assert_eq!(h.context().name(), "GravityRootTest");
    }

    #[test]
    fn a_reset_root_keeps_its_context() {
        let mut root = Root::new("GravityRootReset");
        let before = root.mcx().context() as *const _;
        root.reset();
        assert_eq!(root.mcx().context() as *const _, before);
        assert_eq!(root.mcx().context().name(), "GravityRootReset");
    }

    #[test]
    fn a_bump_root_frees_its_bytes_on_reset() {
        let mut root = Root::bump("GravityRootBump");
        const N: usize = 64 * 1024;
        // A small first allocation claims the (small) keeper block, so the large one takes a block of its own.
        root.mcx().alloc_bytes(64).unwrap();
        root.mcx().alloc_bytes(N).unwrap();
        assert!(root.used() >= N, "a bump context holds what it handed out until reset");
        root.reset();
        // A reset bump context keeps its first (keeper) block, so `used` falls below what was handed out, not to 0.
        assert!(root.used() < N, "reset frees the blocks beyond the keeper: used {}", root.used());
    }
}
