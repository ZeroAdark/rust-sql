//! `setop.recursive`'s generation object (W8-RECURSE; `ProtoKind::Generation`, lowerv4 setop/recursive.rs; its layout is
//! kernels host_abi/v4/m9.rs's `GenCell`). PostgreSQL's RecursiveUnion (nodeRecursiveunion.c) hands each term row on as
//! the term produces it, so a term error surfaces at the pull after the last good row. The lowering runs a generation's
//! term whole (`m8_rerun`): an error it raises is kept here (`m9_gen_hold`) while the rows the term appended before it
//! are returned, then raised (`m9_gen_raise`); a reader that stops earlier never sees it, as PostgreSQL's never pulls it.
//!
//! Only an ordinary ERROR PostgreSQL raises at that term row is kept (F1). Passed straight through, raised at once: a
//! cancel, a statement timeout or a shutdown (class 57), a system or I/O error, spill I/O included (58), a resource error
//! (53), an internal error or Bug (XX), a run-time refusal (0A000 from `Error::Refused`, `runner::refusal_of`), any level
//! above ERROR, and no stashed error at all (a parked unwind, V4R-16 / D310: the program's caller resumes it). The
//! landed precedent is m8_init.rs's `late_words` (a cancel is never kept).
//!
//! A recursive UNION (setop P7, D1378) keeps PG's hash table of the rows seen here (`Seen`, `OP_GEN_SEEN`): the IR offers
//! each seed and term row's key image before the row enters its table and drops a row whose image is held. The images
//! are exact (equal words are equal keys under the union's equality, NULLs equal: lowerv4 setop/recursive.rs refuses an
//! inexact one by name), so a hit compares words only. Like PG's table it lives to the execution's end and never spills;
//! its memory is the process allocator's, fallible (53200). Each growth of its capacity is billed to the grant pool as an
//! overdraft (driver `object_overdraw`, `Overdraw::Seen`: never refused, counted past G on the statement), held until the
//! set empties or drops, so admission sees what it holds (D1378).
//!
//! The object lives in the protocol's `Live`, made at each execution's prepare (its handle laid in its word) and dropped at
//! its end. Created by W8-RECURSE (owner M9-HOSTS).
use core::cell::RefCell;
use std::collections::HashMap;

use gravity_abi::status::ERROR;
use gravity_fn::arena::{Charge, Door, Kind, Owner, Release};
use gravity_kernels::ops::setop::entries as abi;
use gravity_program::protos::{Proto, ProtoKind};
use types_error::{PgError, PgResult};

use super::super::{Live, Prepare, Protocol};
use crate::glue;

pub(crate) struct Gen;

/// The object's words: the cell (head, held word), then the address of its state.
pub(super) const WORDS: usize = core::mem::size_of::<abi::GenCell>() / 8 + 1;

pub(super) struct GenRt {
    held: RefCell<Option<Box<PgError>>>,
    seen: RefCell<Seen>,
}

/// A recursive UNION's rows seen so far, by key image: each image's words once in `words` (every image `n` words, fixed by
/// the first), chained from its KeyHash's head in `heads` through `next` (entry `i`'s successor + 1; 0 ends a chain);
/// `billed` of its capacity's bytes (`bytes`) held in the pool by `holds` (`bill`).
///
/// CONVERT wave 2 (site 2): those bytes are also on the ledger, `charge` following `bytes` exactly (`sync`, at each growth:
/// the vectors and the table grow by doubling), charged to the owner `own` gives at the first new image (the slot the
/// union's pipeline produces into, else the statement's: no breaker holds a recursive UNION's table); `charged` of them
/// committed. Declared first, so dropped first (door 1: the bytes move to the freeing thread's account, where the frees land).
#[derive(Default)]
struct Seen {
    charge: Option<Charge>,
    charged: u64,
    /// `own` ran (with `None`: charged to no one; a unit test's set).
    owned: bool,
    n: usize,
    words: Vec<u64>,
    next: Vec<u32>,
    heads: HashMap<u64, u32>,
    billed: u64,
    holds: Vec<gravity_driver::Held>,
}

/// A `heads` entry's bytes in the table: its key and value padded, its control byte and the load factor's spare slots.
const HEAD_BYTES: u64 = 20;

