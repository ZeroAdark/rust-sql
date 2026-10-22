use gravity_driver::mem::root::Root;

mod raw;

/// A host's bump contexts: `node` lives as long as the host, `exec` is reset at unbind, and `bind` (a host whose `exec`
/// is reset more often than per bind, `with_bind`) holds what lives from a bind to its unbind.
pub struct NodeScratch {
    node: Root,
    exec: Root,
    bind: Option<Root>,
    /// D4464: the owner the roots were last homed on (`home`); `None` while they stay on the one they took at their
    /// creation (the building statement's, then where its end sent its bytes).
    at: Option<memledger::Owner>,
}

std::thread_local! {
    /// `small_roots`' switch: `NodeScratch::new` makes small-first roots while a guard lives on this thread.
    static SMALL: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// While the guard lives, every `NodeScratch` made on this thread (`new`, `with_bind`) gets small-first roots
/// (`Root::small_wholesale`: a 1 KiB first block, 8 KiB blocks, reset wholesale like a bump root) in place of bump roots
/// (an 8 KiB first block, kept over reset): for a host kept between statements that holds little while parked (pglpoint's kept pieces, a memo
/// entry's), as D4494's long-lived roots. The guard puts the previous setting back.
pub fn small_roots() -> SmallRoots {
    SmallRoots(SMALL.with(|s| s.replace(true)))
}

/// `small_roots`' guard.
pub struct SmallRoots(bool);

impl Drop for SmallRoots {
    fn drop(&mut self) {
        let was = self.0;
        SMALL.with(|s| s.set(was));
    }
}

/// Whether a `small_roots` guard lives on this thread: the contexts a host makes now hold little while it is parked
/// (`NodeScratch`'s roots, `IndexMaint`'s AM cache context).
pub(crate) fn small() -> bool {
    SMALL.with(core::cell::Cell::get)
}

fn root(name: &'static str) -> Root {
    if small() { Root::small_wholesale(name) } else { Root::bump(name) }
}

impl NodeScratch {
    pub fn new(name: &'static str) -> NodeScratch {
        NodeScratch { node: root(name), exec: root("GravityExecRegion"), bind: None, at: None }
    }

    /// D3195: a per-bind context, reset at unbind (`reset_bind`). A bitmap source resets `exec` at every seek (D2970), so its
    /// scans, begun at each bind, lived in `node`, which nothing resets: a worker slot binds every host at each section's
    /// open, and a rerun set's member that runs as a section opens one per rescan, so each worker's bitmap source grew by its
    /// scans' bytes at every rescan (a correlated SubPlan over a bitmap inner at W8: gigabytes per worker lane, out of memory).
    pub fn with_bind(mut self) -> NodeScratch {
        self.bind = Some(root("GravityBindRegion"));
        self
    }

    pub fn reset_exec(&mut self) {
        self.exec.reset()
    }

    /// D3195: what the last bind placed in the per-bind context is dead (the scans ended); none without one.
    pub fn reset_bind(&mut self) {
        if let Some(b) = self.bind.as_mut() {
            b.reset()
        }
    }

    pub fn used(&self) -> usize {
        self.node.used() + self.exec.used() + self.bind.as_ref().map_or(0, Root::used)
    }

    /// The roots' footprints (`Root::footprint`: their blocks' bytes), node, exec, bind (0 without one).
    pub fn used_parts(&self) -> [usize; 3] {
        [self.node.footprint(), self.exec.footprint(), self.bind.as_ref().map_or(0, Root::footprint)]
    }

    /// D4464: the roots and every context under them on `to` from now, their bytes moving with them in one step. A host
    /// keeps its scratch for its Node's life, past the statement that built the Node: a parked Node bound by a later
    /// statement charges what the scratch holds and grows to that statement (`home_statement`), and a worker set the slot
    /// depot keeps moves it to the depot's holder at the park and to the taking statement at the take
    /// (`Source::transfer_held`). Nothing when the roots are already `to`'s.
    pub fn home(&mut self, to: memledger::Owner) {
        if self.at == Some(to) {
            return;
        }
        self.node.set_owner_tree(to);
        self.exec.set_owner_tree(to);
        if let Some(b) = self.bind.as_ref() {
            b.set_owner_tree(to);
        }
        self.at = Some(to);
    }

    /// The bind's `home`: the binding statement's (or portal's) owner (`gravity_driver::stmt_context`); nothing outside a
    /// statement (a rig's bind).
    pub fn home_statement(&mut self) {
        if let Some((o, _)) = gravity_driver::stmt_context() {
            self.home(o);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{small_roots, NodeScratch};

    /// Under `small_roots` a scratch that holds little holds a small first block, not a bump root's 8 KiB keeper; the
    /// guard puts the bump roots back.
    #[test]
    fn small_roots_hold_a_small_first_block_until_the_guard_drops() {
        let small = {
            let _g = small_roots();
            NodeScratch::new("GravityScratchTest")
        };
        let bump = NodeScratch::new("GravityScratchTest");
        small.mcx().alloc_bytes(64).unwrap();
        small.exec_mcx().alloc_bytes(64).unwrap();
        bump.mcx().alloc_bytes(64).unwrap();
        let [sn, se, _] = small.used_parts();
        assert!(sn <= 2048 && se <= 2048, "small roots: node {sn} exec {se}");
        assert!(bump.used_parts()[0] >= 8192, "a bump root's keeper: {:?}", bump.used_parts());
    }
}
