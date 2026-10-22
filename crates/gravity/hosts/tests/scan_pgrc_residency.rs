//! A part list over parts the catalog holds open (`residency::resident_parts` hands the sources such lists): a source pins
//! them without an open, its rebind and pass allocate nothing, and a new generation's list is served the same way.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

/// The tests' scratch site lanes (a test bound, not the ABI's: sites size their lane arrays per program).
const MAX_LANES: usize = 32;
use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved as StatementParts};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes, BuiltFixture};
use pgrc_read::{PartExpect, PartPin, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKED: AtomicUsize = AtomicUsize::new(0);
static FLAG: AtomicU8 = AtomicU8::new(0);

thread_local! {
    static TOKEN: Cell<u8> = const { Cell::new(0) };
}

fn token() -> usize {
    TOKEN.with(|t| t.as_ptr() as usize)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if token() == TRACKED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn tracked(f: impl FnOnce()) -> usize {
    TRACKED.store(token(), Ordering::Relaxed);
    ALLOCS.store(0, Ordering::Relaxed);
    f();
    TRACKED.store(0, Ordering::Relaxed);
    ALLOCS.load(Ordering::Relaxed)
}

const REL: u32 = 4243;
const ROWS: u64 = 60_000;

fn value(i: u64) -> Option<i64> {
    Some((i as i64 * 31) % 1000)
}

fn fixture() -> BuiltFixture {
    build_fixture(int8_fixture("gravity_resident", 7302, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, value))
}

/// A generation's list as `LISTS` builds it: every part open (as the catalog holds them), pinned, carried by its `PartSpec`.
fn resident(bf: &BuiltFixture, ino_base: u64) -> (Arc<PartList>, Vec<PartPin>) {
    let mut pins = Vec::new();
    let parts = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            let part = open_part_bytes(bytes, ino_base + i as u64).expect("open part");
            pins.push(PartPin::pin(&part));
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: ino_base + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(part) }
        })
        .collect();
    (Arc::new(PartList::new(parts)), pins)
}

fn binding_ref(resolved: &StatementParts) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

fn pass(src: &mut GranuleRangeSource, claims: &GranuleRange, lanes: &mut [Lane; MAX_LANES]) -> u64 {
    claims.reset();
    let mut rows = 0u64;
    while let Some(c) = claims.claim() {
        src.seek(&[c.lo, c.hi], &[0, 0]).unwrap();
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            rows += n as u64;
        }
    }
    rows
}

#[test]
fn a_list_over_open_parts_opens_nothing_and_its_hit_path_allocates_nothing() {
    let bf = fixture();
    let registry = Arc::new(PartRegistry::new(64 << 20));
    let (first, _pins) = resident(&bf, 500);
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::clone(&registry));
    let resolved = StatementParts::for_recipe(&recipe, Arc::clone(&first));
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let claims = GranuleRange::over_parts(first.granules(), ClaimSize::Fixed(1), first.starts());
    let mut lanes = [Lane::NONE; MAX_LANES];
    assert_eq!(pass(&mut src, &claims, &mut lanes), ROWS);
    assert_eq!((src.parts_opened(), src.residency().parts, registry.counters().misses), (0, 3, 0), "resident parts: zero registry opens");
    src.unbind();

    // The hit path allocates nothing: the rebind and a full pass.
    let allocs = tracked(|| {
        src.bind(&binding_ref(&resolved)).unwrap();
        assert_eq!(pass(&mut src, &claims, &mut lanes), ROWS);
        src.unbind();
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations on the resident hit path");

    // A new generation's list: its parts are open too, so the sources still open nothing.
    let (second, _pins2) = resident(&bf, 520);
    assert_ne!(second.generation(), first.generation());
    let resolved2 = StatementParts::for_recipe(src.recipe(), Arc::clone(&second));
    src.bind(&binding_ref(&resolved2)).unwrap();
    let claims2 = GranuleRange::over_parts(second.granules(), ClaimSize::Fixed(1), second.starts());
    assert_eq!(pass(&mut src, &claims2, &mut lanes), ROWS);
    assert_eq!((src.parts_opened(), src.residency().parts, registry.counters().misses), (0, 3, 0), "the new generation's parts are resident too");
    src.unbind();
    let _ = &FLAG;
}

/// The table directory names parse; no catalog prime ran in this process.
#[test]
fn the_table_dirs_parse_and_no_prime_ran() {
    assert_eq!((gravity_hosts::ops::scan::pgrc::prime::parse_table_dir("pgrc_7302"), gravity_hosts::ops::scan::pgrc::prime::parse_table_dir("7302"), gravity_hosts::ops::scan::pgrc::prime::parse_table_dir("pgrc_x")), (Some(7302), None, None));
    assert!(gravity_hosts::ops::scan::pgrc::prime::report().is_none(), "no prime ran in this process");
}
