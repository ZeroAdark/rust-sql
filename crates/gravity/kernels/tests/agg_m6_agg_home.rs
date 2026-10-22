//! V4R-375 (D798's bound): a keyed hosted table's group keeps ONE home state. Every record T1 opens for the group (its
//! first, and each one after an eviction) points at the same state (`m6_agg_home`'s `STATE_HOME` tag), the transitions
//! land there in input order, and the owner fold takes the tag once however many partials and re-reads name it, so a
//! group's memory is one state, never one per partial. A table laid out afresh (a new run) forgets the homes.
use std::collections::{HashMap, HashSet};

use gravity_abi::breaker::{HashAgg, KeyKind, Rec, RecordLayout, SlotKind};
use gravity_abi::ctx::ctx_size;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::host_abi::agg_open;
use gravity_kernels::ops::agg::trans::hosted::{m6_agg_final, m6_agg_home, m6_agg_home_open, m6_agg_trans, owner_combine, AggHead, AGG_MAGIC, AGG_STATE_BYTES, STATE_HOME, STATE_INIT};

/// A stand-in arena: a state is a sum of its inputs; homes by identity words; a combine is never called (the test fails).
#[repr(C)]
struct Fake {
    head: AggHead,
    homes: HashMap<Vec<u64>, Box<[u64]>>,
    forgot: Vec<Box<[u64]>>,
    gathered: HashSet<usize>,
}

unsafe fn f_trans(_h: *mut AggHead, _o: u16, _a: u32, s: *mut u8, args: *const u64, _n: *const u8) -> Result<(), ()> {
    (s as *mut u64).write_unaligned((s as *const u64).read_unaligned() + args.read());
    s.add(STATE_INIT).write(1);
    Ok(())
}

unsafe fn f_final(_h: *mut AggHead, _o: u16, _a: u32, s: *mut u8, out: *mut u64) -> Result<(), ()> {
    out.write((s as *const u64).read_unaligned());
    out.add(1).write(0);
    out.add(2).write(0);
    Ok(())
}

unsafe fn f_fold(_h: *mut AggHead, _o: u16, _a: u32, _s: *mut u8) -> Result<(), ()> {
    panic!("no host-sorted aggregate here")
}

unsafe fn f_combine(_h: *mut AggHead, _o: u16, _a: u32, _i: *mut u8, _f: *mut u8) -> Result<(), ()> {
    panic!("a homed group's partials are never combined")
}

unsafe fn f_combine_keep(_h: *mut AggHead, _o: u16, _a: u32, _i: *mut u8, _f: *const u8) -> Result<(), ()> {
    panic!("a homed group's partials are never combined")
}

unsafe fn f_home(h: *mut AggHead, _ctx: *mut gravity_abi::Ctx, keys: *const u64, nkeys: usize, n: usize) -> *mut u8 {
    let f = &mut *(h as *mut Fake);
    let k = core::slice::from_raw_parts(keys, nkeys).to_vec();
    let w = f.head.wide as usize;
    f.homes.entry(k).or_insert_with(|| vec![0u64; n * w * AGG_STATE_BYTES as usize / 8].into_boxed_slice()).as_mut_ptr() as *mut u8
}

unsafe fn f_clear(h: *mut AggHead) {
    let f = &mut *(h as *mut Fake);
    f.forgot.extend(f.homes.drain().map(|(_, s)| s));
}

/// D3062-b: a wide home's workers' sums added once into its last state.
unsafe fn f_gather(h: *mut AggHead, subs: *mut u8) -> *mut u8 {
    let f = &mut *(h as *mut Fake);
    let w = f.head.wide as usize;
    let last = subs.add((w - 1) * AGG_STATE_BYTES as usize);
    if f.gathered.insert(last as usize) {
        let sum = (0..w - 1).map(|k| (subs.add(k * AGG_STATE_BYTES as usize) as *const u64).read_unaligned()).sum::<u64>();
        (last as *mut u64).write_unaligned(sum);
    }
    last
}

fn fake() -> Box<Fake> {
    wide_fake(1)
}

fn wide_fake(wide: u64) -> Box<Fake> {
    let head = AggHead { magic: AGG_MAGIC, trans: f_trans, final_: f_final, fold: f_fold, combine: f_combine, combine_keep: f_combine_keep, home: f_home, homes_clear: f_clear, direct: 0, given: 0, stable: 0, wide, gather: f_gather };
    Box::new(Fake { head, homes: HashMap::new(), forgot: Vec::new(), gathered: HashSet::new() })
}

const SHARE: u32 = 1 << 20;

/// A Ctx over one armed share region (bypass_route.rs's `Bound`).
struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    _own: Vec<u64>,
}

impl Bound {
    fn new() -> Bound {
        let mut own = vec![0u64; SHARE as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, _own: own }
    }

