//! The MergeAppend cut at the entry (rung12-subplan.md "Seventh landing", need 13): each arm program pushes its rows —
//! already in the merge keys' order — into its own buffer; `drain` cuts the buffers k-way by the keys into the real
//! sink (ties by arm order). The keys are `mergearms::MergeKey`s: by-value words compared as i64 (tier A).
use gravity_abi::Datum;
use gravity_driver::{BorrowedBatch, Error, Flow, Result, Sink};
use gravity_lower::mergearms::MergeKey;
use memledger::{ChargedVec, Kind, Owner};

/// CONVERT wave 2 (site 3): an arm's buffered rows, `ChargedVec`s on the statement's owner once the runner sets it
/// (`ArmMerge::set_owner`; Kind::Region: statement-held output, RULINGS-1 R1), given back with the merge.
struct ArmRows {
    values: ChargedVec<Datum>,
    isnull: ChargedVec<u8>,
    /// The pushed blocks' by-reference regions, each datum kept as its offset into `bytes` until the drain re-points it (as `legs::Leg`).
    bytes: ChargedVec<u8>,
}

impl ArmRows {
    fn of(owner: Option<Owner>) -> ArmRows {
        ArmRows { values: ChargedVec::new(owner, Kind::Region), isnull: ChargedVec::new(owner, Kind::Region), bytes: ChargedVec::new(owner, Kind::Region) } // alloc: Exec
    }

    /// The same rows on `owner`'s charge (the blocks move, nothing is copied).
    fn onto(&mut self, owner: Option<Owner>) {
        fn mv<T>(from: &mut ChargedVec<T>, to: &mut ChargedVec<T>) {
            let v = from.with_vec(core::mem::take);
            to.with_vec(|t| *t = v);
        }
        let mut n = ArmRows::of(owner);
        mv(&mut self.values, &mut n.values);
        mv(&mut self.isnull, &mut n.isnull);
        mv(&mut self.bytes, &mut n.bytes);
        *self = n;
    }

    /// One pushed block's rows appended; a growth the allocator refuses is the statement's 53200.
    fn append(&mut self, values: &[Datum], isnull: &[u8], region: &[u8]) -> Result<()> {
        self.grow(values, isnull, region).map_err(|_| Error::out_of_memory(((values.len() * 8 + isnull.len() + region.len()) as u64).max(self.held())))
    }

    fn grow(&mut self, values: &[Datum], isnull: &[u8], region: &[u8]) -> core::result::Result<(), std::collections::TryReserveError> {
        self.values.extend_from_slice(values)?; // alloc: Exec charged the statement owner (set_owner)
        self.isnull.extend_from_slice(isnull)?; // alloc: Exec charged the statement owner (set_owner)
        self.bytes.extend_from_slice(region) // alloc: Exec charged the statement owner (set_owner)
    }

    /// The bytes the buffers hold (their capacity).
    fn held(&self) -> u64 {
        (self.values.capacity() * 8 + self.isnull.capacity() + self.bytes.capacity()) as u64
    }
}

pub struct ArmMerge<'a, S: Sink> {
    /// The merge's own tables (`arms`, `byref`, `keys`, sized at `new`) on the statement's owner from `set_owner` (fixed:
    /// they never grow). FIRST field: a door-1 charge drops before the bytes it counts are freed.
    held: memledger::Charge,
    inner: &'a mut S,
    natts: usize,
    byref: Vec<bool>,
    keys: Vec<MergeKey>,
    arms: Vec<ArmRows>,
    cur: usize,
    through: u64,
    /// `set_owner` ran (the first set wins).
    owned: bool,
}