impl Seen {
    /// Whether image `img` under KeyHash `h` is new: then held, `Ok(true)`; `Ok(false)` when an equal image is held.
    fn offer(&mut self, h: u64, img: &[u64]) -> PgResult<bool> {
        if self.next.is_empty() {
            self.n = img.len();
        } else if img.len() != self.n {
            return Err(bad(&format!("m9_gen_seen: an image of {} words where the union's are {}", img.len(), self.n))); // alloc: Cold
        }
        let mut at = self.heads.get(&h).copied().unwrap_or(0);
        while at != 0 {
            let i = at as usize - 1;
            if self.words.get(i * self.n..(i + 1) * self.n) == Some(img) {
                return Ok(false);
            }
            at = self.next.get(i).copied().unwrap_or(0);
        }
        let Ok(id) = u32::try_from(self.next.len() + 1) else { return Err(bad("m9_gen_seen: more than 2^32 - 1 distinct rows")) };
        if self.words.try_reserve(img.len()).is_err() || self.next.try_reserve(1).is_err() || self.heads.try_reserve(1).is_err() {
            return Err(oom(8 * img.len() + 16));
        }
        self.words.extend_from_slice(img); // alloc: Exec
        let prev = self.heads.insert(h, id).unwrap_or(0); // alloc: Exec
        self.next.push(prev); // alloc: Exec
        Ok(true)
    }

    /// D3374 (e): whether image `img` under KeyHash `h` is held (`offer`'s walk, nothing kept).
    fn holds(&self, h: u64, img: &[u64]) -> bool {
        let mut at = self.heads.get(&h).copied().unwrap_or(0);
        while at != 0 {
            let i = at as usize - 1;
            if img.len() == self.n && self.words.get(i * self.n..(i + 1) * self.n) == Some(img) {
                return true;
            }
            at = self.next.get(i).copied().unwrap_or(0);
        }
        false
    }

    /// The owner the set's bytes are charged to from now, with what it holds already.
    fn own(&mut self, owner: Option<(Owner, Kind)>) {
        self.owned = true;
        if let Some((o, k)) = owner {
            self.charge = Some(Charge::open(o, Release::Statement, k, Door::Heap, 0));
            self.sync();
        }
    }

    /// The charge re-synced to `bytes`: a commit of the growth this thread just allocated (nothing shrinks before the drop).
    fn sync(&mut self) {
        let b = self.bytes();
        if let Some(c) = self.charge.as_mut().filter(|_| b > self.charged) {
            c.commit(b - self.charged);
            self.charged = b;
        }
    }

    /// The bytes its capacity holds (the vectors' and the table's, each grown by doubling).
    fn bytes(&self) -> u64 {
        8 * self.words.capacity() as u64 + 4 * self.next.capacity() as u64 + HEAD_BYTES * self.heads.capacity() as u64
    }

    /// Its capacity's growth since the last bill billed (`overdraw`: the pool hold of that many bytes; `None` where there is
    /// no execution to bill, a unit test's object), the hold kept to the set's end. Panic-free as `op`: a hold the list has no
    /// room for goes back at once (the bill is skipped, never an abort).
    fn bill(&mut self, overdraw: &mut dyn FnMut(u64) -> Option<gravity_driver::Held>) {
        let b = self.bytes();
        if b > self.billed && self.holds.try_reserve(1).is_ok() {
            if let Some(h) = overdraw(b - self.billed) {
                self.holds.push(h); // alloc: Exec (reserved above)
            }
            self.billed = b;
        }
    }
}

struct GenLive {
    _cell: Box<[u64]>,
    rt: Box<GenRt>,
    /// The union's driver pipeline (the proto's).
    pipeline: u16,
    /// The cell's held word (`abi::GEN_HELD`), which the IR reads.
    held_word: super::raw::HeldWord,
}

impl Live for GenLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::REOPEN
    }

    /// setop_rec-0043: the driver re-armed with its CTE (a correlated SubPlan's or a nest's rerun set re-runs the whole
    /// plan; exec ops/ctescan/proto.rs `rearm`): nothing held and no row seen, as PG's ExecReScanRecursiveUnion empties its
    /// hash table and tables.
    fn reopen(&self, members: &[u16]) -> PgResult<()> {
        if members.contains(&self.pipeline) {
            let Ok(mut held) = self.rt.held.try_borrow_mut() else { return Err(bad("setop.recursive: its object re-armed inside its own call")) };
            held.take();
            self.held_word.clear();
            *self.rt.seen.borrow_mut() = Seen::default();
        }
        Ok(())
    }
}

impl Protocol for Gen {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Generation
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let base = p.words.base as usize;
        if p.words.n != 1 || base >= cx.words.len() || base >= cx.nulls.len() {
            return Err(bad("setop.recursive's declaration is not its object's (one handle word)"));
        }
        let (mut cell, rt) = lay();
        let held_word = super::raw::HeldWord::of(&mut cell);
        cx.words[base] = handle(&cell);
        cx.nulls[base] = 0;
        *cx.nparams = (*cx.nparams).max(base + 1);
        Ok(Some(Box::new(GenLive { _cell: cell, rt, pipeline: p.pipeline, held_word }))) // alloc: Exec
    }
}

