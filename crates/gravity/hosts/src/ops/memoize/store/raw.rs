//! `nest.memo`'s store, its raw half (lint R6; construct bundle memoize-04): the table's probe and row reads through the
//! addresses the IR lays (`lookup`, `store` and their helpers), moved as is from ops/memoize/store.rs with their safety
//! contracts. A child module of store.rs, so it reads the table's own fields with no visibility change.
#![allow(unsafe_code)]
use datum::Datum;
use types_error::PgResult;

use super::{key_bytes, murmur32, Col, Cold, Hot, Lookup, MemoTable, Probe, Stored, COMPLETE, ENTRY_BYTES, LIVE, MINTUPLE_HEADER, NIL, TUPLE_BYTES, WORD1};

impl Stored {
    fn bytes(&self) -> &[u8] {
        match &self.image {
            // SAFETY: `image` holds at least `len` bytes (`copy_image`).
            Some(b) => unsafe { core::slice::from_raw_parts(b.as_ptr().cast::<u8>(), self.len) },
            None => &[],
        }
    }
}

impl MemoTable {
    /// PG's `MemoizeHash_hash`: each key's hash rotated in, a NULL as 0, then `murmurhash32`.
    ///
    /// # Safety
    /// A non-NULL by-reference probe's `word` points at `len` readable bytes.
    unsafe fn hash(&mut self, probe: &[Probe]) -> PgResult<u32> {
        let mut h: u32 = 0;
        // bounded("other: one hash per cache key of a probe, the Memoize node's param_exprs: the query text bounds them, as PG's MemoizeHash_hash walks the same keys with no poll")
        for (i, p) in probe.iter().enumerate() {
            h = h.rotate_left(1);
            if p.null {
                continue;
            }
            let k = if self.binary {
                image_hash(p)
            } else if p.len == 0 && self.fns[i].word_eq {
                // D3075: a word-equality key hashes its word (`KeyFns::word_eq`), no call
                (p.word ^ (p.word >> 32)) as u32
            } else {
                let f = &mut self.fns[i].hash;
                let r = f.call_site(&[Datum::from_u64(p.word)], &[false]);
                f.reset_window();
                r?.value.as_u32()
            };
            h ^= k;
        }
        Ok(murmur32(h))
    }

