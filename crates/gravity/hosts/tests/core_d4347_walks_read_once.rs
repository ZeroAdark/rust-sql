//! D4347: a claim reads a verbatim column once, not once per whole walk. A verbatim text column read as code lanes with
//! a LIKE table over it is walked whole by the ranks (`text_hosts::row_codes`), and the LIKE table's granule scans or the
//! rows' render read it again in the same claim. With D4315's pass on load the first walk passed every extent and a trim
//! under a capped cache evicted them, so the server read the column again (SF10 orders, one statement at a 64 MB cap:
//! 723 MB re-read for LIKE, 450 MB for the sort; the server-side witness, box 18). This harness's read does not reach the
//! server's second read, so it guards the result (a capped read faults the part once, with the trim hook armed as the
//! server's) rather than reproducing the bug. A process of its own: the part cache's budget and census are process-wide.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::Arc;

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, Resolved};
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::PartRegistry;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

/// Eight bands of 65,536 rows: eight extents of the column in one part.
const ROWS: u64 = 8 * 65_536;
const REL: u32 = 4533;

/// A distinct ~24-byte value a row: no dictionary policy, so the writer stores the column verbatim (about 1.5 MB an extent).
fn value(i: u64) -> Vec<u8> {
    format!("{i:08}-{}", "y".repeat(15)).into_bytes()
}

fn verbatim_part() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_d4347_walks",
        dir: "/qa/t4533".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 4533,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(value(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: ROWS, ..PartCutPolicy::default() },
    };
    let cands = CodecCandidates::new(ColumnPosture::default());
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, ROWS, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// One statement's read of the whole part as code lanes with a LIKE table over the lane, in one claim (a W8 worker's
/// whole-part claim), the uses released as the server's are; `ino` a fresh part identity a call, so no cached dictionary
/// or table spares the walks. The part cache bytes the read faulted in.
fn scan(bf: &BuiltFixture, ino: u64, registry: &Arc<PartRegistry>) -> u64 {
    let list = part_list_at(bf, ino);
    assert_eq!(list.len(), 1, "the fixture is one part");
    let like = TableSpec { over: 0, func: TableFn::Like { pattern: b"%0042%".to_vec(), ilike: false, negated: false } };
    let recipe = GranuleRecipe::for_test_codes(REL, &[1], &[1], Arc::clone(registry)).with_tables(vec![like]);
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(None);
    resolved.install(REL, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    let before = memledger::value::stats(memledger::Consumer::PartCache).miss_bytes;
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        rows += n as u64;
    }
    assert_eq!(rows, ROWS);
    src.unbind();
    memledger::value::stats(memledger::Consumer::PartCache).miss_bytes - before
}

/// The server's trim hook (hosts `residency::part_cache_trim`, armed with the part cache): a pass past the cache's level
/// trims at once, over this test's registry.
static REGISTRY: std::sync::OnceLock<Arc<PartRegistry>> = std::sync::OnceLock::new();

fn trim_hook() {
    if let Some(r) = REGISTRY.get() {
        r.trim_if_over();
    }
}

#[test]
fn a_column_walked_twice_is_read_once() {
    let bf = verbatim_part();
    let registry = Arc::clone(REGISTRY.get_or_init(|| Arc::new(PartRegistry::new(1 << 30))));
    pgrc_read::partcache::set_trim_hook(trim_hook);

    // memory to spare: nothing is evicted, so the read faults the part's images once
    pgrc_read::partcache::set_budget(u64::MAX);
    let once = scan(&bf, 4_347_000, &registry);
    let whole = pgrc_read::partcache::bytes();
    assert!(whole >= 8 << 20 && once >= whole, "the part's images faulted once: {once} bytes, {whole} cached");

    // cold, past the level: a fresh part identity walks twice again; the second walk reads what the first left cached
    registry.trim_contents(0);
    pgrc_read::partcache::set_budget(whole / 8);
    let capped = scan(&bf, 4_347_100, &registry);
    assert!(capped <= once + once / 10, "the capped read faulted {capped} bytes for the {once} a read without eviction faults (the second walk read the column again)");
}
