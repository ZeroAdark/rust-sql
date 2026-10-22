//! D154: T1's bypass routes a pair-collapse record by its group words — no `hand-kernels` feature, so the gate's crate tests run it.

use gravity_abi::breaker::{Candidates, HashAgg, KeyKind, Rec, RecordLayout, SlotKind, SubSplit, HASHAGG_HDR, T1, T1_BYPASS};
use gravity_abi::hash::{Depth, KeyHash};
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::partial;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::table::owner_fold::owner_fold;
use gravity_kernels::host_abi::{agg_finish, agg_miss, agg_open, AGG_STATE_BYTES};

const SHARE: u32 = 1 << 20;

/// A bound Ctx over one armed share region (the driver's arming, no spill host: the rows below never fill the pool).
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

/// The pair collapse's record `[group fp words, x, part]` partitioned by its two group words, fed through `agg_miss` under the
/// estimate's bypass (switched by the miss that confirms it after 64 evictions) and the open's: every record, the switching one
/// too, sits in the partition of its GROUP words' hash — the owner sums a group per partition, so a pair routed by its full
/// hash split its group (q22's `phrase 1011` counted 39 + 1). Crafted group words, no fingerprint seed; the observed thrash
/// switch needs 64 Ki inserts past this pool and shares the fix in `locate`.
#[test]
fn the_bypass_routes_a_pair_collapse_record_by_its_group_words() {
    const PAIR: RecordLayout = RecordLayout::new_nn(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::Count]).partitioned_by(2);
    const ROWS: u64 = 6_000;
    let group = |g: u64| [g.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x8c50_ecfa_aaf9_41a5, g.rotate_left(17) ^ 0xdb23_4c44_ac7e_c41f];
    for groups in [5_998_605, 0] {
        let mut b = Bound::new();
        let ctx = b.ctx();
        let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
        let mut img = [0u8; 256];
        unsafe {
            assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &PAIR, 0, groups), 0);
            let t = T1::view(partial(ctx));
            for x in 0..ROWS {
                img.fill(0);
                let src = Rec::at(img.as_mut_ptr(), &PAIR);
                let [lo, hi] = group(x % 97);
                src.open(&[lo, hi, x, 0]);
                src.add_count(0, 1);
                assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0, "groups {groups}: row {x}");
            }
            assert_ne!(t.flags & T1_BYPASS, 0, "groups {groups}: the table reached the bypass");
            assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
            let mut seen = 0u64;
            for p in 0..t.parts() as u32 {
                for c in t.chain(p) {
                    for i in 0..c.recs {
                        let r = Rec::at((c.ptr as *mut u8).add(i * PAIR.size() as usize), &PAIR);
                        let k = r.keys();
                        assert_eq!(KeyHash::of(&k[..2]).part(t.pbits as u32), p, "groups {groups}: pair x={} in its group's partition", k[2]);
                        seen += r.count(0) as u64;
                    }
                }
            }
            assert_eq!(seen, ROWS, "groups {groups}: every row's pair reached a partition");
        }
    }
}

