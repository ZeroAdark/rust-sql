//! The hashed SubPlan's set at the entry (rung12-subplan.md "Tenth landing", need 17): the build program's rows — one
//! by-value key column — pushed into a statement-held open-addressing set whose header the main program's probe
//! reads in IR (`gravity_lower::subhash::probe`): no host call per row. PostgreSQL's `ExecHashSubPlan` over one
//! column: a NULL key is counted, not stored (`HAS_NULL`); an empty set answers false for every row.
use std::collections::TryReserveError;

use gravity_driver::{BorrowedBatch, Error, Flow, Result, Sink};
use gravity_lower::ops::subplan::keyset::{slot_of, width_mask, KeyTable, HAS_NULL, NONEMPTY};
use gravity_program::cost::KEYSET_MIN_SLOTS;
use memledger::charge::Owner;
use memledger::{ChargedVec, Kind};

/// CONVERT wave 2 (site 6): the set's two arrays are `ChargedVec`s, each grown block charged to the build's owner
/// (`own`: the hashed SubPlan's build pipeline's Distinct slot, set at its first insert) and given back with the set.
/// memsig E-pub: a new set holds no block; its header reads `EMPTY_KEYS` / `EMPTY_TAGS` (a probe of the empty set finds
/// every tag 0), and its first insert grows the 16-slot start on the owner like every later growth.
pub struct KeySet {
    keys: ChargedVec<u64>,
    tags: ChargedVec<u8>,
    len: usize,
    flags: u64,
    width_mask: u64,
    header: Box<KeyTable>,
    /// The owner (and kind) a growth's arrays are charged to; `owned` once `own` ran (with `None`: charged to no one).
    owner: Option<(Owner, Kind)>,
    owned: bool,
}

/// The arrays a set with no block yet publishes (`KeySet::new`): read only (a probe), never written (`insert` grows first).
static EMPTY_KEYS: [u64; KEYSET_MIN_SLOTS] = [0; KEYSET_MIN_SLOTS];
static EMPTY_TAGS: [u8; KEYSET_MIN_SLOTS] = [0; KEYSET_MIN_SLOTS];

impl KeySet {
    /// A set over keys of `width` bytes (1, 2, 4 or 8): a narrower datum's high bits are not part of the key.
    pub fn new(width: u8) -> KeySet {
        let width_mask = width_mask(width);
        let header = Box::new(KeyTable { keys: gravity_abi::word(core::ptr::null()), tags: gravity_abi::word(core::ptr::null()), mask: 0, flags: 0 }); // alloc: Exec
        let mut s = KeySet { keys: ChargedVec::none(), tags: ChargedVec::none(), len: 0, flags: 0, width_mask, header, owner: None, owned: false };
        s.publish();
        s
    }

    /// Whether the set still waits for its owner (`own`): its first insert's caller reads it, so the owner lookup is once.
    pub fn needs_owner(&self) -> bool {
        !self.owned
    }

