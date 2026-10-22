//! D4552 (a): a dictionary one statement opens and keeps warm in its part list's cell (D4410) is freed wherever its last
//! holder drops it: another backend's statement that adopted it from the cell (`adopt_warm`), or the thread that runs the
//! part cache's evicted hook (`let_warm_go`). Every byte the entry holds must leave that thread's account as it came, so
//! its account ends where it began: an entry's bytes the opener's account kept are freed below zero elsewhere (a floored
//! fold under `ledger_floor`). Its own binary: the global allocator here is door 1 and, inside a cache scope, the cache
//! road, as the server's wrapper is.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;

use memledger::arena::{cache_took, in_cache_scope};
use memledger::{account_row, bind_backend, door_heap, fold, statement_begin, statement_end, unbind_backend, Acct};

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` / `cache_took` are const thread-local adds (no allocation, no
// destructor), a fold atomics only.
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
            if in_cache_scope() {
                cache_took(l.size() as isize);
            }
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
            if in_cache_scope() {
                cache_took(l.size() as isize);
            }
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
        if in_cache_scope() {
            cache_took(-(l.size() as isize));
        }
    }
}

#[global_allocator]
static A: Door1 = Door1;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use granule_fixtures::*;
use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{let_warm_go, GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_read::{PartExpect, PartRegistry};

/// The fixture's parts as files under a fresh directory, as the server opens them (a path, not a copy in memory).
fn file_parts(bf: &pgrc_qa::corpus::BuiltFixture, tag: u64) -> Arc<PartList> {
    let dir = std::env::temp_dir().join(format!("gravity-d4552-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let parts = bf
        .manifest
        .parts
        .iter()
        .map(|rec| {
            let name = part_file_name(rec.part_no);
            let path = dir.join(&name);
            std::fs::write(&path, bf.files.get(&name).expect("part file")).unwrap();
            let mut expect = PartExpect::none();
            expect.part_no = Some(rec.part_no);
            PartSpec { io: PartIoSpec::Path(std::ffi::CString::new(path.to_str().unwrap()).unwrap()), expect, rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }
        })
        .collect();
    Arc::new(PartList::new(parts))
}

/// A free on the dropping thread of a byte its account never took: far below one dictionary's loose bytes (its tables,
/// its streams' extent lists, its frame tables).
const SLACK: i64 = 256;

/// The rows of claim `[lo, hi)` through the code lane's windows; with `render`, every 64th code of each window rendered
/// (its payload faulted, as a statement's emit does).
fn code_rows(src: &mut GranuleRangeSource, lo: u64, hi: u64, render: bool) -> u64 {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return rows;
        }
        let l = w.lanes()[0];
        assert_eq!(l.kind, lane::KIND_CODES);
        let part = w.base().part_id;
        let codes: Vec<u32> = (0..n as usize).step_by(64).map(|i| unsafe { *(l.ptr as *const u32).add(i) }).collect();
        if render {
            for c in codes {
                src.dict_value(part, 0, c).unwrap();
            }
        }
        rows += n as u64;
    }
}

/// One statement on backend `procno` reading every row of `list`'s code lane (`render`: rendering some), its
/// dictionaries kept warm in the list's cells or taken from them. Returns the account's change over the statement.
fn statement(procno: u32, list: &Arc<PartList>, reg: &Arc<PartRegistry>, uses_release: bool, render: bool) -> i64 {
    let a = Acct::backend(procno);
    bind_backend(procno, 90_000 + procno);
    statement_begin();
    fold();
    let base = account_row(a).account_bytes;
    {
        let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::clone(reg));
        let mut resolved = Resolved::new();
        if uses_release {
            // the server's streaming / uses-only release under the part cache's level: the statement's own cells
            // (StmtDicts) and the list's warm cell both hold each dictionary
            resolved.set_part_uses_release(true);
            resolved.set_part_own_dicts(true);
            resolved.set_part_share(Some(u64::MAX));
        }
        resolved.install(REL_TEXT, Arc::clone(list));
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        assert_eq!(code_rows(&mut src, 0, list.granules(), render), list.rows());
        src.unbind();
        drop(src);
        drop(resolved);
    }
    let _ = statement_end();
    fold();
    let delta = account_row(a).account_bytes - base;
    unbind_backend();
    delta
}

/// Statement X (backend `x`) opens every part's dictionary and keeps it warm in the list's cells; statement Y (backend
/// `y`) reads them from the cells (`adopt_warm`); then backend `z` runs the part cache's evicted hook, which lets the
/// cells go: z's thread holds the last `Arc` of each entry and frees it. Every byte of an entry must leave z's account as
/// it came, so z's account ends where it began.
fn last_holder_on_another_backend(bf: pgrc_qa::corpus::BuiltFixture, ino: u64, (x, y, z): (u32, u32, u32), uses_release: bool, evicted: bool) -> i64 {
    let list = file_parts(&bf, ino);
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let nparts = list.len();
    let (l, r) = (Arc::clone(&list), Arc::clone(&reg));
    let opener = std::thread::spawn(move || statement(x, &l, &r, uses_release, true)).join().unwrap();
    assert_eq!(list.shared_dicts(), nparts, "the list's cells keep every part's dictionary warm");
    let (l, r) = (Arc::clone(&list), Arc::clone(&reg));
    let adopter = std::thread::spawn(move || statement(y, &l, &r, uses_release, true)).join().unwrap();
    assert_eq!(list.shared_dicts(), nparts, "the adopter left the cells as it found them");
    let r = Arc::clone(&reg);
    let dropper = std::thread::spawn(move || {
        let a = Acct::backend(z);
        bind_backend(z, 90_000 + z);
        fold();
        let base = account_row(a).account_bytes;
        if evicted {
            // the part cache lets the parts go on z's thread (the registry's entries and their map's nodes): an entry's
            // handle holds the last Arc of its part
            r.clear();
        }
        let_warm_go();
        fold();
        let delta = account_row(a).account_bytes - base;
        unbind_backend();
        delta
    })
    .join()
    .unwrap();
    assert_eq!(list.shared_dicts(), 0, "the evicted hook emptied the list's cells");
    eprintln!("{}: opener {opener:+} B, adopter {adopter:+} B, last holder {dropper:+} B ({nparts} parts, uses_release {uses_release}, parts evicted {evicted})", bf.fx.name);
    dropper
}

/// The values of a dictionary whose front-coded payload is not wrapped: an unwrapped payload faults by frame (its frame
/// tables per extent).
fn seq_value(i: u64) -> Vec<u8> {
    format!("{i:016}").into_bytes()
}

/// One test, the cases in turn: the evicted hook walks every live list.
#[test]
fn a_warm_dictionary_leaves_the_account_of_the_statement_that_drops_it_as_it_came() {
    // the server's defaults: the homes on (a shared object's bytes free where they were made), the floor off here so an
    // account's end below zero shows
    memledger::set_home(true);
    let mut ino = 45_520;
    let mut ends = Vec::new();
    for evicted in [false, true] {
        ends.push(("two small parts", evicted, last_holder_on_another_backend(text_fixture(), ino, (3945, 3946, 3947), false, evicted)));
        // a wide one (50,000 entries, a wrapped block-grain payload) in the statement's own cells too (StmtDicts)
        ends.push(("wide", evicted, last_holder_on_another_backend(wide_text_fixture("gravity_d4552_wide", 7552), ino + 10, (3948, 3949, 3950), true, evicted)));
        // an unwrapped multi-frame payload (frame-lazy / frame-direct)
        ends.push(("multi-frame", evicted, last_holder_on_another_backend(text_fixture_of("gravity_d4552_seq", 7553, 300_000, 400_000, seq_value), ino + 20, (3951, 3952, 3953), true, evicted)));
        ino += 100;
    }
    let below: Vec<_> = ends.iter().filter(|e| e.2 < -SLACK).collect();
    assert!(below.is_empty(), "the last holder freed bytes its account never took (case, parts evicted, its account's end): {below:?}");
}
