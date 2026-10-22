#![cfg(not(loom))]
//! CONVERT wave 3 (cvwrite job 2): a parquet load's decoder buffers sit on its statement's owner by construction. A
//! row-group reader opened on an owner charges its column chunks from the moment they are reserved (before the
//! read), and each column's page, levels and dictionary as they are decoded; the fast decoder likewise. Everything
//! goes back when the reader drops. Its own binary: the global allocator here is door 1 (every allocation and free
//! counted on the calling thread's account, `memledger::door_heap`), as the server's allocator is.

use std::alloc::{GlobalAlloc, Layout, System};

use memledger::charge::{context, owner_row};
use memledger::{account_row, bind_backend, door_heap, fold, statement_begin, statement_end, unbind_backend, Acct, Owner};
use parquet_read::{ColumnBatch, FastRowGroup, FileReader, Phys, RowGroupReader};

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// What the runtime around the test thread moves between accounts: far below what these tests charge.
const SLACK: i64 = 64 << 10;

/// Each test on its own thread and backend slot, its statement running: `context()` is the statement owner, as on a
/// COPY leader.
fn on(procno: u32, f: impl FnOnce(Acct, Owner) + Send + 'static) {
    std::thread::spawn(move || {
        bind_backend(procno, 8300 + procno);
        statement_begin();
        let (o, _) = context();
        assert!(matches!(o, Owner::Statement { .. }), "a running statement's context is its owner");
        f(Acct::backend(procno), o);
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

fn committed(o: Owner) -> i64 {
    owner_row(o).committed
}

fn fixture(name: &str) -> String {
    format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn a_row_group_reader_charges_chunks_pages_levels_and_dictionaries_and_gives_them_back() {
    on(51, |a, o| {
        let base = account(a);
        for name in ["basic.parquet", "zstd.parquet", "dictfallback.parquet", "gzip.parquet"] {
            let path = fixture(name);
            let fr = FileReader::open(&path).expect("fixture");
            let file = fr.try_clone_file().expect("dup");
            let cols: Vec<usize> = (0..fr.meta.columns.len()).collect();
            let vutf8: Vec<bool> = fr.meta.columns.iter().map(|c| c.phys == Phys::ByteArray).collect();
            let mut batches: Vec<ColumnBatch> = fr.meta.columns.iter().map(|c| ColumnBatch::new_for(c.phys)).collect();
            for rg in 0..fr.meta.row_groups.len() {
                let mut rgr = RowGroupReader::open_on(&file, &path, &fr.meta, rg, &cols, &vutf8, Some(o)).expect("open");
                let chunks = rgr.compressed_bytes as i64;
                assert!(committed(o) >= chunks, "{name} rg {rg}: the chunks are the statement's ({} < {chunks})", committed(o));
                let mut peak = committed(o);
                while rgr.rows_remaining() > 0 {
                    let n = rgr.rows_remaining().min(333) as usize;
                    rgr.read_batches(&mut batches, n).expect("decode");
                    peak = peak.max(committed(o));
                }
                if matches!(name, "zstd.parquet" | "gzip.parquet") {
                    assert!(peak > chunks, "{name} rg {rg}: the decompressed pages are the statement's too");
                }
                drop(rgr);
                assert_eq!(committed(o), 0, "{name} rg {rg}: every byte leaves with the reader");
            }
        }
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
        let fr = FileReader::open(&fixture("basic.parquet")).expect("fixture");
        let file = fr.try_clone_file().expect("dup");
        let cols: Vec<usize> = (0..fr.meta.columns.len()).collect();
        let vutf8 = vec![false; cols.len()];
        let plain = RowGroupReader::open(&file, fr.path(), &fr.meta, 0, &cols, &vutf8).expect("open");
        assert_eq!(committed(o), 0, "a reader opened without an owner charges nothing");
        drop(plain);
    });
}

/// memsig E-pub t5 witness (memlead 2026-10-15T21:30Z shape): a file reader opened on an owner holds its parsed footer on
/// it (`FileMeta::heap_bytes`, its capacities) from the parse until the footer's last `Arc` drops, on whichever thread; the
/// footer bytes read for the parse are the owner's only while they live; the account is back after.
#[test]
fn a_file_readers_footer_is_its_owners_until_the_last_arc_drops() {
    on(53, |a, o| {
        let base = account(a);
        for name in ["basic.parquet", "zstd.parquet", "dictfallback.parquet", "gzip.parquet", "sortable.parquet"] {
            let fr = FileReader::open_on(&fixture(name), Some(o)).expect("fixture");
            let held = fr.meta.heap_bytes() as i64;
            assert!(held > 0, "{name}: the footer holds heap bytes");
            assert_eq!(fr.meta.held_bytes() as i64, held, "{name}: the charge is synced to the footer's capacities");
            assert_eq!(committed(o), held, "{name}: the owner holds the parsed footer; the read's footer bytes left with the parse");
            let meta = fr.meta_arc();
            drop(fr);
            assert_eq!(committed(o), held, "{name}: an Arc of the footer keeps it the owner's");
            std::thread::spawn(move || drop(meta)).join().unwrap();
            assert_eq!(committed(o), 0, "{name}: the last Arc's drop, on another thread, gives it back");
        }
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
        let fr = FileReader::open(&fixture("basic.parquet")).expect("fixture");
        assert_eq!((committed(o), fr.meta.held_bytes()), (0, 0), "a reader opened without an owner charges nothing");
    });
}

#[test]
fn the_fast_decoder_charges_its_chunks_and_pages_and_gives_them_back() {
    on(52, |a, o| {
        let base = account(a);
        let mut opened = 0;
        for name in ["basic.parquet", "zstd.parquet", "dictfallback.parquet", "sortable.parquet"] {
            let path = fixture(name);
            let fr = FileReader::open(&path).expect("fixture");
            let file = fr.try_clone_file().expect("dup");
            let cols: Vec<usize> = (0..fr.meta.columns.len())
                .filter(|&c| {
                    let s = &fr.meta.columns[c];
                    s.max_rep == 0
                        && matches!(s.phys, Phys::Int32 | Phys::Int64 | Phys::Float | Phys::Double | Phys::ByteArray)
                })
                .collect();
            if cols.is_empty() {
                continue;
            }
            let vutf8: Vec<bool> = cols.iter().map(|&c| fr.meta.columns[c].phys == Phys::ByteArray).collect();
            for rg in 0..fr.meta.row_groups.len() {
                let Ok(mut frg) = FastRowGroup::open_on(&file, &path, &fr.meta, rg, &cols, &vutf8, Some(o)) else { continue };
                opened += 1;
                let chunks = frg.compressed_bytes as i64;
                assert!(committed(o) >= chunks, "{name} rg {rg}: the chunks are the statement's");
                let rows = frg.num_rows as usize;
                let mut peak = committed(o);
                for col in frg.cols.iter_mut() {
                    let mut left = rows;
                    while left > 0 {
                        let Ok(run) = col.next_run(left.min(1024)) else { break };
                        left -= run.n;
                        peak = peak.max(committed(o));
                    }
                }
                if name == "zstd.parquet" {
                    assert!(peak > chunks, "{name} rg {rg}: the decompressed pages are the statement's too");
                }
                drop(frg);
                assert_eq!(committed(o), 0, "{name} rg {rg}: every byte leaves with the row group");
            }
        }
        assert!(opened > 0, "some fixture is fast-decodable");
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
    });
}
