//! The capacities exec's statement entry is built with: fixed tables sized once at compile time. Each is a bound, never a
//! speed knob and never an answer.

/// Phase marks kept per statement for the phase report (exec phases.rs: a fixed thread-local array, no allocation on the
/// marked path). Chosen capacity, not measured: marks past it are left out of the report only.
pub const PHASE_MARKS_MAX: usize = 16;

/// Shelved nested serves per session (exec admit/entry/shelf.rs). Chosen capacity, not measured: a nested body cycling
/// more plan keys than this rebuilds the oldest one's serve (a shelved serve is shed, its context empty).
pub const SHELVED_SERVES_MAX: usize = 16;

/// Statement levels on one thread whose backend state sits in static slots (exec admit/reentry.rs; a served statement
/// whose SQL function or trigger runs another takes the next). Chosen capacity, not measured: a deeper statement takes a
/// level built on demand, and only the stack depth check bounds the nesting.
pub const ENTRY_LEVELS_MAX: usize = 8;

/// D3098: plansources a running level queues for its next borrow (exec admit/reentry.rs `drop_handle`: a plpgsql EXECUTE
/// drops its one-shot plan on every call inside the calling statement). Chosen capacity, not measured: 8 bytes each, 32 KiB
/// a level at most; past it the level flushes whole at its next borrow (correct, only slower).
pub const ENTRY_PENDING_DROPS_MAX: usize = 4096;

/// Distinct (reason, statement) pairs the row executor's census counts per backend (exec admit/entry/census.rs: each pair
/// logs its 1st, 10th, 100th, ... crossing, as a sort comparator's body crosses once per call). Chosen capacity, not
/// measured: 16 bytes each, 64 KiB a backend at most; the pairs past it share one count.
pub const CENSUS_PAIRS_MAX: usize = 4096;
