//! A part's `regexp_replace` derived-fingerprint table (`TableFn::DerivedFingerprint`) is the statement's: a query
//! constant keys it, so it is never cached across statements (the user 09-19). Each statement over the same parts builds its
//! own tables, one per part, with the same fingerprints; a spec that differs in its pattern, its replacement or its
//! collation fingerprints its own derived values. A derived table over entries the regexp raised 2201B on carries no fault
//! bitmap and those entries are [0, 0] (lowerv4's Fp128 convention: re-evaluate the row); a cancel mid-build raises 57014
//! and publishes nothing. One test: the build and cache census counters (and the interrupt seam) are process-wide. Beside
//! it, the payload view: a resident dictionary's entries walk hands on the bulk cursor's entries.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_hosts::dict::text_hosts::{derived_tables_built, fp_seed, fp_tables_resident, RegexpPlan, TableFn, TableSpec};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::BuiltFixture;
use pgrc_read::{PartExpect, PartRegistry};
use types_error::{PgError, ERRCODE_INVALID_REGULAR_EXPRESSION, ERRCODE_QUERY_CANCELED};

const PG_UTF8: i32 = 6;

fn derived(pattern: &[u8], replacement: &[u8], collation: u32) -> TableSpec {
    TableSpec { over: 0, func: TableFn::DerivedFingerprint { pattern: pattern.to_vec(), replacement: replacement.to_vec(), collation, encoding: PG_UTF8, global: false } }
}

/// The fixture's parts in memory under device `dev` (a part's uuid folds its device and inode: another `dev` is another part).
fn parts_on(bf: &BuiltFixture, dev: u64) -> Arc<PartList> {
    let parts = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
            PartSpec { io: PartIoSpec::Mem { bytes, dev, ino: 900 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }
        })
        .collect();
    Arc::new(PartList::new(parts))
}

/// One statement over every window of `list` with `spec` as lane 2, `visit` called per window with the source, the part,
/// the code lane, the rows and the filled table lane; the statement's error (a host's, stashed) as the `Err`.
/// The registry is the process's, shared by every statement: it keeps the parts open, as the resident catalog does (a part
/// the cache's entry outlives is swept at the next insert).
fn scan(reg: &Arc<PartRegistry>, list: &Arc<PartList>, rel: u32, spec: &TableSpec, visit: &mut dyn FnMut(&mut GranuleRangeSource, u32, Lane, usize, &Lane)) -> Result<(), Box<PgError>> {
    let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::clone(reg)).with_tables(vec![spec.clone()]);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let out = loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = match src.window(&mut w) {
            Ok(0) => break Ok(()),
            Ok(n) => n as usize,
            Err(_) => break Err(gravity_hosts::take_error().expect("a host's error is stashed")),
        };
        let (codes, p) = (w.lanes()[0], w.base().part_id);
        let mut t = Lane::NONE;
        match src.lane(2, &[], &mut t) {
            Ok(filled) => assert_eq!(filled, 1, "the derived lane fills"),
            Err(_) => break Err(gravity_hosts::take_error().expect("a host's error is stashed")),
        }
        visit(&mut src, p, codes, n, &t);
    };
    src.unbind();
    out
}

/// One statement over every window of `list` with `spec` as lane 2: `(part, code) -> fingerprint` and the entry's value.
fn statement(reg: &Arc<PartRegistry>, list: &Arc<PartList>, rel: u32, spec: &TableSpec) -> (BTreeMap<(u32, u32), [u64; 2]>, BTreeMap<(u32, u32), Vec<u8>>) {
    let (mut fps, mut vals) = (BTreeMap::new(), BTreeMap::new());
    scan(reg, list, rel, spec, &mut |src, p, codes, n, t| {
        let table = unsafe { gravity_dict::Fp128::from_lane(t) }.expect("fp128");
        for i in 0..n {
            if !unsafe { codes.is_valid(i) } {
                continue;
            }
            let c = unsafe { *(codes.ptr as *const u32).add(i) };
            fps.insert((p, c), unsafe { table.get(c) });
            if !vals.contains_key(&(p, c)) {
                let (datum, len) = src.dict_value(p, 0, c).unwrap();
                vals.insert((p, c), unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) }.to_vec());
            }
        }
    })
    .unwrap_or_else(|e| panic!("statement: {e:?}"));
    (fps, vals)
}

/// `regexp_replace(v, '^([a-z]+)[0-9]*$', prefix || '\1')` for the fixture's `w00`..`w32` entries (every one matches).
fn want(v: &[u8], prefix: &[u8]) -> Vec<u8> {
    let letters = v.iter().take_while(|b| b.is_ascii_lowercase()).count();
    if letters > 0 && v[letters..].iter().all(u8::is_ascii_digit) {
        [prefix, &v[..letters]].concat()
    } else {
        v.to_vec()
    }
}

