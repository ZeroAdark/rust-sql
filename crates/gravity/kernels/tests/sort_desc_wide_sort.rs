//! W8-SETOP-WIDEKEYS: a sorted group by more than `DESC_MAX_SORT` keys lays its sort words on past `DESC_PROCS` and its
//! comparator words after them, their byte offset in the `DESC_NSORT` word's high half (`DESC_PROCS_AT_SHIFT`); `agg_sort`
//! reads the count from the low half and each `SK_CMP` word's comparator there. Ten key words, the tenth ordered by
//! `btint4cmp` (by value, not by its word's bits); a descriptor with no offset keeps `DESC_PROCS`.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, WideLayout, LAYOUT_WORDS};
use gravity_abi::limits::DESC_MAX_SORT;
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_LAYOUT, DESC_NSORT, DESC_PROCS, DESC_PROCS_AT_SHIFT, DESC_SORT, SK_CMP, SK_DESC};

const NKEY: usize = 10;
/// `btint4cmp` (pg_proc.dat).
const BTINT4CMP: u32 = 351;

/// The records' keys in order after `agg_sort` under a descriptor of `NKEY` sort words (word 9 `SK_CMP`, `desc9` DESC),
/// laid wide (`wide`) or at `DESC_PROCS`.
fn sorted(last: &[i32], desc9: bool, wide: bool) -> Vec<i32> {
    // past eight keys a record is wide (breaker.rs `MAX_RECORD_KEYS`)
    let off: &'static mut [u32] = Box::leak(Box::new([0u32; 1]));
    let w: &'static WideLayout = Box::leak(Box::new(WideLayout::compute(&[KeyKind::I64; NKEY], &[SlotKind::Count], off, false)));
    let layout = RecordLayout::wide(w, false, false);
    let words = layout.size() as usize / 8;
    let recs: Vec<Vec<u64>> = last.iter().map(|&v| {
        let mut r = vec![0u64; words];
        (0..NKEY - 1).for_each(|k| r[k] = 7);
        r[NKEY - 1] = v as u32 as u64; // an int4 datum word: -1's bits order after 1's
        r
    }).collect();
    let mut ptrs: Vec<*const u8> = recs.iter().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: recs.len() as u64, kept: recs.len() as u64 };
    let at = if wide { DESC_SORT + 8 * NKEY } else { DESC_PROCS };
    let mut d = vec![0u64; (DESC_BYTES as usize).max(at + 8 * NKEY) / 8];
    let lw: [u64; LAYOUT_WORDS] = layout.words();
    d[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    d[DESC_NSORT / 8] = NKEY as u64 | if wide { (at as u64) << DESC_PROCS_AT_SHIFT } else { 0 };
    for k in 0..NKEY {
        d[DESC_SORT / 8 + k] = k as u64;
    }
    d[DESC_SORT / 8 + NKEY - 1] |= SK_CMP | if desc9 { SK_DESC } else { 0 };
    let proc = gravity_fn::resolved::resolve(BTINT4CMP).expect("a builtin resolves").expect("not refused");
    d[at / 8 + NKEY - 1] = proc as u64;
    // SAFETY: a ctx is plain words; zero is its closed state (no text key renders).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: `st` holds live records of `layout`, `d` a descriptor of it.
    assert_eq!(unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (d.as_ptr() as *const u8) as u64) }, 0);
    // SAFETY: every pointer is one of `recs`.
    ptrs.iter().map(|&p| unsafe { *(p as *const u64).add(NKEY - 1) } as u32 as i32).collect()
}

#[test]
fn a_wide_descriptor_reads_its_comparator_words_after_its_sort_words() {
    assert!(NKEY > DESC_MAX_SORT);
    assert_eq!(sorted(&[1, -1, 3, -7], false, true), [-7, -1, 1, 3], "btint4cmp's order, not the words' bits");
    assert_eq!(sorted(&[1, -1, 3, -7], true, true), [3, 1, -1, -7], "DESC reverses the comparator");
}

#[test]
fn a_descriptor_with_no_offset_keeps_desc_procs() {
    // eight words or fewer never set the offset; a word past eight with its comparator at DESC_PROCS reads it there as before
    assert_eq!(sorted(&[1, -1, 3, -7], false, false), [-7, -1, 1, 3]);
}
