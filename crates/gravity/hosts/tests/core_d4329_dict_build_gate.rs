//! D4329 (D4315's second holder): a verbatim text column's per-part dictionary build (`text_hosts::row_codes`: the
//! column's bytes copied into one store, its row arrays, the fingerprint table) held its working memory outside every
//! grant: on a statement's scan threads it took hundreds of MB the grant pool never saw (the alloc-site probe: a W8
//! ORDER BY over orders with o_comment held 506-757 MB of heap mid-run beside an 18-34 MB sort region). Now the build
//! charges the grant pool before it grows (`ScratchGate`, `scratch_draw`: the claim's purse, the statement's part of the
//! pool, else an overdraft past the pool, `Overdraw::Scratch`), so admission and the statements beside it see it.
//! PostgreSQL holds no such memory, so the build is never refused for it (a 53200 PostgreSQL never raises: the first
//! cut's refusal failed e2e memlead_d4168_spool_file_road's pgrcolumnar statements under its 48 MiB pool).
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::{Arc, Mutex};

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, Resolved};
use gravity_hosts::take_error;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::PartRegistry;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

/// The grant pool is process-wide: the tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

const ROWS: u64 = 40_000;

/// A distinct ~200-byte value a row: no dictionary policy, so the writer stores the column verbatim and the reader builds
/// each part's dictionary when it reads it (about 4 MB of copied bytes a part).
fn value(i: u64) -> Vec<u8> {
    format!("{i:08}-{}", "x".repeat(190)).into_bytes()
}

fn verbatim_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_d4329_verbatim",
        dir: "/qa/t7329".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7329,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(value(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
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

/// Every row of the column read as code lanes (each part's dictionary built at its first window): the rows read, or the
/// first error. `ino`: the parts' inode numbers (a fresh part identity a call, so no cached table serves it).
fn read_codes(bf: &BuiltFixture, ino: u64) -> Result<u64, Box<types_error::PgError>> {
    // a host's PostgreSQL error is stashed on the thread (`gravity_hosts::stash`) behind the driver's error
    let pg = |e: gravity_driver::Error| take_error().unwrap_or_else(|| Box::new(types_error::PgError::error(format!("no stashed error: {e:?}"))));
    let list = part_list_at(bf, ino);
    let total = list.granules();
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT + 29, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).map_err(pg)?;
    src.seek(&[0, total], &[0, 0]).map_err(pg)?;
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0u64;
    let r = loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        match src.window(&mut w) {
            Ok(0) => break Ok(rows),
            Ok(n) => {
                assert_eq!(w.lanes()[0].kind, gravity_abi::ctx::lane::KIND_CODES, "a verbatim part publishes ranked codes");
                rows += n as u64;
            }
            Err(e) => break Err(pg(e)),
        }
    };
    src.unbind();
    r
}

/// A grant pool with no room for the build's working memory: the build still reads every row (never refused), its
/// working memory held in the pool as an overdraft past it (on trunk the pool never saw it: no overdraft, nothing held),
/// and every byte back once it ended.
#[test]
fn a_verbatim_parts_dictionary_build_charges_the_pool_past_its_room() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bf = verbatim_fixture();
    gravity_driver::POOL.arm(256 << 10);
    let past = || gravity_driver::POOL.overdraws().iter().fold((0u64, 0u64), |a, o| (a.0 + o.0, a.1 + o.1));
    let (held, before) = (gravity_driver::POOL.held(), past());
    let r = read_codes(&bf, 4_329_000);
    let (after_held, after) = (gravity_driver::POOL.held(), past());
    gravity_driver::POOL.arm(0);
    assert_eq!(r.expect("a build past the pool's room is served, never refused"), ROWS);
    assert!(after.0 > before.0, "the build's working memory went past the pool as an overdraft ({before:?} -> {after:?})");
    // each part's copied column alone is about 4 MB (20,000 rows of ~200 bytes)
    assert!(after.1 - before.1 >= 4 << 20, "the overdraft covers the build's copy of the column ({before:?} -> {after:?})");
    assert_eq!(after_held, held, "the build's overdraft went back at its end");
}

/// With room: every row reads as before, the build's draws came from the pool, and every byte is back once it ended.
#[test]
fn with_room_the_build_reads_every_row_and_gives_its_draws_back() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bf = verbatim_fixture();
    gravity_driver::POOL.arm(1 << 30);
    let held = gravity_driver::POOL.held();
    let r = read_codes(&bf, 4_329_100);
    let after = gravity_driver::POOL.held();
    gravity_driver::POOL.arm(0);
    assert_eq!(r.expect("served with room"), ROWS);
    assert_eq!(after, held, "the build's draws went back at its end");
}
