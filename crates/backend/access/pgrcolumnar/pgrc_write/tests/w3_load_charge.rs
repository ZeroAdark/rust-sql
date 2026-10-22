#![cfg(not(loom))]
//! CONVERT wave 3 (W3-2b): a load's buffers sit on its statement's owner by construction. The column buffers, the
//! captured chunks, the dictionary build and the serial writer each hold one charge that follows the bytes they hold,
//! filled on one thread and freed on another (a worker) as the parallel session does, and give it all back with the
//! data. Its own binary: the global allocator here is door 1 (every allocation and free counted on the calling thread's
//! account, `memledger::door_heap`), as the server's allocator is.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use memledger::charge::{context, enter_as, owner_row};
use memledger::{account_row, bind_backend, door_heap, fold, statement_begin, statement_end, unbind_backend, Acct, Owner};
use pgrc_write::dict::{DictBuilder, TextSemantics};
use pgrc_write::ingest::ColBuffer;
use pgrc_write::par::{ParEngine, ParIngestOpts, RowChunk, SharedMemVfsProvider, VfsProvider};
use pgrc_write::seal::PartSpec;
use pgrc_write::testkit::{
    append_int8_rows, int8_col, mem_with_dir, open_writer_policy, par_providers, shared_mem_with_dir, stamp, text_col,
    with_mixed_row, Kit, DIR,
};
use pgrc_write::writer::{PartCutPolicy, SealEnv};
use pgrc_write::wvfs::{WriteVfs, WvFd};
use pgrc_write::WriteResult;

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

/// What the runtime around a spawn / join (and the tests' own small values) moves between two threads' accounts: far
/// below the blocks these tests charge.
const SLACK: i64 = 64 << 10;

