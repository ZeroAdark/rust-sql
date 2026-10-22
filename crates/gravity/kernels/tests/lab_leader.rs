//! The leader merge face on the rig (lower.md §15.8): hand-built leader regions of several owners
//! whose kept copies carry `(part, code)` representatives (the generic owner's form) or rendered datums
//! (t3's), merged and emitted by a hand pipeline that walks the run the way the probe's IR does —
//! the order (count DESC, then the keys, text by bytes, NULL last), the bound and the offset, the renders
//! carrying each record's own part (§15.7's class), and a stop mid-run resuming on the same run.
use gravity_kernels_rig as common;

use std::cell::{Cell, RefCell};

use common::*;
use gravity_abi::breaker::{Owned, OwnerTop, RecordLayout, MINCODE_NONE, MOVING, OWNED_HDR, OWNERTOP_HDR, TABLE_OWNED};
use gravity_abi::ctx::{resume, RESUME_NONE};
use gravity_abi::status::{suspend, ERROR};
use gravity_abi::{Ctx, Datum};
use gravity_kernels::ops::sort::topn::t3;
use gravity_kernels::ops::sort::topn::t3::{packed, shape_word, K_INT, K_TEXT, MODE_HASH, ORDER_COUNT_DESC, OUT_COUNT, OUT_KEY, S_COUNT};
use gravity_kernels::leader::{leader_run, LeaderRun, LeaderSpec, FORM_CODED, FORM_RENDERED};
use gravity_kernels::dict::reps::{varlena_bytes, Reps, MAX_REPS, REP_NONE};
use gravity_kernels::util::{done, emit, int8_datum, out_count, HOST_STOP};
use gravity_program::hand::Oids;
use gravity_program::{OutCol, Program};

#[derive(Clone, Copy)]
enum Col {
    Text(usize),
    Count(usize),
}

struct Cfg {
    spec: LeaderSpec,
    offset: usize,
    cols: Vec<Col>,
}

