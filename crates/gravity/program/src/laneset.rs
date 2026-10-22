use crate::arena::{ArenaRef, ArenaVec};

/// A set of lane indexes as bit words in an arena (the program's `driving` / `filter_only` /
/// `answered` lanes): grown to the highest lane inserted, so a program with three lanes holds
/// one word and a program with 300 holds five. `words()` is the form a host keeps (`Vec<u64>`
/// sized at Node build) and the free functions read and write that form.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LaneSet {
    words: ArenaVec<u64>,
}

impl LaneSet {
    pub const fn new_in(a: ArenaRef) -> LaneSet {
        LaneSet { words: ArenaVec::new_in(a) }
    }

    /// The lanes of `mask` (bit `k` = lane `k`): the small sets tests and hand programs write.
    pub fn from_mask_in(a: ArenaRef, mask: u64) -> LaneSet {
        let mut s = LaneSet::new_in(a);
        if mask != 0 {
            s.words.push(mask);
        }
        s
    }

    pub fn from_words_in(a: ArenaRef, words: &[u64]) -> LaneSet {
        let n = words.iter().rposition(|&w| w != 0).map_or(0, |i| i + 1);
        let mut s = LaneSet { words: ArenaVec::with_capacity_in(n, a) };
        s.words.extend_from_slice(&words[..n]);
        s
    }

    pub fn copy_in(&self, a: ArenaRef) -> LaneSet {
        LaneSet::from_words_in(a, &self.words)
    }

    pub fn contains(&self, k: usize) -> bool {
        has(&self.words, k)
    }

    pub fn insert(&mut self, k: usize) {
        let w = k / 64;
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        self.words[w] |= 1 << (k % 64);
    }

    pub fn remove(&mut self, k: usize) {
        if let Some(w) = self.words.get_mut(k / 64) {
            *w &= !(1 << (k % 64));
        }
        self.trim();
    }

    pub fn clear(&mut self) {
        self.words.clear();
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    pub fn count(&self) -> usize {
        count(&self.words)
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        iter(&self.words)
    }

    /// `mask` names the same lanes (bit `k` = lane `k`; lanes past 64 make it false).
    pub fn eq_mask(&self, mask: u64) -> bool {
        self.words.first().copied().unwrap_or(0) == mask && self.words.iter().skip(1).all(|&w| w == 0)
    }

    fn trim(&mut self) {
        while self.words.last() == Some(&0) {
            self.words.pop();
        }
    }
}

pub fn has(words: &[u64], k: usize) -> bool {
    words.get(k / 64).is_some_and(|w| w >> (k % 64) & 1 != 0)
}

/// Set lane `k` in a word slice sized for it (`words_for(nlanes)` words).
pub fn set(words: &mut [u64], k: usize) {
    words[k / 64] |= 1 << (k % 64);
}

pub fn unset(words: &mut [u64], k: usize) {
    words[k / 64] &= !(1 << (k % 64));
}

pub const fn words_for(nlanes: usize) -> usize {
    nlanes.div_ceil(64)
}

pub fn count(words: &[u64]) -> usize {
    words.iter().map(|w| w.count_ones() as usize).sum()
}

pub fn any(words: &[u64]) -> bool {
    words.iter().any(|&w| w != 0)
}

pub fn iter(words: &[u64]) -> impl Iterator<Item = usize> + '_ {
    words.iter().enumerate().flat_map(|(i, &w)| {
        let mut m = w;
        core::iter::from_fn(move || {
            if m == 0 {
                return None;
            }
            let b = m.trailing_zeros() as usize;
            m &= m - 1;
            Some(i * 64 + b)
        })
    })
}

/// `dst = words(src)` over `dst`'s length (a host copying a program set at bind).
pub fn copy_into(dst: &mut [u64], src: &[u64]) {
    for (i, d) in dst.iter_mut().enumerate() {
        *d = src.get(i).copied().unwrap_or(0);
    }
}

/// Every lane below `nlanes` is in `words`.
pub fn is_all(words: &[u64], nlanes: usize) -> bool {
    (0..nlanes).all(|k| has(words, k))
}
