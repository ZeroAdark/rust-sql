//! D3024 (b), the sorted wide emit's reader: `merge_runs_into` (the k-way merge of `agg_merge_runs`) over runs that
//! `agg_sort` ordered gives `agg_sort`'s order over their union: every record for whole runs, the first `n` when each run
//! was sorted under the bound `n` and kept its first `n` (a claim's top). Up to `RUNS_MAX` runs, empty ones among them;
//! sort words over a numeric sum, a wide sum, sum(int8), a count and the int8 key, ASC / DESC, NULLS FIRST / LAST. And the
//! run table's own road: no run is an empty state, one run is that run's array (no copy), its `kept` the bound's. D3336
//! (emit.parmerge): `agg_merge_runs_part` on any number of threads lays the same merge as the reader's, which then
//! answers it; under the floor it lays nothing and the reader merges.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS, NUM_SENT_HI};
use gravity_abi::program::PIN_STATEMENT;
use gravity_abi::{Ctx, ProgramBlock};
use gravity_kernels::host_abi::{agg_merge_runs, agg_merge_runs_part, agg_merge_runs_part_k, agg_runs_claim, agg_sort, RUNS_CLAIM_ALL, RUNS_CLAIM_NONE, RUNS_CLAIM_PARTS, merge_runs_into, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_BOUND_SHIFT, FORM_NULLKEY_SHIFT, PAR_MIN_RECORDS, RUNS_ENTRY_BYTES, RUNS_MAX, RUNS_STATE_OFF, RUNS_TABLE_WORDS, SK_DESC, SK_NULLS_FIRST, SK_SLOT};

/// Keys: an int8 key (word 0), the NULL-flags word (word 1: its high bits the record's index, so the key rows are unique).
/// Slots: 0 sum(numeric), 1 a wide sum(numeric), 2 sum(int8), 3 count.
const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumNum, SlotKind::SumNumWide, SlotKind::SumI128, SlotKind::Count]);

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `m` records with small sums (many ties), the specials (NaN, +Infinity, -Infinity, OVERFLOW) and NULLs; `big`: some
/// sums past one word, so a run's sort takes the comparator's road where another's takes the word rows'.
fn records(rng: &mut Rng, m: usize, big: bool) -> Vec<Vec<u64>> {
    let words = LAYOUT.size() as usize / 8;
    let slot = |j: usize| LAYOUT.slot_off[j] as usize / 8;
    let pick = |rng: &mut Rng| -> Result<i128, u64> {
        let small = [-7i128, -1, 0, 0, 3, 3, 1 << 40, -(1 << 61)];
        match rng.below(12) {
            0 => Err(1),
            1 => Err(2),
            2 => Err(3),
            3 => Err(4),
            4 if big => Ok(1 << 62),
            _ => Ok(small[rng.below(small.len() as u64) as usize]),
        }
    };
    (0..m)
        .map(|i| {
            let mut r = vec![0u64; words];
            r[0] = [i64::MIN, -1, 0, 5, i64::MAX][rng.below(5) as usize] as u64;
            r[1] = (i as u64) << 8;
            let narrow = |s: Result<i128, u64>| match s {
                Ok(v) => [v as u64, (v as u128 >> 64) as u64],
                Err(code) => [code, NUM_SENT_HI as u64],
            };
            [r[slot(0)], r[slot(0) + 1]] = narrow(pick(rng));
            [r[slot(1)], r[slot(1) + 1], r[slot(1) + 2], r[slot(1) + 3]] = match pick(rng) {
                Ok(v) => {
                    let hi = v >> 127;
                    [v as u64, (v as u128 >> 64) as u64, hi as u64, hi as u64]
                }
                Err(code) => [code, 0, 0, NUM_SENT_HI as u64],
            };
            let c = pick(rng).unwrap_or(9);
            [r[slot(2)], r[slot(2) + 1]] = [c as u64, (c as u128 >> 64) as u64];
            r[slot(3)] = rng.below(3);
            let bits = (0..3).fold(0b1000u8, |b, j| if rng.below(6) == 0 { b } else { b | 1 << j });
            let (o, _) = LAYOUT.valid_bit(0);
            // SAFETY: the validity byte lies inside the record's `size()` bytes.
            unsafe { *(r.as_mut_ptr() as *mut u8).add(o) = bits };
            r
        })
        .collect()
}

