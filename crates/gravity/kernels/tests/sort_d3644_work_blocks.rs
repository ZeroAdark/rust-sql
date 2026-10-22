//! D3644 (b): `agg_sort`'s per-record arrays (the decorated rows, the index, the reorder copy; on the text road the taken
//! images, the heads and the index) are asked of the spill host (`SPILL_WORK_TAKE`, the statement's grant: the driver's
//! `v4::take`) and given back (`SPILL_WORK_GIVE`), not the allocator's. A served block (poisoned first: every array is
//! written before it is read) gives the allocator road's order; a refused one is the sort's ERROR (the driver holds the
//! refusal for the emit's raise); the policy's floor (the host's 1) keeps the allocator road.
use std::sync::Mutex;

use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::program::{SPILL_WORK_GIVE, SPILL_WORK_TAKE};
use gravity_abi::{word, Ctx, ProgramBlock};
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_BOUND_SHIFT, SK_DESC, SK_HEAP, SK_TEXT};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Serve,
    Refuse,
    Floor,
}

/// The fake host's state: what it answers, the bytes of each take, the blocks it lent (address -> words) and the gives.
struct Host {
    mode: Mode,
    takes: Vec<u64>,
    lent: Vec<(usize, Vec<u64>)>,
    gives: usize,
}

static HOST: Mutex<Host> = Mutex::new(Host { mode: Mode::Serve, takes: Vec::new(), lent: Vec::new(), gives: 0 });

unsafe extern "C" fn spill(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    if op == gravity_abi::program::SPILL_ARENA_CHUNK {
        return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
    }
    let mut h = HOST.lock().unwrap();
    match op {
        SPILL_WORK_TAKE => {
            h.takes.push(arg);
            match h.mode {
                Mode::Refuse => 0,
                Mode::Floor => 1,
                Mode::Serve => {
                    let mut b = vec![0xA5A5_A5A5_A5A5_A5A5u64; (arg as usize).div_ceil(8)];
                    let p = b.as_mut_ptr() as usize;
                    h.lent.push((p, b));
                    p as i64
                }
            }
        }
        SPILL_WORK_GIVE => {
            let i = h.lent.iter().position(|(p, _)| *p == arg as usize).expect("a give of a lent block");
            h.lent.swap_remove(i);
            h.gives += 1;
            0
        }
        _ => panic!("op {op}"),
    }
}

/// `recs` sorted by `agg_sort` under sort words `sort` (the first `bound` when some), with the fake host behind the ctx
/// (`mode`) or none (`None`: a kernel test's ctx, the allocator road): the status and the records' first words in the order
/// left.
fn run_cut(recs: &mut [Vec<u64>], layout: &RecordLayout, sort: &[u64], mode: Option<Mode>, bound: Option<usize>) -> (i64, Vec<u64>) {
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: recs.len() as u64, kept: recs.len() as u64 };
    let mut d = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = layout.words();
    d[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    d[DESC_FORM / 8] = bound.map_or(0, |n| (n as u64 + 1) << FORM_BOUND_SHIFT);
    d[DESC_NSORT / 8] = sort.len() as u64;
    d[DESC_SORT / 8..DESC_SORT / 8 + sort.len()].copy_from_slice(sort);
    let mut block = ProgramBlock { spill: word(Some(spill as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64)), ..ProgramBlock::NONE };
    // SAFETY: a ctx is plain words; zero is its closed state, its program block the fake host's when a mode is given.
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    if let Some(m) = mode {
        HOST.lock().unwrap().mode = m;
        ctx.program = word(&mut block as *mut ProgramBlock);
    }
    // SAFETY: `st` holds live records of `layout`, `d` a descriptor of it.
    let r = unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (d.as_ptr() as *const u8) as u64) };
    (r, ptrs.iter().map(|&p| unsafe { *(p as *const u64) }).collect())
}

fn run(recs: &mut [Vec<u64>], layout: &RecordLayout, sort: &[u64], mode: Option<Mode>) -> (i64, Vec<u64>) {
    run_cut(recs, layout, sort, mode, None)
}