    fn ctx(&mut self) -> *mut Ctx {
        self.ctx.as_mut_ptr() as *mut Ctx
    }
}

const L: RecordLayout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::AggState, SlotKind::AggState, SlotKind::MaxI64]);

/// Group `key`'s record as T1 opens it and the fresh arm homes it (hashagg.rs `open_rec`: the handle, then `m6_agg_home`).
fn opened(ctx: *mut Ctx, h: u64, key: u64) -> Vec<u64> {
    let mut r = vec![0u64; L.size() as usize / 8];
    let p = r.as_mut_ptr() as *mut u8;
    unsafe {
        Rec::at(p, &L).open(&[key]);
        (p.add(L.slot_off(3) as usize) as *mut u64).write_unaligned(h);
        assert_eq!(m6_agg_home(ctx as u64, h, p as u64, 1, L.slot_off(1) as u64, 2), 0);
    }
    r
}

fn slot(r: &mut [u64], j: usize) -> *mut u8 {
    unsafe { (r.as_mut_ptr() as *mut u8).add(L.slot_off(j) as usize) }
}

fn home_of(r: &mut [u64], j: usize) -> u64 {
    let s = slot(r, j);
    unsafe {
        assert_eq!(s.add(STATE_HOME).read(), 1, "a keyed hosted slot is tagged");
        (s as *const u64).read_unaligned()
    }
}

fn step(ctx: *mut Ctx, h: u64, r: &mut [u64], j: usize, v: u64) {
    let (args, nulls) = ([v], [0u8]);
    assert_eq!(unsafe { m6_agg_trans(ctx as u64, h, j as u64, (slot(r, j)) as u64, args.as_ptr() as u64, nulls.as_ptr() as u64) }, 0);
}

fn final_of(ctx: *mut Ctx, h: u64, r: &mut [u64], j: usize) -> u64 {
    let mut out = [0u64; 2];
    assert_eq!(unsafe { m6_agg_final(ctx as u64, h, j as u64, (slot(r, j)) as u64, out.as_mut_ptr() as u64) }, 0);
    out[0]
}

/// The owner's record of the group, built as the keyed owner fold builds it (a copy of the first partial with its states
/// emptied, then every partial folded in through `owner_combine`), over `parts` in order.
fn owned(parts: &mut [&mut Vec<u64>]) -> Vec<u64> {
    let mut o = vec![0u64; L.size() as usize / 8];
    let po = o.as_mut_ptr() as *mut u8;
    unsafe {
        let owner = Rec::at(po, &L);
        owner.copy_from(&Rec::at(parts[0].as_mut_ptr() as *mut u8, &L));
        owner.clear_states();
        for p in parts.iter_mut() {
            owner.fold_with(&Rec::at(p.as_mut_ptr() as *mut u8, &L), &mut owner_combine).expect("a homed partial folds");
        }
    }
    o
}

#[test]
fn a_keyed_group_keeps_one_home_state_across_its_records_and_the_owner_takes_it_once() {
    let (mut b, mut f) = (Bound::new(), fake());
    let (ctx, h) = (b.ctx(), &mut f.head as *mut AggHead as u64);
    // group 7's first record: two rows into aggregate 1, one into aggregate 2
    let mut a = opened(ctx, h, 7);
    step(ctx, h, &mut a, 1, 1);
    step(ctx, h, &mut a, 1, 2);
    step(ctx, h, &mut a, 2, 10);
    // T1 evicts it; the group's next record opens fresh and finds the same home; group 8 has its own
    let mut b7 = opened(ctx, h, 7);
    assert_eq!((home_of(&mut a, 1), home_of(&mut a, 2)), (home_of(&mut b7, 1), home_of(&mut b7, 2)), "two opens of one key share one state");
    assert_eq!(home_of(&mut a, 2), home_of(&mut a, 1) + AGG_STATE_BYTES as u64, "a record's states are its home's in order");
    step(ctx, h, &mut b7, 1, 4);
    let mut c = opened(ctx, h, 8);
    assert_ne!(home_of(&mut c, 1), home_of(&mut a, 1), "another group, another home");
    step(ctx, h, &mut c, 1, 100);
    assert_eq!(f.homes.len(), 2, "one home per group, not per record");
    assert_eq!((final_of(ctx, h, &mut a, 1), final_of(ctx, h, &mut b7, 1), final_of(ctx, h, &mut b7, 2)), (7, 7, 10), "every record reads the one state");
    // the owner takes the tag once: in either order, a partial named twice, and a re-read, all read the one state
    let home7 = home_of(&mut a, 1);
    for oldest_first in [true, false] {
        let mut o = if oldest_first { owned(&mut [&mut a, &mut b7]) } else { owned(&mut [&mut b7, &mut a]) };
        assert_eq!((home_of(&mut o, 1), final_of(ctx, h, &mut o, 1), final_of(ctx, h, &mut o, 2)), (home7, 7, 10));
        unsafe {
            let owner = Rec::at(o.as_mut_ptr() as *mut u8, &L);
            owner.fold_with(&Rec::at(a.as_mut_ptr() as *mut u8, &L), &mut owner_combine).expect("a re-read partial folds again");
        }
        assert_eq!((home_of(&mut o, 1), final_of(ctx, h, &mut o, 1)), (home7, 7), "a re-read is idempotent");
    }
    assert_eq!(final_of(ctx, h, &mut c, 1), 100);
    // a partial on another group's home beside an owner already on one is refused, never merged
    let mut o = owned(&mut [&mut a]);
    let bad = unsafe { Rec::at(o.as_mut_ptr() as *mut u8, &L).fold_with(&Rec::at(c.as_mut_ptr() as *mut u8, &L), &mut owner_combine) };
    assert!(bad.is_err(), "two homes in one owner record raise");
}

