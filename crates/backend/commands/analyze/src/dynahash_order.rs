//! dynahash.c's bucket layout for the Lossy Counting tables: hash_seq_search
//! order is the qsort input C truncates by frequency, and it is a
//! linear-hashing artifact (next-pow2 initial buckets, one split per
//! HASH_ENTER once nentries > max_bucket, chains appended at the tail).

use mcx::{Mcx, PgVec};
use types_error::PgResult;

pub(crate) struct DynaHash<'m> {
    mcx: Mcx<'m>,
    chains: PgVec<'m, PgVec<'m, (u32, u32)>>,
    max_bucket: u32,
    low_mask: u32,
    high_mask: u32,
    nentries: usize,
}

impl<'m> DynaHash<'m> {
    // hash_create(nelem) -> init_htab: nbuckets = next_pow2_int(nelem).
    pub(crate) fn new(mcx: Mcx<'m>, nelem: i64) -> PgResult<Self> {
        let nbuckets = next_pow2_int(nelem);
        let mut chains = PgVec::with_capacity_in(nbuckets as usize, mcx);
        for _ in 0..nbuckets {
            chains.push(PgVec::new_in(mcx));
        }
        Ok(DynaHash {
            mcx,
            chains,
            max_bucket: nbuckets - 1,
            low_mask: nbuckets - 1,
            high_mask: (nbuckets << 1) - 1,
            nentries: 0,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.nentries
    }

    fn calc_bucket(&self, hash: u32) -> usize {
        let mut b = hash & self.high_mask;
        if b > self.max_bucket {
            b &= self.low_mask;
        }
        b as usize
    }

    // hash_search(HASH_ENTER): the split check (dynahash.c:1001) precedes the
    // chain walk, found or not. Returns the (hash, id) chain to match against.
    pub(crate) fn enter(&mut self, hash: u32) -> PgResult<&[(u32, u32)]> {
        if self.nentries > self.max_bucket as usize {
            self.expand_table()?;
        }
        let b = self.calc_bucket(hash);
        Ok(&self.chains[b])
    }

    pub(crate) fn insert(&mut self, hash: u32, id: u32) {
        let b = self.calc_bucket(hash);
        self.chains[b].push((hash, id));
        self.nentries += 1;
    }

    // hash_seq_search over the table with HASH_REMOVE of every entry `keep`
    // rejects (prune_*_hashtable); chain order is preserved.
    pub(crate) fn prune(&mut self, mut keep: impl FnMut(u32) -> bool) {
        let mut removed = 0usize;
        for chain in self.chains.iter_mut() {
            chain.retain(|&(_, id)| {
                let k = keep(id);
                if !k {
                    removed += 1;
                }
                k
            });
        }
        self.nentries -= removed;
    }

    pub(crate) fn seq(&self) -> impl Iterator<Item = u32> + '_ {
        self.chains.iter().flat_map(|c| c.iter().map(|&(_, id)| id))
    }

    fn expand_table(&mut self) -> PgResult<()> {
        let new_bucket = self.max_bucket + 1;
        self.max_bucket = new_bucket;
        let old_bucket = (new_bucket & self.low_mask) as usize;
        if new_bucket > self.high_mask {
            self.low_mask = self.high_mask;
            self.high_mask = new_bucket | self.low_mask;
        }
        let old = core::mem::replace(&mut self.chains[old_bucket], PgVec::new_in(self.mcx));
        let mut new_chain: PgVec<'m, (u32, u32)> = PgVec::new_in(self.mcx);
        for &(hash, id) in old.iter() {
            if self.calc_bucket(hash) == old_bucket {
                self.chains[old_bucket].push((hash, id));
            } else {
                new_chain.push((hash, id));
            }
        }
        self.chains.push(new_chain);
        Ok(())
    }
}

fn next_pow2_int(num: i64) -> u32 {
    let num = num.min(i32::MAX as i64 / 2);
    let mut limit = 1u32;
    while (limit as i64) < num {
        limit <<= 1;
    }
    limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcx::MemoryContext;

    fn order(t: &DynaHash<'_>) -> Vec<u32> {
        t.seq().collect()
    }

    #[test]
    fn initial_layout_is_next_pow2_and_chains_append_at_the_tail() {
        let cx = MemoryContext::new("dynahash");
        let mut t = DynaHash::new(cx.mcx(), 10).unwrap();
        assert_eq!((t.max_bucket, t.low_mask, t.high_mask), (15, 15, 31));
        // Same bucket (hash & 31 = 3 for both): chain order is insertion order.
        t.enter(3).unwrap();
        t.insert(3, 0);
        t.enter(19).unwrap();
        t.insert(19, 1);
        // Bucket 19 > max_bucket folds through low_mask to bucket 3.
        assert_eq!(order(&t), vec![0, 1]);
        // Bucket 1 precedes bucket 3 in a sequential scan.
        t.enter(1).unwrap();
        t.insert(1, 2);
        assert_eq!(order(&t), vec![2, 0, 1]);
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn splits_happen_one_bucket_per_enter_once_nentries_exceeds_max_bucket() {
        let cx = MemoryContext::new("dynahash");
        let mut t = DynaHash::new(cx.mcx(), 2).unwrap();
        assert_eq!((t.max_bucket, t.low_mask, t.high_mask), (1, 1, 3));
        for (i, h) in [0u32, 2, 1, 3].iter().enumerate() {
            t.enter(*h).unwrap();
            t.insert(*h, i as u32);
        }
        // ENTER #3 (nentries 2 > max_bucket 1) split bucket 0 into 0/2 by the
        // second hash bit (h=2 moved), ENTER #4 split bucket 1 into 1/3.
        assert_eq!(t.max_bucket, 3);
        assert_eq!(order(&t), vec![0, 2, 1, 3]);
        // ENTER #5: new bucket 4 passes high_mask 3 -> low 3, high 7; h=5
        // folds through low_mask 3 to bucket 1, appended after id 2.
        t.enter(5).unwrap();
        assert_eq!((t.max_bucket, t.low_mask, t.high_mask), (4, 3, 7));
        t.insert(5, 4);
        assert_eq!(order(&t), vec![0, 2, 4, 1, 3]);
        // Removal keeps the surviving chain order.
        t.prune(|id| id != 3);
        assert_eq!(order(&t), vec![0, 2, 4, 1]);
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn masks_double_once_the_new_bucket_passes_high_mask() {
        let cx = MemoryContext::new("dynahash");
        let mut t = DynaHash::new(cx.mcx(), 1).unwrap();
        assert_eq!((t.max_bucket, t.low_mask, t.high_mask), (0, 0, 1));
        t.enter(0).unwrap();
        t.insert(0, 0);
        // nentries 1 > max_bucket 0: new_bucket 1 <= high_mask 1, masks stay.
        t.enter(1).unwrap();
        assert_eq!((t.max_bucket, t.low_mask, t.high_mask), (1, 0, 1));
        t.insert(1, 1);
        // nentries 2 > 1: new_bucket 2 > high_mask 1 -> low 1, high 3.
        t.enter(2).unwrap();
        assert_eq!((t.max_bucket, t.low_mask, t.high_mask), (2, 1, 3));
        t.insert(2, 2);
        assert_eq!(order(&t), vec![0, 1, 2]);
    }
}
