//! The allocator's rung and its retained reading (ALLOC-LADDER (a) + (b); memlead's LADDER-DESIGN section 11).

/// The retained reading's period while memory is short (`cache::memory_short`), and otherwise. CHOSEN: one exact read
/// costs ~25 ms at 16 GiB of allocator mappings (box 10, 2026-10-13), so 2.5 % of one thread while short, nothing to
/// speak of otherwise; a rung makes the next sample read it at once.
pub const RETAINED_SHORT_MS: i64 = 1000;
pub const RETAINED_IDLE_MS: i64 = 60_000;

/// The first rung (every thread collects its own pages, one purge) runs at most once per this. CHOSEN (memlead section
/// 11's cool-down): measured yields are 1-6 MiB, a worker's collect 0.03-1.6 ms.
pub const RUNG1_COOLDOWN_MS: u64 = 1000;

/// The retained reading's period now.
pub fn retained_every_ms(short: bool) -> i64 {
    if short { RETAINED_SHORT_MS } else { RETAINED_IDLE_MS }
}