impl<'a, S: Sink> ArmMerge<'a, S> {
    /// `narms` buffers (the main is arm 0) over `natts` output columns, `byref` marking the by-reference ones.
    pub fn new(inner: &'a mut S, natts: usize, byref: &[bool], keys: &[MergeKey], narms: usize) -> ArmMerge<'a, S> {
        let mut arms = Vec::with_capacity(narms); // alloc: Exec charged held (set_owner)
        arms.resize_with(narms, || ArmRows::of(None)); // alloc: Exec charged held (set_owner)
        ArmMerge { held: memledger::Charge::NONE, inner, natts, byref: byref.to_vec(), keys: keys.to_vec(), arms, cur: 0, through: 0, owned: false } // alloc: Exec charged held (set_owner)
    }

    /// CONVERT wave 2 (site 3, RULINGS-1 R3): the statement's owner, which the runner hands over once the first arm's
    /// Node bound (after `new`, before any row: the buffers are empty then): the arms' buffers are charged to it from now
    /// on. The first set wins; `None` leaves them uncharged.
    pub fn set_owner(&mut self, owner: Option<Owner>) {
        if core::mem::replace(&mut self.owned, true) {
            return;
        }
        self.arms.iter_mut().for_each(|a| a.onto(owner));
        // the tables `new` allocated on this thread move to the owner, once
        if let Some(o) = owner {
            let bytes = self.arms.capacity() * core::mem::size_of::<ArmRows>() + self.byref.capacity() * core::mem::size_of::<bool>() + self.keys.capacity() * core::mem::size_of::<MergeKey>();
            self.held = memledger::Charge::open(o, memledger::charge::release_of(o), Kind::Region, memledger::charge::Door::Heap, bytes as u64);
        }
    }

    /// The arm whose pushes follow (the entry sets it before each arm program runs).
    pub fn arm(&mut self, i: usize) {
        self.cur = i.min(self.arms.len().saturating_sub(1));
    }

    /// Rows that reached the real sink.
    pub fn through(&self) -> u64 {
        self.through
    }

    /// The k-way cut: every buffered row into the real sink in the keys' order, ties by arm order; returns the rows emitted.
    pub fn drain(&mut self) -> Result<u64> {
        let natts = self.natts.max(1);
        let before = self.through;
        for a in self.arms.iter_mut() {
            if !a.bytes.is_empty() {
                gravity_driver::relocate(&mut a.values, &a.isnull, &self.byref, (0, a.bytes.len()), a.bytes.as_ptr());
            }
        }
        let o = self.arms.first().and_then(|a| a.values.owner());
        let mut heads: ChargedVec<usize> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        heads.with_vec(|v| v.resize(self.arms.len(), 0)); // alloc: Exec charged o
        loop {
            let mut best: Option<usize> = None;
            for (i, a) in self.arms.iter().enumerate() {
                let at = heads[i] * natts;
                if at >= a.values.len() {
                    continue;
                }
                best = match best {
                    None => Some(i),
                    Some(b) => {
                        let bt = heads[b] * natts;
                        let (bv, bn) = (&self.arms[b].values[bt..bt + natts], &self.arms[b].isnull[bt..bt + natts]);
                        if compare(&self.keys, &a.values[at..at + natts], &a.isnull[at..at + natts], bv, bn) == core::cmp::Ordering::Less { Some(i) } else { Some(b) }
                    }
                };
            }
            let Some(i) = best else { break };
            let at = heads[i] * natts;
            heads[i] += 1;
            let a = &self.arms[i];
            let f = self.inner.push(BorrowedBatch { natts, rows: 1, values: &a.values[at..at + natts], isnull: &a.isnull[at..at + natts], region: &[] })?;
            self.through += 1;
            if f == Flow::Stop {
                break;
            }
        }
        for a in self.arms.iter_mut() {
            a.values.clear();
            a.isnull.clear();
            a.bytes.clear();
        }
        Ok(self.through - before)
    }
}

/// The keys' order of two rows: NULLs first or last per key, the direction per key, equal when every key ties.
pub fn compare(keys: &[MergeKey], av: &[Datum], an: &[u8], bv: &[Datum], bn: &[u8]) -> core::cmp::Ordering {
    use core::cmp::Ordering::*;
    for k in keys {
        let c = k.col as usize;
        let o = match (an[c] != 0, bn[c] != 0) {
            (true, true) => Equal,
            (true, false) => if k.nulls_first { Less } else { Greater },
            (false, true) => if k.nulls_first { Greater } else { Less },
            (false, false) => {
                let o = (av[c] as i64).cmp(&(bv[c] as i64));
                if k.desc { o.reverse() } else { o }
            }
        };
        if o != Equal {
            return o;
        }
    }
    Equal
}

impl<S: Sink> Sink for ArmMerge<'_, S> {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        let a = &mut self.arms[self.cur];
        let n = b.natts * b.rows;
        let (at, base) = (a.values.len(), a.bytes.len());
        a.append(&b.values[..n], &b.isnull[..n], b.region)?;
        if !b.region.is_empty() {
            gravity_driver::relocate(&mut a.values[at..], &a.isnull[at..], &self.byref, (b.region.as_ptr() as u64, b.region.len()), base as *const u8);
        }
        Ok(Flow::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rows(Vec<(i64, bool)>, usize);

    impl Sink for Rows {
        fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
            for (v, n) in b.iter() {
                self.0.push((v[0] as i64, n[0] != 0));
            }
            Ok(if self.1 > 0 && self.0.len() >= self.1 { Flow::Stop } else { Flow::Continue })
        }
    }

