//! rowfilter.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs): the words a HASH lay-out
//! opens past its directory, the serial lay-out's bits and the partitioned merge's atomic ones.
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::ops::hashjoin::build::{ALLOW, A_RF, BITS, DIR, RF_BITS};
use crate::ops::hashjoin::rt::{rd, wr, Rt};

/// A build's open words: the lay-out sets each record's bits in them.
pub(crate) struct Words {
    at: *mut u64,
    fb: u32,
}

impl Words {
    /// The serial lay-out (the leader's region is its alone): `h`'s bits set.
    ///
    /// # Safety
    /// The words are this region's, open (`open`).
    pub(crate) unsafe fn set(&self, h: KeyHash) {
        let (at, m) = place(h, self.fb);
        *self.at.add(at) |= m;
    }

    /// A partitioned merge's job (the jobs share the words): `h`'s bits ORed in atomically.
    ///
    /// # Safety
    /// The words are this region's, open (`open` in the merge's prepare, `shared` in the job).
    pub(crate) unsafe fn put(&self, h: KeyHash) {
        let (at, m) = place(h, self.fb);
        (*(self.at.add(at) as *const AtomicU64)).fetch_or(m, Relaxed);
    }

    /// The bytes they take past the directory (`bytes`).
    pub(crate) fn len(&self) -> usize {
        bytes(self.fb)
    }
}

/// The bytes past the directory a HASH lay-out of region `t` lays its words in: 0 unless its probe reads them (`A_RF`)
/// and it holds a key.
///
/// # Safety
/// `t` is a build region.
pub(crate) unsafe fn need(t: &Rt) -> usize {
    match bits_for(t.n as u64) {
        Some(fb) if rd::<u8>(t.d, ALLOW) & A_RF != 0 && t.n > 0 => bytes(fb),
        _ => 0,
    }
}

/// A HASH lay-out of region `t` whose directory ends at byte `end`: the words opened there (the state line and the words
/// zeroed, `RF_BITS` their log2) when its probe reads them, it holds a key and the region has the room; otherwise no
/// words and `RF_BITS` 0 (a filter is a speed choice: no room is never an error).
///
/// # Safety
/// `t` is a build region being laid out (the leader's, or under a partitioned merge's prepare), `end` past its directory.
pub(crate) unsafe fn open(t: &Rt, end: usize) -> Option<Words> {
    let fb = bits_for(t.n as u64).filter(|&fb| need(t) != 0 && t.room(end, bytes(fb)).is_ok());
    wr::<u8>(t.d, RF_BITS, fb.map_or(0, |fb| fb as u8));
    let fb = fb?;
    core::ptr::write_bytes(t.d.add(end), 0, bytes(fb));
    fires::fire_shared("rowfilter.laid"); // shapecheck fire signal (D3194): a build lays the Bloom words its probe tests
    Some(Words { at: t.d.add(end + STATE) as *mut u64, fb })
}

/// The words a partitioned merge's prepare opened (`open`), for its jobs: `None` when it opened none.
///
/// # Safety
/// `t` is the leader's region under a partitioned merge, its directory laid out (`DIR`, `BITS`).
pub(crate) unsafe fn shared(t: &Rt) -> Option<Words> {
    let fb = rd::<u8>(t.d, RF_BITS) as u32;
    let end = rd::<u64>(t.d, DIR) as usize + (8usize << rd::<u32>(t.d, BITS));
    (fb != 0).then(|| Words { at: t.d.add(end + STATE) as *mut u64, fb })
}

gravity_abi::host! {
    /// D3323 (cost.rs `verdict`): a sample's verdict turned its probe's filter off, or armed it again (state `st`), as a
    /// shapecheck fire signal.
    #[guard(0)]
    pub(crate) fn rowfilter_turned(st: u64) -> u64 {
        fires::fire_shared(super::cost::turned(st)); // shapecheck fire signal (D3323): rowfilter.off / rowfilter.rearm
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{shared, Words};
    use crate::ops::hashjoin::build::{ALLOW, A_RF, A_TAGS, END, FLAGS, FORM, F_RANGE, HASH, HDR, HI, KIND, LO, N, PHASE, REC, RF_BITS, TOP, WORDS};
    use crate::ops::hashjoin::rt::{rd, wr, Rt};
    use crate::ops::hashjoin::strategies::rowfilter::{bits_for, place};
    use gravity_abi::hash::KeyHash;

    /// A phase-1 region of one-word keys (16-byte records: the key, the link), `allow` its ALLOW byte, `len` bytes.
    fn region(keys: &[i64], allow: u8, len: usize) -> Vec<u64> {
        let mut buf = vec![0u64; len / 8];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the region, `len` bytes, the records within it.
        unsafe {
            (wr::<u8>(d, 15, KIND), wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 16), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            crate::ops::hashjoin::layout::set_pay(d, 0);
            (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, TOP, len as u64), wr::<u64>(d, END, len as u64));
            (wr::<i64>(d, LO, keys.iter().copied().min().unwrap_or(0)), wr::<i64>(d, HI, keys.iter().copied().max().unwrap_or(0)));
            wr::<u8>(d, RF_BITS, 0xAA); // garbage, as a worker's header carries past the 64 bytes the driver zeroes
            keys.iter().enumerate().for_each(|(i, &k)| wr::<i64>(d, HDR + 16 * i, k));
        }
        buf
    }

    /// Whether region `d`'s laid-out words hold key `k`'s bits.
    unsafe fn holds(d: *mut u8, k: i64) -> bool {
        let t = Rt::of(d, 0);
        let w: Words = shared(&t).expect("words");
        let (at, m) = place(KeyHash::word(k as u64), w.fb);
        *w.at.add(at) & m == m
    }

    /// A HASH lay-out of a build its probe filters lays the words past the directory with every record's bits, and
    /// writes RF_BITS; one whose probe reads none, or without the room, writes RF_BITS 0 over the header's garbage.
    #[test]
    fn a_hash_lay_out_lays_the_words_only_where_its_probe_reads_them() {
        let keys: Vec<i64> = (0..3000).map(|i| i * 7919 % 1_000_003 + 1_000_000_000).collect();
        // room for the directory alone: the records, then the heads a cache line up
        let tight = (HDR + 16 * 3000).next_multiple_of(64) + (8 << crate::ops::hashjoin::rt::dir_bits(3000));
        for (allow, len, want) in [(A_TAGS | A_RF, 1 << 20, true), (A_TAGS, 1 << 20, false), (A_TAGS | A_RF, tight, false)] {
            let mut buf = region(&keys, allow, len);
            let d = buf.as_mut_ptr() as *mut u8;
            // SAFETY: `buf` is a phase-1 build region with every record in.
            unsafe {
                crate::ops::hashjoin::rt::lay_out(d, len).unwrap();
                assert_eq!(rd::<u8>(d, FORM), HASH, "allow {allow}");
                assert_eq!(rd::<u8>(d, RF_BITS) != 0, want, "allow {allow} len {len}");
                if want {
                    assert_eq!(Some(rd::<u8>(d, RF_BITS) as u32), bits_for(3000));
                    assert!(keys.iter().all(|&k| holds(d, k)), "every record's bits");
                }
            }
        }
    }
}