    /// The owner every later growth of the arrays is charged to (`None`: no one; a unit test's set, a slot with no owner).
    pub fn own(&mut self, owner: Option<(Owner, Kind)>) {
        (self.owner, self.owned) = (owner, true);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn publish(&mut self) {
        let (keys, tags): (&[u64], &[u8]) = match self.keys.is_empty() {
            true => (&EMPTY_KEYS, &EMPTY_TAGS),
            false => (&self.keys, &self.tags),
        };
        *self.header = KeyTable { keys: gravity_abi::word(keys.as_ptr()), tags: gravity_abi::word(tags.as_ptr()), mask: keys.len() as u64 - 1, flags: self.flags };
    }

    fn put(keys: &mut [u64], tags: &mut [u8], key: u64) -> bool {
        let mask = keys.len() as u64 - 1;
        let mut h = slot_of(key, mask);
        // Bounded walk: `insert` grows first, so the set is at most half full and an empty tag ends it.
        for _ in 0..=mask {
            if tags[h as usize] == 0 {
                (keys[h as usize], tags[h as usize]) = (key, 1);
                return true;
            }
            if keys[h as usize] == key {
                return false;
            }
            h = (h + 1) & mask;
        }
        unreachable!("gravity: a hashed SubPlan's key set is at most half full")
    }

    /// The arrays doubled, every key put again; a refused block leaves the set as it was.
    fn grow(&mut self) -> core::result::Result<(), TryReserveError> {
        let n = (self.keys.len() * 2).max(KEYSET_MIN_SLOTS);
        let (o, k) = (self.owner.map(|(o, _)| o), self.owner.map_or(Kind::Distinct, |(_, k)| k));
        let (mut keys, mut tags) = (ChargedVec::<u64>::new(o, k), ChargedVec::<u8>::new(o, k));
        keys.resize(n, 0)?; // alloc: Exec charged self.owner
        tags.resize(n, 0)?; // alloc: Exec charged self.owner
        for (k, t) in self.keys.iter().zip(self.tags.iter()) {
            if *t != 0 {
                KeySet::put(&mut keys, &mut tags, *k);
            }
        }
        (self.keys, self.tags) = (keys, tags);
        Ok(())
    }

    /// One build row's key (`isnull`: a NULL key, counted, never stored); a growth the allocator refuses is the error (the
    /// statement's out of memory), the set unchanged.
    pub fn insert(&mut self, value: u64, isnull: bool) -> core::result::Result<(), TryReserveError> {
        self.flags |= NONEMPTY;
        if isnull {
            self.flags |= HAS_NULL;
            return Ok(());
        }
        if (self.len + 1) * 2 > self.keys.len() {
            self.grow()?;
        }
        if KeySet::put(&mut self.keys, &mut self.tags, value & self.width_mask) {
            self.len += 1;
        }
        Ok(())
    }

    /// The bytes the arrays hold (their capacity).
    pub fn bytes(&self) -> usize {
        self.keys.capacity() * 8 + self.tags.capacity()
    }

    /// The bytes the next growth asks for (both arrays at twice the slots, the 16-slot start first): an out of memory's
    /// request.
    pub fn grow_bytes(&self) -> usize {
        (self.keys.len() * 2).max(KEYSET_MIN_SLOTS) * 9
    }

    /// The word the main program's probe reads (`subhash::word_id`): the header's address, stable while `self` lives; call after the build program's last row.
    pub fn word(&mut self) -> u64 {
        self.publish();
        &*self.header as *const KeyTable as u64
    }
}

impl Sink for KeySet {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        if b.natts != 1 {
            return Err(Error::host("gravity: a hashed SubPlan's build row is not one key column"));
        }
        for (v, n) in b.iter() {
            self.insert(v[0], n[0] != 0).map_err(|_| Error::out_of_memory(self.grow_bytes() as u64))?;
        }
        Ok(Flow::Continue)
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_lower::ops::subplan::keyset::lookup;

    #[test]
    fn keys_nulls_and_growth() {
        let mut s = KeySet::new(4);
        // SAFETY: the header is the set's own, live here.
        assert_eq!(unsafe { lookup(s.word() as *const KeyTable, 5, 4) }, (false, false, false));
        for i in 0..1000u64 {
            s.insert(i * 7 | 0xdead_0000_0000_0000, false).unwrap();
            s.insert(i * 7, false).unwrap();
        }
        assert_eq!(s.len(), 1000);
        let w = s.word() as *const KeyTable;
        // SAFETY: as above.
        unsafe {
            assert_eq!(lookup(w, 14, 4), (true, true, false));
            assert_eq!(lookup(w, 14 | 0xffff_ffff_0000_0000, 4), (true, true, false));
            assert_eq!(lookup(w, 15, 4), (true, false, false));
        }
        s.insert(0, true).unwrap();
        let w = s.word() as *const KeyTable;
        // SAFETY: as above.
        assert_eq!(unsafe { lookup(w, 15, 4) }, (true, false, true));
    }

    #[test]
    fn a_batch_is_its_rows() {
        let mut s = KeySet::new(8);
        let (values, nulls) = ([3u64, 0, 3, u64::MAX], [0u8, 1, 0, 0]);
        s.push(BorrowedBatch { natts: 1, rows: 4, values: &values, isnull: &nulls, region: &[] }).unwrap();
        assert_eq!(s.len(), 2);
        // SAFETY: as above.
        assert_eq!(unsafe { lookup(s.word() as *const KeyTable, u64::MAX, 8) }, (true, true, true));
    }

    #[test]
    fn a_full_table_probe_ends() {
        // No empty tag anywhere: the probe walks each slot once and answers absent.
        let (keys, tags) = ([1u64, 2, 3, 4], [1u8; 4]);
        let t = KeyTable { keys: gravity_abi::word(keys.as_ptr()), tags: gravity_abi::word(tags.as_ptr()), mask: 3, flags: NONEMPTY };
        // SAFETY: `t`'s arrays are `mask + 1` long and live here.
        unsafe {
            assert_eq!(lookup(&t, 9, 8), (true, false, false));
            assert_eq!(lookup(&t, 3, 8), (true, true, false));
        }
    }
}
