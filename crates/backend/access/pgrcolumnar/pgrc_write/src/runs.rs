//! The streaming k-way merge's heap over sorted sources (the merged scan of a sorted_by table's parts; the pgrc line's
//! run pass). The caller lends the slots and passes its `less` (over each source's current row) to every call: `less`
//! borrows the caller's cursors, and a tie it breaks by source number keeps the merge stable.

/// The sources that still have a row, the least on top, in `slots[..len]`.
#[derive(Debug)]
pub struct Heap<'s> {
    v: &'s mut [u32],
    n: usize,
}

impl<'s> Heap<'s> {
    /// A heap over the sources in `slots[..live]`.
    pub fn new(slots: &'s mut [u32], live: usize, less: &mut dyn FnMut(u32, u32) -> bool) -> Heap<'s> {
        let mut h = Heap::resume(slots, live);
        for i in (0..live / 2).rev() {
            h.down(i, less);
        }
        h
    }

    /// The heap a previous [`Heap`] over the same slots left, `live` being its [`Heap::len`].
    pub fn resume(slots: &'s mut [u32], live: usize) -> Heap<'s> {
        assert!(live <= slots.len(), "heap: {live} live sources in {} slots", slots.len());
        Heap { v: slots, n: live }
    }

    /// The source whose current row is least.
    pub fn top(&self) -> Option<u32> {
        (self.n > 0).then(|| self.v[0])
    }

    /// The least source after the top: the top's rows up to this source's current row come out in a run.
    pub fn runner_up(&self, less: &mut dyn FnMut(u32, u32) -> bool) -> Option<u32> {
        match self.n {
            0 | 1 => None,
            2 => Some(self.v[1]),
            _ => Some(if less(self.v[2], self.v[1]) { self.v[2] } else { self.v[1] }),
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The top source moved to its next row.
    pub fn fix_top(&mut self, less: &mut dyn FnMut(u32, u32) -> bool) {
        if self.n > 0 {
            self.down(0, less);
        }
    }

    /// The top source has no row left; it stays in `slots[len]`.
    pub fn pop_top(&mut self, less: &mut dyn FnMut(u32, u32) -> bool) {
        if self.n == 0 {
            return;
        }
        self.n -= 1;
        self.v.swap(0, self.n);
        if self.n > 1 {
            self.down(0, less);
        }
    }

    fn down(&mut self, mut i: usize, less: &mut dyn FnMut(u32, u32) -> bool) {
        let n = self.n;
        loop {
            let l = 2 * i + 1;
            if l >= n {
                return;
            }
            let c = if l + 1 < n && less(self.v[l + 1], self.v[l]) { l + 1 } else { l };
            if !less(self.v[c], self.v[i]) {
                return;
            }
            self.v.swap(c, i);
            i = c;
        }
    }
}