fn desc(words: &[u64], bound: Option<usize>) -> Vec<u64> {
    let mut d = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    d[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    d[DESC_FORM / 8] = 2 << FORM_NULLKEY_SHIFT | bound.map_or(0, |n| (n as u64 + 1) << FORM_BOUND_SHIFT);
    d[DESC_NSORT / 8] = words.len() as u64;
    d[DESC_SORT / 8..DESC_SORT / 8 + words.len()].copy_from_slice(words);
    d
}

/// `ptrs` in `agg_sort`'s order under `words` (the first `bound` of them under a bound).
fn sort(ptrs: &mut [*const u8], words: &[u64], bound: Option<usize>) {
    let d = desc(words, bound);
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: ptrs.len() as u64, kept: ptrs.len() as u64 };
    // SAFETY: a ctx is plain words; zero is its closed state (no text word reads a site).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: `st` holds live records of `LAYOUT`, `d` a descriptor of it.
    let r = unsafe { agg_sort(&mut ctx as *mut _ as u64, &mut st as *mut EmitState as u64, d.as_ptr() as u64) };
    assert_eq!(r, 0);
}

fn word(rng: &mut Rng) -> u64 {
    let base = [SK_SLOT, SK_SLOT | 1, SK_SLOT | 2, SK_SLOT | 3, 0][rng.below(5) as usize];
    base | if rng.below(2) == 0 { SK_DESC } else { 0 } | if rng.below(2) == 0 { SK_NULLS_FIRST } else { 0 }
}

/// The records of `recs` cut into `k` runs at random (some empty), each sorted (under `bound`, keeping its first `bound`).
fn runs(rng: &mut Rng, recs: &mut [Vec<u64>], k: usize, words: &[u64], bound: Option<usize>) -> Vec<Vec<*const u8>> {
    let mut out: Vec<Vec<*const u8>> = vec![Vec::new(); k];
    for r in recs.iter_mut() {
        out[rng.below(k as u64) as usize].push(r.as_ptr() as *const u8);
    }
    for run in out.iter_mut() {
        sort(run, words, bound);
        run.truncate(bound.unwrap_or(usize::MAX));
    }
    out
}

fn merged(runs: &[Vec<*const u8>], words: &[u64], n: usize) -> Vec<*const u8> {
    let views: Vec<&[*const u8]> = runs.iter().map(|r| &r[..]).collect();
    let mut out = vec![core::ptr::null::<u8>(); n];
    let d = desc(words, None);
    // SAFETY: a zeroed ctx as in `sort`.
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: every run holds live records of `LAYOUT`; `out` holds at most their count.
    let r = unsafe { merge_runs_into(&mut ctx, d.as_ptr() as *const u8, &views, &mut out) };
    assert_eq!(r, 0);
    out
}

#[test]
fn merged_runs_are_the_union_sorted() {
    let mut rng = Rng(0x3024_B302_4B30_24B3);
    for case in 0..600 {
        let m = rng.below(300) as usize;
        let mut recs = records(&mut rng, m, case % 3 == 2);
        let nw = 1 + rng.below(3) as usize;
        let words: Vec<u64> = (0..nw).map(|_| word(&mut rng)).collect();
        let mut want: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
        sort(&mut want, &words, None);
        let k = 1 + rng.below(RUNS_MAX as u64) as usize;
        let whole = runs(&mut rng, &mut recs, k, &words, None);
        assert_eq!(merged(&whole, &words, m), want, "case {case}: {k} runs, words {words:x?}");
        // each run cut at the bound, as a claim's top keeps its first n: the merge's first n are the union's
        for n in [1, 2, m / 3, m] {
            let cut = runs(&mut rng, &mut recs, k, &words, Some(n));
            let n = n.min(m);
            assert_eq!(merged(&cut, &words, n), want[..n], "case {case}: {k} runs bound {n}, words {words:x?}");
        }
    }
}

