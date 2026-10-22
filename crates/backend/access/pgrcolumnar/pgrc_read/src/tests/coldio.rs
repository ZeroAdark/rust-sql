//! Cold-I/O faces (docs/gravity/partitioned-record.md §8): the open path's
//! whole-file access hint fires exactly once per open, and a claim's run read
//! coalesces a stream's extents into one pread (holes up to the tolerance read
//! and discarded), leaving every later decode a cache hit. `inspect_part`
//! (ignored) prints a real part's stream directory for the format census.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pgrc_format::abi::{ByteArena, DecodeOut};
use pgrc_format::part::StreamRole;

use crate::cursor::{reference_binding_leaked, StreamCursor};
use crate::io::{MemPartIo, PartIo};
use crate::openpart::{OpenPart, PartExpect};
use crate::testpart::{build_part, seq_i64_col, PartSpec};
use crate::ReadResult;

use super::ArenaBuf;

/// A part image that counts its preads and access hints.
struct CountIo {
    inner: MemPartIo,
    preads: Arc<AtomicU64>,
    advised: Arc<AtomicU64>,
}

impl PartIo for CountIo {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn dev_ino(&self) -> (u64, u64) {
        self.inner.dev_ino()
    }
    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
        self.preads.fetch_add(1, Ordering::Relaxed);
        self.inner.pread_exact(off, buf, at)
    }
    fn advise_random(&self) {
        self.advised.fetch_add(1, Ordering::Relaxed);
    }
}

/// `cuts`: the granule ordinals where a new values extent starts (the production writer cuts
/// one extent per band of 8 granules; the test builder cuts where told).
fn counted_part(rows: u64, cuts: &[u32]) -> (Arc<OpenPart>, Arc<AtomicU64>, Arc<AtomicU64>) {
    let mut col = seq_i64_col(1, rows);
    col.extent_cuts = cuts.to_vec();
    let b = build_part(&PartSpec::new(rows, vec![col]));
    let preads = Arc::new(AtomicU64::new(0));
    let advised = Arc::new(AtomicU64::new(0));
    let io = CountIo { inner: b.mem_io(1, 77), preads: Arc::clone(&preads), advised: Arc::clone(&advised) };
    let part = Arc::new(OpenPart::open(Box::new(io), &PartExpect::none()).expect("open"));
    (part, preads, advised)
}

#[test]
fn open_advises_random_access_once_before_any_read() {
    let (_part, preads, advised) = counted_part(9_000, &[]);
    assert_eq!(advised.load(Ordering::Relaxed), 1, "one whole-file hint per open");
    assert_eq!(preads.load(Ordering::Relaxed), 4, "the open path's four validated reads");
}

#[test]
fn claim_run_read_is_one_pread_per_stream_and_decodes_hit() {
    // 200_000 rows = 25 granules = 4 bands = 4 values extents.
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    assert_eq!(ps.extents.len(), 4, "one extent per band");
    let cur = StreamCursor::open(Arc::clone(&part), reference_binding_leaked(), 1, 0).expect("cursor");
    let before = preads.load(Ordering::Relaxed);
    let r = cur.prefetch_granules(0, cur.granule_count(), 8 << 20, 256 << 10);
    assert_eq!((r[0], r[2], r[4]), (4, 0, 1), "4 extents inserted by ONE run read");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1);
    // Every decode of the claim is now a cache hit: no further reads.
    let mut cur = cur;
    let mut datums = vec![0u64; 8192];
    let mut ab = ArenaBuf::new(64);
    for g in 0..cur.granule_count() {
        let rows = cur.rows_in_granule(g);
        let n = cur.decode_full(g, &mut DecodeOut { datums: &mut datums[..rows as usize], arena: ByteArena::new(ab.bytes_mut()) }).expect("decode");
        assert_eq!(n, rows);
    }
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1, "decodes after the run read fault nothing");
    // A second run over resident granules issues nothing.
    let r2 = cur.prefetch_granules(0, cur.granule_count(), 8 << 20, 256 << 10);
    assert_eq!((r2[0], r2[2], r2[4]), (0, 4, 0));
}

