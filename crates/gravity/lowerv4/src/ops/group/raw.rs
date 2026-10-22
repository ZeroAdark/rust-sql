//! group's raw half (its unsafe code): the streamed group's hosts.
//! - `GroupEqHosted`, entered through the contract's landing pad (kernels contract.rs `entry3`): the user-code equality a
//!   key under it compares by (stream.rs `user_same`, D2719).
//! - `dset_insert`: the per-group distinct set of a streamed sorted group's DISTINCT aggregate (group_distinct.rs, which
//!   emits the set's clear and its call): open addressing over the argument's image words in four frame words (table,
//!   capacity, length, generation), the table in statement-lived pinned chunks.
#![allow(unsafe_code)]
use gravity_abi::hash::KeyHash;
use gravity_abi::Ctx;

/// `proc(a, x)` under `coll` over two key Datums (the open group's first key, then the row's), the third word
/// `proc | coll << 32` (`proc` the equality's resolution id, stream.rs `UserEq`): 1 equal, 0 not equal (false or NULL),
/// or 2 when the bind or the call raised (its error stashed), through KeySem's bound-call table (`keysem::call::equal`),
/// as the IR reads it.
pub(crate) struct GroupEqHosted;

impl gravity_kernels::contract::Host for GroupEqHosted {
    type Args = (u64, u64, u64);

    unsafe fn run(_ctx: *mut gravity_abi::Ctx, (a, x, pc): (u64, u64, u64)) -> u64 {
        match gravity_fn::typesem::keysem::call::equal(pc as u32, (pc >> 32) as u32, a, x) {
            Ok(t) => t as u64,
            Err(e) => {
                gravity_fn::stash_error(e);
                2
            }
        }
    }

    /// A panic in the function reads as raised.
    fn fail(_a: (u64, u64, u64)) -> u64 {
        2
    }
}

/// The first table's bytes: the driver's smallest pin (hostimpl/arena.rs CHUNK, 64 KiB), so the first table fills it.
const FIRST_BYTES: usize = 1 << 16;

/// The first table's entries for an image of `n` words: the largest power of two whose entries fit `FIRST_BYTES`.
fn first_cap(n: usize) -> usize {
    let fit = FIRST_BYTES / ((n + 1) * 8);
    1 << (usize::BITS - 1 - fit.max(1).leading_zeros())
}

/// The set's entry `i` (its generation word, then the image's `n` words).
unsafe fn entry(t: *mut u64, i: usize, n: usize) -> *mut u64 {
    t.add(i * (n + 1))
}

/// Entry index of `key` in a table of 2^`bits` entries: its first free (another generation's) or equal one.
unsafe fn find(t: *mut u64, bits: u32, key: &[u64], gen: u64) -> (usize, bool) {
    let (n, mask) = (key.len(), (1usize << bits) - 1);
    let mut i = KeyHash::of(key).part(bits) as usize;
    loop {
        let e = entry(t, i, n);
        if *e != gen {
            return (i, false);
        }
        if core::slice::from_raw_parts(e.add(1), n) == key {
            return (i, true);
        }
        i = (i + 1) & mask;
    }
}

/// The argument's `key` offered to the set at `set` (the four frame words), a table past half full (or none yet) first
/// doubled into `alloc`'s zeroed chunk of the bytes asked, the current generation's entries moved: 1 when new (now
/// held), 0 when held, -1 when `alloc` gave no room.
unsafe fn offer(set: *mut u64, key: &[u64], alloc: impl FnOnce(usize) -> *mut u64) -> i64 {
    let (n, gen) = (key.len(), *set.add(3));
    if *set == 0 || 2 * (*set.add(2) + 1) > *set.add(1) {
        let (old, cap) = (*set as *mut u64, *set.add(1) as usize);
        let new_cap = (2 * cap).max(first_cap(n));
        let t = alloc(new_cap * (n + 1) * 8);
        if t.is_null() {
            return -1;
        }
        let bits = new_cap.trailing_zeros();
        for i in 0..if old.is_null() { 0 } else { cap } {
            let e = entry(old, i, n);
            if *e == gen {
                let (j, _) = find(t, bits, core::slice::from_raw_parts(e.add(1), n), gen);
                core::ptr::copy_nonoverlapping(e, entry(t, j, n), n + 1);
            }
        }
        (*set, *set.add(1)) = (t as u64, new_cap as u64);
    }
    let (t, bits) = (*set as *mut u64, (*set.add(1)).trailing_zeros());
    let (i, held) = find(t, bits, key, gen);
    if held {
        return 0;
    }
    let e = entry(t, i, n);
    *e = gen;
    core::ptr::copy_nonoverlapping(key.as_ptr(), e.add(1), n);
    *set.add(2) += 1;
    1
}