/// The run table's road: entry `i` filed by the claim that started at slice `i`, the state after the entries.
#[test]
fn run_table_one_run_is_its_own_array() {
    let mut rng = Rng(0x00E1_7A8E_5000_0001);
    let mut recs = records(&mut rng, 40, false);
    let words = [SK_SLOT | SK_DESC];
    let mut run: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    sort(&mut run, &words, None);
    let d = desc(&words, None);
    let mut table = vec![0u64; RUNS_TABLE_WORDS];
    // SAFETY: a zeroed ctx: no pin is asked for one run or none.
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // a fresh execution's state each time (the table's state words zeroed, as exec lays it): the merge runs once per state
    let call = |table: &mut Vec<u64>, ctx: &mut Ctx, n: u64| -> *const EmitState {
        table[RUNS_STATE_OFF / 8..].iter_mut().for_each(|w| *w = 0);
        // SAFETY: the table's entries name live arrays of live records of `LAYOUT`.
        let r = unsafe { agg_merge_runs(ctx as *mut Ctx as u64, d.as_ptr() as u64, table.as_mut_ptr() as u64, n) };
        assert_eq!(r as usize, table.as_ptr() as usize + RUNS_STATE_OFF);
        r as usize as *const EmitState
    };
    // no run: an empty state
    let st = call(&mut table, &mut ctx, u64::MAX);
    // SAFETY: the state the call answered, inside the table.
    unsafe { assert_eq!(((*st).len, (*st).kept), (0, 0)) };
    // one run at slice 37: the state names its own array, every record or the bound's first
    let e = 37 * RUNS_ENTRY_BYTES / 8;
    (table[e], table[e + 1]) = (run.as_ptr() as u64, run.len() as u64);
    for (n, k) in [(u64::MAX, 40u64), (7, 7), (40, 40), (90, 40)] {
        let st = call(&mut table, &mut ctx, n);
        // SAFETY: as above.
        unsafe {
            assert_eq!((*st).ptrs as usize, run.as_ptr() as usize, "one run: no copy");
            assert_eq!(((*st).len, (*st).kept), (k, k));
        }
    }
    // a second call in one execution (a resumed drain's open) answers the state the first filed, the entries unread
    let st = call(&mut table, &mut ctx, 7);
    (table[e], table[e + 1]) = (0, 0);
    // SAFETY: as above.
    let again = unsafe { agg_merge_runs(&mut ctx as *mut Ctx as u64, d.as_ptr() as u64, table.as_mut_ptr() as u64, u64::MAX) };
    assert_eq!(again as usize, st as usize);
    // SAFETY: as above.
    unsafe { assert_eq!(((*st).ptrs as usize, (*st).kept), (run.as_ptr() as usize, 7)) };
}

/// The test's `Program.pin`: a zeroed region of its own (the statement bit dropped), leaked: kept to the process's end.
unsafe extern "C" fn test_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
    vec![0u64; ((bytes & !PIN_STATEMENT) as usize).div_ceil(8)].leak().as_mut_ptr() as *mut u8
}

/// A zeroed ctx whose program pins through `test_pin` (the merged array's statement pin), with its program block.
fn pin_ctx() -> (Box<Ctx>, Box<ProgramBlock>) {
    let mut program = Box::new(ProgramBlock::NONE);
    program.pin = Some(test_pin);
    // SAFETY: a ctx is plain words; zero is its closed state (no text word reads a site).
    let mut ctx: Box<Ctx> = Box::new(unsafe { core::mem::zeroed() });
    ctx.program = &mut *program;
    (ctx, program)
}

/// D3336: the run table of `runs` (entry `i` run `i`), its state words zeroed as exec lays them.
fn table_of(runs: &[Vec<*const u8>]) -> Vec<u64> {
    let mut table = vec![0u64; RUNS_TABLE_WORDS];
    for (i, r) in runs.iter().enumerate() {
        let e = i * RUNS_ENTRY_BYTES / 8;
        (table[e], table[e + 1]) = (r.as_ptr() as u64, r.len() as u64);
    }
    table
}

/// D3336 (emit.parmerge): each of `threads` threads calls the section end's part (its own ctx, as each claim thread's), all
/// at once; every call answers 0.
fn parts(table: &mut [u64], d: &[u64], threads: usize) {
    let (tbl, dp) = (table.as_mut_ptr() as usize, d.as_ptr() as usize);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(move || {
                let (mut ctx, _program) = pin_ctx();
                // SAFETY: the table's entries name live arrays of live records of `LAYOUT`; `dp` a descriptor of it.
                let r = unsafe { agg_merge_runs_part(&mut *ctx as *mut Ctx as u64, dp as u64, tbl as u64) };
                assert_eq!(r, 0);
            });
        }
    });
}