#[test]
fn hole_tolerance_bridges_a_skipped_extent_and_zero_does_not() {
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    let e = &ps.extents;
    // Extents 0 and 2 with extent 1 as the hole between them.
    let recs = [(0u32, e[0]), (2u32, e[2])];
    let hole = e[2].file_off - (e[0].file_off + e[0].len);
    assert!(hole >= e[1].len, "extent 1 lies in the gap");
    let before = preads.load(Ordering::Relaxed);
    let r = part.prefetch_extent_run_holes(&ps.entry, &recs, 8 << 20, hole, None, None);
    assert_eq!((r.0, r.4), (2, 1), "one pread bridges the hole");
    assert_eq!(r.3, e[2].file_off + e[2].len - e[0].file_off, "the hole's bytes are read and discarded");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1);
    // Extents 1 and 3 with hole 0: two reads.
    let recs = [(1u32, e[1]), (3u32, e[3])];
    let r = part.prefetch_extent_run_holes(&ps.entry, &recs, 8 << 20, 0, None, None);
    assert_eq!((r.0, r.4), (2, 2), "hole 0 = strictly contiguous runs");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 3);
}

#[test]
fn run_read_covers_only_the_claimed_granules() {
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let cur = StreamCursor::open(Arc::clone(&part), reference_binding_leaked(), 1, 0).expect("cursor");
    let before = preads.load(Ordering::Relaxed);
    // Granules 9..17 lie in bands 1 and 2 (granules 8..24).
    let r = cur.prefetch_granules(9, 17, 8 << 20, 256 << 10);
    assert_eq!((r[0], r[4]), (2, 1));
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1);
    // A run cap below two extents splits the read.
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    let r = cur.prefetch_granules(0, 8, ps.extents[0].len, 0);
    assert_eq!((r[0], r[4]), (1, 1));
}

#[test]
fn admitted_runs_in_neighbouring_bands_share_one_run_read() {
    // 200_000 rows = 25 granules = 4 bands (0..8, 8..16, 16..24, 24..25), one values extent each.
    let cursor = |part: &Arc<OpenPart>| StreamCursor::open(Arc::clone(part), reference_binding_leaked(), 1, 0).expect("cursor");
    // A run per call: runs in bands 2 and 3 are a pread each (a cold equality scan's split at the padding).
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let cur = cursor(&part);
    let before = preads.load(Ordering::Relaxed);
    for (lo, hi) in [(17u32, 18u32), (24, 25)] {
        cur.prefetch_granules(lo, hi, 8 << 20, 0);
    }
    assert_eq!(preads.load(Ordering::Relaxed) - before, 2);
    // The same runs in one call (padding-sized holes bridged): one pread; a run inside a covered band adds nothing.
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let cur = cursor(&part);
    let before = preads.load(Ordering::Relaxed);
    let r = cur.prefetch_granule_runs(&[(17, 18), (20, 22), (24, 25)], 8 << 20, 64);
    assert_eq!((r[0], r[2], r[4]), (2, 0, 1), "bands 2 and 3 in one pread, each once");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1);
    // A band no run covers is a hole: bridged under max_hole, a split at hole 0.
    let dir = part.stream_directory().expect("dir");
    let e = &dir.lookup(1, 0, StreamRole::Values).expect("values stream").extents;
    let hole = e[2].file_off - (e[0].file_off + e[0].len);
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let cur = cursor(&part);
    let before = preads.load(Ordering::Relaxed);
    let r = cur.prefetch_granule_runs(&[(1, 2), (16, 17)], 8 << 20, hole);
    assert_eq!((r[0], r[4]), (2, 1), "band 1 read and discarded between the runs");
    assert_eq!(r[3], e[2].file_off + e[2].len - e[0].file_off);
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1);
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let cur = cursor(&part);
    let before = preads.load(Ordering::Relaxed);
    let r = cur.prefetch_granule_runs(&[(1, 2), (16, 17)], 8 << 20, 0);
    assert_eq!((r[0], r[4]), (2, 2), "hole 0: the uncovered band splits the read");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 2);
}

