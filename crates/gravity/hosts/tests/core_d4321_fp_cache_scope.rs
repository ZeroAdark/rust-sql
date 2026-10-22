//! D4321 step 1, part 2 (LADDER-DESIGN section 6, item 2): a verbatim part's fingerprint table is a cache image (the
//! fingerprint cache keeps its `Ranks` and table buffer for the part's life), so its build runs inside a cache scope
//! (`memledger::arena::cache_scope`): the global allocator puts the table's arrays on the cache arena, and an evicted table
//! frees pages that hold cache blocks only. The probe allocator below counts the build's large ZEROED allocations (the
//! table's code array, `vec![0u32; rows]`, and its table buffer) made inside a cache scope: none on trunk. Zeroed only, so
//! the part cache's extent images (plain allocations, in their own scope since D4321 part 1) do not count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// While set, the zeroed allocations of at least `BIG` bytes are counted, inside and outside a cache scope.
static WATCH: AtomicU64 = AtomicU64::new(0);
static INSIDE: AtomicU64 = AtomicU64::new(0);
static OUTSIDE: AtomicU64 = AtomicU64::new(0);
const BIG: usize = 32 << 10;

struct Probe;

// SAFETY: every call goes to `System` unchanged; the probe only counts (atomics and the scope's thread-local depth).
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, passed through.
        unsafe { System.alloc(l) }
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if WATCH.load(Relaxed) != 0 && l.size() >= BIG {
            if memledger::arena::in_cache_scope() {
                INSIDE.fetch_add(1, Relaxed);
            } else {
                OUTSIDE.fetch_add(1, Relaxed);
            }
        }
        // SAFETY: the caller's layout, passed through.
        unsafe { System.alloc_zeroed(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: a block `alloc` above took from `System` with this layout.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Probe = Probe;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::Arc;

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
        name: "gravity_d4321_verbatim",
        dir: "/qa/t7321".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7321,
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
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT + 21, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
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

#[test]
fn a_verbatim_parts_fingerprint_table_is_built_inside_a_cache_scope() {
    let bf = verbatim_fixture();
    // as the server is once allocarena installed the heap: a scope's test reads true inside one
    memledger::arena::set_cache_heap_ready();
    gravity_driver::POOL.arm(1 << 30);
    let (i0, o0) = (INSIDE.load(Relaxed), OUTSIDE.load(Relaxed));
    WATCH.store(1, Relaxed);
    let r = read_codes(&bf, 4_321_000);
    WATCH.store(0, Relaxed);
    gravity_driver::POOL.arm(0);
    let (inside, outside) = (INSIDE.load(Relaxed) - i0, OUTSIDE.load(Relaxed) - o0);
    assert_eq!(r.expect("served"), ROWS);
    // the sort's zeroed scratch stays outside on purpose (transient, on the general arenas)
    assert!(inside >= 2, "each part's zeroed code array came from the cache arena (inside {inside}, outside {outside})");
    assert!(!memledger::arena::in_cache_scope(), "the scope ends with the build");
}