/// Each test on its own thread and backend slot (no other test's), its statement running: `context()` is the statement
/// owner, as on a COPY leader.
fn on(procno: u32, f: impl FnOnce(Acct, Owner) + Send + 'static) {
    std::thread::spawn(move || {
        bind_backend(procno, 8200 + procno);
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

#[test]
fn a_column_buffer_charges_what_it_holds_through_splice_and_permute_and_gives_it_back() {
    on(41, |a, o| {
        let base = account(a);
        let mut c = ColBuffer::with_owner(text_col(2), Some(o));
        c.reserve_rows(5000, 80_000).unwrap();
        assert_eq!(committed(o), c.held_bytes() as i64, "synced at the reserve");
        for i in 0..5000 {
            c.append_varlena_payload(format!("value-{i:06}-payload").as_bytes()).unwrap();
        }
        // per-row appends: the writer syncs them once per granule
        c.sync_charge();
        assert_eq!(committed(o), c.held_bytes() as i64);
        let mut acc = ColBuffer::with_owner(text_col(2), c.owner());
        acc.reserve_splice([&c].into_iter()).unwrap();
        acc.splice_chunk(&c).unwrap();
        assert_eq!(committed(o), (c.held_bytes() + acc.held_bytes()) as i64, "the part accumulator is the owner's too");
        let perm: Vec<u32> = (0..acc.rows() as u32).rev().collect();
        let p = acc.permuted(&perm).unwrap();
        assert_eq!(p.owner(), Some(o), "the sorted copy inherits the owner");
        assert_eq!(committed(o), (c.held_bytes() + acc.held_bytes() + p.held_bytes()) as i64);
        drop((c, acc, p));
        assert_eq!(committed(o), 0, "every byte leaves with its buffer");
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
        let un = ColBuffer::new(text_col(2));
        assert_eq!(un.owner(), None, "a plain buffer stays uncharged");
    });
}

#[test]
fn a_row_chunk_filled_here_and_dropped_on_a_worker_leaves_no_residue() {
    on(42, |a, o| {
        let base = account(a);
        let mut ch = RowChunk::with_owner(2, 4096, Some(o));
        assert_eq!(committed(o), ch.held_bytes() as i64, "the cells are charged from the first block");
        for i in 0..4096 {
            with_mixed_row(i, |r| ch.push_row(r).unwrap());
        }
        assert_eq!(committed(o), ch.held_bytes() as i64, "a push that grew a block synced it");
        assert!(committed(o) > 4096 * 2 * 16);
        std::thread::spawn(move || {
            let _acct = enter_as(o);
            drop(ch);
        })
        .join()
        .unwrap();
        assert_eq!(committed(o), 0, "the worker's drop gave the charge back");
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
    });
}

#[test]
fn the_dictionary_build_is_charged_until_it_drops() {
    on(43, |a, o| {
        let base = account(a);
        let mut b = DictBuilder::new(TextSemantics::BytesOnly).charged(Some(o));
        for i in 0..200_000u64 {
            b.observe(format!("dict-value-{:08}", (i * 7919) % 50_000).as_bytes());
        }
        let lag = b.held_bytes() as i64 - committed(o);
        assert!((0..=(64 << 10) + 64).contains(&lag), "the observe charge lags at most 64 KiB of payloads: {lag}");
        assert!(committed(o) > 50_000 * 16);
        let built = b.build();
        assert_eq!(built.owner(), Some(o));
        assert_eq!(committed(o), built.held_bytes() as i64, "the built dictionary is synced");
        drop(built);
        assert_eq!(committed(o), 0);
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
    });
}

#[test]
fn the_serial_writer_charges_its_buffered_part_until_the_seal() {
    on(44, |a, o| {
        let base = account(a);
        let mut vfs = mem_with_dir();
        let mut kit = Kit::new();
        let policy = PartCutPolicy { max_rows: 1 << 20, max_bytes: u64::MAX, cut_granule_rows: 1024 };
        let mut w = open_writer_policy(vec![int8_col(1)], stamp(77, 1), policy);
        w.set_owner(Some(o));
        assert_eq!(w.owner(), Some(o));
        append_int8_rows(&mut w, &mut vfs, &mut kit, 10_000, |i| Some(i as i64));
        // synced at the 9216-row granule: at least that many words (8 B) on the statement
        assert!(committed(o) >= 9216 * 8, "committed {}", committed(o));
        {
            let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            w.finish(&mut env).unwrap();
        }
        assert_eq!(w.sealed_parts().len(), 1);
        assert_eq!(committed(o), 0, "the sealed part's buffers are gone, the fresh ones empty");
        drop(w);
        drop(vfs);
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
    });
}

/// The parallel session's seal: what the statement holds when a part's seal opens its file (the spliced part and the
/// chunks still in flight).
struct Sampling {
    inner: SharedMemVfsProvider,
    o: Owner,
    peak: Arc<AtomicI64>,
}

impl VfsProvider for Sampling {
    fn make(&self) -> Box<dyn WriteVfs + Send> {
        self.peak.fetch_max(committed(self.o), Ordering::SeqCst);
        self.inner.make()
    }
}

#[test]
fn a_parallel_session_charges_its_statement_from_capture_through_the_seal() {
    on(45, |_a, o| {
        let shared = shared_mem_with_dir();
        let peak = Arc::new(AtomicI64::new(0));
        let mut providers = par_providers(&shared);
        providers.vfs = Arc::new(Sampling { inner: SharedMemVfsProvider(shared.clone()), o, peak: Arc::clone(&peak) });
        let schema = vec![int8_col(1), text_col(2)];
        let spec = PartSpec {
            spc: 1663,
            db: 5,
            relfilenumber: 777,
            schema_fingerprint: pgrc_format::ident::schema_fingerprint(&schema),
        };
        let e = ParEngine::new(
            providers,
            schema,
            spec,
            DIR.to_string(),
            4001,
            PartCutPolicy { max_rows: 8192, max_bytes: u64::MAX, cut_granule_rows: 1024 },
            ParIngestOpts { chunk_rows: 1024, max_chunks_in_flight: 64, max_parts_in_flight: 64 },
            0,
        )
        .unwrap()
        .with_owner(Some(o));
        assert_eq!(e.owner(), Some(o));
        let mut id = 0u64;
        let mut cur = RowChunk::with_owner(2, 1024, e.owner());
        for i in 0..20 * 1024 {
            with_mixed_row(i, |r| cur.push_row(r).unwrap());
            if cur.rows() == 1024 {
                e.publish_chunk(std::mem::replace(&mut cur, RowChunk::with_owner(2, 1024, e.owner())), id).unwrap();
                id += 1;
            }
        }
        let captured = committed(o);
        assert!(captured > 20 * 1024 * 2 * 16, "the captured chunks are the statement's: {captured}");
        for k in 0..id {
            e.run_chunk(0, k);
        }
        e.close_input(id);
        let sealed = e.collect().unwrap();
        assert_eq!(sealed.len(), 3);
        assert!(peak.load(Ordering::SeqCst) > 8192 * 8, "a part's seal held its columns on the statement");
        drop(sealed);
        drop(cur);
        drop(e);
        assert_eq!(committed(o), 0, "nothing of the session stays on the statement");
    });
}

/// A write VFS that records, at each file write, what the statement holds and how many bytes the write carries.
struct WriteSampler<'a> {
    inner: &'a mut dyn WriteVfs,
    o: Owner,
    at_writes: Vec<(i64, usize)>,
}

impl WriteVfs for WriteSampler<'_> {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.inner.create_rw(path)
    }
    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.inner.open_rw(path)
    }
    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        self.at_writes.push((committed(self.o), bytes.len()));
        self.inner.pwrite_at(fd, off, bytes)
    }
    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        self.inner.fsync_file(fd)
    }
    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        self.inner.close_file(fd)
    }
    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        self.inner.read_full(path)
    }
    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        self.inner.rename_path(from, to)
    }
    fn unlink_path(&mut self, path: &str) -> WriteResult<()> {
        self.inner.unlink_path(path)
    }
    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()> {
        self.inner.fsync_dir(dir)
    }
    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>> {
        self.inner.list_dir(dir)
    }
    fn mkdir_path(&mut self, dir: &str) -> WriteResult<()> {
        self.inner.mkdir_path(dir)
    }
    fn exists_path(&mut self, path: &str) -> WriteResult<bool> {
        self.inner.exists_path(path)
    }
}

