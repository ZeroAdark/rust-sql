//! D1293: a collation compare that raises is PostgreSQL's error, reaching the statement as PostgreSQL's (SQLSTATE and
//! message). `agg_sort`'s text comparator (`text_cmp`) read the failure as `Equal` and stashed nothing, so the emit's sort
//! returned 0 and the rows left in the wrong order with no error; a `RowCmp` sort (`put_var`, the var run's put) returned
//! `ERROR` but overwrote the compare's stashed error with "gravity: a sort compare failed". The collation seam here raises
//! for every compare, as ICU's `collation failed` or a locale that cannot be opened does.
use gravity_abi::breaker::{KeyKind, Rec, RecordLayout, SlotKind, SortRun, LAYOUT_WORDS};
use gravity_abi::status::ERROR;
use gravity_abi::{Ctx, Site};
use gravity_kernels::host_abi::{agg_sort, sort_full_var, sort_image, EmitState, SortSpec, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, SK_TEXT, SK_TEXT_COLL_SHIFT, VAR_TBL, VAR_WIDTH};

/// Not C (950) nor 0: the compare goes to the collation seam.
const COLL: u32 = 12345;
const RAISED: &str = "collation failed: D1293 pin";

fn arm_seam() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| pg_locale_seams::varstr_cmp_locale::set(|_coll, _a, _b| Err(Box::new(gravity_fn::PgError::error(RAISED)))));
}

/// The error the statement would raise: the stash's, its message.
fn stashed() -> Option<String> {
    gravity_fn::take_error().map(|e| e.message().to_owned())
}

/// The dictionary's two entries as 4-byte-header varlena images.
static ENTRIES: [[u8; 8]; 2] = [[(8 << 2) as u8, 0, 0, 0, b'b', b'i', b'n', b'g'], [(7 << 2) as u8, 0, 0, 0, b'm', b'a', b'p', 0]];

unsafe extern "C" fn dict(_s: *mut Site, _part: u32, _lane: u32, code: u32, out: *mut u64) -> i64 {
    match ENTRIES.get(code as usize) {
        Some(e) => {
            *out = e.as_ptr() as u64;
            0
        }
        None => ERROR,
    }
}

fn with_ctx<T>(f: impl FnOnce(*mut Ctx) -> T) -> T {
    // SAFETY: a site and a ctx are plain words; zero is their closed state.
    let mut site: Site = unsafe { core::mem::zeroed() };
    site.dict = Some(dict);
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.sites = &mut site;
    ctx.width = 8;
    f(&mut ctx)
}

/// Two kept records whose text sort key renders ("bing", "map") and compares under a collation that raises: the sort
/// returns `ERROR` with the collation's error stashed for the emit to raise, not 0 with the records in key-word order.
#[test]
fn agg_sort_raises_a_text_keys_failed_collation_compare() {
    arm_seam();
    const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinCode]);
    let words = LAYOUT.size() as usize / 8;
    let mut recs = [vec![0u64; words], vec![0u64; words]];
    for (i, r) in recs.iter_mut().enumerate() {
        r[0] = i as u64;
        r[LAYOUT.slot_off[0] as usize / 8] = 3 << 32 | (1 - i as u64);
        // SAFETY: `r` is `LAYOUT.size()` 8-aligned bytes.
        assert_eq!(unsafe { Rec::at(r.as_mut_ptr() as *mut u8, &LAYOUT) }.i64(0) as u64, 3 << 32 | (1 - i as u64));
    }
    let mut ptrs: Vec<*const u8> = recs.iter().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: 2, kept: 2 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 0;
    desc[DESC_NSORT / 8] = 1;
    desc[DESC_SORT / 8] = SK_TEXT | (COLL as u64) << SK_TEXT_COLL_SHIFT;
    let _ = stashed();
    // SAFETY: `st` holds two live records of `LAYOUT`, `desc` a descriptor of it; site 0 renders through `dict`.
    let r = with_ctx(|ctx| unsafe { agg_sort(ctx as u64, (&mut st as *mut EmitState as *mut u8) as u64, (desc.as_ptr() as *const u8) as u64) });
    assert_eq!(r, ERROR, "a failed collation compare sorted as equal and the sort returned 0: rows out of order, no error (D1293)");
    assert_eq!(stashed().as_deref(), Some(RAISED), "the statement raises the collation's own error");
}

const ROW_BYTES: u32 = 40;

/// One text key (its image's slot at 16) under `COLL`, the tag at 24, the record word at 32 (kernels tests/v4_m3.rs `vtbl`).
fn vtbl() -> [u32; 12] {
    let spec = SortSpec::pack(&[(Some((16, VAR_WIDTH)), false)]).unwrap().0;
    [ROW_BYTES, 2, 3 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 1, 32, 2, 16, COLL, 0]
}

unsafe fn push_text(run: *mut u8, x: &[u8], tag: u64) {
    let r = run as *mut SortRun;
    let p = (*r).rows_ptr().add((*r).len as usize * ROW_BYTES as usize) as *mut u64;
    *p.add(4) = 0;
    assert_eq!(sort_image(run, x.as_ptr(), x.len() as u64, 16, 32), 0);
    let mut w = [0u8; 8];
    w[..x.len().min(8)].copy_from_slice(&x[..x.len().min(8)]);
    (*r).push();
    (*p, *p.add(1), *p.add(3)) = (1, u64::from_be_bytes(w), tag);
}

/// A var run whose two rows tie on the key's prefix word, so the tail's collation compare decides and raises: the put
/// returns `ERROR` with the collation's error in the stash, not "gravity: a sort compare failed" over it.
#[test]
fn a_var_runs_put_raises_the_compares_own_error() {
    arm_seam();
    let tbl = vtbl();
    let mut region = vec![0u64; 512];
    let run = region.as_mut_ptr() as *mut u8;
    // SAFETY: `region` is 4 KiB of 8-aligned words, armed as a share (word 2 = its bytes) of `ROW_BYTES` rows.
    unsafe {
        (*(run as *mut SortRun)).row_bytes = (region.len() * 8) as u32;
        SortRun::at_share(run, ROW_BYTES);
        push_text(run, b"strawberry jam", 0);
        push_text(run, b"strawberry fool", 1);
    }
    let _ = stashed();
    // SAFETY: `run` holds two rows of `tbl`'s shape with their copies; the put fails at the compare, before any host call.
    let r = with_ctx(|ctx| unsafe { sort_full_var(ctx, run, tbl.as_ptr()) });
    assert_eq!(r, ERROR, "the compare failed: the put fails");
    assert_eq!(stashed().as_deref(), Some(RAISED), "the compare's error, PostgreSQL's, not the kernel's own over it (D1293)");
}
