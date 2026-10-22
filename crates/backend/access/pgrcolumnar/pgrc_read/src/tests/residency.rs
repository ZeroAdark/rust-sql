//! D259: the row scan's bulk-read posture. A `Residency::Private` cursor
//! decodes exactly what a `Shared` one does, admits nothing to the part's
//! residency plane (no segment / unwrapped image, no fault-log entry, no
//! `resident` bytes past the stream directory), keeps one values extent and one validity extent while
//! it walks forward, and still serves an image another reader made resident.

use pgrc_format::abi::{ByteArena, DecodeOut};

use crate::cursor::{reference_binding_leaked, Residency, StreamCursor};
use crate::openpart::OpenPart;
use crate::testpart::{build_part, seq_i64_col, text_col, BuiltPart, DictSpec, PartSpec};

use super::decode_props::assert_granule;
use super::{open_built, ArenaBuf};

const ROWS: u64 = 5 * 8192 + 1000;

/// The tests that poison passed extents (`evict_passed(true)`; a trim under `partcache::set_park_poison`) run one at a
/// time. `partcache::park_poisoned_bytes` is process-wide and cargo runs one binary's tests on parallel threads, so
/// another test's poisoning between one test's two reads of it added to its delta (PS2 witness: left 1263158, right
/// 1000886). Every test of this crate that poisons, or reads that counter, holds this for its whole body.
static POISON_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn poison_test() -> std::sync::MutexGuard<'static, ()> {
    POISON_TESTS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Six granules: an i64 column cut into five extents, a text column with NULLs
/// (a validity stream) in three, and a dictionary text column.
fn wide_part() -> BuiltPart {
    let mut words = seq_i64_col(1, ROWS);
    words.extent_cuts = vec![1, 2, 3, 4];
    let mut text = text_col(2, ROWS, Some(9));
    text.extent_cuts = vec![2, 4];
    let mut dict = text_col(3, ROWS, Some(5));
    dict.dict = Some(DictSpec { entries: (0..300).map(|i| format!("entry-{i:05}").into_bytes()).collect() });
    dict.dict_exec_flag = true;
    build_part(&PartSpec::new(ROWS, vec![words, text, dict]))
}

/// Decode every granule of `attno` through `cur`, checking each against the
/// builder's expectation; `each` sees the cursor and part after every granule.
fn walk(b: &BuiltPart, part: &OpenPart, cur: &mut StreamCursor<'_>, attno: u32, mut each: impl FnMut(&StreamCursor<'_>, &OpenPart)) {
    let exp = b.expected.iter().find(|e| e.attno == attno && e.path_ord == 0).expect("expected col");
    let mut base = 0usize;
    for g in 0..cur.granule_count() {
        let n = cur.values_in_granule(g).expect("values") as usize;
        let mut datums = vec![0u64; n.max(1)];
        let mut ab = ArenaBuf::new(4 << 20);
        let mut out = DecodeOut { datums: &mut datums, arena: ByteArena::new(ab.bytes_mut()) };
        let wrote = cur.decode_full(g, &mut out).expect("decode_full");
        assert_eq!(wrote as usize, n, "attno {attno} granule {g} row count");
        assert_granule(exp.class, out.datums, &exp.valid, &exp.canon, base, n);
        let mut vw = vec![0u64; n.div_ceil(64).max(1)];
        cur.validity(g, &mut vw).expect("validity");
        each(cur, part);
        base += n;
    }
}

#[test]
fn private_cursor_decodes_like_shared_and_admits_nothing() {
    let b = wide_part();
    let binding = reference_binding_leaked();
    for attno in 1..=3u32 {
        // The shared posture (Gravity / SQE): the plane keeps what was read.
        let shared = open_built(&b, 70, attno as u64);
        let mut cur = StreamCursor::open(shared.clone(), binding, attno, 0).expect("open shared");
        walk(&b, &shared, &mut cur, attno, |_, _| {});
        let dir_only = open_built(&b, 73, attno as u64);
        let _ = StreamCursor::open(dir_only.clone(), binding, attno, 0).expect("open");
        assert!(shared.resident() > dir_only.resident(), "attno {attno}: the shared cursor admits its extents");

        // The private posture (the row scan): same values, nothing admitted,
        // one extent per stream at a time.
        let private = open_built(&b, 71, attno as u64);
        let mut cur = StreamCursor::open(private.clone(), binding, attno, 0)
            .expect("open private")
            .with_residency(Residency::Private);
        // Opening parsed the part's stream directory (part metadata, shared by
        // both postures); the walk must add nothing past it.
        let opened = (private.resident(), private.faults().len());
        walk(&b, &private, &mut cur, attno, |c, p| {
            assert_eq!(
                (p.resident(), p.faults().len()),
                opened,
                "attno {attno}: a private cursor admits no extent or region to the plane"
            );
            let (v, val) = c.loaded_extents();
            assert!(v <= 1 && val <= 1, "attno {attno}: one extent per stream, held {v} values / {val} validity");
        });
    }
}

