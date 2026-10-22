// snapmgr.c RegisteredSnapshots: a min-heap on xmin (xmin_cmp) so the oldest
// registered xmin is O(1) and a registration comes and goes in O(log n).
// C uses a pairing heap with an intrusive node; this is a binary heap whose
// entries record their slot in the snapshot (`regd_pos`).

use types_core::TransactionIdPrecedes;
use types_snapshot::REGD_POS_NONE;

use crate::Snapshot;

#[derive(Default)]
pub(crate) struct RegHeap {
    heap: Vec<Snapshot>,
}

impl RegHeap {
    pub(crate) const fn new() -> RegHeap {
        RegHeap { heap: Vec::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub(crate) fn min_xmin(&self) -> Option<types_core::TransactionId> {
        self.heap.first().map(|s| s.xmin)
    }

    pub(crate) fn push(&mut self, snap: Snapshot) {
        debug_assert_eq!(snap.regd_pos.get(), REGD_POS_NONE);
        let i = self.heap.len();
        snap.regd_pos.set(i as u32);
        self.heap.push(snap);
        self.sift_up(i);
    }

    pub(crate) fn remove(&mut self, snap: &Snapshot) {
        let pos = snap.regd_pos.get();
        if pos == REGD_POS_NONE {
            return;
        }
        let i = pos as usize;
        debug_assert!(std::rc::Rc::ptr_eq(&self.heap[i], snap));
        let last = self.heap.len() - 1;
        self.heap.swap(i, last);
        let removed = self.heap.pop().expect("non-empty");
        removed.regd_pos.set(REGD_POS_NONE);
        if i < self.heap.len() {
            self.heap[i].regd_pos.set(i as u32);
            self.sift_down(i);
            self.sift_up(i);
        }
    }

    pub(crate) fn clear(&mut self) {
        for s in self.heap.drain(..) {
            s.regd_pos.set(REGD_POS_NONE);
        }
    }

    fn precedes(&self, a: usize, b: usize) -> bool {
        TransactionIdPrecedes(self.heap[a].xmin, self.heap[b].xmin)
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.heap[a].regd_pos.set(a as u32);
        self.heap[b].regd_pos.set(b as u32);
    }

    fn sift_up(&mut self, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            if !self.precedes(i, parent) {
                break;
            }
            self.swap(i, parent);
            i = parent;
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut least = i;
            if l < self.heap.len() && self.precedes(l, least) {
                least = l;
            }
            if r < self.heap.len() && self.precedes(r, least) {
                least = r;
            }
            if least == i {
                break;
            }
            self.swap(i, least);
            i = least;
        }
    }
}
