//! W5 multilock Row A: a program of several locks (`Program.locks`) derives one lock host per lock, in its order and with
//! its own plan; `lock_row`'s argument (`LockSite::arg`) routes each call to its mark's host; a Recheck writes the
//! traversed version into the called site's position (no cells), which the stop's settle keeps for the runner's recheck;
//! WHERE CURRENT OF reads each relation's own host. A program's one lock keeps argument 0.
use gravity_driver_rig as common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::ctx::{site_ptr, RESUME_NONE};
use gravity_abi::site::{next_row, seek};
use gravity_abi::status::{recheck, DONE, ERROR, OK, STOP, WRITE_RECHECK, WRITE_SKIPPED};
use gravity_abi::{Ctx, ScanPos};
use gravity_driver::node::locked_in;
use gravity_driver::{box_locker, ArenaBox, ArenaRef, BindingRef, Call, Derive, Entry, Locker, Outcome, SourceBox, Step, Writer};
use gravity_program::program::LockSite;
use gravity_program::{Arena, Code, Layout, LockKind, OutCol, Program, SiteDesc, WaitPolicy, WritePlan};

const NULLS: [u8; 2] = [0, 0];
const A: u32 = 1005;
const B: u32 = 2005;

type Log = Rc<RefCell<Vec<(u32, ScanPos)>>>;

/// A lock host of relation `rel` answering `outcomes` in turn; its newest version is (rel, 90, 9).
struct Scripted {
    rel: u32,
    outcomes: Vec<Outcome>,
    calls: usize,
    last: ScanPos,
    log: Log,
}

impl Locker for Scripted {
    fn bind(&mut self, _b: &BindingRef<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }
    fn lock(&mut self, pos: ScanPos) -> gravity_driver::Result<Outcome> {
        self.log.borrow_mut().push((self.rel, pos));
        let o = self.outcomes.get(self.calls).copied().unwrap_or(Outcome::Done);
        self.calls += 1;
        self.last = if o == Outcome::Recheck { self.recheck_pos() } else { pos };
        Ok(o)
    }
    fn recheck_pos(&self) -> ScanPos {
        ScanPos::new(self.rel, 90, 9)
    }
    fn current(&self) -> ScanPos {
        self.last
    }
    fn unbind(&mut self) {}
}

/// `FakeDerive`'s sources, a scripted lock host per plan (by its relation), and the plans the node asked for.
struct Derived {
    inner: FakeDerive,
    script: fn(u32) -> Vec<Outcome>,
    plans: Vec<(u32, LockKind, WaitPolicy)>,
    log: Log,
}

impl Derive for Derived {
    fn size_hint(&self, desc: &SiteDesc) -> usize {
        self.inner.size_hint(desc)
    }
    fn derive_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        self.inner.derive_source(desc, a)
    }
    fn writer_size_hint(&self) -> usize {
        self.inner.writer_size_hint()
    }
    fn derive_writer(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Writer> {
        self.inner.derive_writer(plan, a)
    }
    fn locker_size_hint(&self) -> usize {
        core::mem::size_of::<Scripted>()
    }
    fn derive_locker(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Locker> {
        self.plans.push((plan.target_rel, plan.lock, plan.wait));
        box_locker(Scripted { rel: plan.target_rel, outcomes: (self.script)(plan.target_rel), calls: 0, last: ScanPos::NONE, log: self.log.clone() }, a)
    }
}

/// Site 0's rows (`five_stock`), each locked by `lock_row(ctx, LockSite::arg(0, i))` for i < NLOCKS (0 for one lock):
/// SKIPPED drops the row, the marks after it untried; a RECHECK stops after the last mark with `recheck(0)`. The site is
/// re-derived after every host call (D1711: the host rewrites the block in place).
unsafe extern "C-unwind" fn k_locks<const NLOCKS: usize>(ctx: *mut Ctx) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        let s = &mut *site_ptr(ctx, 0);
        for k in 0..2 {
            *s.keys.add(k) = *(*ctx).params.add(k);
            *s.keys_null.add(k) = *(*ctx).params_null.add(k);
        }
        seek(site_ptr(ctx, 0));
        (*ctx).resume = 1;
    }
    loop {
        let r = next_row(site_ptr(ctx, 0), 1);
        if r < 0 {
            return r;
        }
        if r == 0 {
            (*ctx).resume = RESUME_NONE;
            let held = (*(*ctx).out).count > 0;
            return if held && !matches!(gravity_abi::program::push(ctx), STOP | OK) { ERROR } else { DONE };
        }
        let (mut rs, mut skipped) = (0, false);
        for i in 0..NLOCKS {
            let arg = if NLOCKS == 1 { 0 } else { LockSite::arg(0, i) };
            let l = gravity_abi::program::lock(ctx, arg);
            if l < 0 {
                return l;
            }
            if l == WRITE_SKIPPED {
                skipped = true;
                break;
            }
            if l == WRITE_RECHECK {
                rs = recheck(0);
            }
        }
        if skipped {
            continue;
        }
        if rs != 0 {
            return rs;
        }
        let s = &*site_ptr(ctx, 0);
        let o = &mut *(*ctx).out;
        *o.values.add(o.count as usize) = *s.values;
        *o.isnull.add(o.count as usize) = *s.isnull;
        o.count += 1;
    }
}

