//! M1's host entries (kernels host_abi v4 m1) over hand-built sites; `raise_held` over a fake spill op.
use gravity_abi::ctx::lane::{KIND_BYREF, KIND_CODES};
use gravity_abi::ctx::Lane;
use gravity_abi::{Ctx, Site};
use gravity_kernels::host_abi::raise_held;
use gravity_kernels::ops::agg::entries::like_frame;

/// A 4-byte-header varlena image of `s`, padded to 8 bytes.
fn image(s: &str) -> Vec<u8> {
    let mut v = (((4 + s.len()) as u32) << 2).to_le_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v.resize(v.len().next_multiple_of(8), 0);
    v
}

/// `like_frame` over one window of `rows` (`None` = NULL) on lane 0 of site 0: the images laid out in row order in one
/// arena (`tiled`) or in reverse order; the verdict per row, or `None` when the host declined.
fn frame(rows: &[Option<&str>], tiled: bool, pattern: &str, negated: bool, kind: u8) -> Option<Vec<bool>> {
    let mut arena = Vec::new();
    let mut at = vec![usize::MAX; rows.len()];
    let order: Vec<usize> = if tiled { (0..rows.len()).collect() } else { (0..rows.len()).rev().collect() };
    for &i in order.iter() {
        if let Some(s) = rows[i] {
            at[i] = arena.len();
            arena.extend(image(s));
        }
    }
    let ptrs: Vec<u64> = at.iter().map(|&o| if o == usize::MAX { 0 } else { arena.as_ptr() as u64 + o as u64 }).collect();
    let mut valid = vec![0u64; rows.len().div_ceil(64).max(1)];
    for (i, r) in rows.iter().enumerate() {
        valid[i / 64] |= (r.is_some() as u64) << (i % 64);
    }
    let mut lanes = [Lane { ptr: ptrs.as_ptr() as *const u8, valid: valid.as_ptr(), width: 8, kind, ..Lane::NONE }];
    // SAFETY: a site and a ctx are plain words; zero is their closed state.
    let mut site: Site = unsafe { core::mem::zeroed() };
    (site.rows, site.nlanes, site.lanes) = (rows.len() as u32, 1, lanes.as_mut_ptr());
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.sites = &mut site;
    let pat = image(pattern);
    let mut out = vec![u64::MAX; rows.len() / 64 + 1];
    // SAFETY: the site's lane and images outlive the call; `out` holds `rows / 64 + 1` words.
    let st = unsafe { like_frame(&mut ctx, 0, 0, pat.as_ptr() as u64, negated as u64, out.as_mut_ptr(), out.len() as u64) };
    if st != 0 {
        return None;
    }
    assert_eq!(out[rows.len() / 64] >> (rows.len() % 64), 0, "no bit past the rows");
    Some((0..rows.len()).map(|i| out[i / 64] >> (i % 64) & 1 == 1).collect())
}

/// PG's LIKE for the escape-free patterns these tests use: `%` any run, `_` one character.
fn like(s: &[u8], p: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'%', rest)) => (0..=s.len()).any(|k| like(&s[k..], rest)),
        Some((&c, rest)) => s.split_first().is_some_and(|(&h, t)| (c == b'_' || c == h) && like(t, rest)),
    }
}

fn rows() -> Vec<Option<&'static str>> {
    let texts = ["special requests", "no", "", "requests special", "xspecialyrequestsz", "specia", "special", "furious"];
    (0..150).map(|i| if i % 7 == 3 { None } else { Some(texts[i % texts.len()]) }).collect()
}

#[test]
fn like_frame_matches_pg_over_tiled_and_untiled_windows_both_forms_and_nulls() {
    let rows = rows();
    for pattern in ["%special%requests%", "%special%", "special%", "%requests", "%s_ecial%", "no"] {
        for tiled in [true, false] {
            for negated in [false, true] {
                let got = frame(&rows, tiled, pattern, negated, KIND_BYREF).expect("decided");
                for (i, r) in rows.iter().enumerate() {
                    let want = r.is_some_and(|s| like(s.as_bytes(), pattern.as_bytes()) != negated);
                    assert_eq!(got[i], want, "{pattern:?} tiled {tiled} negated {negated} row {i} {r:?}");
                }
            }
        }
    }
}

#[test]
fn like_frame_declines_a_pattern_with_no_literal_and_a_code_lane() {
    let rows = rows();
    assert_eq!(frame(&rows, true, "%", false, KIND_BYREF), None);
    assert_eq!(frame(&rows, true, "_%_", false, KIND_BYREF), None);
    assert_eq!(frame(&rows, true, "%special%", false, KIND_CODES), None);
}

thread_local! {
    static HELD: core::cell::Cell<(i64, Vec<(u32, u64)>)> = const { core::cell::Cell::new((0, Vec::new())) };
}

/// The driver's spill op over a fake binding: every call recorded, `HELD`'s status returned.
unsafe extern "C" fn fake_spill(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    HELD.with(|h| {
        let (s, mut calls) = h.take();
        calls.push((op, arg));
        h.set((s, calls));
        s
    })
}

#[test]
fn raise_held_is_the_drivers_op_14_and_passes_its_status_through() {
    let mut program = Box::new(gravity_abi::ProgramBlock::NONE);
    program.spill = Some(fake_spill);
    let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(0) / 8];
    let c = ctx.as_mut_ptr() as *mut Ctx;
    unsafe { (*c).program = &mut *program };
    for held in [0i64, -7] {
        HELD.with(|h| h.set((held, Vec::new())));
        assert_eq!(unsafe { raise_held(c) }, held, "nothing held reads 0, a held error its status");
        assert_eq!(HELD.with(|h| h.take().1), vec![(gravity_abi::program::SPILL_RAISE_HELD, 0)]);
    }
}