    fn feed(m: &mut ArmMerge<'_, Rows>, arm: usize, rows: &[(i64, bool)]) {
        m.arm(arm);
        for (v, n) in rows {
            m.push(BorrowedBatch { natts: 1, rows: 1, values: &[*v as u64], isnull: &[*n as u8], region: &[] }).unwrap();
        }
    }

    fn run(keys: &[MergeKey], arms: &[&[(i64, bool)]], stop: usize) -> Vec<(i64, bool)> {
        let mut out = Rows(Vec::new(), stop); // alloc: Cold
        let mut m = ArmMerge::new(&mut out, 1, &[false], keys, arms.len());
        for (i, a) in arms.iter().enumerate() {
            feed(&mut m, i, a);
        }
        m.drain().unwrap();
        out.0
    }

    #[test]
    fn ascending_nulls_last() {
        let k = [MergeKey { col: 0, desc: false, nulls_first: false }];
        let got = run(&k, &[&[(1, false), (3, false), (0, true)], &[(2, false), (4, false)], &[]], 0);
        assert_eq!(got, [(1, false), (2, false), (3, false), (4, false), (0, true)]);
    }

    #[test]
    fn descending_nulls_first_and_the_stop() {
        let k = [MergeKey { col: 0, desc: true, nulls_first: true }];
        let got = run(&k, &[&[(0, true), (5, false), (1, false)], &[(3, false)]], 0);
        assert_eq!(got, [(0, true), (5, false), (3, false), (1, false)]);
        let got = run(&k, &[&[(0, true), (5, false), (1, false)], &[(3, false)]], 2);
        assert_eq!(got, [(0, true), (5, false)]);
    }

    #[test]
    fn ties_by_arm_order() {
        let k = [MergeKey { col: 0, desc: false, nulls_first: false }];
        let mut out = Rows(Vec::new(), 0); // alloc: Cold
        let mut m = ArmMerge::new(&mut out, 1, &[false], &k, 2);
        feed(&mut m, 1, &[(7, false)]);
        feed(&mut m, 0, &[(7, false)]);
        assert_eq!(m.drain().unwrap(), 2);
        assert_eq!(m.through(), 2);
    }

    /// CONVERT wave 2 (site 3): once the runner sets the statement's owner, the arms' buffers (rows held before it
    /// included) are on its charge, the merge's order unchanged; a second set is ignored; the drop gives the bytes back.
    #[test]
    fn the_arms_buffers_are_on_the_statement_owner_from_the_set_until_the_drop() {
        std::thread::spawn(|| {
            memledger::bind_backend(3995, 0);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let committed = || memledger::charge::owner_row(o).committed;
            let base = committed();
            let k = [MergeKey { col: 0, desc: false, nulls_first: false }];
            let mut out = Rows(Vec::new(), 0); // alloc: Cold
            {
                let mut m = ArmMerge::new(&mut out, 1, &[false], &k, 2);
                feed(&mut m, 0, &[(0, false)]);
                m.set_owner(Some(o));
                m.set_owner(None);
                let even: Vec<(i64, bool)> = (1..5000).map(|i| (2 * i, false)).collect(); // alloc: Cold
                let odd: Vec<(i64, bool)> = (0..5000).map(|i| (2 * i + 1, false)).collect(); // alloc: Cold
                feed(&mut m, 0, &even);
                feed(&mut m, 1, &odd);
                let held = m.arms.iter().map(ArmRows::held).sum::<u64>();
                // and the merge's own tables (arms, byref, keys), on the owner from the set
                let tables = m.held.committed();
                assert!(tables >= 2 * core::mem::size_of::<ArmRows>() as u64, "the tables: {tables}");
                assert!(held >= 10_000 * 9 && committed() - base == (held + tables) as i64, "held {held} + tables {tables}, committed {}", committed() - base);
                assert_eq!(m.drain().unwrap(), 10_000);
            }
            assert_eq!(out.0, (0..10_000).map(|i| (i, false)).collect::<Vec<_>>()); // alloc: Cold
            assert_eq!(committed(), base, "the buffers' bytes left the owner with the merge");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