#[test]
fn derived_fingerprint_tables() {
    // every regexp build compiles once through the fmgr call first (D354), the open-coded program's too
    fmgr_regexp_setup();
    each_statement_builds_its_own_derived_fingerprint_tables();
    a_faulted_entry_is_zero_with_no_fault_bitmap_and_a_cancel_mid_build_publishes_nothing();
    a_call_table_fingerprints_each_entrys_result_and_zeroes_a_raising_entry();
    a_global_spec_replaces_every_match();
}

/// `regexp_replace(v, p, r, 'g')` (`global`): every match of an entry replaced, on the pattern program's road (the
/// replace-all loop) and on the function manager's (the 4-argument call with `'g'`, the automaton with n = 0); the
/// same spec without `global` replaces the first match only, and fingerprints its own values. A row's re-evaluation
/// calls the 4-argument form.
fn a_global_spec_replaces_every_match() {
    fp_seed();
    fmgr_regexp_setup();
    let bf = text_fixture();
    let list = parts_on(&bf, 45);
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let spec = |pattern: &[u8], replacement: &[u8], global: bool| TableSpec { over: 0, func: TableFn::DerivedFingerprint { pattern: pattern.to_vec(), replacement: replacement.to_vec(), collation: 950, encoding: PG_UTF8, global } };
    let digits = |v: &[u8], each: &dyn Fn(u8) -> Vec<u8>, all: bool| {
        let mut out = Vec::new();
        let mut done = false;
        for &b in v {
            if b.is_ascii_digit() && !done {
                out.extend(each(b));
                done = !all;
            } else {
                out.push(b);
            }
        }
        out
    };
    let roads: [(&[u8], &[u8], &dyn Fn(u8) -> Vec<u8>, bool); 2] = [
        (b"[0-9]", b"#", &|_| b"#".to_vec(), true),
        ("([0-9\u{1c2}])".as_bytes(), b"<\\1>", &|b| vec![b'<', b, b'>'], false),
    ];
    for (pat, rep, each, open_coded) in roads {
        let what = String::from_utf8_lossy(pat);
        assert_eq!(RegexpPlan::of(pat, rep, 950, PG_UTF8).open_coded(), open_coded, "{what}: the road");
        for global in [true, false] {
            let (fps, vals) = statement(&reg, &list, REL_TEXT + 64 + global as u32, &spec(pat, rep, global));
            assert!(!fps.is_empty());
            for ((p, c), fp) in &fps {
                let v = &vals[&(*p, *c)];
                assert_eq!(Some(*fp), gravity_dict::fp::fp128(&digits(v, each, global)), "{what} global {global}: part {p} code {c} ({:?})", String::from_utf8_lossy(v));
            }
        }
        gravity_hosts::dict::text_hosts::reevaluate(&spec(pat, rep, true), b"w12").expect("the 4-argument call with 'g'");
    }
}

/// STEP-01 / H01: any immutable builtin of the entry and varlena constants (`TableFn::DerivedCall`) is built per part by
/// the source that reads it, as the program's tables are: each entry's fingerprint is its result's (`replace(v, 'w', 'x')`), and a
/// call that raises on an entry (an invalid pattern, 2201B) leaves that entry [0, 0] with no fault bitmap: its rows
/// re-evaluate and raise where PostgreSQL evaluates.
fn a_call_table_fingerprints_each_entrys_result_and_zeroes_a_raising_entry() {
    use gravity_program::table_fn::Call;
    fp_seed();
    let bf = text_fixture();
    let list = parts_on(&bf, 44);
    let nparts = list.len() as u64;
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let call = |funcid: u32, args: &[Option<&[u8]>]| TableSpec { over: 0, func: TableFn::DerivedCall { call: Call { funcid, args: args.iter().map(|a| a.map(<[u8]>::to_vec)).collect() }, collation: 950, encoding: PG_UTF8, adaptive: false } };
    let b = derived_tables_built();
    let (fps, vals) = statement(&reg, &list, REL_TEXT + 62, &call(2087, &[None, Some(b"w"), Some(b"x")]));
    assert_eq!(derived_tables_built() - b, nparts, "one call table built per part");
    assert!(!fps.is_empty());
    for ((p, c), fp) in &fps {
        let v = &vals[&(*p, *c)];
        let want: Vec<u8> = v.iter().map(|&x| if x == b'w' { b'x' } else { x }).collect();
        assert_eq!(Some(*fp), gravity_dict::fp::fp128(&want), "part {p} code {c}");
    }
    let (fps, _) = statement(&reg, &list, REL_TEXT + 62, &call(2285, &[None, Some(b"("), Some(b"x"), Some(b"g")]));
    assert!(!fps.is_empty() && fps.values().all(|fp| *fp == [0, 0]), "an entry the call raised on is [0, 0]");
}

