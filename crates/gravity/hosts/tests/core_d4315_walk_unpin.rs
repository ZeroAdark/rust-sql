//! D4315 (step 2): a whole-column walk pins its read-ahead, not the part. A verbatim text column read as code lanes builds
//! each part's dictionary at its first window (`text_hosts::row_codes`), walking the part's whole column through the lane's
//! cursor; a LIKE table by granule scans (`ranked_scan_bits`) walks it the same way. The cursor held every extent it loaded
//! until the claim left the part, and the per-band read-ahead brought the walked extents back again, so a cold scan under
//! a capped cache pinned the whole part (SF10 orders' o_comment: 55 MB a part, 8 parts at W8). Now the cursor passes each
//! extent as it loads the next and the read-ahead starts past what the cursor read: the walk pins about one extent and
//! its read-ahead. Read through the registry's floor (`in_use_bytes`: cached bytes less the passed ones). A process of
//! its own: the part cache's budget and census are process-wide.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::Arc;

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, Resolved};
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::PartRegistry;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

/// Eight bands of 65,536 rows: eight extents of the column in one part.
const ROWS: u64 = 8 * 65_536;
const REL: u32 = 4532;

/// A distinct ~24-byte value a row: no dictionary policy, so the writer stores the column verbatim (about 1.5 MB an extent).
fn value(i: u64) -> Vec<u8> {
    format!("{i:08}-{}", "y".repeat(15)).into_bytes()
}

fn verbatim_part() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_d4315_walk",
        dir: "/qa/t4532".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 4532,
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

/// One statement's read of the whole part as code lanes in one claim (a W8 worker's whole-part claim), the uses released
/// as the server's are; `ino` a fresh part identity a call, so no cached dictionary spares the walk. After each window
/// `(registry in-use bytes, part cache bytes)`.
fn scan(bf: &BuiltFixture, ino: u64, registry: &Arc<PartRegistry>) -> Vec<(u64, u64)> {
    let list = part_list_at(bf, ino);
    assert_eq!(list.len(), 1, "the fixture is one part");
    let recipe = GranuleRecipe::for_test_codes(REL, &[1], &[1], Arc::clone(registry));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(None);
    resolved.install(REL, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut seen = Vec::new();
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!(w.lanes()[0].kind, gravity_abi::ctx::lane::KIND_CODES, "a verbatim part publishes ranked codes");
        rows += n as u64;
        seen.push((registry.in_use_bytes(), pgrc_read::partcache::bytes()));
    }
    assert_eq!(rows, ROWS);
    src.unbind();
    seen
}

#[test]
fn a_whole_column_walk_pins_its_read_ahead_not_the_part() {
    let bf = verbatim_part();
    let registry = Arc::new(PartRegistry::new(1 << 30));

    // memory to spare: the walk and the scan cache the part's column
    pgrc_read::partcache::set_budget(u64::MAX);
    scan(&bf, 4_315_000, &registry);
    let whole = pgrc_read::partcache::bytes();
    assert!(whole >= 8 << 20, "the part's eight extents are cached after the scan: {whole} bytes");

    // cold, past the level: a fresh part identity walks again; the trim takes what the cursor passed while the part is in use
    registry.trim_contents(0);
    pgrc_read::partcache::set_budget(whole / 8);
    let seen = scan(&bf, 4_315_100, &registry);
    let pinned = seen.iter().map(|s| s.0).max().unwrap();
    assert!(pinned <= whole / 2, "pinned at most about one extent and its read-ahead: {pinned} of the part's {whole} bytes (step 1: the whole part) {seen:?}");
}