    /// PG's `MemoizeHash_equal`: stored key `s` against the probe.
    ///
    /// # Safety
    /// As `hash`.
    unsafe fn equal(&mut self, s: usize, probe: &[Probe]) -> PgResult<bool> {
        let binary = self.binary;
        let e = self.hot[s];
        // D2988: a one-word key under binary mode or a word-equality operator compares beside the chain (`Hot::word`),
        // never through `Cold::key` nor the equality call
        if e.flags & WORD1 != 0 && (binary || self.fns.first().is_some_and(|f| f.word_eq)) {
            if let [p] = probe {
                return Ok(!p.null && p.len == 0 && p.word == e.word);
            }
        }
        let (cold, fns) = (&self.cold, &mut self.fns);
        // bounded("other: one compare per cache key of a probe, the Memoize node's param_exprs: the query text bounds them, as PG's MemoizeHash_equal walks the same keys with no poll")
        for (i, (k, p)) in cold[s].key.iter().zip(probe.iter()).enumerate() {
            if k.null || p.null {
                // NOT DISTINCT: both NULL are equal, one NULL is not
                if k.null != p.null {
                    return Ok(false);
                }
                continue;
            }
            let same = if binary {
                match (&k.image, p.len) {
                    (None, 0) => k.word == p.word,
                    (Some(_), n) if n as usize == k.len => k.bytes() == core::slice::from_raw_parts(p.word as usize as *const u8, n as usize),
                    _ => false,
                }
            } else if fns[i].word_eq && k.image.is_none() && p.len == 0 {
                // D3075: a word-equality column of a many-column key, no call
                k.word == p.word
            } else {
                let f = &mut fns[i].eq;
                let r = f.call_site(&[k.datum(), Datum::from_u64(p.word)], &[false, false]);
                f.reset_window();
                let r = r?;
                !r.isnull && r.value.as_u64() & 0xff != 0
            };
            if !same {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The entry whose key equals `probe` under hash `h`, if any.
    unsafe fn find(&mut self, h: u32, probe: &[Probe]) -> PgResult<Option<usize>> {
        if self.buckets.is_empty() {
            return Ok(None);
        }
        // the bucket's chain walked one candidate at a time, a candidate of another hash passed over unread (`equal` takes
        // the table for its function calls, which never change the chains)
        let mut s = self.buckets[self.bucket(h)];
        // bounded("other: one bucket's chain: the buckets are as many as the live entries or more (`chain_in`), so a chain is a few entries of a mixed hash; PG's simplehash probe polls no deeper")
        while s != NIL {
            let i = s as usize;
            if self.hot[i].hash == h && self.equal(i, probe)? {
                return Ok(Some(i));
            }
            s = self.hot[i].chain;
        }
        Ok(None)
    }

    /// MEMO_CACHE_LOOKUP (`cache_lookup` and ExecMemoize's first state).
    ///
    /// # Safety
    /// As `hash`.
    pub unsafe fn lookup(&mut self, probe: &[Probe]) -> PgResult<Lookup> {
        self.current = None;
        let h = self.hash(probe)?;
        if let Some(s) = self.find(h, probe)? {
            self.touch(s);
            if self.hot[s].flags & COMPLETE != 0 {
                self.stats.hits += 1;
                return Ok(Lookup::Hit(self.hot[s].rows as usize as *const u64));
            }
            // a scan of this key that did not run to completion: its rows are dropped and the inner runs again
            self.stats.misses += 1;
            let c = &mut self.cold[s];
            let freed = c.mem - (ENTRY_BYTES + key_bytes(&c.key));
            c.mem -= freed;
            c.rows.truncate(2);
            c.rows[0] = 0;
            let gone = c.copies.iter().map(|b| b.len() as u64 * 8).sum::<u64>();
            c.copies.clear();
            c.heap -= gone;
            self.entries_heap -= gone;
            self.mem_used -= freed;
            self.current = Some(s);
            self.sync(super::SYNC_SLACK);
            return Ok(Lookup::Miss { current: true });
        }
        self.stats.misses += 1;
        let key: Vec<Stored> = probe.iter().map(|p| store_key(p)).collect(); // alloc: Exec charged self.charge
        let mem = ENTRY_BYTES + key_bytes(&key);
        let images = key.iter().map(|k| k.image.as_ref().map_or(0, |b| b.len() as u64 * 8)).sum::<u64>();
        let heap = (key.capacity() * core::mem::size_of::<Stored>()) as u64 + images;
        let (word, flags) = match key.as_slice() {
            [k] if !k.null && k.image.is_none() => (k.word, LIVE | WORD1),
            _ => (0, LIVE),
        };
        let s = match self.free.pop() {
            Some(s) => s as usize,
            None => {
                self.hot.push(Hot::FREE); // alloc: Exec charged self.charge
                self.cold.push(Cold::default()); // alloc: Exec charged self.charge
                self.hot.len() - 1
            }
        };
        self.cold[s] = Cold { key, rows: vec![0, 0], copies: Vec::new(), mem, heap }; // alloc: Exec charged self.charge
        let heap = heap + self.cold[s].rows.capacity() as u64 * 8;
        (self.cold[s].heap, self.entries_heap) = (heap, self.entries_heap + heap);
        let rows = self.cold[s].rows.as_ptr() as u64;
        self.hot[s] = Hot { hash: h, prev: NIL, next: NIL, chain: NIL, word, rows, flags };
        self.chain_in(s);
        self.push_tail(s);
        self.mem_used += mem;
        if self.mem_used > self.limit && !self.reduce(s) {
            self.sync(super::SYNC_SLACK);
            return Ok(Lookup::Miss { current: false });
        }
        self.current = Some(s);
        self.sync(super::SYNC_SLACK);
        Ok(Lookup::Miss { current: true })
    }

    /// MEMO_FILLING_CACHE's `cache_store_tuple`: `row` appended to the current entry. `false` once the current entry is
    /// gone (none made, or evicted to make room): MEMO_CACHE_BYPASS_MODE for the rest of the scan.
    ///
    /// # Safety
    /// A non-NULL by-reference column's `word` points at `len` readable bytes.
    pub unsafe fn store(&mut self, row: &[Col]) -> PgResult<bool> {
        let Some(s) = self.current else {
            self.stats.overflows += 1;
            return Ok(false);
        };
        let singlerow = self.singlerow;
        if self.hot[s].flags & COMPLETE != 0 {
            // PG: an elog(ERROR) when the planner's singlerow was wrong
            return Err(Box::new(types_error::PgError::error("cache entry already complete").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))); // alloc: Cold
        }
        let e = &mut self.cold[s];
        let mut bytes = TUPLE_BYTES + MINTUPLE_HEADER;
        let (cap0, mut copied) = ((e.rows.capacity() * 8 + e.copies.capacity() * entry(&e.copies)) as u64, 0u64);
        // bounded("key-columns: one cell per column of the inner's row; PG polls no deeper (a datum copy is one unit there too), at most MaxTupleAttributeNumber (1664) columns")
        for c in row.iter() {
            let (w, n) = match (c.null, c.len) {
                (true, _) => (0, 1),
                (false, 0) => {
                    bytes += 8;
                    (c.word, 0)
                }
                (false, len) => {
                    let b = copy_image(c.word, len as usize);
                    let at = b.as_ptr() as u64;
                    copied += b.len() as u64 * 8;
                    e.copies.push(b); // alloc: Exec charged self.charge
                    bytes += len.next_multiple_of(8);
                    (at, 0)
                }
            };
            e.rows.push(w); // alloc: Exec charged self.charge
            e.rows.push(n); // alloc: Exec charged self.charge
        }
        e.rows[0] += 1;
        e.rows[1] = row.len() as u64;
        e.mem += bytes;
        let grew = (e.rows.capacity() * 8 + e.copies.capacity() * entry(&e.copies)) as u64 + copied - cap0;
        e.heap += grew;
        self.entries_heap += grew;
        let rows = e.rows.as_ptr() as u64;
        self.hot[s].rows = rows;
        if singlerow {
            self.hot[s].flags |= COMPLETE;
        }
        self.mem_used += bytes;
        if self.mem_used > self.limit && !self.reduce(s) {
            self.current = None;
            self.stats.overflows += 1;
            self.sync(super::SYNC_SLACK);
            return Ok(false);
        }
        self.sync(super::SYNC_SLACK);
        Ok(true)
    }
}

/// The probe key kept by the entry (a by-reference image copied).
///
/// # Safety
/// As `MemoTable::hash`.
unsafe fn store_key(p: &Probe) -> Stored {
    match (p.null, p.len) {
        (true, _) => Stored { null: true, word: 0, image: None, len: 0 },
        (false, 0) => Stored { null: false, word: p.word, image: None, len: 0 },
        (false, n) => Stored { null: false, word: 0, image: Some(copy_image(p.word, n as usize)), len: n as usize },
    }
}

/// An entry's size, read off the list's own element type.
fn entry<'a, T: 'a>(_: impl IntoIterator<Item = &'a T>) -> usize {
    core::mem::size_of::<T>()
}

/// `len` bytes at `at`, copied into 8-aligned words (never moved while the copy lives).
///
/// # Safety
/// `at` points at `len` readable bytes.
unsafe fn copy_image(at: u64, len: usize) -> Box<[u64]> {
    let mut b = vec![0u64; len.div_ceil(8).max(1)].into_boxed_slice(); // alloc: Exec charged the table's charge
    if len > 0 {
        core::ptr::copy_nonoverlapping(at as usize as *const u8, b.as_mut_ptr().cast::<u8>(), len);
    }
    b
}

/// A by-value word's or a by-reference image's hash under `binary_mode` (PG's `datum_image_hash`'s role: equal images
/// hash equal).
///
/// # Safety
/// As `MemoTable::hash`.
unsafe fn image_hash(p: &Probe) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    };
    if p.len == 0 {
        p.word.to_le_bytes().into_iter().for_each(&mut mix);
    } else {
        core::slice::from_raw_parts(p.word as usize as *const u8, p.len as usize).iter().for_each(|&b| mix(b));
    }
    (h ^ (h >> 32)) as u32
}