#[test]
fn a_table_laid_out_afresh_forgets_the_homes_and_a_resumed_one_keeps_them() {
    let (mut b, mut f) = (Bound::new(), fake());
    let (ctx, h) = (b.ctx(), &mut f.head as *mut AggHead as u64);
    let mut a = opened(ctx, h, 7);
    step(ctx, h, &mut a, 1, 5);
    let old = home_of(&mut a, 1);
    // the region's header is zeroed: the next agg_open lays T1 out afresh (a new run), so the homes are forgotten
    assert_eq!(unsafe { m6_agg_home_open(ctx as u64, h) }, 0);
    assert!(f.homes.is_empty() && f.forgot.len() == 1, "a fresh layout forgets the homes and keeps their states");
    let mut st = vec![0u64; 16];
    assert_eq!(unsafe { agg_open(ctx, st.as_mut_ptr() as *mut u8, &L, 0, 64) }, 0);
    let mut n = opened(ctx, h, 7);
    assert_ne!(home_of(&mut n, 1), old, "the new run's group starts a state of its own");
    assert_eq!((final_of(ctx, h, &mut n, 1), final_of(ctx, h, &mut a, 1)), (0, 5), "the forgotten state still reads");
    // a resumed table (laid out: the claim after the first) keeps them
    assert_eq!(unsafe { m6_agg_home_open(ctx as u64, h) }, 0);
    let mut again = opened(ctx, h, 7);
    assert_eq!(home_of(&mut again, 1), home_of(&mut n, 1), "a resumed table's group finds its home");
}

/// D3062-b: a wide arena (two workers + the gathered state): both workers' records of a group name its one home, each
/// worker advances its own state there (a nested lane's ordinal the first), the owner fold takes the one tag, and the
/// final reads the workers' states gathered once.
#[test]
fn a_wide_home_keeps_a_state_per_worker_and_the_final_gathers_them_once() {
    let (mut b, mut f) = (Bound::new(), wide_fake(3));
    let (ctx, h) = (b.ctx(), &mut f.head as *mut AggHead as u64);
    let mut w0 = opened(ctx, h, 7);
    step(ctx, h, &mut w0, 1, 1);
    unsafe { (*ctx).worker_ordinal = 1 };
    let mut w1 = opened(ctx, h, 7);
    assert_eq!(home_of(&mut w1, 1), home_of(&mut w0, 1), "both workers' records of the group name one home");
    assert_eq!(home_of(&mut w0, 2), home_of(&mut w0, 1) + 3 * AGG_STATE_BYTES as u64, "an aggregate's states are three apart");
    step(ctx, h, &mut w1, 1, 20);
    step(ctx, h, &mut w1, 2, 5);
    unsafe { (*ctx).worker_ordinal = 0xFFF0 };
    step(ctx, h, &mut w1, 1, 300);
    let base = home_of(&mut w0, 1);
    let at = |k: u64| unsafe { ((base + k * AGG_STATE_BYTES as u64) as *const u64).read_unaligned() };
    assert_eq!((at(0), at(1), at(2)), (301, 20, 0), "worker 1 its own state, a nested lane the first");
    unsafe { (*ctx).worker_ordinal = 0 };
    let mut o = owned(&mut [&mut w1, &mut w0]);
    assert_eq!(home_of(&mut o, 1), home_of(&mut w0, 1), "the owner takes the one tag");
    assert_eq!((final_of(ctx, h, &mut o, 1), final_of(ctx, h, &mut o, 1), final_of(ctx, h, &mut o, 2)), (321, 321, 5), "gathered once, read twice");
    assert_eq!(f.gathered.len(), 2, "one gather per aggregate of the group");
    assert_eq!(f.homes.len(), 1);
}
