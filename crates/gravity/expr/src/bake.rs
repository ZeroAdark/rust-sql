//! The IR's bake log (V4R-18, main 09-24): a read of a by-reference constant's value in a compiled expression
//! (`ConstNode::read`) names the constant here by its image's address (the plan Const's `constvalue`), so the front key
//! learns which literals a program baked (its code, its tables or its shape depend on the bytes) and which it binds as
//! words per execution. gravity_decompose keeps the plan tree's twin (its `bake.rs`: the crates share no module).
//! Scopes nest, one per admission (a statement admitted while another lowers, a plan-time fold, logs into its own); a
//! read outside every scope (the interpreter at execution) is logged nowhere.
use core::cell::RefCell;

leader_tls::leader_tls! {
    thread_local! {
        static SCOPES: RefCell<Vec<Vec<usize>>> = const { RefCell::new(Vec::new()) }; // alloc: Cold
    }
}

/// A bake scope (`open`) over the reads that follow, closed when it drops (`close`), on any exit, an unwind too.
#[must_use = "the scope closes when the guard drops"]
pub struct Scope(usize);

impl Scope {
    pub fn open() -> Scope {
        Scope(open())
    }

    /// The scope's reads so far, sorted and deduplicated.
    pub fn reads(&self) -> Vec<usize> {
        reads(self.0)
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        close(self.0);
    }
}

/// Opens a scope over the reads that follow; its depth is `take`'s and `close`'s handle.
fn open() -> usize {
    SCOPES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(Vec::new()); // alloc: Cold
        s.len()
    })
}

/// The reads of scope `depth` so far, sorted and deduplicated (none once it closed).
fn reads(depth: usize) -> Vec<usize> {
    SCOPES.with(|s| {
        let mut v = s.borrow().get(depth.wrapping_sub(1)).cloned().unwrap_or_default();
        v.sort_unstable();
        v.dedup();
        v
    })
}

/// Closes scope `depth` and any a caller left open above it.
fn close(depth: usize) {
    SCOPES.with(|s| s.borrow_mut().truncate(depth.saturating_sub(1)));
}

/// A read of the by-reference image at `image` (its address).
pub fn note(image: usize) {
    SCOPES.with(|s| {
        if let Some(top) = s.borrow_mut().last_mut() {
            top.push(image); // alloc: Cold
        }
    });
}
