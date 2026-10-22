
use crate::selection::Selection;
use crate::validity::{words_for, Validity};

#[derive(Clone, Debug, PartialEq, Eq)]
enum MaskRep {
    ConstTrue,
    ConstFalse,
    Words { words: Vec<u64>, popcount: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    n: u32,
    rep: MaskRep,
}

impl Mask {
    pub fn const_true(n: u32) -> Mask {
        Mask { n, rep: MaskRep::ConstTrue }
    }

    pub fn const_false(n: u32) -> Mask {
        Mask { n, rep: MaskRep::ConstFalse }
    }

    pub fn from_words(n: u32, mut words: Vec<u64>) -> Mask {
        let nwords = words_for(n as usize);
        assert!(words.len() >= nwords, "mask words must cover n slots");
        words.truncate(nwords);
        let tail = (n as usize) % 64;
        if tail != 0 {
            if let Some(w) = words.last_mut() {
                *w &= (1u64 << tail) - 1;
            }
        }
        let popcount: u32 = words.iter().map(|w| w.count_ones()).sum();
        if popcount == n {
            Mask::const_true(n)
        } else if popcount == 0 {
            Mask::const_false(n)
        } else {
            Mask { n, rep: MaskRep::Words { words, popcount } }
        }
    }

    pub fn from_fn(n: u32, mut f: impl FnMut(u32) -> bool) -> Mask {
        let mut words = vec![0u64; words_for(n as usize)];
        for i in 0..n {
            if f(i) {
                words[(i / 64) as usize] |= 1u64 << (i % 64);
            }
        }
        Mask::from_words(n, words)
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.n
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    #[inline]
    pub fn popcount(&self) -> u32 {
        match &self.rep {
            MaskRep::ConstTrue => self.n,
            MaskRep::ConstFalse => 0,
            MaskRep::Words { popcount, .. } => *popcount,
        }
    }

    #[inline]
    pub fn all_true(&self) -> bool {
        self.popcount() == self.n
    }

    #[inline]
    pub fn all_false(&self) -> bool {
        self.popcount() == 0
    }

    #[inline]
    pub fn is_const_true(&self) -> bool {
        matches!(self.rep, MaskRep::ConstTrue)
    }

    #[inline]
    pub fn is_const_false(&self) -> bool {
        matches!(self.rep, MaskRep::ConstFalse)
    }

    #[inline]
    pub fn get(&self, i: u32) -> bool {
        debug_assert!(i < self.n, "mask slot out of range");
        match &self.rep {
            MaskRep::ConstTrue => true,
            MaskRep::ConstFalse => false,
            MaskRep::Words { words, .. } => (words[(i / 64) as usize] >> (i % 64)) & 1 != 0,
        }
    }

    pub fn and_with(&mut self, other: &Mask) {
        assert_eq!(self.n, other.n, "mask AND over disagreeing slot counts");
        match (&mut self.rep, &other.rep) {
            (_, MaskRep::ConstTrue) => {}
            (MaskRep::ConstFalse, _) => {}
            (_, MaskRep::ConstFalse) => self.rep = MaskRep::ConstFalse,
            (MaskRep::ConstTrue, MaskRep::Words { .. }) => {
                self.rep = other.rep.clone();
            }
            (MaskRep::Words { words, popcount }, MaskRep::Words { words: ow, .. }) => {
                let mut pc = 0u32;
                for (w, o) in words.iter_mut().zip(ow.iter()) {
                    *w &= *o;
                    pc += w.count_ones();
                }
                *popcount = pc;
                if pc == 0 {
                    self.rep = MaskRep::ConstFalse;
                } else if pc == self.n {
                    self.rep = MaskRep::ConstTrue;
                }
            }
        }
    }

    pub fn to_selection(&self, out: &mut Selection) {
        match &self.rep {
            MaskRep::ConstTrue => out.fill_identity(self.n),
            MaskRep::ConstFalse => out.clear(),
            MaskRep::Words { words, .. } => {
                out.clear();
                for (i, &w) in words.iter().enumerate() {
                    out.push_mask_word(i, w);
                }
            }
        }
    }

    pub fn from_validity(n: u32, validity: &Validity) -> Mask {
        let nwords = words_for(n as usize);
        debug_assert!(validity.words.len() >= nwords);
        Mask::from_words(n, validity.words[..nwords].to_vec())
    }

    pub fn from_selection(n: u32, sel: &Selection) -> Mask {
        if sel.positions.len() == n as usize {
            return Mask::const_true(n);
        }
        let mut words = vec![0u64; words_for(n as usize)];
        for &p in &sel.positions {
            debug_assert!(p < n, "selection position outside the mask");
            words[(p / 64) as usize] |= 1u64 << (p % 64);
        }
        Mask::from_words(n, words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn const_reps_carry_popcount_without_words() {
        let t = Mask::const_true(130);
        let f = Mask::const_false(130);
        assert_eq!(t.popcount(), 130);
        assert_eq!(f.popcount(), 0);
        assert!(t.all_true() && t.is_const_true() && !t.all_false());
        assert!(f.all_false() && f.is_const_false() && !f.all_true());
        assert!(t.get(0) && t.get(129));
        assert!(!f.get(0) && !f.get(129));
    }

    #[test]
    fn from_words_normalizes_and_masks_tail() {
        let all = vec![!0u64; 2];
        let m = Mask::from_words(70, all.clone());
        assert!(m.is_const_true(), "all-set normalizes to const-true");
        assert_eq!(m.popcount(), 70);

        let none = Mask::from_words(70, vec![0, !0u64 << 6]); // only tail bits
        assert!(none.is_const_false(), "tail-only bits normalize to const-false");

        let mixed = Mask::from_words(70, vec![0b1010, 0]);
        assert!(!mixed.is_const_true() && !mixed.is_const_false());
        assert_eq!(mixed.popcount(), 2);
        assert!(mixed.get(1) && mixed.get(3) && !mixed.get(0) && !mixed.get(69));
    }

    #[test]
    fn from_fn_matches_per_slot_reads() {
        let m = Mask::from_fn(200, |i| i % 3 == 0);
        assert_eq!(m.popcount(), 67);
        for i in 0..200 {
            assert_eq!(m.get(i), i % 3 == 0, "slot {i}");
        }
    }

    #[test]
    fn and_composition_short_circuits_and_renormalizes() {
        let mixed = Mask::from_fn(100, |i| i < 50);
        let other = Mask::from_fn(100, |i| i >= 25);

        let mut a = mixed.clone();
        a.and_with(&Mask::const_true(100));
        assert_eq!(a, mixed);

        let mut b = mixed.clone();
        b.and_with(&Mask::const_false(100));
        assert!(b.is_const_false());
        let mut c = Mask::const_false(100);
        c.and_with(&mixed);
        assert!(c.is_const_false());

        let mut d = Mask::const_true(100);
        d.and_with(&mixed);
        assert_eq!(d, mixed);

        let mut e = mixed.clone();
        e.and_with(&other);
        assert_eq!(e.popcount(), 25);
        for i in 0..100 {
            assert_eq!(e.get(i), (25..50).contains(&i));
        }
        let mut g = Mask::from_fn(100, |i| i < 25);
        g.and_with(&Mask::from_fn(100, |i| i >= 75));
        assert!(g.is_const_false());
    }

    #[test]
    fn to_selection_matches_bits_and_identity_fast_path() {
        let mut sel = Selection::new();
        Mask::const_true(5).to_selection(&mut sel);
        assert_eq!(sel.as_slice(), &[0, 1, 2, 3, 4]);

        Mask::const_false(5).to_selection(&mut sel);
        assert!(sel.is_empty());

        let m = Mask::from_fn(130, |i| i % 7 == 0);
        m.to_selection(&mut sel);
        let want: Vec<u32> = (0..130).filter(|i| i % 7 == 0).collect();
        assert_eq!(sel.as_slice(), want.as_slice());
        assert_eq!(sel.len() as u32, m.popcount());
    }

    #[test]
    fn from_selection_roundtrips_and_normalizes() {
        let mut sel = Selection::new();
        sel.positions.extend(0..70u32);
        assert!(Mask::from_selection(70, &sel).is_const_true());

        sel.positions.clear();
        assert!(Mask::from_selection(70, &sel).is_const_false());

        let m = Mask::from_fn(130, |i| i % 5 == 2);
        let mut lowered = Selection::new();
        m.to_selection(&mut lowered);
        let back = Mask::from_selection(130, &lowered);
        assert_eq!(back, m);
        assert_eq!(back.popcount(), m.popcount());
    }

    #[test]
    fn from_validity_strips_nulls() {
        let mut v = Validity::new();
        v.reset_all_valid(66);
        v.set_null(3);
        v.set_null(65);
        let m = Mask::from_validity(66, &v);
        assert_eq!(m.popcount(), 64);
        assert!(!m.get(3) && !m.get(65) && m.get(0) && m.get(64));

        v.reset_all_valid(66);
        assert!(Mask::from_validity(66, &v).is_const_true());
    }
}