#[test]
fn private_cursor_serves_images_already_resident() {
    let b = wide_part();
    let binding = reference_binding_leaked();
    let part = open_built(&b, 72, 1);
    let mut warm = StreamCursor::open(part.clone(), binding, 2, 0).expect("open shared");
    walk(&b, &part, &mut warm, 2, |_, _| {});
    let (resident, faults) = (part.resident(), part.faults().len());
    let mut cur = StreamCursor::open(part.clone(), binding, 2, 0).expect("open private").with_residency(Residency::Private);
    walk(&b, &part, &mut cur, 2, |_, p| {
        assert_eq!((p.resident(), p.faults().len()), (resident, faults), "a resident image is served, never re-admitted");
    });
}

/// D4315: a shared cursor that passes the extents behind its granule holds one values extent and one validity extent at
/// a time; the passed images stay cached (unpinned) until a trim's pass over the part, which takes them though the part
/// is in use, and keeps the one the cursor reads; a read of a passed extent takes it back and un-passes it.
#[test]
fn passed_extents_stay_cached_and_go_only_to_the_passed_pass() {
    let _serial = poison_test();
    let b = wide_part();
    let binding = reference_binding_leaked();
    let part = open_built(&b, 74, 2);
    let mut cur = StreamCursor::open(part.clone(), binding, 2, 0).expect("open shared");
    walk(&b, &part, &mut cur, 2, |_, p| {
        assert!(p.passed_bytes() <= p.resident(), "passed images are cached ones");
    });
    // the walk above loaded the column's three extents; passing to the last granule leaves only the last one held
    let resident = part.resident();
    let last = cur.granule_count() - 1;
    let passed = cur.pass_behind(last);
    assert!(passed > 0, "the extents before the last were passed");
    assert_eq!(part.passed_bytes(), passed);
    let (v, val) = cur.loaded_extents();
    assert!(v <= 1 && val <= 1, "one extent per stream held after the pass, held {v} values / {val} validity");
    assert_eq!(part.resident(), resident, "passing evicts nothing");
    assert_eq!(part.pinned_bytes(), part.cache_bytes() - passed);
    assert!(part.users() > 0, "the cursor's use keeps the part in use");

    // the passed pass takes the passed images, poisoned under the check, and leaves the held one
    let poisoned = crate::partcache::park_poisoned_bytes();
    let freed = part.evict_passed(true);
    assert_eq!(freed, passed, "every passed image went (no clone shared one)");
    assert_eq!(crate::partcache::park_poisoned_bytes() - poisoned, passed, "and was poisoned first");
    assert_eq!(part.passed_bytes(), 0);
    assert_eq!(part.resident(), resident - passed);

    // reading a passed extent again faults it back, held and not passed
    let mut again = StreamCursor::open(part.clone(), binding, 2, 0).expect("open shared");
    walk(&b, &part, &mut again, 2, |_, _| {});
    assert_eq!(part.resident(), resident, "the evicted extents faulted back");
    assert_eq!(part.passed_bytes(), 0, "a re-read extent is not passed");
}

/// D4315: a cursor set to pass behind it holds one values extent and one validity extent as it walks forward (a scan's
/// windows, a whole-column table walk): each load passes the extents before it, which stay cached.
#[test]
fn a_cursor_passing_behind_holds_one_extent_per_stream() {
    let b = wide_part();
    let binding = reference_binding_leaked();
    let part = open_built(&b, 75, 2);
    let mut cur = StreamCursor::open(part.clone(), binding, 2, 0).expect("open shared");
    cur.set_pass_behind(true);
    walk(&b, &part, &mut cur, 2, |c, p| {
        let (v, val) = c.loaded_extents();
        assert!(v <= 1 && val <= 1, "one extent per stream held, held {v} values / {val} validity");
        assert!(p.passed_bytes() <= p.resident(), "passed images stay cached");
    });
    assert!(part.passed_bytes() > 0, "the walk passed the extents before its last");
    assert_eq!(part.pinned_bytes(), part.cache_bytes() - part.passed_bytes());
    // the scan's read-ahead starts past what the cursor read: a whole walk read to the column's end
    assert_eq!(cur.read_hi(), cur.granule_count(), "the walk's furthest loaded extent ends at the column's end");
    cur.set_pass_behind(true);
    assert_eq!(cur.read_hi(), 0, "a rebind starts its reads afresh");
}