#[test]
fn part_run_read_on_a_file_is_one_pread_the_depth_census_sees() {
    // 200_000 rows = 25 granules = 4 bands: a whole part's lane run is ONE pread on the production fd.
    let mut col = seq_i64_col(1, 200_000);
    col.extent_cuts = vec![8, 16, 24];
    let b = build_part(&PartSpec::new(200_000, vec![col]));
    let path = std::env::temp_dir().join(format!("pgrc-coldio-{}.part", std::process::id()));
    std::fs::write(&path, &b.bytes).expect("write");
    let cpath = std::ffi::CString::new(path.to_str().expect("utf8")).expect("path");
    let io = crate::io::VfsPartIo::open(&cpath).expect("open");
    let part = Arc::new(OpenPart::open(Box::new(io), &PartExpect::none()).expect("part"));
    let cur = StreamCursor::open(Arc::clone(&part), reference_binding_leaked(), 1, 0).expect("cursor");
    // D769: this thread's census is exact; the process-global one also counts the tests running beside this one (a
    // concurrent part open is 5 preads), so it is held to "sees at least this run".
    let _ = crate::io::take_read_depth_max();
    let _ = crate::io::take_thread_read_depth_max();
    let (calls, bytes) = crate::io::pread_census();
    let (tcalls, tbytes) = crate::io::thread_pread_census();
    let r = cur.prefetch_granules(0, cur.granule_count(), 2 << 20, 256 << 10);
    let (calls2, bytes2) = crate::io::pread_census();
    let (tcalls2, tbytes2) = crate::io::thread_pread_census();
    assert_eq!((r[0], r[4]), (4, 1), "the part's four extents by one run read");
    assert_eq!(tcalls2 - tcalls, 1, "one pread on the fd");
    assert_eq!(tbytes2 - tbytes, r[3], "the census counts the run's bytes");
    assert!(calls2 - calls >= 1 && bytes2 - bytes >= r[3], "the global census sees the run");
    assert!(r[3] >= r[1] && r[3] - r[1] < 256 << 10, "the holes stay under the tolerance");
    assert!(crate::io::take_read_depth_max() >= 1, "the run was in flight");
    assert_eq!(crate::io::take_thread_read_depth_max(), 1, "this thread's run was in flight, one pread deep");
    assert_eq!(crate::io::take_thread_read_depth_max(), 0, "taken: nothing in flight now");
    drop(cur);
    drop(part);
    let _ = std::fs::remove_file(&path);
}

/// M3's walk residency: a claim's granules name their stream extents (only the ones covering it), and the part file's
/// page cache answers for them (cachestat through vfs; unknown on an in-memory image or a kernel without it).
#[test]
fn a_claims_extents_and_their_page_cache_residency() {
    let mut col = seq_i64_col(1, 200_000);
    col.extent_cuts = vec![8, 16, 24];
    let b = build_part(&PartSpec::new(200_000, vec![col]));
    let path = std::env::temp_dir().join(format!("pgrc-resident-{}.part", std::process::id()));
    std::fs::write(&path, &b.bytes).expect("write");
    let cpath = std::ffi::CString::new(path.to_str().expect("utf8")).expect("path");
    let part = Arc::new(OpenPart::open(Box::new(crate::io::VfsPartIo::open(&cpath).expect("open")), &PartExpect::none()).expect("part"));
    let cur = StreamCursor::open(Arc::clone(&part), reference_binding_leaked(), 1, 0).expect("cursor");
    let extents = |lo, hi| {
        let mut v = Vec::new();
        cur.granule_extents(lo, hi, &mut |off, len| v.push((off, len)));
        v
    };
    let all = extents(0, cur.granule_count());
    assert_eq!(all.len(), 4, "four values extents, no validity stream");
    assert_eq!(extents(9, 17), all[1..3].to_vec(), "granules 9..17 lie in the second and third extents");
    assert!(extents(3, 3).is_empty());
    match cur.page_resident(all[0].0, all[0].1) {
        // just written through the page cache: every page of the extent is there
        Some((cached, pages)) => assert!(pages >= 1 && cached == pages, "{cached} of {pages}"),
        None => assert!(cfg!(not(target_os = "linux")) || vfs::get_errno() == libc::ENOSYS),
    }
    let (mem, _, _) = counted_part(9_000, &[]);
    assert_eq!(mem.page_resident(0, 4096), None, "an in-memory image: unknown");
    drop(cur);
    drop(part);
    let _ = std::fs::remove_file(&path);
}

