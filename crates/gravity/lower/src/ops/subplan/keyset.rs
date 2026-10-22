//! The hashed SubPlan's key set, its runtime face (`gravity_exec::subhash::KeySet`, PostgreSQL's `ExecHashSubPlan` verdict).
use gravity_abi::Word;
pub use super::raw::lookup;

pub const NONEMPTY: u64 = 1;
pub const HAS_NULL: u64 = 2;
pub const MIX: u64 = 0x9E37_79B9_7F4A_7C15;

/// The set's header as the main program reads it: `keys` / `tags` are `mask + 1` long (tag 0 = empty), the load at or under one half.
#[repr(C)]
pub struct KeyTable {
    pub keys: Word<*const u64>,
    pub tags: Word<*const u8>,
    pub mask: u64,
    pub flags: u64,
}

pub const fn word_id(plan_id: i32) -> i32 {
    -1 - plan_id
}

pub const fn width_mask(width: u8) -> u64 {
    if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (width as u32 * 8)) - 1
    }
}

pub fn slot_of(key: u64, mask: u64) -> u64 {
    (key.wrapping_mul(MIX) >> 29) & mask
}

/// `key = ANY (set)` as PostgreSQL answers it: (value as a bool datum, valid; valid false = unknown).
pub fn verdict(nonempty: bool, found: bool, has_null: bool, key_valid: bool) -> (u64, bool) {
    match (nonempty, key_valid, found, has_null) {
        (false, _, _, _) => (0, true),
        (true, false, _, _) => (0, false),
        (true, true, true, _) => (1, true),
        (true, true, false, true) => (0, false),
        (true, true, false, false) => (0, true),
    }
}
