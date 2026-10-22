// guc_nondef_list (guc.c:226): a dlist threaded through the variables, so
// set_guc_source links and unlinks in O(1) and walks keep insertion order.

const NIL: u32 = u32::MAX;
const UNLINKED: u32 = u32::MAX - 1;

#[derive(Clone, Debug)]
pub(crate) struct NondefList {
    head: u32,
    tail: u32,
    prev: Vec<u32>,
    next: Vec<u32>,
}

impl Default for NondefList {
    fn default() -> Self {
        Self { head: NIL, tail: NIL, prev: Vec::new(), next: Vec::new() }
    }
}

impl NondefList {
    // Sized by GucRegistry::define so linking never allocates.
    pub(crate) fn try_grow(&mut self, len: usize) -> Result<(), ()> {
        let extra = len.saturating_sub(self.prev.len());
        self.prev.try_reserve(extra).map_err(|_| ())?;
        self.next.try_reserve(extra).map_err(|_| ())?;
        self.prev.extend(core::iter::repeat_n(UNLINKED, extra));
        self.next.extend(core::iter::repeat_n(UNLINKED, extra));
        Ok(())
    }

    pub(crate) fn contains(&self, idx: usize) -> bool {
        self.prev.get(idx).is_some_and(|&p| p != UNLINKED)
    }

    pub(crate) fn push_tail(&mut self, idx: usize) {
        if self.contains(idx) {
            return;
        }
        let i = idx as u32;
        self.prev[idx] = self.tail;
        self.next[idx] = NIL;
        if self.tail == NIL {
            self.head = i;
        } else {
            self.next[self.tail as usize] = i;
        }
        self.tail = i;
    }

    pub(crate) fn remove(&mut self, idx: usize) {
        if !self.contains(idx) {
            return;
        }
        let (p, n) = (self.prev[idx], self.next[idx]);
        if p == NIL {
            self.head = n;
        } else {
            self.next[p as usize] = n;
        }
        if n == NIL {
            self.tail = p;
        } else {
            self.prev[n as usize] = p;
        }
        self.prev[idx] = UNLINKED;
        self.next[idx] = UNLINKED;
    }

    pub(crate) fn first(&self) -> Option<usize> {
        (self.head != NIL).then_some(self.head as usize)
    }

    pub(crate) fn next_of(&self, idx: usize) -> Option<usize> {
        let n = self.next[idx];
        (n != NIL).then_some(n as usize)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        let mut cur = self.first();
        core::iter::from_fn(move || {
            let idx = cur?;
            cur = self.next_of(idx);
            Some(idx)
        })
    }
}