/// D3336 (emit.parmerge): past the floor, the parts merged by 1..8 threads are the one merge of every run (the order of a
/// whole-array `agg_sort`), filed behind the table's state, and the reader's `agg_merge_runs` answers that state as merged.
#[test]
fn parallel_parts_lay_the_reader_merge() {
    let mut rng = Rng(0xD333_6D33_36D3_336D);
    for (case, (m, k, threads)) in [(PAR_MIN_RECORDS + 1000, RUNS_MAX, 8), (PAR_MIN_RECORDS * 3, 17, 3), (PAR_MIN_RECORDS + 5, 2, 1), (PAR_MIN_RECORDS * 2, RUNS_MAX, 5)].into_iter().enumerate() {
        let mut recs = records(&mut rng, m, case % 2 == 1);
        let nw = 1 + rng.below(3) as usize;
        let words: Vec<u64> = (0..nw).map(|_| word(&mut rng)).collect();
        let mut want: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
        sort(&mut want, &words, None);
        let rs = runs(&mut rng, &mut recs, k, &words, None);
        let d = desc(&words, None);
        let mut table = table_of(&rs);
        parts(&mut table, &d, threads);
        let st = (table.as_ptr() as usize + RUNS_STATE_OFF) as *const EmitState;
        // SAFETY: the state the parts filed, inside the table: `len` records of the merged array.
        let got = unsafe {
            assert_eq!(((*st).len, (*st).kept), (m as u64, m as u64), "case {case}: the parts filed every record");
            core::slice::from_raw_parts((*st).ptrs as *const *const u8, m)
        };
        assert!(got == &want[..], "case {case}: {k} runs on {threads} threads, words {words:x?}");
        let (mut ctx, _program) = pin_ctx();
        // SAFETY: as `parts`.
        let r = unsafe { agg_merge_runs(&mut *ctx as *mut Ctx as u64, d.as_ptr() as u64, table.as_mut_ptr() as u64, u64::MAX) };
        assert_eq!(r as usize, st as usize, "case {case}: the reader answers the parts' state");
    }
}

/// D3336: under the floor (or over one run) the parts file nothing, so the reader's own merge lays the records as before.
#[test]
fn parallel_parts_under_the_floor_leave_the_reader_merge() {
    let mut rng = Rng(0xD333_6000_0000_0002);
    for (case, (m, k)) in [(PAR_MIN_RECORDS - 1, 9), (300, RUNS_MAX), (PAR_MIN_RECORDS + 10, 1), (0, 4)].into_iter().enumerate() {
        let mut recs = records(&mut rng, m, false);
        let words = [SK_SLOT | 2, 0];
        let mut want: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
        sort(&mut want, &words, None);
        let rs = runs(&mut rng, &mut recs, k, &words, None);
        let d = desc(&words, None);
        let mut table = table_of(&rs);
        parts(&mut table, &d, 4);
        let st = (table.as_ptr() as usize + RUNS_STATE_OFF) as *const EmitState;
        // SAFETY: the table's state words.
        unsafe { assert!((*st).ptrs.is_null(), "case {case}: no state filed under the floor") };
        let (mut ctx, _program) = pin_ctx();
        // SAFETY: as `parts`.
        let r = unsafe { agg_merge_runs(&mut *ctx as *mut Ctx as u64, d.as_ptr() as u64, table.as_mut_ptr() as u64, u64::MAX) };
        assert_eq!(r as usize, st as usize);
        // SAFETY: the reader's state: `len` records.
        let got = unsafe { core::slice::from_raw_parts((*st).ptrs as *const *const u8, (*st).len as usize) };
        assert!(got == &want[..], "case {case}: the reader's merge of {k} runs");
    }
}

/// D3336 (A1a, win.partfill): `claim`'s answer for run table `table` and an object table of `n` objects (handles 1..=n):
/// the claim's `[lo, hi)`, its object and its kind.
fn claim(table: &[u64], n: u64, lo: u64, hi: u64) -> (u64, u64, u64, i64) {
    let wtbl: Vec<u64> = core::iter::once(n).chain(1..=n).chain([0]).collect();
    // SAFETY: a ctx is plain words; zero is its closed state.
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.claim = gravity_abi::ctx::MorselRange::new(lo, hi);
    let mut out = [7u64; 3];
    // SAFETY: a live run table, an object table of `n` handles, three words out.
    let r = unsafe { agg_runs_claim(&mut ctx as *mut Ctx as u64, table.as_ptr() as u64, wtbl.as_ptr() as u64, out.as_mut_ptr() as u64) };
    (out[0], out[1], out[2], r)
}