/// A fresh object: its cell (nothing held) and its state, both kept by the `Live` to the execution's end.
fn lay() -> (Box<[u64]>, Box<GenRt>) {
    let rt = Box::new(GenRt { held: RefCell::new(None), seen: RefCell::new(Seen::default()) }); // alloc: Exec
    let cell = super::raw::lay_cell(&*rt as *const GenRt as u64);
    (cell, rt)
}

fn handle(cell: &[u64]) -> u64 {
    cell.as_ptr() as u64
}

/// F1: an error PostgreSQL raises at the term row that raised it, kept until the rows before it are returned.
fn holdable(e: &PgError) -> bool {
    use types_error::{errcode_to_category as class, ERRCODE_INSUFFICIENT_RESOURCES, ERRCODE_INTERNAL_ERROR, ERRCODE_QUERY_CANCELED, ERRCODE_SYSTEM_ERROR};
    let c = class(e.sqlstate());
    e.level() == types_error::ERROR
        && ![ERRCODE_QUERY_CANCELED, ERRCODE_SYSTEM_ERROR, ERRCODE_INSUFFICIENT_RESOURCES, ERRCODE_INTERNAL_ERROR].iter().any(|&k| class(k) == c)
        && crate::runner::refusal_of(e).is_none()
}

/// The object's operation (raw.rs `gen_op`, its `Head::op`, reads the cell): `OP_GEN_HOLD` / `OP_GEN_RAISE` /
/// `OP_GEN_CHECK` / `OP_GEN_SEEN` (`img`: the image words raw.rs `image` read, `n` of them past the KeyHash). Panic-free
/// (its hosts run under no `guard!`, crash policy): no unwrap, expect or indexing; a re-entered object is an error by name.
pub(super) fn op(rt: &GenRt, held_word: &super::raw::HeldWord, code: u64, img: Option<&[u64]>, n: u64, overdraw: &mut dyn FnMut(u64) -> Option<gravity_driver::Held>, owner: &dyn Fn() -> Option<(Owner, Kind)>) -> i64 {
    if code == abi::OP_GEN_SEEN {
        return seen(rt, img, n, overdraw, owner);
    }
    if code == abi::OP_GEN_HAS {
        return has(rt, img, n);
    }
    let Ok(mut held) = rt.held.try_borrow_mut() else { return fail(bad("setop.recursive: its object entered inside its own call")) };
    match code {
        abi::OP_GEN_HOLD => match gravity_fn::take_error() {
            // no error stashed (a parked unwind resumes past the program): nothing kept, the IR raises at once
            None => ERROR,
            Some(e) if holdable(&e) => {
                *held = Some(e);
                held_word.set(1);
                0
            }
            Some(e) => fail(e),
        },
        abi::OP_GEN_RAISE => match held.take() {
            Some(e) => {
                held_word.set(0);
                fail(e)
            }
            None => fail(bad("m9_gen_raise: no kept error")),
        },
        // D2231: the runner's D743 check (a statement_timeout past its deadline fires though the timer thread has not run),
        // then CHECK_FOR_INTERRUPTS
        abi::OP_GEN_CHECK => match crate::runner::check_after_teardown() {
            Ok(()) => 0,
            Err(e) => fail(e),
        },
        _ => fail(bad(&format!("setop.recursive has no operation {code}"))), // alloc: Cold
    }
}

/// `OP_GEN_SEEN`: 1 when the image `words` (its KeyHash first, then `n` words: exec raw.rs `image`) is new, 0 when seen; no
/// words (a null address, no image word) or a failed allocation is an error by name. A new image's growth of the set's
/// capacity is billed (`Seen::bill`, through `overdraw`) and charged (`Seen::sync`, to `owner`'s answer at the set's first
/// new image).
fn seen(rt: &GenRt, words: Option<&[u64]>, n: u64, overdraw: &mut dyn FnMut(u64) -> Option<gravity_driver::Held>, owner: &dyn Fn() -> Option<(Owner, Kind)>) -> i64 {
    let Some((&h, img)) = words.and_then(|w| w.split_first()) else { return fail(bad(&format!("m9_gen_seen: no image of {n} words"))) }; // alloc: Cold
    let Ok(mut set) = rt.seen.try_borrow_mut() else { return fail(bad("setop.recursive: its set entered inside its own call")) };
    match set.offer(h, img) {
        Ok(true) => {
            set.bill(overdraw);
            if !set.owned {
                set.own(owner());
            }
            set.sync();
            1
        }
        Ok(false) => 0,
        Err(e) => fail(e),
    }
}

