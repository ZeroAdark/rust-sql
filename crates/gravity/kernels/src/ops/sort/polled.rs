//! C9's kernel face (D4140): the one polled step of the kernels crate's host code. The kernels
//! depend on the ABI and nothing else of the runtime's (docs/gravity/abi-v2.md section 0; the wasm build compiles them),
//! so they have no `cancelpoll::Step`: a kernel-side host step polls its statement through `Poll`, which the host entry
//! builds over `gravity_abi::ctx::cancel_pending(ctx)` (the lowered back-edge's own check: the owner's interrupts on its
//! own byte, the poll-gap probe's mark), and through nothing else. Its forms and constants are `cancelpoll`'s (`tick`
//! per unit, `batch` per `BATCH` units of about 100 ns or less, `sort_by` in `SORT_RUN` runs merged pairwise), measured on
//! c8g for C9 (lanes/keyrep/c9-design.md). Every later kernel site that needs a poll uses this module
//! (crates/gravity/lint/POLL-DEBT/README).
use core::cmp::Ordering;

/// Units between two ticks of `batch`, and the runs `sort_by` sorts before merging them pairwise (the ABI's constants).
pub(crate) use gravity_abi::ctx::{POLL_BATCH as BATCH, POLL_SORT_RUN as SORT_RUN};

/// The statement's abort was seen: the step stops and passes it up (`?`); the host entry returns `CANCELLED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stopped;

/// A kernel-side host step's poll: `pending` is the host entry's `cancel_pending(ctx)` (raw.rs builds it).
pub(crate) struct Poll<'a> {
    pending: &'a dyn Fn() -> bool,
}

impl<'a> Poll<'a> {
    pub(crate) fn new(pending: &'a dyn Fn() -> bool) -> Poll<'a> {
        Poll { pending }
    }

    /// One unit done: `Err` when the statement's abort is pending.
    #[inline]
    pub(crate) fn tick(&mut self) -> Result<(), Stopped> {
        if (self.pending)() {
            return Err(Stopped);
        }
        Ok(())
    }

    /// Unit `i` of a loop of about 100 ns or less per unit: a tick every `BATCH` units.
    #[inline(always)]
    pub(crate) fn batch(&mut self, i: usize) -> Result<(), Stopped> {
        if i & (BATCH - 1) == BATCH - 1 {
            return self.tick();
        }
        Ok(())
    }
}

/// `v` sorted by `cmp` under `poll`: runs of `SORT_RUN` sorted with a tick before each, then merged pairwise into
/// `scratch` and back with a tick per `BATCH` elements (`cancelpoll::sort_by`'s algorithm). `scratch` holds at least
/// `v.len()` when `v.len() > SORT_RUN` (the caller allocates it). `Err`: `v` holds its elements in no order.
pub(crate) fn sort_by<T: Copy>(poll: &mut Poll<'_>, v: &mut [T], scratch: &mut [T], cmp: impl Fn(&T, &T) -> Ordering) -> Result<(), Stopped> {
    for run in v.chunks_mut(SORT_RUN) {
        poll.tick()?;
        run.sort_unstable_by(&cmp);
    }
    let n = v.len();
    if n <= SORT_RUN {
        return Ok(());
    }
    let scratch = &mut scratch[..n];
    let (mut in_v, mut width, mut r) = (true, SORT_RUN, Ok(()));
    'passes: while width < n {
        let (src, dst): (&[T], &mut [T]) = if in_v { (&*v, &mut *scratch) } else { (&*scratch, &mut *v) };
        let mut k = 0usize;
        for start in (0..n).step_by(2 * width) {
            let (mid, end) = ((start + width).min(n), (start + 2 * width).min(n));
            let (mut i, mut j) = (start, mid);
            for slot in &mut dst[start..end] {
                if let Err(e) = poll.batch(k) {
                    r = Err(e);
                    break 'passes;
                }
                k += 1;
                *slot = if j >= end || i < mid && cmp(&src[i], &src[j]) != Ordering::Greater {
                    i += 1;
                    src[i - 1]
                } else {
                    j += 1;
                    src[j - 1]
                };
            }
        }
        in_v = !in_v;
        width *= 2;
    }
    // a stopped pass leaves its source whole: `v` gets it back
    if !in_v {
        v.copy_from_slice(scratch);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    #[test]
    fn sort_by_sorts_and_a_stopped_sort_keeps_every_element() {
        let n = 3 * SORT_RUN + 17;
        let input: Vec<u32> = (0..n as u32).map(|i| i.wrapping_mul(2654435761) % 100_003).collect();
        let mut want = input.clone();
        want.sort_unstable();
        let never = || false;
        let mut quiet = Poll::new(&never);
        let mut small: Vec<u32> = input[..100].to_vec();
        assert_eq!(sort_by(&mut quiet, &mut small, &mut [], |a, b| a.cmp(b)), Ok(()));
        assert!(small.windows(2).all(|w| w[0] <= w[1]));
        let (mut v, mut scratch) = (input.clone(), vec![0u32; n]); // alloc: Cold
        assert_eq!(sort_by(&mut quiet, &mut v, &mut scratch, |a, b| a.cmp(b)), Ok(()));
        assert_eq!(v, want);
        // the abort lands halfway through the last merge pass: 4 runs' ticks, the first pass's n / BATCH, half the second's
        let polls = Cell::new(0usize);
        let late = || {
            polls.set(polls.get() + 1);
            polls.get() > 4 + n / BATCH + n / BATCH / 2
        };
        let mut poll = Poll::new(&late);
        let mut v = input.clone();
        assert_eq!(sort_by(&mut poll, &mut v, &mut scratch, |a, b| a.cmp(b)), Err(Stopped), "stopped in a merge pass");
        v.sort_unstable();
        assert_eq!(v, want, "a stopped sort leaves every element in v");
    }

    #[test]
    fn batch_ticks_once_per_batch() {
        let yes = || true;
        let mut poll = Poll::new(&yes);
        assert_eq!((0..3 * BATCH).position(|i| poll.batch(i).is_err()), Some(BATCH - 1));
    }
}