fn each_statement_builds_its_own_derived_fingerprint_tables() {
    fp_seed();
    let bf = text_fixture();
    let list = parts_on(&bf, 41);
    let nparts = list.len() as u64;
    let built = derived_tables_built;
    let pat = b"^([a-z]+)[0-9]*$";
    let spec = derived(pat, b"\\1", 950);
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let resident = fp_tables_resident().2;

    // 1. the first statement builds one table per part; the fingerprints are the derived values'
    let b0 = built();
    let (fps1, vals) = statement(&reg, &list, REL_TEXT + 60, &spec);
    assert_eq!(built() - b0, nparts, "one derived table built per part");
    for ((p, c), fp) in &fps1 {
        assert_eq!(Some(*fp), gravity_dict::fp::fp128(&want(&vals[&(*p, *c)], b"")), "part {p} code {c}");
    }

    // 2. the next statement with the same spec over the same parts builds its own tables, with the same answer; none is
    //    kept in the process's fingerprint cache
    let b1 = built();
    let (fps2, _) = statement(&reg, &list, REL_TEXT + 60, &spec);
    assert_eq!(built() - b1, nparts, "a second statement builds its own table per part");
    assert_eq!(fps2, fps1);
    assert_eq!(fp_tables_resident().2, resident, "no derived table resident across statements");

    // 3. a spec that differs only in its replacement, only in its collation or only in its pattern fingerprints its own values
    let others = [("replacement", derived(pat, b"x\\1", 950), &b"x"[..]), ("collation", derived(pat, b"\\1", 100), &b""[..]), ("pattern", derived(b"^([a-z]+)[0-9]+$", b"\\1", 950), &b""[..])];
    for (what, other, prefix) in &others {
        let b = built();
        let (fps, vals) = statement(&reg, &list, REL_TEXT + 60, other);
        assert_eq!(built() - b, nparts, "a spec differing in its {what}: one table per part");
        for ((p, c), fp) in &fps {
            assert_eq!(Some(*fp), gravity_dict::fp::fp128(&want(&vals[&(*p, *c)], prefix)), "{what}: part {p} code {c}");
        }
    }

    // 4. the same bytes as new parts: their own tables, the same derived values fingerprinted alike
    let fresh = parts_on(&bf, 42);
    let b = built();
    let (fps3, _) = statement(&reg, &fresh, REL_TEXT + 61, &spec);
    assert_eq!(built() - b, nparts, "a new part builds its own table");
    assert_eq!(fps3, fps1, "the same derived values, fingerprinted alike");
    assert_eq!(fp_tables_resident().2, resident, "no derived table resident across statements");
}

thread_local! {
    /// The interrupt check that raises the cancel: 0 never, else the n-th check from arming (a cancel mid-build).
    static CANCEL_AT: Cell<u32> = const { Cell::new(0) };
}

fn check() -> types_error::PgResult<()> {
    let n = CANCEL_AT.with(Cell::get);
    if n == 0 {
        return Ok(());
    }
    CANCEL_AT.with(|c| c.set(n - 1));
    if n == 1 {
        return Err(Box::new(PgError::error("canceling statement due to user request".to_string()).with_sqlstate(ERRCODE_QUERY_CANCELED)));
    }
    Ok(())
}

/// The backend state the fmgr road's `regexp_replace` reads outside a server: the interrupt seam (`check`), the regex
/// seams (`pg_set_regex_collation` and the Spencer engine's), the database default locale (C here: the default-collation
/// spec's call resolves it), the database encoding and the forced RE2 engine.
fn fmgr_regexp_setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        postgres_seams::check_for_interrupts::set(check);
        regex_core::init_seams();
    });
    pg_locale::arm_default_locale(Some(&pg_locale::C_LOCALE));
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
    regexp_alt::set_regex_engine(guc_tables::consts::REGEX_ENGINE_RE2);
}