/// D250 (ruling 10, KeyHash only): the phase-1 table homes a key at `KeyHash::home(cap_bits, Depth::TOP)` or its neighbour,
/// and the owner table homes it at the `KeyHash` home below its route bits, linear-probed from there. Before the port both
/// tables homed by the retired 32-bit fold (`hash_keys(keys) & mask`), so a home the IR computes through `emit_home` missed.
#[test]
fn the_phase1_and_owner_tables_home_a_key_where_keyhash_does() {
    let layout = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
    let key = |x: u64| [x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 7, x ^ 0x5555_5555];
    let mut b = Bound::new();
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let mut img = [0u8; 256];
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &layout, 0, 64), 0);
        let t = T1::view(partial(ctx));
        let (bits, mask) = (t.cap.trailing_zeros(), t.cap as usize - 1);
        for x in 0..200u64 {
            img.fill(0);
            let src = Rec::at(img.as_mut_ptr(), &layout);
            src.open(&key(x));
            src.add_count(0, 1);
            assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0, "row {x}");
        }
        assert_eq!(t.flags & T1_BYPASS, 0, "200 keys stay in the table");
        let mut held = 0;
        for i in 0..t.cap as usize {
            let r = t.record(i);
            if r.used() {
                let home = KeyHash::of(r.keys()).home(bits, Depth::TOP);
                assert!(i == home || i == (home + 1) & mask, "T1 slot {i}: keys {:?} home at {home}", r.keys());
                held += 1;
            }
        }
        assert!(held > 150, "the table holds the keys ({held})");
    }
    // the owner table: records of the layout folded by the kernels' owner fold, each found from its KeyHash home
    let (n, cap) = (500usize, 1024u32);
    let rec = layout.size() as usize;
    let mut recs = vec![0u8; n * rec];
    for x in 0..n {
        let r = unsafe { Rec::at(recs[x * rec..].as_mut_ptr(), &layout) };
        r.open(&key(x as u64 * 7 + 3));
        r.add_count(0, 1);
    }
    let mut region = vec![0u64; (HASHAGG_HDR + cap as usize * rec) / 8 + 1];
    let bytes = (region.len() * 8) as u32;
    let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, &layout, cap, bytes) as *mut HashAgg };
    let mut cand = Candidates::none();
    assert_eq!(unsafe { owner_fold(core::ptr::null(), t, recs.as_ptr(), n, SubSplit::NONE, &mut cand) }, n as i64);
    let t = unsafe { &*t };
    let bits = t.cap.trailing_zeros();
    for j in 0..t.cap as usize {
        if !t.record(j).used() {
            continue;
        }
        let home = KeyHash::of(t.record(j).keys()).home(bits, Depth::TOP);
        let mut i = home;
        while i != j {
            assert!(t.record(i).used(), "owner slot {j}: a hole at {i} after its KeyHash home {home}");
            i = (i + 1) & (t.cap as usize - 1);
        }
    }
}

/// V4R-375 (D798 part 1): a hosted table (its layout holds `AggState` slots) never flips to the bypass at run time, so a group
/// keeps one state per partial, never one per row: not by the open's rule (no estimate), not by the planner's estimate, not by
/// the observed thrash under forced eviction (the window's counters set hot). The same rows over the layout without the
/// `AggState` slot take each bypass.
#[test]
fn a_hosted_table_stays_keyed_where_a_plain_one_takes_the_bypass() {
    use gravity_abi::breaker::{adapt_win, T1_EST};
    // the implicit count as the sentinel, as a keyed record's (an opened record reads as used before its first fold)
    const HOSTED: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::AggState, SlotKind::MaxI64], true, None);
    const PLAIN: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::MaxI64], true, None);
    for (layout, hosted) in [(&HOSTED, true), (&PLAIN, false)] {
        for (groups, what) in [(0u64, "the open's rule"), (5_000_000, "the estimate"), (64, "the observation")] {
            let mut b = Bound::new();
            let ctx = b.ctx();
            let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
            let mut img = [0u8; 256];
            unsafe {
                assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, layout, 0, groups), 0);
                let t = partial(ctx) as *mut T1;
                match what {
                    "the open's rule" => assert_eq!((*t).flags & T1_BYPASS != 0, !hosted, "hosted {hosted}: {what}"),
                    "the estimate" => assert_eq!((*t).flags & (T1_BYPASS | T1_EST) != 0, !hosted, "hosted {hosted}: {what}"),
                    _ => {
                        assert_eq!((*t).flags & (T1_BYPASS | T1_EST), 0, "hosted {hosted}: keyed at the open");
                        (*t).inserts = (*t).win_at + adapt_win((*t).cap as u64);
                        (*t).evictions = (*t).win_ev + adapt_win((*t).cap as u64);
                        let before = (*t).evictions;
                        for x in 0..3_000u64 {
                            img.fill(0);
                            let src = Rec::at(img.as_mut_ptr(), layout);
                            src.open(&[(x + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1]); // never 0 (a zero word reads as an empty slot)
                            assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0, "hosted {hosted}: row {x}");
                            if (*t).flags & T1_BYPASS != 0 {
                                break;
                            }
                        }
                        assert!((*t).evictions > before || !hosted, "hosted {hosted}: the rows forced evictions");
                        assert_eq!((*t).flags & T1_BYPASS != 0, !hosted, "hosted {hosted}: {what}");
                    }
                }
            }
        }
    }
}
