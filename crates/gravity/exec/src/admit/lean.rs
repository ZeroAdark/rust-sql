//! D4600 (lane pgladmit): the admission's per-execution overhead on its steady state (a front-key hit), cut without
//! changing what any admission answers. `PGRUST_GRAVITY_ADMIT_LEAN` is the cuts' internal A/B switch (an env switch,
//! not a GUC, as the other per-statement kill switches): unset or any value but `0` on; exactly `0` takes every cut
//! back to its former road. Read once per process.
//!
//! What the switch covers (each road answers alike either way; only the instruction count differs):
//! - `graph`'s clock reads: the admission's own clock is read only for its report line (`Backend::reporting`);
//! - `regen::check_build` and the v4 build bound (`ops::hashjoin::admit::build_would_spill`): no copy of a pipeline
//!   and no list of all of them unless a hash build is there to bound;
//! - the backend's memos by content key (`Backend::graph_words`, `Backend::graph_registry`) and the front key's memo
//!   (inputs/front.rs `MEMO`): a content or front key is a 128-bit hash already, so its halves go through the one-word
//!   key hash instead of SipHash (`KeyFold`); no pass over those maps depends on their order (they are cleared whole
//!   when full, the front memo's `forget` retains by value);
//! - the front key's alias passes, fused into one (gravity_lower key.rs `alias_walk_fused`, which puts what
//!   `alias_walk` puts);
//! - `graph`'s admission scopes (union nests, deferred and bound extern parameters, re-estimated rows, the partial
//!   reader, the deps check): at the thread's outermost admission each holds its default value (`graph` is their only
//!   setter and each puts its previous value back on any exit), so one whose value is that default is not entered;
//! - `graph` without parameters (the simple protocol's): no extern facts, deferred slots or re-estimated rows are
//!   gathered, and the admission keeps the empty folded / genp / deferred (and, unrecorded, reads) `bind` wrote;
//! - the key salt (`gravity_decompose::admit::externs_salt`, a fold over the extern facts, deferred slots and
//!   re-estimated rows in scope) of an admission that binds none of them: 0, not computed;
//! - the removed families' check (`removed::check`, a plan walk) under the graph admission: not on a front hit, whose
//!   key implies it passes (admit `admit_in`); first on every other road, and again ahead of any refusal or error.

use core::hash::{BuildHasher, Hasher};
use core::sync::atomic::{AtomicU8, Ordering};
use std::collections::hash_map::{DefaultHasher, RandomState};

/// The switch's name.
pub const SWITCH: &str = "PGRUST_GRAVITY_ADMIT_LEAN";

static LEAN: AtomicU8 = AtomicU8::new(0);

/// The switch: on unless `PGRUST_GRAVITY_ADMIT_LEAN` is exactly `0` (read once per process).
#[inline]
pub fn on() -> bool {
    match LEAN.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::inputs::knobs::env_u64(SWITCH) != Some(0);
            LEAN.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// The hasher of the backend's memos by content key: when the switch is on, the key's two halves through the
/// one-word key hash (gravity_abi `KeyHash::word`, a bijection; the key is a hash already); the std map's SipHash
/// with its random keys when off. No `Clone`: no memo map is copied.
pub struct KeyFold {
    lean: bool,
    sip: RandomState,
}

impl KeyFold {
    pub fn new() -> KeyFold {
        KeyFold { lean: on(), sip: RandomState::new() }
    }
}

impl Default for KeyFold {
    fn default() -> KeyFold {
        KeyFold::new()
    }
}

impl BuildHasher for KeyFold {
    type Hasher = KeyFolder;

    #[inline]
    fn build_hasher(&self) -> KeyFolder {
        if self.lean { KeyFolder::Fold(0) } else { KeyFolder::Sip(self.sip.build_hasher()) }
    }
}

/// `KeyFold`'s hasher.
pub enum KeyFolder {
    Fold(u64),
    Sip(DefaultHasher),
}

impl KeyFolder {
    /// One word into the fold: the running value rotated, xored with the word, through `KeyHash::word`.
    #[inline]
    fn fold(h: &mut u64, x: u64) {
        *h = gravity_abi::hash::KeyHash::word(h.rotate_left(23) ^ x).to_abi();
    }
}

impl Hasher for KeyFolder {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        match self {
            KeyFolder::Fold(h) => {
                for c in bytes.chunks(8) {
                    let mut w = [0u8; 8];
                    w[..c.len()].copy_from_slice(c);
                    Self::fold(h, u64::from_le_bytes(w));
                }
            }
            KeyFolder::Sip(s) => s.write(bytes),
        }
    }

    #[inline]
    fn write_u128(&mut self, x: u128) {
        match self {
            // the high half through the word hash first, so keys alike but for which half holds a value differ
            KeyFolder::Fold(h) => Self::fold(h, x as u64 ^ gravity_abi::hash::KeyHash::word((x >> 64) as u64).to_abi()),
            KeyFolder::Sip(s) => s.write_u128(x),
        }
    }

    #[inline]
    fn write_u64(&mut self, x: u64) {
        match self {
            KeyFolder::Fold(h) => Self::fold(h, x),
            KeyFolder::Sip(s) => s.write_u64(x),
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        match self {
            KeyFolder::Fold(h) => *h,
            KeyFolder::Sip(s) => s.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::ContentKey;
    use std::collections::HashMap;

    /// Both roads find what they filed; the folded one spreads 4096 consecutive and 4096 high-half-only keys over
    /// distinct hashes (a content key is a hash, but a test key need not be).
    #[test]
    fn key_fold_files_and_finds_on_both_roads() {
        for lean in [true, false] {
            let mut m: HashMap<ContentKey, u32, KeyFold> = HashMap::with_hasher(KeyFold { lean, sip: RandomState::new() });
            for i in 0..4096u32 {
                m.insert(ContentKey(i as u128), i);
                m.insert(ContentKey((i as u128) << 64), i + 4096);
            }
            assert_eq!(m.len(), 8191, "key 0 is filed twice");
            for i in 1..4096u32 {
                assert_eq!(m.get(&ContentKey(i as u128)), Some(&i));
                assert_eq!(m.get(&ContentKey((i as u128) << 64)), Some(&(i + 4096)));
            }
            assert_eq!(m.get(&ContentKey(u128::MAX)), None);
        }
        let b = KeyFold { lean: true, sip: RandomState::new() };
        let hs: std::collections::HashSet<u64> = (0..4096u128).flat_map(|i| [b.hash_one(ContentKey(i)), b.hash_one(ContentKey(i << 64))]).collect();
        assert_eq!(hs.len(), 8191);
    }
}