fn program(locks: &[LockSite], one: bool) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::index_source(st.handle(), A, 1105, &[1, 2], &[0, 1], &[3])];
    let layout = Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 };
    let code = if one { Code::hand(k_locks::<1>) } else { Code::hand(k_locks::<2>) };
    Program::new(layout, &sites, &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::locking(A, LockKind::Exclusive, WaitPolicy::Block), code).with_locks(locks)
}

const TWO: [LockSite; 2] = [LockSite { rti: 1, relid: A, lock: LockKind::Exclusive, wait: WaitPolicy::Block }, LockSite { rti: 2, relid: B, lock: LockKind::Share, wait: WaitPolicy::Skip }];

fn derived(script: fn(u32) -> Vec<Outcome>) -> Derived {
    Derived { inner: FakeDerive::new([five_stock, no_rows, no_rows, no_rows]), script, plans: Vec::new(), log: Rc::new(RefCell::new(Vec::new())) }
}

/// A's host skips the second row, B's rechecks the third: row 1 out, row 2 dropped before B is tried, row 3 stops with
/// B's newest version in site 0's settled position (as the node's `recheck_pos`); each relation's CURRENT OF is its own.
#[test]
fn several_locks_route_by_mark_in_order() {
    let program = Arc::new(program(&TWO, false));
    assert_ne!(program.key, self::program(&[], false).key, "the locks are in the program's key");
    let mut d = derived(|rel| if rel == A { vec![Outcome::Done, Outcome::Skipped, Outcome::Done] } else { vec![Outcome::Done, Outcome::Recheck] });
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut d, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(d.plans, vec![(A, LockKind::Exclusive, WaitPolicy::Block), (B, LockKind::Share, WaitPolicy::Skip)], "one host per lock, in Program.locks order, each with its own plan");
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Recheck { site: 0 });
    let row = |r: u32| ScanPos::new(A, r, 1);
    assert_eq!(*d.log.borrow(), vec![(A, row(1)), (B, row(1)), (A, row(2)), (A, row(3)), (B, row(3))], "PG's lock order; a skip leaves the later marks untried");
    assert_eq!(sink.rows(), 1, "the row before the recheck is sent before the stop (D699)");
    assert_eq!(exec.node().positions()[0], ScanPos::new(B, 90, 9), "the traversed mark's newest version is the site's settled position");
    assert_eq!(exec.recheck_pos(), ScanPos::new(B, 90, 9));
    assert_eq!((exec.node().locked(A), exec.node().locked(B), exec.node().locked(7)), (row(3), ScanPos::new(B, 90, 9), ScanPos::NONE));
    entry.end(exec, false);
}

/// A program's one lock: argument 0 names it, whatever relation CURRENT OF asks for (a family's leaf is the caller's
/// check); a several-lock argument there, or argument 0 in a program of several locks, is an error, not another host.
#[test]
fn one_lock_keeps_argument_zero() {
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut entry = Entry::new(2);
    let one = Arc::new(program(&[], true));
    let mut d = derived(|_| vec![Outcome::Done]);
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &one, &mut d, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(d.plans, vec![(A, LockKind::Exclusive, WaitPolicy::Block)]);
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!((sink.rows(), d.log.borrow().len()), (5, 5));
    assert_eq!((exec.node().locked(A), exec.node().locked(B)), (ScanPos::new(A, 5, 1), ScanPos::new(A, 5, 1)));
    entry.end(exec, true);
    let wrong = Arc::new(program(&TWO, true));
    let mut d = derived(|_| vec![Outcome::Done]);
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &wrong, &mut d, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert!(entry.run(&mut exec, &mut b, Call::batch(64)).is_err(), "argument 0 names no host of a program of several locks");
    assert!(d.log.borrow().is_empty());
    entry.end(exec, false);
}

/// W6-D652-R2: the InitPlan pair's B over a parent's leaves is ONE lock (its first leaf `L1`'s entry); its host's last row
/// may be in a later leaf (here site 0's relation A), which answers WHERE CURRENT OF for that leaf (PostgreSQL's one active
/// child ExecRowMark), and for `L1` too (the caller's leaf check then finds no row there). `L1`'s lock is tried first: the
/// other lock's Recheck on row 2 then rewrites the site's position after `L1` locked row 2. A non-family multilock is
/// unchanged: each relation its own lock, and a relation no lock names nor holds the last row of gets NONE.
#[test]
fn family_lock_answers_the_leaf_of_its_last_row() {
    const L1: u32 = 3005;
    let fam = [LockSite { rti: 3, relid: L1, lock: LockKind::Share, wait: WaitPolicy::Block }, TWO[1]];
    let program = Arc::new(program(&fam, false));
    let mut d = derived(|rel| if rel == B { vec![Outcome::Done, Outcome::Recheck] } else { vec![Outcome::Done] });
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut d, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Recheck { site: 0 });
    let (row2, newest) = (ScanPos::new(A, 2, 1), ScanPos::new(B, 90, 9));
    assert_eq!(exec.node().lockeds(), vec![(Some(L1), row2), (Some(B), newest)]);
    assert_eq!((exec.node().locked(A), exec.node().locked(L1), exec.node().locked(B), exec.node().locked(7)), (row2, row2, newest, ScanPos::NONE));
    entry.end(exec, false);
    let two = [(Some(A), ScanPos::new(A, 3, 1)), (Some(B), newest)];
    assert_eq!((locked_in(&two, A), locked_in(&two, B), locked_in(&two, 7)), (ScanPos::new(A, 3, 1), newest, ScanPos::NONE));
    assert_eq!(locked_in(&[(None, row2)], 7), row2, "a program's one lock answers for any relation");
}