gravity_kernels::contract::host! {
    /// `offer` from the program: the argument's `n` image words at `key`, the table in statement-lived pinned chunks.
    /// # Safety — `set` is the stream's four set words, cleared at least once (a generation of at least 1); `key` holds `n` words.
    pub(super) unsafe fn dset_insert(ctx: u64, set: u64, key: u64, n: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let set = set as *mut u64;
        let key = key as *const u64;
        offer(set, core::slice::from_raw_parts(key, n as usize), |bytes| {
            let t = gravity_abi::program::pin(ctx, bytes as u64) as *mut u64;
            if t.is_null() {
                gravity_kernels::host_abi::pin_refused(bytes); // D1392 (b)
            }
            t
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `offer` over heap chunks (the test binds no pin), each kept to the test's end as the statement keeps its pins.
    fn offered(set: &mut [u64; 4], pool: &mut Vec<Vec<u64>>, key: &[u64]) -> bool {
        let r = unsafe {
            offer(set.as_mut_ptr(), key, |bytes| {
                pool.push(vec![0u64; bytes / 8]); // alloc: Cold
                pool.last_mut().unwrap().as_mut_ptr()
            })
        };
        assert!(r >= 0);
        r == 1
    }

    #[test]
    fn a_group_counts_each_value_once_and_the_next_group_starts_empty() {
        let (mut set, mut pool) = ([0u64; 4], Vec::new());
        set[3] = 1; // the first group's open
        let fresh: Vec<bool> = [5u64, 7, 5, 9, 7, 5].iter().map(|&k| offered(&mut set, &mut pool, &[k])).collect();
        assert_eq!(fresh, [true, true, false, true, false, false]);
        assert_eq!(set[2], 3);
        (set[3], set[2]) = (2, 0); // the next group's open: nothing is held
        assert!(offered(&mut set, &mut pool, &[5]) && !offered(&mut set, &mut pool, &[5]));
    }

    #[test]
    fn a_group_past_half_the_table_grows_it_and_keeps_every_value() {
        let (mut set, mut pool) = ([0u64; 4], Vec::new());
        set[3] = 1;
        let keys: Vec<u64> = (0..3000u64).map(|k| k * 1_000_003 + 7).collect();
        assert!(keys.iter().all(|&k| offered(&mut set, &mut pool, &[k, !k])));
        assert!(keys.iter().all(|&k| !offered(&mut set, &mut pool, &[k, !k])));
        assert_eq!(set[2], 3000);
        // two-word images: the first table's 2048 entries fill 64 KiB, doubled at 1024 and at 2048 values
        assert_eq!((set[1], pool.len()), (8192, 3));
        // the next group reuses the grown table, empty
        (set[3], set[2]) = (2, 0);
        assert!(offered(&mut set, &mut pool, &[keys[0], !keys[0]]));
        assert_eq!(set[2], 1);
    }

    /// D1392 (b): a ctx whose program binds no pin (`gravity_abi::program::pin` answers null, as a refused pool does).
    fn refused_pin_ctx() -> (Vec<u64>, Box<gravity_abi::ProgramBlock>) {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE);
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        (ctx, program)
    }

    /// The stashed error is PostgreSQL's "out of memory" (ERRCODE_OUT_OF_MEMORY), never nothing (the client's "an error
    /// without a message (raise code 7)").
    fn assert_out_of_memory() {
        let e = gravity_fn::take_error().expect("a refused pin stashes its error for the HOST raise");
        assert_eq!((e.message(), e.sqlstate()), ("out of memory", types_error::ERRCODE_OUT_OF_MEMORY));
        assert!(e.detail().is_some_and(|d| d.contains("pinned region")), "{:?}", e.detail());
    }

    #[test]
    fn a_distinct_set_the_pool_refuses_names_out_of_memory() {
        let (mut ctx, _program) = refused_pin_ctx();
        let (mut set, key) = ([0, 0, 0, 1u64], [5u64]);
        drop(gravity_fn::take_error());
        // SAFETY: a bound ctx; `set` the four set words (generation 1); `key` one image word.
        let r = unsafe { dset_insert((ctx.as_mut_ptr() as *mut Ctx) as u64, set.as_mut_ptr() as u64, key.as_ptr() as u64, 1) };
        assert!(r < 0, "no room: the raise exit");
        assert_out_of_memory();
    }

}
