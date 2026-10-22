//! The 128-bit cross-part identity of a dictionary value: keyed SipHash-2-4-128 over the entry
//! bytes (ClickHouse's `uniqExact`, SQE's `entry_fp128`) under one seed per process (`install`;
//! never persisted, never compared across processes). Collision odds at n groups ≈ n² / 2^129.
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

static SEED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static STATE: AtomicU8 = AtomicU8::new(0);
const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

/// Install the process seed once; later calls keep the first seed (they return false).
pub fn install(k0: u64, k1: u64) -> bool {
    if STATE.compare_exchange(EMPTY, WRITING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        while STATE.load(Ordering::Acquire) != READY {
            core::hint::spin_loop();
        }
        return false;
    }
    SEED[0].store(k0, Ordering::Relaxed);
    SEED[1].store(k1, Ordering::Relaxed);
    STATE.store(READY, Ordering::Release);
    true
}

/// The installed seed, `None` before `install`.
#[inline]
pub fn seed() -> Option<(u64, u64)> {
    if STATE.load(Ordering::Acquire) != READY {
        return None;
    }
    Some((SEED[0].load(Ordering::Relaxed), SEED[1].load(Ordering::Relaxed)))
}

/// The fingerprint of `b` under the process seed as `[lo, hi]`; `None` before `install`.
#[inline]
pub fn fp128(b: &[u8]) -> Option<[u64; 2]> {
    Some(keys()?.fp_bytes(b))
}

/// The process seed held by a caller fingerprinting many byte strings (a verbatim part's rows).
#[derive(Clone, Copy, Debug)]
pub struct Keys {
    k0: u64,
    k1: u64,
}

/// The installed seed as `Keys`, `None` before `install`.
#[inline]
pub fn keys() -> Option<Keys> {
    let (k0, k1) = seed()?;
    Some(Keys { k0, k1 })
}

impl Keys {
    /// The fingerprint of `b`: what an `Fp128` table holds for a dictionary entry of the same bytes.
    #[inline]
    pub fn fp_bytes(&self, b: &[u8]) -> [u64; 2] {
        siphash128(self.k0, self.k1, b)
    }
}

/// The canonical text key image's inline bound (textkeys step 3, lowerv4 road `key.canon`): a payload of at most this many
/// bytes is its own image.
pub const INLINE_BYTES: usize = 15;
/// The canonical image's one flag bit, bit 63 of its second word: clear, the inline form; set, the hashed form.
pub const HASHED: u64 = 1 << 63;

/// The canonical text key image of a payload of at most `INLINE_BYTES` bytes (`b`; longer is a caller's bug, checked in
/// debug builds): bytes 0..8 and 8.. as little-endian words, zero above them, the length in the second word's bits
/// 56..60, bit 63 clear. Equal images are equal payloads.
#[inline]
pub fn canon_inline(b: &[u8]) -> [u64; 2] {
    debug_assert!(b.len() <= INLINE_BYTES);
    let mut w = [0u8; 16];
    w[..b.len()].copy_from_slice(b);
    let (x, y) = w.split_at(8);
    let word = |v: &[u8]| v.iter().rev().fold(0u64, |a, &c| a << 8 | c as u64);
    [word(x), word(y) | (b.len() as u64) << 56]
}

/// The canonical text key image of payload `b` whose Fp128 is `fp` (read only past `INLINE_BYTES` bytes): the inline form,
/// else `fp` with `HASHED` set (its other 127 bits). The one constructor every source's image is (a dictionary's image
/// table derives each entry's from its bytes and its Fp128 entry: no second hash).
#[inline]
pub fn canon_of(b: &[u8], fp: impl FnOnce() -> [u64; 2]) -> [u64; 2] {
    if b.len() <= INLINE_BYTES {
        return canon_inline(b);
    }
    let [lo, hi] = fp();
    [lo, hi | HASHED]
}

impl Keys {
    /// `b`'s canonical text key image (`canon_of`), its Fp128 under this seed.
    #[inline]
    pub fn canon_bytes(&self, b: &[u8]) -> [u64; 2] {
        canon_of(b, || self.fp_bytes(b))
    }
}

#[inline(always)]
fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-2-4-128 keyed by `(k0, k1)`: `[lo, hi]` = the reference's 16 output bytes, little-endian.
#[inline]
pub fn siphash128(k0: u64, k1: u64, b: &[u8]) -> [u64; 2] {
    let mut v = [k0 ^ 0x736f_6d65_7073_6575, k1 ^ 0x646f_7261_6e64_6f6d ^ 0xee, k0 ^ 0x6c79_6765_6e65_7261, k1 ^ 0x7465_6462_7974_6573];
    let mut rest = b;
    while rest.len() >= 8 {
        let m = u64::from_le_bytes([rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6], rest[7]]);
        v[3] ^= m;
        sip_round(&mut v);
        sip_round(&mut v);
        v[0] ^= m;
        rest = &rest[8..];
    }
    let mut m = (b.len() as u64) << 56;
    for (i, &x) in rest.iter().enumerate() {
        m |= (x as u64) << (8 * i);
    }
    v[3] ^= m;
    sip_round(&mut v);
    sip_round(&mut v);
    v[0] ^= m;
    v[2] ^= 0xee;
    for _ in 0..4 {
        sip_round(&mut v);
    }
    let lo = v[0] ^ v[1] ^ v[2] ^ v[3];
    v[1] ^= 0xdd;
    for _ in 0..4 {
        sip_round(&mut v);
    }
    [lo, v[0] ^ v[1] ^ v[2] ^ v[3]]
}
