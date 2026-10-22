//! A host's lane set: `gravity_program::laneset`'s words in a `Vec` sized once at Node build.
use gravity_program::laneset;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaneBits {
    words: Vec<u64>,
}

impl LaneBits {
    pub const fn empty() -> LaneBits {
        LaneBits { words: Vec::new() } // alloc: Node
    }

    /// A set over `nlanes` lanes, none in it.
    pub fn new(nlanes: usize) -> LaneBits {
        LaneBits { words: vec![0; laneset::words_for(nlanes)] } // alloc: Node
    }

    pub fn from_words(nlanes: usize, words: &[u64]) -> LaneBits {
        let mut b = LaneBits::new(nlanes);
        laneset::copy_into(&mut b.words, words);
        b
    }

    /// Size the set for `nlanes` lanes (a source's bind), keeping nothing.
    pub fn resize(&mut self, nlanes: usize) {
        self.words.clear();
        self.words.resize(laneset::words_for(nlanes), 0); // alloc: Node
    }

    #[inline(always)]
    pub fn has(&self, k: usize) -> bool {
        laneset::has(&self.words, k)
    }

    #[inline(always)]
    pub fn set(&mut self, k: usize) {
        laneset::set(&mut self.words, k);
    }

    pub fn unset(&mut self, k: usize) {
        laneset::unset(&mut self.words, k);
    }

    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// The heap block the words take (its capacity).
    pub fn heap_bytes(&self) -> usize {
        self.words.capacity() * core::mem::size_of::<u64>()
    }

    pub fn assign(&mut self, o: &LaneBits) {
        laneset::copy_into(&mut self.words, &o.words);
    }

    pub fn or(&mut self, o: &LaneBits) {
        // bounded("key-columns: the words of one lane set, one bit per scanned column: at most MaxTupleAttributeNumber (1664) columns")
        for (d, s) in self.words.iter_mut().zip(&o.words) {
            *d |= *s;
        }
    }

    pub fn and(&mut self, o: &LaneBits) {
        // bounded("key-columns: the words of one lane set, one bit per scanned column: at most MaxTupleAttributeNumber (1664) columns")
        for (i, d) in self.words.iter_mut().enumerate() {
            *d &= o.words.get(i).copied().unwrap_or(0);
        }
    }

    pub fn and_not(&mut self, o: &LaneBits) {
        // bounded("key-columns: the words of one lane set, one bit per scanned column: at most MaxTupleAttributeNumber (1664) columns")
        for (d, s) in self.words.iter_mut().zip(&o.words) {
            *d &= !*s;
        }
    }

    pub fn any(&self) -> bool {
        laneset::any(&self.words)
    }

    pub fn count(&self) -> usize {
        laneset::count(&self.words)
    }

    /// Every lane below `nlanes` is in `self` or `o`.
    pub fn union_covers(&self, o: &LaneBits, nlanes: usize) -> bool {
        (0..nlanes).all(|k| self.has(k) || o.has(k))
    }
}
