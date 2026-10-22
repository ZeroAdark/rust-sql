//! D344: a code render whose dictionary read fails is an error the caller raises, never a NULL. `agg_render` answers the
//! failed read's status (below zero as an `i64`), where it answered null, the NULL the emit then served in the row; and
//! `agg_sort` returns the status of a text key's failed render, where it sorted the key as a NULL and returned 0. The
//! site's dictionary host fails a code past its entries with `ERROR`, the read failure of a corrupt or short dictionary.
use gravity_abi::breaker::{KeyKind, Rec, RecordLayout, SlotKind, LAYOUT_WORDS, MINCODE_NONE};
use gravity_abi::status::ERROR;
use gravity_abi::{Ctx, Site};
use gravity_kernels::host_abi::{agg_render, agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, SK_TEXT, SK_TEXT_COLL_SHIFT};

/// The dictionary's two entries as 4-byte-header varlena images.
static ENTRIES: [[u8; 8]; 2] = [[(8 << 2) as u8, 0, 0, 0, b'b', b'i', b'n', b'g'], [(7 << 2) as u8, 0, 0, 0, b'm', b'a', b'p', 0]];

/// `Site.dict`: the entry's image, or `ERROR` for a code past the entries (the read failed).
unsafe extern "C" fn dict(_s: *mut Site, _part: u32, _lane: u32, code: u32, out: *mut u64) -> i64 {
    match ENTRIES.get(code as usize) {
        Some(e) => {
            *out = e.as_ptr() as u64;
            0
        }
        None => ERROR,
    }
}

/// `f` over a ctx whose site 0 renders through `dict`.
fn with_ctx<T>(f: impl FnOnce(*mut Ctx) -> T) -> T {
    // SAFETY: a site and a ctx are plain words; zero is their closed state.
    let mut site: Site = unsafe { core::mem::zeroed() };
    site.dict = Some(dict);
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.sites = &mut site;
    f(&mut ctx)
}

/// The failed read is its status, not null; a code with an entry still renders it and `MINCODE_NONE` is still NULL.
#[test]
fn agg_render_answers_a_failed_dictionary_read_as_its_status_not_null() {
    with_ctx(|ctx| unsafe {
        let p = agg_render(ctx as u64, 0, 0, 3 << 32 | 1) as *const u8;
        assert_eq!(gravity_kernels::dict::reps::varlena_bytes(p), b"map", "an entry renders");
        assert!((agg_render(ctx as u64, 0, 0, MINCODE_NONE) as *const u8).is_null(), "no code is NULL");
        let bad = agg_render(ctx as u64, 0, 0, 3 << 32 | 100) as *const u8;
        assert!(!bad.is_null(), "a failed dictionary read answered null: the emit serves it as a NULL (D344)");
        assert_eq!(bad as usize as i64, ERROR, "the dictionary host's status");
    });
}

/// Two kept records whose text sort key renders through a code past the dictionary: the sort returns the failed read's
/// status for the emit to raise, not 0 with the keys ordered as NULLs.
#[test]
fn agg_sort_returns_a_text_keys_failed_render() {
    const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinCode]);
    let words = LAYOUT.size() as usize / 8;
    let mut recs = [vec![0u64; words], vec![0u64; words]];
    for (i, r) in recs.iter_mut().enumerate() {
        r[0] = i as u64;
        r[LAYOUT.slot_off[0] as usize / 8] = 3 << 32 | (100 + i as u64);
        // SAFETY: `r` is `LAYOUT.size()` 8-aligned bytes.
        assert_eq!(unsafe { Rec::at(r.as_mut_ptr() as *mut u8, &LAYOUT) }.i64(0) as u64, 3 << 32 | (100 + i as u64));
    }
    let mut ptrs: Vec<*const u8> = recs.iter().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: 2, kept: 2 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 0;
    desc[DESC_NSORT / 8] = 1;
    // key 0's text representative: slot 0 (SK_TEXT_SLOT_SHIFT), code lane 0 (SK_TEXT_LANE_SHIFT) of site 0, under C
    desc[DESC_SORT / 8] = SK_TEXT | 950 << SK_TEXT_COLL_SHIFT;
    // SAFETY: `st` holds two live records of `LAYOUT`, `desc` a descriptor of it; site 0 renders through `dict`.
    let r = with_ctx(|ctx| unsafe { agg_sort(ctx as u64, (&mut st as *mut EmitState as *mut u8) as u64, (desc.as_ptr() as *const u8) as u64) });
    assert_eq!(r, ERROR, "a text key's failed render sorted as a NULL (D344)");
}