/// `PGRC_INSPECT_PART=<path> cargo test -p pgrc_read inspect_part -- --ignored --nocapture`:
/// every stream of a real part — attno, role, encoding, class, width, wrapper, extents, bytes.
#[test]
#[ignore]
fn inspect_part() {
    let Ok(path) = std::env::var("PGRC_INSPECT_PART") else { return };
    let cpath = std::ffi::CString::new(path).expect("path");
    let io = crate::io::VfsPartIo::open(&cpath).expect("open");
    let part = OpenPart::open(Box::new(io), &PartExpect::none()).expect("part");
    let dir = part.stream_directory().expect("dir");
    println!("rows={} granules={} grain={} sections={}", part.rows(), part.footer().granule_count, part.grain().rows(), part.sections().len());
    for ps in dir.streams() {
        let bytes: u64 = ps.extents.iter().map(|r| r.len).sum();
        let gaps: u64 = ps.extents.windows(2).map(|w| w[1].file_off - (w[0].file_off + w[0].len)).sum();
        println!(
            "attno={:3} role={:?} enc={} class={} width={} wrapper={} values={} extents={} bytes={} gaps={} B/value={:.2}",
            ps.entry.attno, ps.role, ps.entry.encoding, ps.entry.class, ps.entry.width, ps.entry.wrapper, ps.entry.values, ps.extents.len(), bytes, gaps,
            if ps.entry.values > 0 { bytes as f64 / ps.entry.values as f64 } else { 0.0 }
        );
    }
}

/// The boundary of the run's dictionary carry: the test part stores a dictionary beside VERBATIM
/// values (no `DictCodes` stream), so the cursor attaches no dictionary streams and the run brings
/// the values only — the dictionary stays `DictHandle`'s to fault. (The carry itself is proven on a
/// real `DictCodes` election in `gravity_hosts`' `granule_source` tests.)
#[test]
fn a_run_over_verbatim_values_leaves_the_dictionary_to_the_handle() {
    use crate::dicthandle::DictHandle;
    let b = super::dict::dict_part(50, false);
    let preads = Arc::new(AtomicU64::new(0));
    let advised = Arc::new(AtomicU64::new(0));
    let io = CountIo { inner: b.mem_io(1, 78), preads: Arc::clone(&preads), advised: Arc::clone(&advised) };
    let part = Arc::new(OpenPart::open(Box::new(io), &PartExpect::none()).expect("open"));
    let dir = part.stream_directory().expect("dir");
    let n_values = dir.lookup(1, 0, StreamRole::Values).expect("values").extents.len() as u64;
    assert!(dir.lookup(1, 0, StreamRole::DictPayload).is_some(), "the fixture carries a dictionary");
    let binding = reference_binding_leaked();
    let cur = StreamCursor::open(Arc::clone(&part), binding, 1, 0).expect("cursor");
    let before = preads.load(Ordering::Relaxed);
    let r = cur.prefetch_granules(0, cur.granule_count(), 8 << 20, 256 << 10);
    assert_eq!((r[0], preads.load(Ordering::Relaxed) - before), (n_values, 1), "the values run only");
    let roles: Vec<u8> = part.faults().iter().filter_map(|f| match f.tag { crate::openpart::FaultTag::StreamExtent { role, .. } => Some(role), _ => None }).collect();
    assert!(!roles.contains(&(StreamRole::DictPayload as u8)), "{roles:?}");
    let after_run = preads.load(Ordering::Relaxed);
    let h = DictHandle::open(Arc::clone(&part), None, &[], 1, 0).expect("handle");
    assert!(h.ncodes() > 0 && h.byte_len_only(0).expect("entry 0") > 0);
    assert!(preads.load(Ordering::Relaxed) > after_run, "the handle faulted the dictionary itself");
}