fn taken_and_given() -> (Vec<u64>, usize, usize) {
    let mut h = HOST.lock().unwrap();
    let out = (core::mem::take(&mut h.takes), h.lent.len(), core::mem::replace(&mut h.gives, 0));
    h.lent.clear();
    out
}

#[test]
fn a_group_sorts_arrays_are_asked_of_the_grant_and_given_back() {
    let _one = TEST.lock().unwrap();
    // the word road: an int8 key, record i's key a scramble of i (descending sort word), m records
    let layout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count]);
    let m = 20_000usize;
    let words = layout.size() as usize / 8;
    let mut recs: Vec<Vec<u64>> = (0..m)
        .map(|i| {
            let mut r = vec![0u64; words];
            r[0] = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20;
            r
        })
        .collect();
    let sort = [SK_DESC];
    let (r0, want) = run(&mut recs, &layout, &sort, None);
    assert_eq!(r0, 0);
    taken_and_given();
    let (r, got) = run(&mut recs, &layout, &sort, Some(Mode::Serve));
    assert_eq!((r, &got), (0, &want), "a served block gives the allocator road's order");
    let (takes, live, gives) = taken_and_given();
    let stride = 2 + 1;
    for need in [m * stride * 8, m * 4, m * 8] {
        assert!(takes.contains(&(need as u64)), "a take of {need} bytes (the decorated rows, the index, the copy): {takes:?}");
    }
    assert_eq!((live, gives), (0, takes.len()), "every block given back");
    let (r, _) = run(&mut recs, &layout, &sort, Some(Mode::Refuse));
    assert!(r < 0, "a refused block is the sort's ERROR ({r})");
    let (takes, live, _) = taken_and_given();
    assert_eq!((takes.len(), live), (1, 0), "the first refusal ends the sort");
    let (r, got) = run(&mut recs, &layout, &sort, Some(Mode::Floor));
    assert_eq!((r, &got), (0, &want), "under the floor the allocator's arrays");
    assert_eq!(taken_and_given().1, 0);
    // a bound past a third of the records: the 2n-row buffer (`select_decorated`) from the grant too
    let n = m / 4;
    let (r, got) = run_cut(&mut recs, &layout, &sort, Some(Mode::Serve), Some(n));
    assert_eq!((r, &got[..n]), (0, &want[..n]), "the bound's first n");
    let (takes, live, gives) = taken_and_given();
    assert!(takes.contains(&(2 * n as u64 * stride as u64 * 8)), "the buffer rows taken: {takes:?}");
    assert_eq!((live, gives), (0, takes.len()));

    // the text road (CAND-25): a heap text key whose heads tell the records apart
    let layout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinVar]);
    let words = layout.size() as usize / 8;
    let images: Vec<Vec<u8>> = (0..m as u64)
        .map(|i| {
            let s = format!("{:016x}", i.wrapping_mul(0xD1B5_4A32_D192_ED03));
            let mut v = (((s.len() + 4) as u32) << 2).to_le_bytes().to_vec();
            v.extend_from_slice(s.as_bytes());
            v
        })
        .collect();
    let mut recs: Vec<Vec<u64>> = images
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let mut r = vec![0u64; words];
            r[0] = i as u64;
            r[layout.slot_off[0] as usize / 8] = v.as_ptr() as u64;
            r
        })
        .collect();
    let sort = [SK_TEXT | SK_HEAP];
    let (r0, want) = run(&mut recs, &layout, &sort, None);
    assert_eq!(r0, 0);
    taken_and_given();
    let (r, got) = run(&mut recs, &layout, &sort, Some(Mode::Serve));
    assert_eq!((r, &got), (0, &want), "the text road's order with served blocks");
    let (takes, live, gives) = taken_and_given();
    assert!(takes.len() >= 4 && takes.contains(&(m as u64 * 4)) && takes.contains(&(m as u64 * 8)), "the images, the heads, the index and the copy taken: {takes:?}");
    assert_eq!((live, gives), (0, takes.len()), "every block given back");
    let (r, _) = run(&mut recs, &layout, &sort, Some(Mode::Refuse));
    assert!(r < 0, "a refused block is the text road's ERROR too ({r})");
    taken_and_given();
}

/// One test at a time over the fake host's state.
static TEST: Mutex<()> = Mutex::new(());
