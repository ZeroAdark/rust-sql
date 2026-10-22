//! A lookup memo for a handful of keys: the first [`MEMO_INLINE`] entries live in an inline
//! array scanned linearly (no allocation, no hashing), later ones move with them into an
//! FxHashMap allocated in the context. Keys and values are `Copy`, so nothing needs dropping.

use core::hash::Hash;
use core::mem::MaybeUninit;

use crate::{Mcx, PgFxHashMap};

pub const MEMO_INLINE: usize = 8;

pub struct PgMemoMap<'mcx, K: Copy, V: Copy> {
    len: usize,
    inline: [MaybeUninit<(K, V)>; MEMO_INLINE],
    map: Option<PgFxHashMap<'mcx, K, V>>,
    mcx: Mcx<'mcx>,
}

impl<'mcx, K: Copy + Eq + Hash, V: Copy> PgMemoMap<'mcx, K, V> {
    #[inline]
    pub fn new_in(mcx: Mcx<'mcx>) -> Self {
        PgMemoMap { len: 0, inline: [MaybeUninit::uninit(); MEMO_INLINE], map: None, mcx }
    }

    #[inline]
    fn entries(&self) -> &[MaybeUninit<(K, V)>] {
        &self.inline[..self.len]
    }

    #[inline]
    pub fn get(&self, key: &K) -> Option<&V> {
        if let Some(map) = &self.map {
            return map.get(key);
        }
        self.entries()
            .iter()
            // SAFETY: the first `len` slots are written by `insert`.
            .map(|e| unsafe { e.assume_init_ref() })
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn insert(&mut self, key: K, value: V) {
        if let Some(map) = &mut self.map {
            map.insert(key, value);
            return;
        }
        for e in &mut self.inline[..self.len] {
            // SAFETY: the first `len` slots are written by `insert`.
            let e = unsafe { e.assume_init_mut() };
            if e.0 == key {
                e.1 = value;
                return;
            }
        }
        if self.len < MEMO_INLINE {
            self.inline[self.len].write((key, value));
            self.len += 1;
            return;
        }
        self.spill(key, value);
    }

    #[cold]
    #[inline(never)]
    fn spill(&mut self, key: K, value: V) {
        let mut map =
            PgFxHashMap::with_capacity_and_hasher_in(2 * MEMO_INLINE, Default::default(), self.mcx);
        for e in self.entries() {
            // SAFETY: the first `len` slots are written by `insert`; (K, V) is Copy.
            let (k, v) = unsafe { e.assume_init_read() };
            map.insert(k, v);
        }
        map.insert(key, value);
        self.len = 0;
        self.map = Some(map);
    }

    pub fn len(&self) -> usize {
        self.map.as_ref().map_or(self.len, |m| m.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// SAFETY: the inline entries are Copy (no drop glue); the spilled map is an arena collection of
// ForgetSafe keys and values.
unsafe impl<'mcx, K: Copy + crate::ForgetSafe, V: Copy + crate::ForgetSafe> crate::ForgetSafe
    for PgMemoMap<'mcx, K, V>
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryContext;

    #[test]
    fn memo_map_inline_then_spilled_keeps_every_key() {
        let ctx = MemoryContext::new("memo-map-test");
        let mut m: PgMemoMap<'_, (u32, i16), u64> = PgMemoMap::new_in(ctx.mcx());
        assert!(m.is_empty() && m.get(&(1, 1)).is_none());
        for i in 0..MEMO_INLINE as u32 {
            m.insert((i, -1), u64::from(i) * 10);
        }
        assert!(m.map.is_none());
        assert_eq!(m.len(), MEMO_INLINE);
        m.insert((3, -1), 33);
        assert_eq!((m.len(), m.get(&(3, -1))), (MEMO_INLINE, Some(&33)));
        for i in MEMO_INLINE as u32..40 {
            m.insert((i, -1), u64::from(i) * 10);
        }
        assert!(m.map.is_some());
        assert_eq!(m.len(), 40);
        for i in 0..40u32 {
            let want = if i == 3 { 33 } else { u64::from(i) * 10 };
            assert_eq!(m.get(&(i, -1)), Some(&want));
        }
        assert!(m.get(&(0, 0)).is_none() && m.get(&(40, -1)).is_none());
        m.insert((7, -1), 1);
        assert_eq!((m.len(), m.get(&(7, -1))), (40, Some(&1)));
    }
}