/// M2-ZONEREAD card C: extents a run stages raw (`ROLE_STAGE_RAW`) come in that run's one pread; the unwrap that
/// follows takes each without a read of its own, exactly once (a second unwrap of the image is a cache hit, and an
/// image evicted and asked again is read again, not served stale from the stage); an evicted stage reads as before.
#[test]
fn staged_raw_extents_are_taken_by_their_unwrap_without_a_read() {
    use crate::openpart::ROLE_STAGE_RAW;
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    let recs: Vec<_> = ps.extents.iter().enumerate().map(|(i, r)| (i as u32, *r, ps.role as u8 | ROLE_STAGE_RAW)).collect();
    let before = preads.load(Ordering::Relaxed);
    let r = part.prefetch_extent_run_holes_roles(&ps.entry, &recs[..3], 8 << 20, 256 << 10, None, None);
    assert_eq!((r.0, r.4), (3, 1), "three extents staged by one pread");
    assert_eq!(part.staged_extents(), 3);
    let resident = part.resident();
    // a staged run again reads nothing: the stage holds them
    assert_eq!(part.prefetch_extent_run_holes_roles(&ps.entry, &recs[..3], 8 << 20, 256 << 10, None, None).2, 3);
    let mut same = |raw: &[u8]| -> ReadResult<Vec<u8>> { Ok(raw.to_vec()) };
    for (i, rec, _) in &recs[..3] {
        let img = part.unwrapped_extent_bytes(&ps.entry, rec, *i, &mut same).expect("unwrap");
        assert_eq!(img.len() as u64, rec.len);
    }
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1, "the unwraps took the staged bytes");
    assert_eq!(part.staged_extents(), 0, "each staged extent is taken once");
    assert_eq!(part.resident(), resident, "the stage's bytes became the images' (identity rebuild)");
    // the fourth extent was never staged: its unwrap reads it
    part.unwrapped_extent_bytes(&ps.entry, &recs[3].1, 3, &mut same).expect("unwrap");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 2);
    // an evicted stage costs its read back, nothing stale
    let (part, preads, _) = counted_part(200_000, &[8, 16, 24]);
    part.prefetch_extent_run_holes_roles(&ps.entry, &recs[..1], 8 << 20, 0, None, None);
    let before = preads.load(Ordering::Relaxed);
    assert_eq!(part.evict_extent_images(&[(recs[0].1.file_off, recs[0].1.len)]), (1, recs[0].1.len));
    assert_eq!((part.staged_extents(), part.resident()), (0, 0));
    part.unwrapped_extent_bytes(&ps.entry, &recs[0].1, 0, &mut same).expect("unwrap");
    assert_eq!(preads.load(Ordering::Relaxed) - before, 1);
}

/// A part image that logs its WILLNEED advice (PERF-COLD read.advise).
struct AdviseIo {
    inner: MemPartIo,
    log: Arc<pgsync::Mutex<Vec<(u64, u64)>>>,
}

impl PartIo for AdviseIo {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn dev_ino(&self) -> (u64, u64) {
        self.inner.dev_ino()
    }
    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
        self.inner.pread_exact(off, buf, at)
    }
    fn advise_willneed(&self, off: u64, len: u64) {
        self.log.lock().unwrap_or_else(|p| p.into_inner()).push((off, len));
    }
}

#[test]
fn claim_advice_covers_the_runs_in_device_pieces_and_skips_resident_extents() {
    // 200_000 rows = 25 granules = 4 bands = 4 values extents, a few bytes apart
    let mut col = seq_i64_col(1, 200_000);
    col.extent_cuts = vec![8, 16, 24];
    let b = build_part(&PartSpec::new(200_000, vec![col]));
    let log = Arc::new(pgsync::Mutex::new(Vec::new()));
    let io = AdviseIo { inner: b.mem_io(1, 78), log: Arc::clone(&log) };
    let part = Arc::new(OpenPart::open(Box::new(io), &PartExpect::none()).expect("open"));
    let dir = part.stream_directory().expect("dir");
    let e = dir.lookup(1, 0, StreamRole::Values).expect("values stream").extents.clone();
    assert_eq!(e.len(), 4);
    let cur = StreamCursor::open(Arc::clone(&part), reference_binding_leaked(), 1, 0).expect("cursor");
    let taken = |log: &pgsync::Mutex<Vec<(u64, u64)>>| core::mem::take(&mut *log.lock().unwrap_or_else(|p| p.into_inner()));
    taken(&log);
    let piece = 64 << 10;
    let (lo, hi) = (e[0].file_off, e[3].file_off + e[3].len);
    // the whole claim: the four extents coalesce across their gaps into one range, advised in piece-sized calls that
    // tile it exactly (one call over the range would advise only its first piece on Linux)
    let (calls, bytes) = cur.advise_granule_runs(&[(0, cur.granule_count())], 256 << 10, piece, u64::MAX);
    let got = taken(&log);
    assert_eq!(bytes, hi - lo);
    assert_eq!(calls, (hi - lo).div_ceil(piece));
    assert_eq!(got.len() as u64, calls);
    assert!(got.iter().all(|&(_, n)| n > 0 && n <= piece), "no call exceeds the piece");
    assert_eq!(got.first().map(|g| g.0), Some(lo));
    assert!(got.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0), "the calls tile the range");
    assert_eq!(got.last().map(|g| g.0 + g.1), Some(hi));
    // hole 0: the extents' gaps split the range into four
    let (_, bytes0) = cur.advise_granule_runs(&[(0, cur.granule_count())], 0, piece, u64::MAX);
    taken(&log);
    assert_eq!(bytes0, e.iter().map(|r| r.len).sum::<u64>(), "only the extents' own bytes");
    // a claim over granules 9..17 advises bands 1 and 2 only
    let (_, b12) = cur.advise_granule_runs(&[(9, 17)], 256 << 10, piece, u64::MAX);
    let got = taken(&log);
    assert_eq!((got[0].0, b12), (e[1].file_off, e[2].file_off + e[2].len - e[1].file_off));
    // the limit (the caller's in-flight budget) clips the advice: whole pieces, then the rest of the budget, never past it
    let (c3, b3) = cur.advise_granule_runs(&[(0, cur.granule_count())], 256 << 10, piece, 3 * piece + 100);
    let got = taken(&log);
    assert_eq!((c3, b3), (4, 3 * piece + 100));
    assert_eq!(got.iter().map(|g| g.1).sum::<u64>(), 3 * piece + 100);
    assert_eq!(cur.advise_granule_runs(&[(0, cur.granule_count())], 256 << 10, piece, 0), (0, 0), "no budget, no call");
    assert!(taken(&log).is_empty());
    // resident extents are never advised: after band 0's run read, the claim starts at band 1; all resident = no call
    cur.prefetch_granules(0, 8, 8 << 20, 256 << 10);
    let (_, rest) = cur.advise_granule_runs(&[(0, cur.granule_count())], 256 << 10, piece, u64::MAX);
    assert_eq!((taken(&log)[0].0, rest), (e[1].file_off, hi - e[1].file_off));
    cur.prefetch_granules(0, cur.granule_count(), 8 << 20, 256 << 10);
    assert_eq!(cur.advise_granule_runs(&[(0, cur.granule_count())], 256 << 10, piece, u64::MAX), (0, 0));
    assert!(taken(&log).is_empty(), "a hot claim makes no advice call");
}