/// D4351: decode granule `g` of `attno` through `cur` (a reader's single step; `walk` checks the values).
fn read_granule(cur: &mut StreamCursor<'_>, g: u32) {
    let n = cur.values_in_granule(g).expect("values") as usize;
    let mut datums = vec![0u64; n.max(1)];
    let mut ab = ArenaBuf::new(4 << 20);
    let mut out = DecodeOut { datums: &mut datums, arena: ByteArena::new(ab.bytes_mut()) };
    cur.decode_full(g, &mut out).expect("decode_full");
    let mut vw = vec![0u64; n.div_ceil(64).max(1)];
    cur.validity(g, &mut vw).expect("validity");
}

/// D4351: two sessions read one part. The reader ahead passes the extents it read; the trim's pass over the part in use
/// (its first tier) takes only what no live reader of the stream has yet to reach, so the reader behind faults nothing
/// again, while a column only the reader ahead reads gives its passed extents back.
#[test]
fn d4351_the_reader_behind_keeps_what_the_reader_ahead_passed() {
    let _serial = poison_test();
    let b = wide_part();
    let binding = reference_binding_leaked();
    let part = open_built(&b, 76, 2);
    let mut behind = StreamCursor::open(part.clone(), binding, 2, 0).expect("open behind");
    behind.set_pass_behind(true);
    read_granule(&mut behind, 0);
    let mut ahead = StreamCursor::open(part.clone(), binding, 2, 0).expect("open ahead");
    ahead.set_pass_behind(true);
    walk(&b, &part, &mut ahead, 2, |_, _| {});
    let passed_col2 = part.passed_bytes();
    assert!(passed_col2 > 0, "the reader ahead passed the extents before its last");
    // a column only the reader ahead reads: everything it passed is behind every live reader of that stream
    let mut only = StreamCursor::open(part.clone(), binding, 1, 0).expect("open only");
    only.set_pass_behind(true);
    walk(&b, &part, &mut only, 1, |_, _| {});
    let passed_col1 = part.passed_bytes() - passed_col2;
    assert!(passed_col1 > 0, "the single reader passed its column's extents before its last");

    let freed = part.evict_passed(true);
    assert!(freed >= passed_col1, "the first tier frees what no live reader is behind: freed {freed}, col 1 passed {passed_col1}");
    let before = part.resident();
    walk(&b, &part, &mut behind, 2, |_, _| {});
    let refaulted = part.resident().saturating_sub(before);
    assert_eq!(refaulted, 0, "the reader behind faulted {refaulted} bytes the reader ahead passed again");
}

/// D4351: give-back is never blocked: a content trim to a target the first tier cannot reach takes the passed extents a
/// live reader has yet to reach too (that reader faults them again, CRC-checked), and a dropped cursor leaves the part's
/// live readers.
#[test]
fn d4351_the_second_tier_still_gives_back_under_pressure() {
    let _serial = poison_test();
    let b = wide_part();
    let binding = reference_binding_leaked();
    let reg = crate::registry::PartRegistry::new(u64::MAX);
    let io = b.mem_io(77, 2);
    let pin = reg
        .open_pinned(&crate::openpart::PartExpect::none(), move || Ok(Box::new(io) as Box<dyn crate::io::PartIo>))
        .expect("open pinned");
    let part = pin.part().clone();
    let mut behind = StreamCursor::open(part.clone(), binding, 2, 0).expect("open behind");
    behind.set_pass_behind(true);
    read_granule(&mut behind, 0);
    let mut ahead = StreamCursor::open(part.clone(), binding, 2, 0).expect("open ahead");
    ahead.set_pass_behind(true);
    walk(&b, &part, &mut ahead, 2, |_, _| {});
    assert_eq!(part.live_readers(), 2, "both cursors read as live readers");
    let passed = part.passed_bytes();
    assert!(passed > 0, "the reader ahead passed extents");
    assert_eq!(part.evict_passed(true), 0, "the first tier keeps what the reader behind has yet to reach");
    let resident = part.resident();
    reg.trim_contents(0);
    assert!(part.passed_bytes() < passed, "the second tier took passed extents the reader behind has yet to reach");
    let after = part.resident();
    assert!(after < resident, "the trim gave bytes back");
    walk(&b, &part, &mut behind, 2, |_, _| {});
    assert!(part.resident() > after, "the reader behind faulted the evicted extents again");
    drop(ahead);
    drop(behind);
    assert_eq!(part.live_readers(), 0, "dropped cursors leave the live readers");
}