fn a_faulted_entry_is_zero_with_no_fault_bitmap_and_a_cancel_mid_build_publishes_nothing() {
    fp_seed();
    fmgr_regexp_setup();
    let bf = text_fixture();
    let list = parts_on(&bf, 43);
    let nparts = list.len() as u64;
    let built = derived_tables_built;
    let reg = Arc::new(PartRegistry::new(256 << 20));

    // 1. a pattern PostgreSQL cannot compile: `(`, with no required literal, and `w9(` (D354), whose required literal `w9`
    //    no entry holds, so the prefilter kept every value unevaluated and raised nothing. The build compiles once through
    //    the rows' call first and faults every entry: the table fills, carries no fault bitmap, and each
    //    faulted entry is exactly [0, 0] (a row re-evaluates and raises PostgreSQL's 2201B).
    assert_eq!(gravity_dict::regexp::required_literal(b"("), None);
    assert_eq!(gravity_dict::regexp::required_literal(b"w9("), Some((0, 2)));
    for pat in [&b"("[..], &b"w9("[..]] {
        let bad = derived(pat, b"x", 950);
        let what = String::from_utf8_lossy(pat);
        assert!(!RegexpPlan::of(pat, b"x", 950, PG_UTF8).open_coded(), "{what}: the fmgr road");
        let raised = gravity_hosts::dict::text_hosts::reevaluate(&bad, b"w00").expect_err("PG raises for an evaluated row");
        assert_eq!(raised.sqlstate(), ERRCODE_INVALID_REGULAR_EXPRESSION);
        let b = built();
        let mut seen = 0u32;
        scan(&reg, &list, REL_TEXT + 62, &bad, &mut |_, p, _, _, t| {
            // SAFETY: the table lane's header, published by the source for this window.
            let h = unsafe { &*(t.ptr as *const gravity_dict::table::Header) };
            assert!(h.faults.is_null() && h.nfaults == 0 && h.flags & gravity_dict::table::FLAG_FAULTS == 0, "{what}: part {p}: no fault bitmap");
            let table = unsafe { gravity_dict::Fp128::from_lane(t) }.expect("fp128");
            assert!(!table.has_faults());
            for c in 0..table.ncodes() {
                assert_eq!(unsafe { table.get(c) }, [0, 0], "{what}: part {p} code {c}: a faulted entry is [0, 0]");
            }
            seen += 1;
        })
        .unwrap_or_else(|e| panic!("{what}: a compile error faults the entries, it never aborts the build: {e:?}"));
        assert!(seen > 0);
        assert_eq!(built() - b, nparts, "{what}: one table per part, published");
    }

    // 2. a cancel raised mid-build (the third interrupt check: the compile check's call takes the first, the entries' replace
    //    loops the rest, the automaton's as the call's) aborts the build with 57014 and publishes nothing: no table built, none
    //    resident, and the next statement builds every part's table. A cancel is no compile error (D354): it aborts. The
    //    pattern's class has a member past ASCII, so the pattern program refuses it (no entry holds that member: the
    //    answer is `([0-9]+)`'s).
    let (pat, rep) = ("([0-9\u{1c2}]+)".as_bytes(), &b"<\\1>"[..]);
    let good = derived(pat, rep, 950);
    assert!(!RegexpPlan::of(pat, rep, 950, PG_UTF8).open_coded(), "the fmgr road");
    let (b, resident, evaluated) = (built(), fp_tables_resident().2, gravity_hosts::dict::text_hosts::derived_entries());
    CANCEL_AT.with(|c| c.set(3));
    let e = scan(&reg, &list, REL_TEXT + 63, &good, &mut |_, p, _, _, _| panic!("part {p}: a window after the cancel")).expect_err("the cancel raises");
    assert_eq!(CANCEL_AT.with(Cell::get), 0, "the cancel was raised at the third check");
    assert_eq!(e.sqlstate(), ERRCODE_QUERY_CANCELED, "{e:?}");
    let d = gravity_hosts::dict::text_hosts::derived_entries() - evaluated;
    assert!((1..23).contains(&d), "mid-build: {d} of the first part's 23 entries evaluated before the cancel");
    assert_eq!(built(), b, "nothing built");
    assert_eq!(fp_tables_resident().2, resident, "nothing resident");
    let (n0, a0) = (gravity_hosts::dict::text_hosts::derived_entries(), gravity_hosts::dict::text_hosts::automaton_entries());
    let (fps, vals) = statement(&reg, &list, REL_TEXT + 63, &good);
    assert_eq!(built() - b, nparts, "the next statement builds every part's table: the cancelled build left none");
    for ((p, c), fp) in &fps {
        let v = &vals[&(*p, *c)];
        let digits = v.iter().position(u8::is_ascii_digit).expect("a digit");
        let want = [&v[..digits], &b"<"[..], &v[digits..], &b">"[..]].concat();
        assert_eq!(Some(*fp), gravity_dict::fp::fp128(&want), "part {p} code {c}");
    }

    // 3. B2-09: an unanchored pattern with a capture, outside the pattern program, the engine dispatches to its automaton:
    //    every evaluated entry above ran it with no call (each fingerprint PG's answer). G4: a back reference (no RE2 form)
    //    and another encoding keep the call.
    use gravity_hosts::dict::text_hosts::automaton;
    if regexp_alt::re2_available() {
        assert!(automaton(pat, rep, PG_UTF8).is_some(), "the engine's automaton");
        let (n, a) = (gravity_hosts::dict::text_hosts::derived_entries() - n0, gravity_hosts::dict::text_hosts::automaton_entries() - a0);
        assert!(n > 0 && a == n, "{a} of {n} evaluated entries by the automaton");
    }
    assert!(automaton(b"(o)\\1", rep, PG_UTF8).is_none(), "a back reference: the call");
    assert!(automaton(pat, rep, 8).is_none(), "LATIN1: the call");
}

