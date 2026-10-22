//! Worker legs for Sink-ending sections (docs/gravity/exec.md): each worker
//! appends its rows to its own leg, one buffer per claim (R1D); the leader drains
//! every claim's rows through the real sink after the section, in claim order
//! (ascending `claim.lo`: the order a serial run emits) or sorted by the plan's keys.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use pgsync::Mutex;

use gravity_abi::{Datum, MorselRange};
use gravity_driver::{BorrowedBatch, Error, Flow, LegRef, Result, Sink, SinkLeg, WireFormat};
use memledger::{ChargedVec, Kind, Owner};

mod raw;

/// One claim's rows, owned: by-reference images copied into `bytes` (their datums kept as offsets, re-pointed at the
/// drain), so nothing here points into what the worker releases at the claim's end (pins, scratch, the out block).
/// Wave 2: filled on the worker, freed on the leader after its emit, each buffer charged to the legs' owner (`LegOwner`).
#[derive(Default)]
struct ClaimBuf {
    lo: u64,
    values: ChargedVec<Datum>,
    isnull: ChargedVec<u8>,
    bytes: ChargedVec<u8>,
    framed: ChargedVec<u8>,
    rows: u64,
}

/// A leg's claims: the one open on its worker and the settled ones; pushes outside a claim (no hooks) go to an open buffer at `lo = u64::MAX`.
#[derive(Default)]
struct Claims {
    open: Option<ClaimBuf>,
    done: ChargedVec<ClaimBuf>,
}

pub struct Leg {
    claims: Mutex<Claims>,
    /// Rows framed as DataRow messages on the worker thread (`wire`); the drain hands them to `Sink::push_wire`.
    wire: Option<WireFormat>,
    shared: Arc<Shared>,
}

/// What the legs share: the receiver stopped (R1D (c): the workers' kernels end their claims at the next push), and the
/// settled claims' bytes waiting for the leader (R1D (b): the window, held in G by the leader).
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    bytes: AtomicU64,
    /// The owner of the rows the legs hold. A Sink section feeds no breaker, so it is the
    /// statement's, or the portal's that executes it (`LegSink::new`'s `owner`); `None`: uncharged.
    owner: Option<Owner>,
    /// Per output column: by reference (the drain's `point_into`), and its `typlen` when by reference (> 0 fixed, -1
    /// varlena, -2 cstring), 0 by value: the length of an image `keep` copies. One copy for every leg, on `owner`.
    byref: ChargedVec<bool>,
    typlen: ChargedVec<i16>,
}

/// A buffer growth the allocator refused: the statement's out of memory (53200), not the process's abort.
fn refused<T>(n: usize) -> Error {
    Error::out_of_memory((n * core::mem::size_of::<T>()) as u64)
}

fn grow<T: Copy>(v: &mut ChargedVec<T>, s: &[T]) -> Result<()> {
    v.extend_from_slice(s).map_err(|_| refused::<T>(s.len())) // alloc: Exec charged the legs' owner
}

fn room<T>(v: &mut ChargedVec<T>, n: usize) -> Result<()> {
    v.try_reserve(n).map_err(|_| refused::<T>(n)) // alloc: Exec charged the legs' owner
}

fn put<T>(v: &mut ChargedVec<T>, x: T) -> Result<()> {
    v.push(x).map_err(|_| refused::<T>(1)) // alloc: Exec charged the legs' owner
}

impl ClaimBuf {
    fn size(&self) -> u64 {
        (self.values.len() * 8 + self.isnull.len() + self.bytes.len() + self.framed.len()) as u64
    }
}

