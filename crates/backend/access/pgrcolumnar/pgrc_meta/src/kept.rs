//! Wave 3 A3 (cvformat): the meta builder's own heap on a named owner, by construction. A [`ColumnMeta`] made with an
//! owner keeps ONE door-1 charge on it (`Kept`) that follows the capacity of everything the builder keeps between calls:
//! the in-flight granule, the sealed granules' HLL / PSMA / bloom blocks, the distinct-set accumulator, the batch span
//! scratch, the dict fold tables. The builder re-computes that byte count from its own capacities and fits the charge to
//! it at each granule's seal and at each part step, on the thread that grew it (the seal thread that makes, feeds and
//! drops the builder), so the lag is at most one granule's growth. The distinct-set accumulator is the exception
//! (D4539): its slot table doubles to tens of MiB inside one granule, so it holds a `Kept` of its own on the same owner,
//! fitted before each grow allocates (`sketch.rs`). With no owner there is no charge: the bytes stay on the allocating
//! thread's account, as before.
//!
//! [`ColumnMeta`]: crate::builder::ColumnMeta

use memledger::charge::{Charge, Door, Owner, Release};
use memledger::{ChargedVec, Kind};
use pgrc_format::{FormatError, FormatResult};

/// The owner and kind lane a call's working memory is charged to (`None`: uncharged, the thread's account as before).
pub type On = Option<(Owner, Kind)>;

/// An empty `ChargedVec` on `on` (uncharged when `None`).
#[inline]
pub(crate) fn charged_vec<T>(on: On) -> ChargedVec<T> {
    match on {
        Some((o, k)) => ChargedVec::new(Some(o), k),
        None => ChargedVec::none(),
    }
}

/// `it` collected into a `ChargedVec` on `on`. D4545: the iterator's known length is reserved first, and that reservation
/// commits its charge as it allocates (`ChargedVec::try_reserve`), so the block is the owner's for its whole fill. Before,
/// `with_vec` fitted the charge only after the fill, and a fill of tens of MiB (the sketch's finish on a 1M-entry distinct
/// set) sat on the thread's account, unattributed, until it ended. What the hint leaves out (a filter's) grows inside the
/// fill and is fitted after it, as before; [`collect_on_n`] reserves a count the hint does not tell (a filter's, D4594).
#[inline]
pub(crate) fn collect_on<T>(on: On, it: impl IntoIterator<Item = T>) -> FormatResult<ChargedVec<T>> {
    let it = it.into_iter();
    let n = it.size_hint().0;
    collect_on_n(on, n, it)
}

/// [`collect_on`] of an iterator that yields `n` items its size hint does not tell (a filter).
#[inline]
pub(crate) fn collect_on_n<T>(on: On, n: usize, it: impl IntoIterator<Item = T>) -> FormatResult<ChargedVec<T>> {
    let mut v = charged_vec(on);
    v.try_reserve(n).map_err(|_| oom::<T>(n))?;
    v.with_vec(|w| w.extend(it)); // alloc: Exec charged on
    Ok(v)
}

pub(crate) fn oom<T>(n: usize) -> FormatError {
    FormatError::OutOfMemory { bytes: n.saturating_mul(core::mem::size_of::<T>()) }
}

/// A growable buffer's block bytes: its capacity, not its length. A macro over anything with `capacity()` that indexes
/// to a slice, so this crate names no buffer type for it (alloclint counts type mentions).
macro_rules! cap_bytes {
    ($v:expr) => {{
        let v = &$v;
        (v.capacity() as u64).saturating_mul($crate::kept::elem_bytes(&v[..]))
    }};
}
pub(crate) use cap_bytes;

/// The size of one element of `s`.
#[inline]
pub(crate) fn elem_bytes<T>(_: &[T]) -> u64 {
    core::mem::size_of::<T>() as u64
}

/// The release point a charge on `o` states (memledger's own rule, `ChargedVec`): a statement's, a portal's or a session's
/// end; a cache's eviction.
fn release_of(o: Owner) -> Release {
    match o {
        Owner::Portal { .. } => Release::Portal,
        Owner::Session(_) => Release::Session,
        Owner::Cache(_) => Release::Evict,
        Owner::Statement { .. } | Owner::WorkerSet(_) | Owner::Op { .. } => Release::Statement,
    }
}

/// A door-1 charge on an owner, fitted to a byte count its keeper computes from its own capacities (`fit`): the meta
/// builder's, and pgrc_am's ANALYZE fold's (the per-part sketches it keeps on the statement).
#[derive(Debug)]
pub struct Kept {
    charge: Charge,
    /// `charge` is open on an owner (`Charge::NONE` takes no commit).
    on: bool,
    /// What `charge` holds committed: the bytes at the last fit.
    charged: u64,
}

impl Kept {
    pub const fn none() -> Kept {
        Kept { charge: Charge::NONE, on: false, charged: 0 }
    }

    pub fn new(owner: Option<(Owner, Kind)>) -> Kept {
        match owner {
            Some((o, k)) => Kept { charge: Charge::open(o, release_of(o), k, Door::Heap, 0), on: true, charged: 0 },
            None => Kept::none(),
        }
    }

    /// The owner and kind lane the bytes are charged to (`None`: uncharged).
    pub fn owner(&self) -> Option<(Owner, Kind)> {
        self.on.then(|| (self.charge.owner(), self.charge.kind()))
    }

    /// The charge fitted to `now` bytes: a commit of what grew since the last fit (allocated on this thread), a free of
    /// what shrank (freed on this thread).
    pub fn fit(&mut self, now: u64) {
        if !self.on {
            return;
        }
        if now > self.charged {
            self.charge.commit(now - self.charged);
        } else if now < self.charged {
            self.charge.free(self.charged - now);
        }
        self.charged = now;
    }

    /// The bytes the charge holds (0 uncharged).
    pub fn charged(&self) -> u64 {
        if self.on { self.charged } else { 0 }
    }
}