/// D4071 / D4183: a wrapped extent's rebuilt image becomes the resident image itself (the build's zero fill and two
/// copies after the rebuild's last poll are gone), and a copy that must still be made stops at its statement's cancel
/// within one piece; outside a kernel call it never stops.
#[test]
fn a_rebuilt_image_is_adopted_and_a_canceled_copy_stops_within_a_piece() {
    use crate::openpart::SegBuf;
    let n = 4 * cancelpoll::PIECE + 5;
    let v: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    let (base, want) = (v.as_ptr(), v.clone());
    let img = SegBuf::adopt(v).expect("adopt");
    assert_eq!(img.bytes(), &want[..]);
    assert_eq!(img.bytes().as_ptr() == base, base as usize % 8 == 0, "an aligned rebuild is the image itself");
    assert_eq!(img.bytes().as_ptr() as usize % 8, 0, "8-aligned either way");
    assert_eq!(SegBuf::try_from_bytes(&want).expect("no call: no stop").bytes(), &want[..]);
    let byte = std::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
    cancelpoll::scoped(&byte, || {
        assert!(matches!(SegBuf::try_from_bytes(&want), Err(crate::ReadError::Canceled)), "the cancel ends the copy");
    });
}

/// D4321 step 1 (LADDER-DESIGN section 6): a wrapped extent's rebuild, whose buffer becomes the resident image
/// (`SegBuf::adopt`), runs inside a cache scope, so the global allocator puts the image on the cache arena; the scope ends
/// with the rebuild. (The built road, `SegBuf::build`: tests/d4321_cache_scope.rs.)
#[test]
fn a_rebuild_runs_inside_a_cache_scope() {
    // as the server is once allocarena installed the heap: a scope's test reads true inside one
    memledger::arena::set_cache_heap_ready();
    let (part, _, _) = counted_part(200_000, &[8, 16, 24]);
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    let rec = ps.extents[1];
    let mut inside = None;
    let mut same = |raw: &[u8]| -> ReadResult<Vec<u8>> {
        inside = Some(memledger::arena::in_cache_scope());
        Ok(raw.to_vec())
    };
    let img = part.unwrapped_extent_bytes(&ps.entry, &rec, 1, &mut same).expect("unwrap");
    assert_eq!(img.len() as u64, rec.len);
    assert_eq!(inside, Some(true), "the rebuild runs inside a cache scope");
    assert!(!memledger::arena::in_cache_scope(), "the scope ends with the rebuild");
}