thread_local! {
    static CFG: RefCell<Option<Cfg>> = const { RefCell::new(None) };
    static BASES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

/// The probe emit's shape as a hand pipeline: the face, then the walk from the offset or the resume
/// position, one out row per record, a stop resuming at the record after the pushed one.
unsafe extern "C-unwind" fn test_emit(ctx: *mut Ctx) -> i64 {
    CALLS.with(|c| c.set(c.get() + 1));
    let (spec, offset, cols) = CFG.with(|c| {
        let c = c.borrow();
        let c = c.as_ref().unwrap();
        (c.spec, c.offset, c.cols.clone())
    });
    let mut run = LeaderRun { base: core::ptr::null_mut(), len: 0 };
    let r = leader_run(ctx, &spec, &mut run);
    if r < 0 {
        return r;
    }
    BASES.with(|b| b.borrow_mut().push(run.base as u64));
    let word = (*ctx).resume;
    let mut i = if word == RESUME_NONE { offset } else { resume::position(word) as usize };
    let rec = spec.layout.size() as usize;
    let mut v = [0 as Datum; 4];
    let mut nl = [0u8; 4];
    while i < run.len as usize {
        let p = run.base.add(i * rec);
        for (c, col) in cols.iter().enumerate() {
            nl[c] = 0;
            v[c] = match *col {
                Col::Text(slot) => {
                    let w = *(p.add(spec.layout.slot_off[slot] as usize) as *const u64);
                    nl[c] = (w == 0) as u8;
                    w
                }
                Col::Count(slot) => {
                    let w = *(p.add(spec.layout.slot_off[slot] as usize) as *const u64);
                    assert_eq!(w & MOVING, 0, "the run's count carries no mark");
                    int8_datum(w as i64)
                }
            };
        }
        i += 1;
        let f = emit(ctx, cols.len(), &[(v.as_ptr(), nl.as_ptr(), cols.len())]);
        if f < 0 {
            return ERROR;
        }
        if f == HOST_STOP {
            (*ctx).resume = resume::word(resume::EMIT, i as u64);
            return suspend(out_count(ctx) == 0);
        }
    }
    done(ctx)
}

/// A record of `layout` from its key words and slot words.
fn record(layout: &RecordLayout, keys: &[u64], slots: &[u64]) -> Vec<u8> {
    let mut r = vec![0u8; layout.size() as usize];
    for (k, w) in keys.iter().enumerate() {
        r[8 * k..8 * k + 8].copy_from_slice(&w.to_le_bytes());
    }
    for (j, w) in slots.iter().enumerate() {
        let o = layout.slot_off[j] as usize;
        r[o..o + 8].copy_from_slice(&w.to_le_bytes());
    }
    r
}

/// A leader region of `owners` (each its kept records), the owner tops sized for `n`.
fn region(layout: &RecordLayout, n: usize, owners: &[Vec<Vec<u8>>]) -> Vec<u64> {
    let rec = layout.size() as usize;
    let table_off = (OWNERTOP_HDR + n.max(1) * rec + 7) & !7;
    let owner_bytes = table_off + 256;
    let bytes = OWNED_HDR + owners.len() * owner_bytes;
    let mut buf = vec![0u64; bytes / 8];
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe {
        *(p as *mut Owned) = Owned { nowners: owners.len() as u32, owner_bytes: owner_bytes as u32, limit: 0, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) };
        for (j, recs) in owners.iter().enumerate() {
            let o = p.add(OWNED_HDR + j * owner_bytes);
            *(o as *mut OwnerTop) = OwnerTop { len: recs.len() as u32, n: n as u32, table_off: table_off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
            for (i, r) in recs.iter().enumerate() {
                assert!(i < n.max(1), "an owner keeps at most n");
                core::ptr::copy_nonoverlapping(r.as_ptr(), o.add(OWNERTOP_HDR + i * rec), rec);
            }
        }
    }
    buf
}

fn program(layout: &RecordLayout, out: &[OutCol], out_rows: u32) -> Program {
    emit_rig_program(Oids::T_A, &[1], 0b1, 1, layout, out, out_rows, 4, test_emit)
}

fn params() -> Vec<Datum> {
    vec![10, 0, shape_word(1, 1, ORDER_COUNT_DESC, MODE_HASH, 0, None, 0, 2), packed(&[(0, K_TEXT)], 4), packed(&[(0, S_COUNT)], 4), packed(&[(0, OUT_KEY), (0, OUT_COUNT)], 4), 0, t3::misc_word(4, 0, 4)]
}

fn text_of(d: Datum) -> String {
    String::from_utf8(unsafe { varlena_bytes(d as *const u8) }.to_vec()).unwrap()
}

fn dict() -> Vec<Vec<u8>> {
    (0..64).map(|i| format!("t{i:02}").into_bytes()).collect()
}

fn reps(slot: u8) -> Reps {
    let mut r = [(REP_NONE, 0u8); MAX_REPS];
    r[0] = (slot, 0);
    r
}

/// The emit over `region` in a fresh rig: the rows as (text or None, count), the render calls, the
/// pipeline calls and the run bases the face answered.
fn run(layout: &RecordLayout, cfg: Cfg, region: &[u64], out_rows: u32, stop_after: Option<usize>) -> (Vec<(Option<String>, i64)>, Vec<String>, usize, Vec<u64>) {
    let out = [OutCol::new("t", 25, -1, -1, false), OutCol::new("c", 20, -1, 8, true)];
    let mut rig = Rig::new(program(layout, &out, out_rows), params());
    rig.site(0).codes_lanes = 0b1;
    rig.site(0).windows_per_part = 3;
    rig.site(0).dict = dict();
    rig.host.stop_after_rows = stop_after;
    assert!(rig.partial.len() >= region.len(), "the rig's partial holds the region");
    rig.partial[..region.len()].copy_from_slice(region);
    CFG.with(|c| *c.borrow_mut() = Some(cfg));
    BASES.with(|b| b.borrow_mut().clear());
    CALLS.with(|c| c.set(0));
    let mut rets = Vec::new();
    loop {
        unsafe { (*rig.ctx()).resume = if matches!(rets.last(), Some(Ret::Suspended { .. })) { (*rig.ctx()).resume } else { 0 } };
        let r = rig.call_pipeline(1);
        rets.push(r);
        if !matches!(r, Ret::Suspended { .. }) {
            break;
        }
        rig.host.stop_after_rows = None;
    }
    assert_eq!(rets.last(), Some(&Ret::Done), "{rets:?}");
    let rows = rig.host.pushed.iter().zip(rig.host.pushed_nulls.iter()).map(|(r, n)| ((n[0] == 0).then(|| text_of(r[0])), r[1] as i64)).collect();
    let calls = rig.host.calls.iter().filter(|c| c.starts_with("dict(")).cloned().collect();
    (rows, calls, CALLS.with(|c| c.get()), BASES.with(|b| b.borrow().clone()))
}

fn spec(layout: RecordLayout, rep_slot: u8, form: u8, n: u32) -> LeaderSpec {
    LeaderSpec { layout, reps: reps(rep_slot), order_slot: 0, site: 0, form, tie_fp: 0, n, skip: 0 }
}

/// One text key: three owners' kept copies with `(part, code)` representatives in three parts; the run is
/// the count order with the ties by the rendered text; each render names the record's own part; the offset.
#[test]
fn coded_representatives_merge_in_count_order_with_text_ties_and_their_own_parts() {
    let layout = t3::layout_of_kinds_nn(&[K_TEXT], 1, &[S_COUNT], true);
    let coded = |part: u64, code: u64| (part << 32) | code;
    let r = |code: u64, count: u64, part: u64| record(&layout, &[code, 0x1234], &[count | MOVING, coded(part, code)]);
    let owners = vec![vec![r(5, 7, 0), r(3, 7, 1), r(9, 2, 2)], vec![r(1, 9, 2), r(7, 7, 0)], vec![r(2, 1, 1), r(8, 7, 1), r(4, 3, 0)]];
    let expect: Vec<(Option<String>, i64)> = [(1, 9), (3, 7), (5, 7), (7, 7), (8, 7), (4, 3), (9, 2), (2, 1)].iter().map(|&(c, n)| (Some(format!("t{c:02}")), n)).collect();
    let part_of = |code: u64| match code {
        5 | 7 | 4 => 0,
        3 | 2 | 8 => 1,
        _ => 2,
    };
    for (n, offset, stop) in [(4usize, 0usize, None), (4, 1, None), (4, 0, Some(2)), (8, 2, Some(1)), (100, 0, Some(3)), (0, 0, None)] {
        // the owners may keep more than the face's bound (the hand program's `out_rows` ≥ LIMIT + OFFSET)
        let reg = region(&layout, n.max(3), &owners);
        let cfg = Cfg { spec: spec(layout, 1, FORM_CODED, n as u32), offset, cols: vec![Col::Text(1), Col::Count(0)] };
        let (rows, calls, ncalls, bases) = run(&layout, cfg, &reg, 1, stop);
        let want: Vec<_> = expect.iter().take(n).skip(offset).cloned().collect();
        assert_eq!(rows, want, "n {n} offset {offset} stop {stop:?}");
        for c in &calls {
            let inner = &c[5..c.len() - 1];
            let f: Vec<u64> = inner.split(", ").map(|x| x.parse().unwrap()).collect();
            assert_eq!(f[1], part_of(f[2]), "a render names the record's own part: {c}");
        }
        for (t, _) in &want {
            let code: u64 = t.as_ref().unwrap()[1..].parse().unwrap();
            assert!(calls.contains(&format!("dict(0, {}, {code})", part_of(code))), "the kept record {code} was rendered: {calls:?}");
        }
        assert!(bases.windows(2).all(|w| w[0] == w[1]), "every resume finds the same run: {bases:?}");
        assert_eq!(ncalls, bases.len());
        if stop.is_some() && n > 0 && want.len() > stop.unwrap() {
            assert!(ncalls >= 2, "the stop suspended the emit");
        }
    }
}

/// `tie_fp`: the sort names no text order, so the count ties order by the key words (here the reverse of the
/// text order) and the face renders the run's records only, once each, after the merge.
#[test]
fn an_unordered_text_key_ties_by_its_words_and_renders_the_run_only() {
    let layout = t3::layout_of_kinds_nn(&[K_TEXT], 1, &[S_COUNT], true);
    let coded = |part: u64, code: u64| (part << 32) | code;
    let r = |code: u64, count: u64, part: u64| record(&layout, &[100 - code, 0x1234], &[count | MOVING, coded(part, code)]);
    let owners = vec![vec![r(5, 7, 0), r(3, 7, 1), r(9, 2, 2)], vec![r(1, 9, 2), r(7, 7, 0)], vec![r(2, 1, 1), r(8, 7, 1), r(4, 3, 0)]];
    let expect: Vec<(Option<String>, i64)> = [(1, 9), (8, 7), (7, 7), (5, 7), (3, 7), (4, 3), (9, 2), (2, 1)].iter().map(|&(c, n)| (Some(format!("t{c:02}")), n)).collect();
    for (n, offset, stop) in [(3usize, 0usize, None), (4, 1, Some(2)), (8, 2, Some(1)), (100, 0, None)] {
        let reg = region(&layout, n.max(3), &owners);
        let mut sp = spec(layout, 1, FORM_CODED, n as u32);
        sp.tie_fp = 1;
        let (rows, calls, _, bases) = run(&layout, Cfg { spec: sp, offset, cols: vec![Col::Text(1), Col::Count(0)] }, &reg, 1, stop);
        let want: Vec<_> = expect.iter().take(n).skip(offset).cloned().collect();
        assert_eq!(rows, want, "n {n} offset {offset} stop {stop:?}");
        assert_eq!(calls.len(), n.min(expect.len()), "one render per record of the run, none to compare: {calls:?}");
        for (t, _) in expect.iter().take(n) {
            let code: u64 = t.as_ref().unwrap()[1..].parse().unwrap();
            assert_eq!(calls.iter().filter(|c| c.ends_with(&format!(", {code})"))).count(), 1, "the run's record {code} rendered once: {calls:?}");
        }
        assert!(bases.windows(2).all(|w| w[0] == w[1]), "every resume finds the same run: {bases:?}");
    }
}

/// The same region with rendered representatives (t3's owner form): no render at all, the same run.
#[test]
fn rendered_representatives_merge_without_a_render() {
    let layout = t3::layout_of_kinds_nn(&[K_TEXT], 1, &[S_COUNT], true);
    let images: Vec<Vec<u8>> = (0..16u32)
        .map(|i| {
            let e = format!("t{i:02}");
            let mut v = ((4 + e.len()) as u32) << 2;
            v = v.to_le();
            let mut img = v.to_le_bytes().to_vec();
            img.extend_from_slice(e.as_bytes());
            img
        })
        .collect();
    let r = |code: usize, count: u64| record(&layout, &[code as u64, 0x1234], &[count, images[code].as_ptr() as u64]);
    let owners = vec![vec![r(5, 7), r(3, 7), r(9, 2)], vec![r(1, 9), r(7, 7)], vec![r(2, 1), r(8, 7), r(4, 3)]];
    let expect: Vec<(Option<String>, i64)> = [(1, 9), (3, 7), (5, 7), (7, 7), (8, 7), (4, 3), (9, 2), (2, 1)].iter().map(|&(c, n)| (Some(format!("t{c:02}")), n)).collect();
    for (n, offset, stop) in [(4usize, 0usize, None), (5, 1, Some(1)), (8, 0, Some(4))] {
        let reg = region(&layout, n, &owners);
        let cfg = Cfg { spec: spec(layout, 1, FORM_RENDERED, n as u32), offset, cols: vec![Col::Text(1), Col::Count(0)] };
        let (rows, calls, _, bases) = run(&layout, cfg, &reg, 1, stop);
        let want: Vec<_> = expect.iter().take(n).skip(offset).cloned().collect();
        assert_eq!(rows, want, "n {n} offset {offset}");
        assert!(calls.is_empty(), "no render under the rendered form: {calls:?}");
        assert!(bases.windows(2).all(|w| w[0] == w[1]));
    }
}

/// A NULL key's group (`MINCODE_NONE` representative) orders last among its count's ties under both forms,
/// and is emitted as NULL.
#[test]
fn a_null_key_orders_last_among_its_ties_and_emits_null() {
    let layout = t3::layout_of_kinds_nn(&[K_TEXT], 1, &[S_COUNT], false);
    let coded = |part: u64, code: u64| (part << 32) | code;
    for form in [FORM_CODED, FORM_RENDERED] {
        let img = {
            let e = b"t05";
            let mut v = (((4 + e.len()) as u32) << 2).to_le_bytes().to_vec();
            v.extend_from_slice(e);
            v
        };
        let rep5 = if form == FORM_CODED { coded(1, 5) } else { img.as_ptr() as u64 };
        let owners = vec![vec![record(&layout, &[0, 0], &[7, MINCODE_NONE]), record(&layout, &[5, 0x1234], &[7, rep5])], vec![record(&layout, &[0, 0], &[8, MINCODE_NONE])]];
        let reg = region(&layout, 3, &owners);
        let cfg = Cfg { spec: spec(layout, 1, form, 3), offset: 0, cols: vec![Col::Text(1), Col::Count(0)] };
        let (rows, calls, _, _) = run(&layout, cfg, &reg, 1, None);
        assert_eq!(rows, vec![(None, 8), (Some("t05".into()), 7), (None, 7)], "form {form}");
        assert_eq!(calls.len(), if form == FORM_CODED { 1 } else { 0 }, "form {form}: {calls:?}");
    }
}

/// Two keys, an int before the text: a count tie orders by the int first, then the text.
#[test]
fn an_int_key_before_the_text_key_orders_the_ties() {
    let layout = t3::layout_of_kinds_nn(&[K_INT, K_TEXT], 1, &[S_COUNT], true);
    let coded = |part: u64, code: u64| (part << 32) | code;
    let r = |k: u64, code: u64, count: u64, part: u64| record(&layout, &[k, code, 0x1234], &[count, coded(part, code)]);
    let owners = vec![vec![r(2, 5, 7, 0), r(1, 9, 7, 2)], vec![r(1, 3, 7, 1), r(2, 1, 9, 2)]];
    let reg = region(&layout, 4, &owners);
    let cfg = Cfg { spec: spec(layout, 1, FORM_CODED, 4), offset: 0, cols: vec![Col::Text(1), Col::Count(0)] };
    let (rows, _, _, _) = run(&layout, cfg, &reg, 1, None);
    assert_eq!(rows, vec![(Some("t01".into()), 9), (Some("t03".into()), 7), (Some("t09".into()), 7), (Some("t05".into()), 7)]);
}

/// The kept records as a heap: 3,000 groups from five owners in a scrambled order with heavy count ties cut to
/// `n` = 1,010 (q38's bound) equal the full sort's first `n`, in order.
#[test]
fn a_bound_past_a_thousand_keeps_the_best_in_order() {
    let layout = t3::layout_of_kinds_nn(&[K_TEXT], 1, &[S_COUNT], true);
    let total = 3000usize;
    let images: Vec<Vec<u8>> = (0..total)
        .map(|i| {
            let e = format!("u{:05}", (i * 7919) % total);
            let mut img = (((4 + e.len()) as u32) << 2).to_le_bytes().to_vec();
            img.extend_from_slice(e.as_bytes());
            img
        })
        .collect();
    let count = |i: usize| 1 + ((i * 31) % 7) as u64;
    let mut owners: Vec<Vec<Vec<u8>>> = vec![Vec::new(); 5];
    for i in 0..total {
        owners[(i * 13) % 5].push(record(&layout, &[i as u64, 0x77], &[count(i), images[i].as_ptr() as u64]));
    }
    let mut expect: Vec<(Option<String>, i64)> = (0..total).map(|i| (Some(format!("u{:05}", (i * 7919) % total)), count(i) as i64)).collect();
    expect.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let (n, offset) = (1010usize, 1000usize);
    let reg = region(&layout, n, &owners);
    let cfg = Cfg { spec: spec(layout, 1, FORM_RENDERED, n as u32), offset, cols: vec![Col::Text(1), Col::Count(0)] };
    let (rows, _, _, _) = run(&layout, cfg, &reg, 64, None);
    assert_eq!(rows, expect[offset..n].to_vec());
    let cfg = Cfg { spec: spec(layout, 1, FORM_RENDERED, n as u32), offset: 0, cols: vec![Col::Text(1), Col::Count(0)] };
    let (rows, _, _, _) = run(&layout, cfg, &reg, 64, None);
    assert_eq!(rows, expect[..n].to_vec());
}