impl Leg {
    fn new(wire: Option<WireFormat>, shared: Arc<Shared>) -> Leg {
        Leg { claims: Mutex::new(Claims::default()), wire, shared }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Claims> {
        self.claims.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn owner(&self) -> Option<Owner> {
        self.shared.owner
    }

    fn fresh(&self, lo: u64) -> ClaimBuf {
        let o = self.owner();
        ClaimBuf { lo, values: ChargedVec::new(o, Kind::Region), isnull: ChargedVec::new(o, Kind::Region), bytes: ChargedVec::new(o, Kind::Region), framed: ChargedVec::new(o, Kind::Region), rows: 0 } // alloc: Exec charged o
    }

    /// Room in the settled list for the open claim (its settle takes it at a claim's begin or end, which cannot fail).
    fn done_room(&self, c: &mut Claims) -> Result<()> {
        if c.done.len() < c.done.capacity() {
            return Ok(());
        }
        if c.done.is_empty() && c.done.owner().is_none() {
            c.done = ChargedVec::new(self.owner(), Kind::Region); // alloc: Exec charged self.owner()
        }
        c.done.try_reserve(1).map_err(|_| refused::<ClaimBuf>(1)) // alloc: Exec charged the legs' owner
    }

    fn settle(&self, c: &mut Claims) {
        if let Some(b) = c.open.take() {
            if b.rows > 0 {
                self.shared.bytes.fetch_add(b.size(), Ordering::Relaxed);
                // room taken at the claim's first push (`done_room`)
                c.done.with_vec(|d| d.push(b)); // alloc: Exec charged the legs' owner
            }
        }
    }
}

impl SinkLeg for Leg {
    fn push(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        if self.shared.stop.load(Ordering::Relaxed) {
            return Ok(Flow::Stop);
        }
        let mut g = self.lock();
        if g.open.is_none() {
            g.open = Some(self.fresh(u64::MAX));
        }
        self.done_room(&mut g)?;
        let c = g.open.as_mut().expect("an open claim");
        c.rows += b.rows as u64;
        if let Some(fmt) = &self.wire {
            let room = b.rows * gravity_wire::datarow::frame_row_max(fmt);
            c.framed.try_reserve(room).map_err(|_| refused::<u8>(room))?;
            // one frame per row of a pushed out block, at most lowerv4 OUT_ROWS (64) rows; the pushing kernel polls between blocks
            c.framed.with_vec(|f| b.iter().for_each(|(v, n)| frame_row(f, fmt, v, n))); // alloc: Exec charged the legs' owner (room reserved above)
            return Ok(Flow::Continue);
        }
        let n = b.natts * b.rows;
        let at = c.values.len();
        grow(&mut c.values, &b.values[..n])?;
        grow(&mut c.isnull, &b.isnull[..n])?;
        if sparse_region(&c.values[at..], &c.isnull[at..], &self.shared.typlen, b.rows, b.region.len()) {
            compact(&mut c.values[at..], &c.isnull[at..], &self.shared.typlen, &mut c.bytes)?;
            return Ok(Flow::Continue);
        }
        let base = c.bytes.len();
        grow(&mut c.bytes, b.region)?;
        keep(&mut c.values[at..], &c.isnull[at..], &self.shared.typlen, (b.region.as_ptr() as u64, b.region.len()), base, &mut c.bytes)?;
        Ok(Flow::Continue)
    }

    fn claim_begin(&self, claim: MorselRange) {
        let mut g = self.lock();
        self.settle(&mut g);
        g.open = Some(self.fresh(claim.lo));
    }

    fn claim_end(&self, _ok: bool) {
        self.settle(&mut self.lock());
    }
}

/// D2985: the pushed rows' by-reference images take under half the block's region (a selective filter keeps a few rows of
/// a block whose region holds every scanned row's images): the leg keeps those images alone (`compact`), not the region.
/// A claim waiting in the ordered window held its blocks' whole regions, ~66 MB per claim of a wide text table read at 1 %
/// selectivity, so even the bounded window was GBs. Gated cheaply first (cost::runtime `LEG_SPARSE_MIN_BYTES` and
/// `LEG_SPARSE_ROW_BYTES` per row), then the images' bytes summed until they reach half the region.
fn sparse_region(v: &[Datum], n: &[u8], typlen: &[i16], rows: usize, region: usize) -> bool {
    use gravity_program::cost::runtime::{LEG_SPARSE_MIN_BYTES, LEG_SPARSE_ROW_BYTES};
    if region < LEG_SPARSE_MIN_BYTES || region < rows.saturating_mul(LEG_SPARSE_ROW_BYTES) {
        return false;
    }
    let w = typlen.len().max(1);
    let half = region / 2;
    let mut need = 0usize;
    // bounded("small-fixed: one datum of a pushed out block, at most lowerv4 OUT_ROWS (64) rows of at most MaxTupleAttributeNumber (1664) columns; the pushing kernel polls between blocks")
    for (i, &d) in v.iter().enumerate() {
        let t = typlen.get(i % w).copied().unwrap_or(0);
        if t != 0 && n[i] == 0 {
            need += (raw::image(d, t).len() + 7) & !7;
            if need >= half {
                return false;
            }
        }
    }
    true
}

/// Each by-reference image copied to the end of `bytes` (8-aligned), its datum re-based to the copy's offset (`keep`'s
/// offsets, which `point_into` turns back into pointers at the drain).
fn compact(v: &mut [Datum], n: &[u8], typlen: &[i16], bytes: &mut ChargedVec<u8>) -> Result<()> {
    let w = typlen.len().max(1);
    // bounded("small-fixed: one datum of a pushed out block, at most lowerv4 OUT_ROWS (64) rows of at most MaxTupleAttributeNumber (1664) columns; the pushing kernel polls between blocks")
    for (i, d) in v.iter_mut().enumerate() {
        let t = typlen.get(i % w).copied().unwrap_or(0);
        if t != 0 && n[i] == 0 {
            let img = raw::image(*d, t);
            let at = (bytes.len() + 7) & !7;
            bytes.resize(at, 0).map_err(|_| refused::<u8>(at))?; // alloc: Exec charged the legs' owner
            grow(bytes, img)?;
            *d = at as u64;
        }
    }
    Ok(())
}

/// By-reference datums a producer pushed pointing outside their block's region (it did not copy its images): `keep` copies
/// each image into the claim's bytes, as PostgreSQL copies a datum out (D470: never a datum into what the worker releases).
/// Counted, and a debug build asserts, so a path that does it is found.
pub static OUTSIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The block's by-reference datums re-based to offsets into the claim's `bytes` (the block's region was copied there at
/// `base`); a datum outside the region has its image copied to the end of `bytes` (8-aligned) and points there.
fn keep(v: &mut [Datum], n: &[u8], typlen: &[i16], (lo, len): (u64, usize), base: usize, bytes: &mut ChargedVec<u8>) -> Result<()> {
    let w = typlen.len().max(1);
    let mut outside = 0;
    // bounded("small-fixed: one datum of a pushed out block, at most lowerv4 OUT_ROWS (64) rows of at most MaxTupleAttributeNumber (1664) columns; the pushing kernel polls between blocks")
    for (i, d) in v.iter_mut().enumerate() {
        let t = typlen.get(i % w).copied().unwrap_or(0);
        if t != 0 && n[i] == 0 {
            if *d >= lo && *d < lo + len as u64 {
                *d = *d - lo + base as u64;
            } else {
                outside += 1;
                let img = raw::image(*d, t);
                let at = (bytes.len() + 7) & !7;
                bytes.resize(at, 0).map_err(|_| refused::<u8>(at))?; // alloc: Exec charged the legs' owner
                grow(bytes, img)?;
                *d = at as u64;
            }
        }
    }
    if outside > 0 {
        OUTSIDE.fetch_add(outside, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(outside == 0, "gravity: a sink leg's by-reference datum outside its block region ({outside} copied)");
    }
    Ok(())
}

/// D366: the kernels' abort check (docs/gravity/driver.md §6) between the drain's sends. The post-run drain is the
/// leader alone on the backend's thread handing buffered claims to the client (~28 ms for 400 k rows), where no kernel
/// back-edge runs; a SIGINT / SIGTERM raises the backend's AbortPending byte at delivery, as PostgreSQL's executor sees
/// a cancel at the next tuple it sends. Unchecked, the statement completed and the cancel was dropped at idle.
/// Rows one push of a claim sends (D379): the drain's step ticks between two pushes, so a cancel landing while a large
/// claim is sent is seen within `SEND_ROWS` rows.
pub const SEND_ROWS: u64 = 1024;

/// The end of the `n` protocol messages (a type byte, then a big-endian length that counts itself) starting at `at`.
fn message_end(framed: &[u8], mut at: usize, n: u64) -> usize {
    // bounded("small-fixed: one header read per message of one push, at most SEND_ROWS (1024); `emit` ticks between pushes")
    for _ in 0..n {
        let len = u32::from_be_bytes(framed[at + 1..at + 5].try_into().expect("a message header"));
        at += 1 + len as usize;
    }
    at
}


/// The offsets `keep` left turned into pointers into `bytes` (the drain's copy).
fn point_into(v: &mut [Datum], n: &[u8], byref: &[bool], bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    gravity_driver::relocate(v, n, byref, (0, bytes.len()), bytes.as_ptr());
}

/// One DataRow message, `gravity_wire::frame_rows` for a single row: byte for byte what `WireNode::send_row` writes for text-format integer columns.
pub fn frame_row(out: &mut Vec<u8>, fmt: &WireFormat, v: &[Datum], n: &[u8]) {
    gravity_wire::frame_rows(out, fmt, v, n, 1);
}

pub struct LegSink<'a, S: Sink> {
    inner: &'a mut S,
    natts: usize,
    legs: ChargedVec<Leg>,
    through: u64,
    stopped: bool,
    hold: bool,
    sorted: bool,
}

impl<'a, S: Sink> LegSink<'a, S> {
    /// `width + 1` legs (their claim buffers grow as rows arrive) (ordinal `width` is the leader's own); `hold` keeps the leader's pushes in its leg too, for a
    /// drain that must order every row (a Sort root over a Sink pipeline). `sorted`: the drain orders the rows, so the legs keep rows; else, when the sink frames
    /// integer columns itself (`wire_format`), the workers' legs frame their rows as they arrive. `typlen`: per output column its
    /// `typlen` when by reference, 0 by value (`Shared::typlen`). `owner`: the ledger owner the legs' claim buffers are charged to
    /// (wave 2: the statement's, or the executing portal's); `None`: uncharged.
    #[allow(clippy::too_many_arguments)]
    pub fn new(inner: &'a mut S, natts: usize, width: usize, _cap_rows: usize, hold: bool, sorted: bool, typlen: &[i16], owner: Option<Owner>) -> LegSink<'a, S> {
        let wire = if hold || sorted { None } else { inner.wire_format() };
        let mut legs: ChargedVec<Leg> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        legs.with_vec(|v| v.reserve_exact(width + 1)); // alloc: Exec charged owner
        // the output columns' byref / typlen, built once for every leg (D4493: not once per leg)
        let mut byref: ChargedVec<bool> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        byref.with_vec(|v| { v.reserve_exact(typlen.len()); v.extend(typlen.iter().map(|&t| t != 0)) }); // alloc: Exec charged owner
        let mut lens: ChargedVec<i16> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        lens.with_vec(|v| { v.reserve_exact(typlen.len()); v.extend_from_slice(typlen) }); // alloc: Exec charged owner
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), bytes: AtomicU64::new(0), owner, byref, typlen: lens }); // alloc: Exec
        legs.with_vec(|v| v.resize_with(width + 1, || Leg::new(wire, shared.clone()))); // alloc: Exec charged owner
        LegSink { inner, natts, legs, through: 0, stopped: false, hold, sorted }
    }

    /// Rows that reached the real sink: the leader's own pushes plus the drained legs.
    pub fn through(&self) -> u64 {
        self.through
    }

    fn forward(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        let f = self.inner.push(b)?;
        self.through += b.rows as u64;
        if f == Flow::Stop {
            self.stop();
        }
        Ok(f)
    }

    fn stop(&mut self) {
        self.stopped = true;
        if let Some(l) = self.legs.first() {
            l.shared.stop.store(true, Ordering::Relaxed);
        }
    }

    /// One claim's rows (leg `i`'s buffer) through the real sink, `SEND_ROWS` rows a push with a tick between (D379):
    /// PostgreSQL checks per tuple, so a cancel landing while a large claim is sent is seen within `SEND_ROWS` rows, not at
    /// the next claim.
    fn emit(&mut self, mut b: ClaimBuf, i: usize) -> Result<()> {
        self.legs[i].shared.bytes.fetch_sub(b.size(), Ordering::Relaxed);
        // C9: the drain's step is on the backend's own abort byte (`glue::abort_u8`): it runs outside any kernel call, so no byte
        // is lent to it
        let mut step = cancelpoll::step_static(crate::glue::abort_u8(), "legs drain: a claim's rows sent");
        if self.legs[i].wire.is_some() {
            let mut at = 0;
            let mut left = b.rows;
            while left > 0 && !self.stopped {
                let n = left.min(SEND_ROWS);
                let end = message_end(&b.framed, at, n);
                let f = self.inner.push_wire(&b.framed[at..end], n)?;
                self.through += n;
                if f == Flow::Stop {
                    self.stop();
                }
                (at, left) = (end, left - n);
                if left > 0 {
                    step.tick()?;
                }
            }
            return Ok(());
        }
        point_into(&mut b.values, &b.isnull, &self.legs[i].shared.byref, &b.bytes);
        // the claim's own row count: a zero-column row (`SELECT FROM t`) stores no value to count by
        let (natts, rows) = (self.natts, b.rows as usize);
        let mut r = 0;
        while r < rows && !self.stopped {
            let n = (rows - r).min(SEND_ROWS as usize);
            let (lo, hi) = (r * natts, (r + n) * natts);
            self.forward(BorrowedBatch { natts, rows: n, values: &b.values[lo..hi], isnull: &b.isnull[lo..hi], region: &b.bytes })?;
            r += n;
            if r < rows {
                step.tick()?;
            }
        }
        Ok(())
    }

    /// Emit every claim's rows: in claim order (ascending `claim.lo`), or sorted ascending (NULLs last)
    /// by the output columns `keys`. Returns the rows emitted. A cancel or termination delivered meanwhile is seen before
    /// the next claim (the next row when sorted): `Error::Cancelled`, which the runner maps to the pending interrupt's
    /// ERROR; the claims not yet sent drop with the drain (D366). One more read after the last row (D377): PostgreSQL's
    /// executor checks again when it asks its plan for the row after the last, so a statement_timeout that lands while
    /// the last rows are sent ends THIS statement; unread, the abort byte, QueryCancelPending and the timeout's
    /// indicator stayed set and the connection's next statement failed with 57014 at its first check.
    pub fn drain(&mut self, keys: &[i16]) -> Result<u64> {
        let natts = self.natts;
        // C9: on the backend's own abort byte, as `emit`'s
        let mut step = cancelpoll::step_static(crate::glue::abort_u8(), "legs drain: the claims sent");
        let before = self.through;
        // the claims' lists and the drain's order, on the legs' owner as the claims are
        let o = self.legs.first().and_then(Leg::owner);
        let mut bufs: ChargedVec<(ClaimBuf, usize)> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        // one leg per worker ordinal and the leader's own (`LegSink::new`: width + 1), at most driver section MAX_WIDTH + 1 (256)
        for (i, leg) in self.legs.iter().enumerate() {
            let mut g = leg.lock();
            leg.settle(&mut g);
            let mut done = core::mem::take(&mut g.done);
            room(&mut bufs, done.len())?;
            for b in done.iter_mut() {
                step.tick()?;
                put(&mut bufs, (core::mem::take(b), i))?;
            }
        }
        // stable: pushes outside a claim (lo = MAX) keep their leg order (a claim's place in `bufs` breaks the tie). C9:
        // the claims' places are sorted by the polled sort (`cancelpoll::sort_by`), its merge's scratch past one sort run,
        // and the claims taken in that order
        let mut places: ChargedVec<(u64, u32)> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        room(&mut places, bufs.len())?;
        for (j, (b, _)) in bufs.iter().enumerate() {
            step.tick()?;
            put(&mut places, (b.lo, j as u32))?;
        }
        let mut scratch: ChargedVec<(u64, u32)> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        if places.len() > cancelpoll::SORT_RUN {
            scratch.resize(places.len(), (0, 0)).map_err(|_| refused::<(u64, u32)>(places.len()))?; // alloc: Exec charged the legs' owner
        }
        cancelpoll::sort_by(&mut step, &mut places, &mut scratch, |a, b| a.cmp(b))?;
        drop(scratch);
        let mut sorted: ChargedVec<(ClaimBuf, usize)> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        room(&mut sorted, places.len())?;
        for &(_, j) in places.iter() {
            step.tick()?;
            put(&mut sorted, core::mem::take(&mut bufs[j as usize]))?;
        }
        drop((places, bufs));
        if keys.is_empty() {
            for k in 0..sorted.len() {
                if self.stopped {
                    break;
                }
                step.tick()?;
                let (b, i) = core::mem::take(&mut sorted[k]);
                self.emit(b, i)?;
            }
            step.tick()?;
            return Ok(self.through - before);
        }
        let mut v: ChargedVec<Datum> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        let mut n: ChargedVec<u8> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        // the claims' regions, alive through the forwards
        let mut kept: ChargedVec<ChargedVec<u8>> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        room(&mut kept, sorted.len())?;
        for k in 0..sorted.len() {
            step.tick()?;
            let (b, i) = core::mem::take(&mut sorted[k]);
            let at = v.len();
            grow(&mut v, &b.values)?;
            grow(&mut n, &b.isnull)?;
            point_into(&mut v[at..], &n[at..], &self.legs[i].shared.byref, &b.bytes);
            put(&mut kept, b.bytes)?;
        }
        let rows = v.len() / natts.max(1);
        let mut order: ChargedVec<u32> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        room(&mut order, rows)?;
        order.with_vec(|x| x.extend(0..rows as u32)); // alloc: Exec charged the legs' owner (room reserved above)
        // C9: the rows' sort is the polled one (`cancelpoll::sort_by`), its merge's scratch past one sort run
        let mut scratch: ChargedVec<u32> = ChargedVec::new(o, Kind::Region); // alloc: Exec charged o
        if rows > cancelpoll::SORT_RUN {
            scratch.resize(rows, 0).map_err(|_| refused::<u32>(rows))?; // alloc: Exec charged the legs' owner
        }
        cancelpoll::sort_by(&mut step, &mut order, &mut scratch, |&a, &b| {
            let (a, b) = (a as usize * natts, b as usize * natts);
            // bounded("key-columns: one compare per sort key of two rows; PG polls no deeper (a datum compare is one unit there too), at most MaxTupleAttributeNumber (1664) keys")
            for &k in keys {
                let k = k as usize;
                let x = (n[a + k] != 0, v[a + k] as i64);
                let y = (n[b + k] != 0, v[b + k] as i64);
                match x.cmp(&y) {
                    core::cmp::Ordering::Equal => continue,
                    o => return o,
                }
            }
            core::cmp::Ordering::Equal
        })?;
        drop(scratch);
        for &i in order.iter() {
            if self.stopped {
                break;
            }
            step.tick()?;
            let at = i as usize * natts;
            self.forward(BorrowedBatch { natts, rows: 1, values: &v[at..at + natts], isnull: &n[at..at + natts], region: &[] })?;
        }
        step.tick()?;
        Ok(self.through - before)
    }
}

impl<S: Sink> Sink for LegSink<'_, S> {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        if self.hold {
            return self.legs.last().expect("the leader's leg").push(b);
        }
        self.forward(b)
    }

    fn ordered(&self) -> bool {
        !self.hold && !self.sorted
    }

    fn buffered(&self) -> u64 {
        self.legs.first().map_or(0, |l| l.shared.bytes.load(Ordering::Relaxed))
    }

    fn drain_ready(&mut self, los: &[u64]) -> Result<Flow> {
        for &lo in los {
            if self.stopped {
                break;
            }
            let found = self.legs.iter().enumerate().find_map(|(i, leg)| {
                let mut g = leg.lock();
                let j = g.done.iter().position(|b| b.lo == lo)?;
                Some((g.done.with_vec(|d| d.swap_remove(j)), i))
            });
            if let Some((b, i)) = found {
                self.emit(b, i)?;
            }
        }
        Ok(if self.stopped { Flow::Stop } else { Flow::Continue })
    }

    fn leg(&self, ordinal: u16) -> Option<LegRef<'_>> {
        self.legs.get(ordinal as usize).map(LegRef::of)
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_driver::MAX_WIRE_ATTS;

    struct Keep(Vec<(u64, u8)>, Vec<Vec<u8>>);

    impl Sink for Keep {
        fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
            for (v, n) in b.iter() {
                self.0.push((v[0], n[0]));
                self.0.push((v[1], n[1]));
                // the by-reference column read at the push, as a wire sink would
                self.1.push(if n[1] == 0 { unsafe { core::slice::from_raw_parts(v[1] as *const u8, 8) }.to_vec() } else { Vec::new() }); // alloc: Cold
            }
            Ok(Flow::Continue)
        }
    }

    /// D379: a framed claim is sent `SEND_ROWS` DataRow messages at a time; `message_end` finds the boundary by the messages' own lengths.
    #[test]
    fn message_end_steps_over_whole_data_rows() {
        let fmt = WireFormat { natts: 1, kinds: { let mut k = [0u8; MAX_WIRE_ATTS]; k[0] = 8; k } };
        let mut framed = Vec::new(); // alloc: Cold
        let v: Vec<u64> = (0..5u64).map(|i| 10u64.pow(i as u32)).collect(); // alloc: Cold
        gravity_wire::frame_rows(&mut framed, &fmt, &v, &[0; 5], 5);
        let mut one = Vec::new(); // alloc: Cold
        gravity_wire::frame_rows(&mut one, &fmt, &v[..2], &[0; 2], 2);
        assert_eq!(message_end(&framed, 0, 2), one.len());
        assert_eq!(message_end(&framed, one.len(), 3), framed.len());
        assert_eq!(message_end(&framed, 0, 0), 0);
    }

    /// lower.md §15.13: a worker's leg keeps the block's region bytes with its rows (the block is reused
    /// after the push), and the drain hands the sink datums pointing at the leg's copy.
    #[test]
    fn a_leg_ships_the_by_reference_region_with_its_rows() {
        let mut keep = Keep(Vec::new(), Vec::new()); // alloc: Cold
        let mut legs = LegSink::new(&mut keep, 2, 1, 4, false, false, &[0, 8], None);
        let mut region = *b"abcdefghXXXXXXXX";
        let v = [7u64, region.as_ptr() as u64, 8, 0];
        let n = [0u8, 0, 0, 1];
        SinkLeg::push(&legs.legs[0], BorrowedBatch { natts: 2, rows: 2, values: &v, isnull: &n, region: &region[..8] }).unwrap();
        region.fill(b'!');
        let mut region2 = *b"ijklmnopXXXXXXXX";
        let v2 = [9u64, region2.as_ptr() as u64];
        SinkLeg::push(&legs.legs[0], BorrowedBatch { natts: 2, rows: 1, values: &v2, isnull: &[0, 0], region: &region2[..8] }).unwrap();
        region2.fill(b'?');
        assert_eq!(legs.drain(&[]).unwrap(), 3);
        assert_eq!(keep.0.iter().map(|r| r.0).step_by(2).collect::<Vec<_>>(), [7, 8, 9]);
        assert_eq!(keep.1, [b"abcdefgh".to_vec(), Vec::new(), b"ijklmnop".to_vec()]); // alloc: Cold
    }

    /// Wave 2: a claim's buffers are on the legs' owner from the worker's push (the worker serving its leader's account, as a
    /// claim does) to the leader's emit, and freed there: the owner ends at 0.
    #[test]
    fn a_claims_buffers_are_on_the_legs_owner_until_the_drain() {
        std::thread::spawn(|| {
            memledger::bind_backend(3967, 8167);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let mut keep = Keep(Vec::new(), Vec::new()); // alloc: Cold
            {
                let mut legs = LegSink::new(&mut keep, 2, 1, 0, false, false, &[0, 8], Some(o));
                // the legs' own lists (legs, byref, typlen) are on the owner too, until the sink drops
                let base = memledger::charge::owner_row(o).committed;
                let region = *b"abcdefgh";
                let leg = &legs.legs[0];
                std::thread::scope(|sc| {
                    sc.spawn(move || {
                        let _serving = memledger::charge::enter_as(o);
                        leg.claim_begin(MorselRange::new(0, 10));
                        for i in 0..1000u64 {
                            SinkLeg::push(leg, BorrowedBatch { natts: 2, rows: 1, values: &[i, region.as_ptr() as u64], isnull: &[0, 0], region: &region }).unwrap();
                        }
                        leg.claim_end(true);
                    });
                });
                let held = memledger::charge::owner_row(o).committed;
                assert!(held >= 1000 * (16 + 2 + 8), "the claim's values, nulls and images are on the owner: {held}");
                assert_eq!(legs.drain(&[]).unwrap(), 1000);
                assert_eq!(memledger::charge::owner_row(o).committed, base, "emitted and freed on the leader");
            }
            assert_eq!(memledger::charge::owner_row(o).committed, 0, "the legs' lists given back at the sink's drop");
            assert_eq!(keep.1[999], b"abcdefgh".to_vec()); // alloc: Cold
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// D4493: the width + 1 legs share ONE copy of the output columns' byref / typlen, on the legs' owner: the owner holds
    /// the legs list plus that one copy (not one per leg), and 0 after the sink drops.
    #[test]
    fn the_legs_share_one_byref_and_typlen_copy_on_the_owner() {
        std::thread::spawn(|| {
            memledger::bind_backend(3971, 8171);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let typlen: Vec<i16> = (0..100).map(|i| if i % 2 == 0 { 0 } else { -1 }).collect(); // alloc: Cold
            let mut keep = Keep(Vec::new(), Vec::new()); // alloc: Cold
            {
                let legs = LegSink::new(&mut keep, 100, 7, 0, false, false, &typlen, Some(o));
                assert_eq!(legs.legs.len(), 8);
                assert!(legs.legs.iter().all(|l| Arc::ptr_eq(&l.shared, &legs.legs[0].shared)), "every leg holds the one Shared");
                let sh = &legs.legs[0].shared;
                assert_eq!((sh.byref.len(), sh.typlen.len()), (100, 100));
                let copy = (sh.byref.charged_bytes() + sh.typlen.charged_bytes()) as i64;
                let list = legs.legs.charged_bytes() as i64;
                assert!(copy >= 100 + 200, "byref and typlen are on the owner: {copy}");
                let held = memledger::charge::owner_row(o).committed - list;
                assert!(held >= copy && held < 2 * copy, "the legs list and ONE byref / typlen copy, not one per leg (8): {held} vs {copy}");
            }
            assert_eq!(memledger::charge::owner_row(o).committed, 0, "the sink's drop gives them back");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// D2985: a selective filter's batch keeps 2 rows of a block whose region (256 KiB) holds every scanned row's images:
    /// the leg keeps the two images alone, not the region, and the drain still hands the sink their bytes.
    #[test]
    fn a_sparse_region_keeps_only_the_rows_images() {
        let mut keep = Keep(Vec::new(), Vec::new()); // alloc: Cold
        let mut legs = LegSink::new(&mut keep, 2, 1, 4, false, false, &[0, 8], None);
        let mut region = vec![b'.'; 256 << 10]; // alloc: Cold
        region[1000..1008].copy_from_slice(b"keepme01");
        region[200_000..200_008].copy_from_slice(b"keepme02");
        let v = [1u64, region[1000..].as_ptr() as u64, 2, region[200_000..].as_ptr() as u64];
        SinkLeg::push(&legs.legs[0], BorrowedBatch { natts: 2, rows: 2, values: &v, isnull: &[0, 0, 0, 0], region: &region }).unwrap();
        legs.legs[0].claim_end(true);
        region.fill(gravity_driver::POISON);
        assert!(legs.buffered() < 1024, "{} bytes kept for two 8-byte images", legs.buffered());
        assert_eq!(legs.drain(&[]).unwrap(), 2);
        assert_eq!(keep.1, [b"keepme01".to_vec(), b"keepme02".to_vec()]); // alloc: Cold
    }

    /// A dense region (every row's images) is still kept whole.
    #[test]
    fn a_dense_region_is_kept_whole() {
        let region = vec![b'x'; 128 << 10]; // alloc: Cold
        let v: Vec<u64> = (0..128u64).flat_map(|i| [i, region[(i as usize) * 1024..].as_ptr() as u64]).collect(); // alloc: Cold
        assert!(!sparse_region(&v, &[0; 256], &[0, 1024], 128, region.len()), "128 rows of 1 KiB images fill the region");
    }

    /// R1D: four workers' claims settle out of order; the drain emits them in claim order (ascending `lo`), each claim's
    /// by-reference bytes its own copy though the worker's block and region are poisoned at every claim's end.
    #[test]
    fn claims_drain_in_claim_order_with_their_own_bytes() {
        let mut keep = Keep(Vec::new(), Vec::new()); // alloc: Cold
        let mut legs = LegSink::new(&mut keep, 2, 4, 0, false, false, &[0, 8], None);
        let mut region = [0u8; 16];
        for (w, lo) in [(2usize, 30u64), (0, 10), (3, 0), (1, 20), (0, 40)] {
            let leg = &legs.legs[w];
            leg.claim_begin(MorselRange::new(lo, lo + 10));
            region[..8].copy_from_slice(format!("row{lo:05}").as_bytes()); // alloc: Cold
            let v = [lo, region.as_ptr() as u64];
            SinkLeg::push(leg, BorrowedBatch { natts: 2, rows: 1, values: &v, isnull: &[0, 0], region: &region[..8] }).unwrap();
            leg.claim_end(true);
            region.fill(gravity_driver::POISON);
        }
        legs.legs[1].claim_begin(MorselRange::new(50, 60));
        legs.legs[1].claim_end(true);
        assert_eq!(legs.drain(&[]).unwrap(), 5, "an empty claim adds no rows");
        assert_eq!(keep.0.iter().map(|r| r.0).step_by(2).collect::<Vec<_>>(), [0, 10, 20, 30, 40]);
        assert_eq!(keep.1, [0, 10, 20, 30, 40].map(|lo: u64| format!("row{lo:05}").into_bytes())); // alloc: Cold
    }

    /// R1D (c): once the receiver stops (a LIMIT met in claim order) every worker's next push is a stop, so running claims
    /// end early instead of scanning to their end.
    #[test]
    fn the_receivers_stop_reaches_every_worker() {
        struct Two(u64);
        impl Sink for Two {
            fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
                self.0 += b.rows as u64;
                Ok(if self.0 >= 2 { Flow::Stop } else { Flow::Continue })
            }
        }
        let mut two = Two(0);
        let mut legs = LegSink::new(&mut two, 1, 2, 0, false, false, &[0], None);
        for (w, lo) in [(0usize, 0u64), (1, 1), (0, 2)] {
            legs.legs[w].claim_begin(MorselRange::new(lo, lo + 1));
            assert_eq!(SinkLeg::push(&legs.legs[w], BorrowedBatch { natts: 1, rows: 1, values: &[lo], isnull: &[0], region: &[] }).unwrap(), Flow::Continue);
            legs.legs[w].claim_end(true);
        }
        assert_eq!(legs.drain_ready(&[0, 1, 2]).unwrap(), Flow::Stop);
        assert_eq!(legs.through(), 2, "the rows through the stop, no more");
        legs.legs[1].claim_begin(MorselRange::new(3, 4));
        assert_eq!(SinkLeg::push(&legs.legs[1], BorrowedBatch { natts: 1, rows: 1, values: &[3], isnull: &[0], region: &[] }).unwrap(), Flow::Stop);
    }

    /// D342: a zero-column row (`SELECT FROM t`) stores no value to count by; each worker's claim reaches a receiver that is not
    /// the wire (a CREATE TABLE AS, EXPLAIN ANALYZE) with its own row count.
    #[test]
    fn a_zero_column_claim_keeps_its_row_count() {
        struct Count(u64);
        impl Sink for Count {
            fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
                self.0 += b.iter().count() as u64;
                Ok(Flow::Continue)
            }
        }
        let mut count = Count(0);
        let mut legs = LegSink::new(&mut count, 0, 2, 0, false, false, &[], None);
        for (w, lo, rows) in [(1usize, 10u64, 3usize), (0, 0, 5)] {
            legs.legs[w].claim_begin(MorselRange::new(lo, lo + 10));
            SinkLeg::push(&legs.legs[w], BorrowedBatch { natts: 0, rows, values: &[], isnull: &[], region: &[] }).unwrap();
            legs.legs[w].claim_end(true);
        }
        assert_eq!(legs.drain(&[]).unwrap(), 8);
        assert_eq!(count.0, 8, "every claim's rows reach the receiver");
    }

    /// D470: a by-reference datum outside the block's region (into a page or scratch the worker releases) has its image
    /// copied into the claim's bytes, the drain's datum pointing at the copy; a debug build asserts (a producer to find).
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "outside its block region"))]
    fn a_datum_outside_its_block_region_is_copied() {
        let mut keep = Keep(Vec::new(), Vec::new()); // alloc: Cold
        let before = OUTSIDE.load(Ordering::Relaxed);
        {
            let mut legs = LegSink::new(&mut keep, 2, 1, 0, false, false, &[0, 8], None);
            let (mut page, region) = (*b"pagebyte", *b"regionby");
            legs.legs[0].claim_begin(MorselRange::new(0, 1));
            SinkLeg::push(&legs.legs[0], BorrowedBatch { natts: 2, rows: 1, values: &[1, page.as_ptr() as u64], isnull: &[0, 0], region: &region }).unwrap();
            legs.legs[0].claim_end(true);
            page.fill(b'!');
            assert_eq!(legs.drain(&[]).unwrap(), 1);
        }
        assert!(OUTSIDE.load(Ordering::Relaxed) > before, "counted");
        assert_eq!(keep.1, [b"pagebyte".to_vec()], "the drain reads the copy, not the released page"); // alloc: Cold
    }

    /// D470: `keep`'s image lengths are `VARSIZE_ANY`'s (1-byte header, 4-byte header) and a fixed type's `typlen`.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "outside its block region"))]
    fn an_outside_image_is_copied_at_its_length() {
        let short = [(5u8 << 1) | 1, b'a', b'b', b'c', b'd', 0xee];
        let long = [(8u32 << 2).to_le_bytes(), *b"wxyz"].concat();
        let cstr = *b"hi\0zz";
        unsafe {
            assert_eq!(raw::image_len(short.as_ptr(), -1), 5);
            assert_eq!(raw::image_len(long.as_ptr(), -1), 8);
            assert_eq!(raw::image_len([1u8, 18].as_ptr(), -1), 18);
            assert_eq!(raw::image_len(cstr.as_ptr(), -2), 3);
            assert_eq!(raw::image_len(short.as_ptr(), 16), 16);
        }
        let mut bytes = ChargedVec::none();
        bytes.extend_from_slice(&[7u8; 3]).unwrap();
        let mut v = [short.as_ptr() as u64, 42];
        keep(&mut v, &[0, 0], &[-1, 0], (0, 0), 0, &mut bytes).unwrap();
        assert_eq!((v, &bytes[8..]), ([8, 42], &short[..5]), "8-aligned after what the claim held; a by-value word untouched");
    }

    /// `frame_row` writes what `WireNode::send_row` puts on the wire for text-format integers:
    /// `'D'`, int32 length (itself included, the type byte not), int16 columns, then per column
    /// `-1` or the digits' int32 length and the digits.
    #[test]
    fn a_framed_row_is_the_datarow_message_byte_for_byte() {
        let mut kinds = [0u8; MAX_WIRE_ATTS];
        kinds[..4].copy_from_slice(&[4, 8, 2, 4]);
        let fmt = WireFormat { natts: 4, kinds };
        let mut out = Vec::new(); // alloc: Cold
        frame_row(&mut out, &fmt, &[1, (-5i64) as u64, (-32768i16 as u16) as u64, (i32::MIN as u32) as u64], &[0, 0, 0, 1]);
        let mut want = vec![b'D']; // alloc: Cold
        let body: Vec<u8> = [&0u16.to_be_bytes()[..0], &4u16.to_be_bytes(), &1u32.to_be_bytes(), b"1", &2u32.to_be_bytes(), b"-5", &6u32.to_be_bytes(), b"-32768", &(-1i32).to_be_bytes()].concat();
        want.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        want.extend_from_slice(&body);
        assert_eq!(out, want);
        // two rows append two messages; the second starts where the first's length says
        frame_row(&mut out, &fmt, &[2147483647, 9223372036854775807u64, 7, 0], &[0, 0, 0, 0]);
        let first = 1 + u32::from_be_bytes([out[1], out[2], out[3], out[4]]) as usize;
        assert_eq!(out[first], b'D');
        assert!(out[first..].ends_with(b"0"));
        assert!(std::str::from_utf8(&out[first..]).unwrap().contains("2147483647"));
        assert!(std::str::from_utf8(&out[first..]).unwrap().contains("9223372036854775807"));
    }
}