/// D3374 (e) (`OP_GEN_HAS`): 1 when the set holds the image `words` (its KeyHash first, then `n` words), 0 when not;
/// nothing is kept. No words is an error by name.
fn has(rt: &GenRt, words: Option<&[u64]>, n: u64) -> i64 {
    let Some((&h, img)) = words.and_then(|w| w.split_first()) else { return fail(bad(&format!("m9_gen_has: no image of {n} words"))) }; // alloc: Cold
    let Ok(set) = rt.seen.try_borrow() else { return fail(bad("setop.recursive: its set entered inside its own call")) };
    i64::from(set.holds(h, img))
}

#[cold]
#[inline(never)]
fn oom(bytes: usize) -> Box<PgError> {
    Box::new(PgError::error("out of memory".to_string()).with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY).with_detail(format!("Failed on request of size {bytes} in a recursive UNION's table."))) // alloc: Cold
}

#[cold]
#[inline(never)]
fn fail(e: Box<PgError>) -> i64 {
    gravity_fn::stash_error(e);
    ERROR
}

#[cold]
#[inline(never)]
fn bad(what: &str) -> Box<PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use types_error::SqlState;

    use super::*;

    fn err(state: SqlState, msg: &str) -> Box<PgError> {
        Box::new(PgError::error(msg).with_sqlstate(state)) // alloc: Cold
    }

    /// Hold `e` (stashed as `m8_rerun` stashes it) on a fresh object: the hold's status, the held word, the error left
    /// stashed after the hold (its message) and the object.
    fn hold(e: Option<Box<PgError>>) -> (i64, u64, Option<String>, (Box<[u64]>, Box<GenRt>)) {
        let o = lay();
        let h = handle(&o.0);
        let _ = gravity_fn::take_error();
        if let Some(e) = e {
            gravity_fn::stash_error(e);
        }
        // SAFETY: a live object.
        let st = unsafe { abi::m9_gen_hold(core::ptr::null_mut(), h) };
        let stashed = gravity_fn::take_error().map(|e| e.message().to_owned());
        (st, o.0[abi::GEN_HELD / 8], stashed, o)
    }

    #[test]
    fn an_ordinary_term_error_is_kept_and_raised_later_once() {
        let (st, held, stashed, o) = hold(Some(err(types_error::ERRCODE_DIVISION_BY_ZERO, "division by zero")));
        assert_eq!((st, held, stashed), (0, 1, None), "kept: nothing left stashed");
        let h = handle(&o.0);
        // SAFETY: a live object.
        assert_eq!(unsafe { abi::m9_gen_raise(core::ptr::null_mut(), h) }, ERROR);
        let e = gravity_fn::take_error().expect("the kept error stashed for the raise");
        assert_eq!((e.message(), e.sqlstate()), ("division by zero", types_error::ERRCODE_DIVISION_BY_ZERO));
        assert_eq!(o.0[abi::GEN_HELD / 8], 0);
        // SAFETY: as above.
        assert_eq!(unsafe { abi::m9_gen_raise(core::ptr::null_mut(), h) }, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("no kept error")));
    }

    /// A PostgreSQL 0A000 that is no run-time refusal is an ordinary error.
    #[test]
    fn a_feature_error_that_is_no_refusal_is_kept() {
        let (st, held, _, _) = hold(Some(err(types_error::ERRCODE_FEATURE_NOT_SUPPORTED, "cannot do that")));
        assert_eq!((st, held), (0, 1));
    }

    /// F1: never kept, left stashed for the IR's raise at once, the object untouched.
    #[test]
    fn a_cancel_a_system_resource_or_internal_error_a_refusal_or_a_fatal_passes_straight_through() {
        let fatal = Box::new(PgError::new(types_error::FATAL, "terminating connection")); // alloc: Cold
        let cases = [
            err(types_error::ERRCODE_QUERY_CANCELED, "canceling statement due to statement timeout"),
            err(types_error::ERRCODE_IO_ERROR, "could not read spill file"),
            err(types_error::ERRCODE_OUT_OF_MEMORY, "out of memory"),
            err(types_error::ERRCODE_INTERNAL_ERROR, "gravity: Bug: pipeline 2 consumes slot 0 that is not live"),
            err(types_error::ERRCODE_DATA_CORRUPTED, "invalid page"),
            crate::runner::refused("graph: a rerun member consumes a live slot past 63"),
            fatal,
        ];
        for e in cases {
            let msg = e.message().to_owned();
            let (st, held, stashed, _) = hold(Some(e));
            assert_eq!((st, held), (ERROR, 0), "{msg}: never kept");
            assert_eq!(stashed.as_deref(), Some(msg.as_str()), "{msg}: left stashed for the raise");
        }
    }

    /// D1378: `seen` over a fresh object: new images are held once, an equal image (same words, any KeyHash chain) is seen,
    /// a NULL flags word that differs makes another image, and a width the union's are not is an error by name.
    #[test]
    fn a_union_holds_each_image_once_and_drops_a_seen_one() {
        let (_cell, rt) = lay();
        let offer = |words: &[u64]| seen(&rt, Some(words), words.len() as u64 - 1, &mut |_| None, &|| None);
        assert_eq!(offer(&[7, 1, 0]), 1, "new");
        assert_eq!(offer(&[7, 1, 0]), 0, "seen");
        assert_eq!(offer(&[7, 2, 0]), 1, "a hash collision with other words is new");
        assert_eq!(offer(&[7, 2, 0]), 0, "and seen through the chain");
        assert_eq!(offer(&[9, 0, 1]), 1, "a NULL key (flags 1) is not the zero value");
        assert_eq!(offer(&[9, 0, 1]), 0, "two NULL keys are one row (PG's grouping equality)");
        let _ = gravity_fn::take_error();
        assert_eq!(offer(&[7, 1]), ERROR, "another width");
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("an image of 1 words where the union's are 2")));
        // SAFETY: a null address is refused before any read.
        assert_eq!(seen(&rt, unsafe { super::super::raw::image(core::ptr::null(), 1) }, 1, &mut |_| None, &|| None), ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m9_gen_seen: no image of 1 words")));
    }

    /// D1378: each growth of the set's capacity is billed once, by the bytes it grew (the pool's
    /// overdraft, here a counter); a seen image grows nothing and bills nothing; the bills add up to the set's bytes.
    #[test]
    fn a_union_bills_each_growth_of_its_set_once() {
        let (_cell, rt) = lay();
        let (mut bills, mut total) = (0u64, 0u64);
        for i in 0..10_000u64 {
            let mut bill = |b: u64| -> Option<gravity_driver::Held> {
                bills += 1;
                total += b;
                None
            };
            assert_eq!(seen(&rt, Some(&[i.wrapping_mul(0x9e37_79b9_7f4a_7c15), i, 0]), 2, &mut bill, &|| None), 1, "image {i} is new");
        }
        let held = rt.seen.borrow().bytes();
        assert_eq!(total, held, "the bills are the set's capacity, {held} bytes");
        assert!((3..=60).contains(&bills), "a bill per doubling, not per row: {bills}");
        let before = (bills, total);
        assert_eq!(seen(&rt, Some(&[0, 0, 0]), 2, &mut |b| panic!("a seen image bills {b}"), &|| None), 0, "image 0 seen");
        assert_eq!((bills, total), before);
    }

    /// CONVERT wave 2 (site 2): the set's capacity is on the owner's charge from its first new image (the owner asked once),
    /// exactly at every growth, and leaves it when a re-arm drops the set.
    #[test]
    fn a_union_charges_its_set_to_the_owner_until_the_set_goes() {
        std::thread::spawn(|| {
            memledger::bind_backend(3996, 0);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let committed = || memledger::charge::owner_row(o).committed;
            let base = committed();
            let (_cell, rt) = lay();
            let asked = core::cell::Cell::new(0);
            let owner = || {
                asked.set(asked.get() + 1);
                Some((o, Kind::Region))
            };
            for i in 0..10_000u64 {
                assert_eq!(seen(&rt, Some(&[i.wrapping_mul(0x9e37_79b9_7f4a_7c15), i, 0]), 2, &mut |_| None, &owner), 1, "image {i} is new");
                assert_eq!(committed() - base, rt.seen.borrow().bytes() as i64, "image {i}");
            }
            assert_eq!(asked.get(), 1, "the owner is asked once");
            assert!(committed() - base >= 10_000 * 16);
            *rt.seen.borrow_mut() = Seen::default();
            assert_eq!(committed(), base, "the re-armed set's bytes left the owner");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// A parked unwind leaves no error stashed: nothing kept, no stand-in stashed.
    #[test]
    fn no_stashed_error_is_never_kept_and_no_stand_in_is_stashed() {
        let (st, held, stashed, _) = hold(None);
        assert_eq!((st, held, stashed), (ERROR, 0, None));
    }
}
