//! The plan tree's bake log (V4R-18, main 09-24): a read of a by-reference Const's value that decides the graph or the
//! program (`read`, `expr::const_bytes`: an image hashed into a fingerprint, walked for an IN list's length, copied
//! into a VALUES table or a recipe, folded into the code) names the Const here by its image's address, so the front key
//! learns which literals a program baked and which it binds as words per execution. gravity_expr keeps the IR's twin
//! (its `bake.rs`: the crates share no module). Scopes nest, one per admission; a read outside every scope is logged
//! nowhere. A read that only keys (gravity_lower key.rs, exec front.rs) or binds a word per execution (exec admit's
//! bind) is no bake and reads `constvalue` itself (exec tests/front_bake_lint.rs lists each such read).
use core::cell::RefCell;

use datum::Datum;
use types_nodes::primnodes::Const;

leader_tls::leader_tls! {
    thread_local! {
        static SCOPES: RefCell<Vec<Vec<usize>>> = const { RefCell::new(Vec::new()) }; // alloc: Cold
        /// The constants decompose built in each open scope (`Derived`): (the scope's depth, the new constant's address, how
        /// it was built). The front key files such a constant by how it was built (exec inputs/front.rs `file`); a scope's
        /// entries go when it closes, and a constant built outside every scope is logged nowhere (its statement keeps today's
        /// path).
        static DERIVED: RefCell<Vec<(usize, usize, Derived)>> = const { RefCell::new(Vec::new()) }; // alloc: Cold
    }
}

/// How decompose built a constant the plan does not hold (`derived_note`), from which the front key files it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Derived {
    /// normalize `retype`: the plan constant at address `src` converted under btree `strategy` (`normalize::retype_value`).
    Retype { src: usize, strategy: u8 },
    /// A copy of the plan constant at address `src` (agg gsets.rs: a later arm's own literal).
    Copy { src: usize },
    /// normalize `like_twin`: the LIKE pattern of the plan text constant at address `src` (`normalize::twin_pattern`,
    /// `%` before it when `head`, after it when `tail`).
    Twin { src: usize, head: bool, tail: bool },
    /// normalize `detext` (D3079): the integer whose output text is the plan text constant at address `src`
    /// (`normalize::detext_value`, of the new constant's type).
    Detext { src: usize },
    /// A value fixed by plan fields the front key holds (agg gsets.rs: an arm's phase id, a key's rolled flag, a
    /// GROUPING mask, from the Agg's grouping sets, chain and the GroupingFunc's columns).
    Fixed,
}

/// Notes constant `new` (its address), built as `how`, in the innermost open scope.
pub fn derived_note(new: usize, how: Derived) {
    SCOPES.with(|s| {
        let depth = s.borrow().len();
        if depth > 0 {
            DERIVED.with(|d| d.borrow_mut().push((depth, new, how))); // alloc: Cold
        }
    });
}

/// How constant `n` was built when decompose noted it in the innermost open scope; `None` for any other node.
pub fn derived(n: types_nodes::Node<'_>) -> Option<Derived> {
    let (a, depth) = (n.as_raw().as_ptr() as usize, SCOPES.with(|s| s.borrow().len()));
    DERIVED.with(|d| d.borrow().iter().rev().find(|e| e.0 == depth && e.1 == a).map(|e| e.2))
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

/// Opens a scope over the reads that follow; its depth is `reads`' and `close`'s handle.
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
    DERIVED.with(|d| d.borrow_mut().retain(|e| e.0 < depth));
}

/// A read of the by-reference image at `image` (its address).
pub fn note(image: usize) {
    SCOPES.with(|s| {
        if let Some(top) = s.borrow_mut().last_mut() {
            top.push(image); // alloc: Cold
        }
    });
}

/// `c`'s value read to decide the graph or the program: a non-null by-reference image is baked.
pub fn read(c: &Const) -> Datum {
    if !c.constbyval && !c.constisnull {
        note(c.constvalue.as_usize());
    }
    c.constvalue
}