/// The payload view: `DictView::Part`'s entries over a resident payload hand on the bulk cursor's entries
/// code for code, byte for byte, and count them where it does. Both opens of the part hold one payload slice: the
/// whole-fault one and the frame-lazy one (the production election; a frame-served arm has no slice and keeps the cursor).
#[test]
fn the_payload_view_hands_on_the_cursors_entries() {
    use gravity_hosts::dict::text_hosts::DictView;
    use pgrc_read::{DictFaultMode, DictHandle, MemPartIo, OpenPart};
    use std::sync::atomic::Ordering::Relaxed;
    let bf = url_fixture(3000, 12_000);
    let file = bf.files.get(&part_file_name(bf.manifest.parts[0].part_no)).expect("part file");
    let unwrappers = pgrc_am::scan::codec_binding().unwrappers;
    for (ino, mode) in [DictFaultMode::WholeFault, DictFaultMode::FrameLazy].into_iter().enumerate() {
        let part = Arc::new(OpenPart::open(Box::new(MemPartIo::new(file.clone(), 62, ino as u64)), &PartExpect::none()).expect("open"));
        let h = DictHandle::open_with_mode(part, None, unwrappers, 1, 0, mode).expect("handle");
        let sliced = h.payload_bytes().expect("payload").is_some();
        let mut cur = h.entries(0, h.ncodes()).expect("cursor");
        let mut want = Vec::new();
        while let Some((c, e)) = cur.next_entry().expect("entry") {
            want.push((c, e.bytes.to_vec()));
        }
        drop(cur);
        assert_eq!(want.len(), 3000, "{mode:?}: every value an entry");
        let (err, mut got, walked) = (Cell::new(None), Vec::new(), pgrc_read::dicthandle::DICT_CURSOR_ENTRIES.load(Relaxed));
        // SAFETY: each view lies in the handle's payload, live through the call.
        gravity_dict::Dict::entries(&DictView::Part(&h, &err), &mut |c, v| got.push((c, unsafe { v.bytes() }.to_vec())));
        assert!(err.take().is_none(), "{mode:?}: no read error");
        assert!(pgrc_read::dicthandle::DICT_CURSOR_ENTRIES.load(Relaxed) - walked >= 3000, "{mode:?}: counted");
        assert_eq!(got, want, "{mode:?}: the cursor's entries");
        assert!(sliced, "{mode:?}: one payload slice, the payload view's walk");
    }
}

/// One part of a dictionary-encoded text column (text_fixture's election): `ndv` distinct URL-like values over `rows`
/// rows, each with a two-byte character.
fn url_fixture(ndv: u64, rows: u64) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer, Fixture, OracleVal};
    use pgrc_write::dict::TextSemantics;
    use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    use pgrc_write::writer::PartCutPolicy;
    use pgrc_write::wvfs::WriteVfs;
    let v = |i: u64| format!("https://h{:05}.example/{}\u{e9}", i % ndv, "p".repeat((i % ndv % 29) as usize)).into_bytes();
    let oracle = vec![(0..rows).map(|i| Some(OracleVal::Bytes(v(i)))).collect()];
    let fx = Fixture { name: "gravity_urls", dir: "/qa/t7309".to_string(), spc: 1663, db: 5, relfilenumber: 7309, schema: vec![pgrc_qa::text_col(1)], oracle, plans: vec![], policy: PartCutPolicy { max_rows: rows, ..PartCutPolicy::default() } };
    let posture = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 100_000, exec_ok: true, sem: TextSemantics::BytesOnly }), ..ColumnPosture::default() };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, posture);
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    assert_eq!(manifest.parts.len(), 1, "one part");
    BuiltFixture { fx, files, manifest }
}