/// splitmix64: values no encoding packs, so the part image is about as large as the column.
fn scatter(i: u64) -> i64 {
    let mut z = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) as i64
}

#[test]
fn the_seal_holds_its_part_image_on_the_statement_until_it_writes_it() {
    on(46, |a, o| {
        let base = account(a);
        let mut vfs = mem_with_dir();
        let mut kit = Kit::new();
        let policy = PartCutPolicy { max_rows: 1 << 20, max_bytes: u64::MAX, cut_granule_rows: 1024 };
        let mut w = open_writer_policy(vec![int8_col(1)], stamp(78, 1), policy);
        w.set_owner(Some(o));
        let n = 48 * 1024;
        append_int8_rows(&mut w, &mut vfs, &mut kit, n, |i| Some(scatter(i)));
        // the last append ended a granule: the column is synced, every word on the statement
        let cols = committed(o);
        assert!(cols >= n as i64 * 8, "committed {cols}");
        let mut sampler = WriteSampler { inner: &mut vfs, o, at_writes: Vec::new() };
        {
            let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv {
                vfs: &mut sampler,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            w.finish(&mut env).unwrap();
        }
        let (held, len) = sampler.at_writes.iter().copied().max_by_key(|&(_, len)| len).expect("the part was written");
        assert!(len as i64 > n as i64 * 6, "the image does not pack: {len}");
        assert!(held >= cols + len as i64, "the image is the statement's while it is written: {held} < {cols} + {len}");
        assert_eq!(committed(o), 0, "the image left with the seal");
        drop(w);
        drop(vfs);
        assert!((account(a) - base).abs() < SLACK, "the account is back: {} vs {base}", account(a));
    });
}