/// Job 3 (cvformat): a wrapped extent's raw bytes are the reader's while they live: a read's from its allocation (its
/// pread under way), a staged one's from the cache's debit, each while the image is rebuilt, and gone with the raw. An
/// image the cache does not keep stays the reader's until it drops; a sub-range read (the pub face: the thread's
/// context) and its slice of a cached extent are the reading statement's.
#[test]
fn a_raw_read_is_the_readers_from_its_allocation_to_its_drop() {
    use crate::openpart::ROLE_STAGE_RAW;
    use memledger::charge::{self, Owner};
    memledger::bind_backend(215, 9215);
    memledger::statement_begin();
    let (stmt, _) = charge::context();
    let owner = Some((stmt, memledger::Kind::Region));
    let row = |o: Owner| charge::owner_row(o).committed;
    let (part, _, _) = counted_part(200_000, &[8, 16, 24]);
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    let recs: Vec<_> = ps.extents.iter().enumerate().map(|(i, r)| (i as u32, *r, ps.role as u8 | ROLE_STAGE_RAW)).collect();
    assert!(recs.len() >= 4, "four extents");
    let s0 = row(stmt);
    let during = std::cell::Cell::new(0i64);
    let mut same = |raw: &[u8]| -> ReadResult<Vec<u8>> {
        during.set(row(stmt) - s0);
        Ok(raw.to_vec())
    };
    // a read: the raw image the rebuild sees is the reader's; the kept image is the cache's
    let rec = recs[0].1;
    let img = part.unwrapped_extent_bytes_for(owner, &ps.entry, &rec, 0, &mut same).expect("unwrap");
    assert!(during.get() >= rec.len as i64, "the raw read's {} bytes charged while rebuilt: {}", rec.len, during.get());
    assert_eq!(row(stmt) - s0, 0, "the raw's charge leaves with it");
    drop(img);
    // a staged raw: the cache's until the unwrap takes it, the reader's from the debit
    let rec = recs[1].1;
    assert_eq!(part.prefetch_extent_run_holes_roles(&ps.entry, &recs[1..2], 8 << 20, 0, None, None).0, 1, "staged");
    assert_eq!(row(stmt) - s0, 0, "a staged raw is the cache's");
    during.set(0);
    let img = part.unwrapped_extent_bytes_for(owner, &ps.entry, &rec, 1, &mut same).expect("unwrap");
    assert_eq!(part.staged_extents(), 0, "the unwrap took the stage");
    assert!(during.get() >= rec.len as i64, "the staged raw's {} bytes charged while rebuilt: {}", rec.len, during.get());
    assert_eq!(row(stmt) - s0, 0);
    drop(img);
    // a read no owner names (a shared dict handle's cursor): the raw is the reading thread's context's all the same
    let rec = recs[0].1;
    assert_eq!(part.evict_extent_images(&[(rec.file_off, rec.len)]).0, 1, "extent 0's image evicted");
    during.set(0);
    let img = part.unwrapped_extent_bytes_for(None, &ps.entry, &rec, 0, &mut same).expect("unwrap");
    assert!(during.get() >= rec.len as i64, "the unnamed read's raw {} bytes on the context: {}", rec.len, during.get());
    assert_eq!(row(stmt) - s0, 0);
    drop(img);
    // an image the cache does not keep: the reader's until it drops, on whichever thread drops it
    let rec = recs[2].1;
    let img = part.unwrapped_extent_bytes_uncached_for(owner, &ps.entry, &rec, 2, &mut same).expect("unwrap");
    assert!(row(stmt) - s0 >= rec.len as i64, "the private image charged");
    std::thread::spawn(move || drop(img)).join().expect("drop");
    assert_eq!(row(stmt) - s0, 0, "and released at its drop elsewhere");
    // a sub-range: read, and sliced from a cached extent (`extent_bytes` keeps the whole one)
    let rec = recs[3].1;
    let n = rec.len.min(4096);
    let sub = part.subrange_bytes(&rec, 0, n, "sub-range").expect("read");
    assert!(row(stmt) - s0 >= n as i64, "a sub-range read is the statement's");
    drop(sub);
    assert_eq!(row(stmt) - s0, 0);
    part.extent_bytes(&ps.entry, &rec, 3).expect("whole");
    let sub = part.subrange_bytes(&rec, 0, n, "sub-range").expect("slice");
    assert!(row(stmt) - s0 >= n as i64, "a slice's copy is the statement's");
    drop(sub);
    assert_eq!(row(stmt) - s0, 0);
    memledger::statement_end();
    memledger::unbind_backend();
}