/// D3336 (A1a, win.partfill): parts cut on the first sort word alone (`agg_merge_runs_part_k`, 1..8 threads) lay the one
/// merge of every run, and the claims of `n` objects (`agg_runs_claim`) tile it in order, one object each, no value of the
/// first word read by two claims; under the floor claim 0 reads every record and the others none; the section's end claim
/// reads none, the leader's whole claim every record.
#[test]
fn parts_cut_on_k_tile_the_merge_by_whole_values() {
    let mut rng = Rng(0xD333_6A1A_0000_0001);
    for (case, (m, k, threads, vals)) in [(PAR_MIN_RECORDS * 3, RUNS_MAX, 8, 500u64), (PAR_MIN_RECORDS * 2, 9, 3, 7), (PAR_MIN_RECORDS + 7, 2, 1, 40_000), (PAR_MIN_RECORDS * 2, RUNS_MAX, 5, 1)].into_iter().enumerate() {
        let mut recs = records(&mut rng, m, false);
        for r in recs.iter_mut() {
            r[0] = rng.below(vals);
        }
        let words = [0, SK_SLOT | 2, SK_SLOT | 3 | SK_DESC];
        let mut want: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
        sort(&mut want, &words, None);
        let rs = runs(&mut rng, &mut recs, k, &words, None);
        let d = desc(&words, None);
        let mut table = table_of(&rs);
        let (tbl, dp) = (table.as_mut_ptr() as usize, d.as_ptr() as usize);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(move || {
                    let (mut ctx, _program) = pin_ctx();
                    // SAFETY: as `parts`.
                    let r = unsafe { agg_merge_runs_part_k(&mut *ctx as *mut Ctx as u64, dp as u64, tbl as u64, 1) };
                    assert_eq!(r, 0);
                });
            }
        });
        let st = (table.as_ptr() as usize + RUNS_STATE_OFF) as *const EmitState;
        // SAFETY: the state the parts filed: `len` records of the merged array.
        let got = unsafe { core::slice::from_raw_parts((*st).ptrs as *const *const u8, (*st).len as usize) };
        assert!(got == &want[..], "case {case}: the K-cut parts are the one merge");
        // SAFETY: a merged record's first word (key 0).
        let key0 = |i: usize| unsafe { *(got[i] as *const u64) };
        for n in [1u64, 2, 7, 64, 300] {
            let mut at = 0u64;
            for i in 0..n {
                let (lo, hi, obj, kind) = claim(&table, n, i, i + 1);
                assert_eq!((kind, obj), (RUNS_CLAIM_PARTS, i + 1), "case {case}: claim {i} of {n} reads parts into its object");
                assert!(lo == at && lo <= hi && hi <= m as u64, "case {case}: claim {i} of {n} at [{lo}, {hi}) after {at}");
                if lo > 0 && lo < m as u64 {
                    assert_ne!(key0(lo as usize - 1), key0(lo as usize), "case {case}: a value read by two claims ({n} objects)");
                }
                at = hi;
            }
            assert_eq!(at, m as u64, "case {case}: the claims of {n} objects read every record");
        }
        assert_eq!(claim(&table, 8, u64::MAX, u64::MAX).3, RUNS_CLAIM_NONE, "the section's end claim");
        assert_eq!(claim(&table, 8, 0, u64::MAX), (0, u64::MAX, 0, RUNS_CLAIM_ALL), "the leader's whole claim");
    }
    // under the floor: no cut, so claim 0 reads every record (dealt) and the others none
    let mut recs = records(&mut rng, 300, false);
    let words = [0, SK_SLOT | 2];
    let rs = runs(&mut rng, &mut recs, 4, &words, None);
    let d = desc(&words, None);
    let mut table = table_of(&rs);
    let (mut ctx, _program) = pin_ctx();
    // SAFETY: as `parts`.
    assert_eq!(unsafe { agg_merge_runs_part_k(&mut *ctx as *mut Ctx as u64, d.as_ptr() as u64, table.as_mut_ptr() as u64, 1) }, 0);
    assert_eq!(claim(&table, 4, 0, 1), (0, u64::MAX, 0, RUNS_CLAIM_ALL), "under the floor claim 0 reads every record");
    assert_eq!(claim(&table, 4, 2, 3).3, RUNS_CLAIM_NONE, "under the floor another claim reads none");
}
