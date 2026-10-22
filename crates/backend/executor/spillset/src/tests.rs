//! SpillSet unit suite (M3.5 inc-1, design §13). Harness mirrors
//! fd/src/tests.rs: seams + per-thread InitFileAccess + a scratch data dir
//! (temp files land under `base/pgsql_tmp` relative to cwd). The
//! cross-thread legs are the load-bearing ones: they prove the
//! write-on-thread-A / open-by-name-read-on-thread-B design against the
//! real thread-local VFD substrate.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Once};

use crate::{SpillFile, SpillSet};

static SETUP: Once = Once::new();
static WAL_SYNC_METHOD: AtomicI32 = AtomicI32::new(0);
// Serializes tests: they chdir into scratch data directories.
static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup_process() {
    SETUP.call_once(|| {
        guc_tables::init_seams();
        elog::init_seams();
        fd::init_seams();

        xact_seams::get_current_sub_transaction_id::set(|| 1);
        aio_seams::pgaio_closing_fd::set(|_| {});
        aio_seams::pgaio_io_start_readv::set(|_, _, _| Ok(()));
        waitevent_seams::pgstat_report_wait_start::set(|_| {});
        waitevent_seams::pgstat_report_wait_end::set(|| {});
        pgstat_seams::pgstat_report_tempfile::set(|_| {});
        guc_tables::vars::wal_sync_method.install(guc_tables::GucVarAccessors {
            get: || WAL_SYNC_METHOD.load(Ordering::Relaxed),
            set: |v| WAL_SYNC_METHOD.store(v, Ordering::Relaxed),
        });
        resowner::init_seams();
        ipc_seams::on_shmem_exit::set(|_cb, _arg| {});
        ipc_seams::before_shmem_exit::set(|_cb, _arg| Ok(()));
        postgres_seams::check_for_interrupts::set(|| Ok(()));
    });
}

/// Per-thread fd substrate: every thread that touches spill files needs its
/// own InitFileAccess + temp permission + a resource owner (the design's
/// §6.2 boundary, exercised for real here).
fn setup_thread() {
    fd::vfd::InitFileAccess();
    fd::vfd::InitTemporaryFileAccess().unwrap();
    let owner =
        resowner::ResourceOwnerCreate(types_resowner::ResourceOwner::NULL, "spillset-test")
            .unwrap();
    resowner_seams::set_current_resource_owner::call(owner);
}

/// A test's scratch data directory: reads as its path (`Display`), and is removed with everything below it when dropped
/// (the test's end, or a panic's unwind), so the suite leaves no pgrust_spillset_test_* in the temp dir.
struct Scratch(String);

impl std::fmt::Display for Scratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scratch_datadirs_leave_nothing_behind() {
    let (dir, cwd) = scratch_datadir("gone");
    std::fs::write(format!("{dir}/base/pgsql_tmp/f"), b"x").unwrap();
    let path = dir.0.clone();
    drop(dir);
    drop(cwd);
    assert!(!std::path::Path::new(&path).exists(), "{path} left behind");
}

fn scratch_datadir(tag: &str) -> (Scratch, std::sync::MutexGuard<'static, ()>) {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let guard = CWD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pgrust_spillset_test_{}_{tag}_{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("base/pgsql_tmp")).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    (Scratch(dir.to_str().unwrap().to_owned()), guard)
}

fn pattern(part: u32, epoch: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (part as usize * 131 + epoch as usize * 17 + i) as u8)
        .collect()
}

#[test]
fn epoch_roundtrip_multi_part_multi_epoch() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("roundtrip");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("agg", 1, 3), 8);

    // Epoch sizes cross the 8K BufFile buffer to exercise real writes.
    let sizes = [100usize, 9000, 40000];
    for (e, size) in sizes.iter().enumerate() {
        let mut w = f.begin_epoch(ctx.mcx()).unwrap();
        // Ascending parts; part 5 skipped in epoch 1 (absent partitions).
        for part in [0u32, 2, 5, 7] {
            if part == 5 && e == 1 {
                continue;
            }
            w.write_part(part, &pattern(part, e as u32, *size)).unwrap();
            // Same-part continuation extends the extent.
            w.write_part(part, &pattern(part, e as u32, 16)).unwrap();
        }
        w.finish().unwrap();
    }
    assert_eq!(f.epochs(), 3);
    let total: u64 = f.spilled_bytes();
    assert!(total > 0);

    for part in 0..8u32 {
        let expect: Vec<u8> = sizes
            .iter()
            .enumerate()
            .filter(|(e, _)| !(part == 5 && *e == 1))
            .flat_map(|(e, size)| {
                let mut v = pattern(part, e as u32, *size);
                v.extend(pattern(part, e as u32, 16));
                v
            })
            .collect();
        match part {
            0 | 2 | 5 | 7 => {
                assert_eq!(f.part_len(part), expect.len() as u64);
                let mut r = f.read_part(ctx.mcx(), part).unwrap().expect("has bytes");
                assert_eq!(r.total_len(), expect.len() as u64);
                let got = r.read_to_end().unwrap();
                r.close().unwrap();
                assert_eq!(got, expect, "part {part}");
            }
            _ => {
                assert_eq!(f.part_len(part), 0);
                assert!(f.read_part(ctx.mcx(), part).unwrap().is_none());
            }
        }
    }
}

/// gravity D3190: a part read from any byte offset is the part's bytes past it, across extents of several epochs
/// (offsets inside an extent, at an extent's start, at the end, past it).
#[test]
fn read_part_at_reads_the_bytes_past_any_offset() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("partat");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("run", 1, 0), 4);
    let sizes = [9000usize, 100, 40000];
    let mut whole = Vec::new();
    for (e, size) in sizes.iter().enumerate() {
        let mut w = f.begin_epoch(ctx.mcx()).unwrap();
        w.write_part(1, &pattern(9, e as u32, 33)).unwrap();
        w.write_part(2, &pattern(2, e as u32, *size)).unwrap();
        whole.extend(pattern(2, e as u32, *size));
        w.finish().unwrap();
    }
    let n = whole.len() as u64;
    for at in [0u64, 1, 8999, 9000, 9001, 9099, 9100, 9101, 30000, n - 1] {
        let mut r = f.read_part_at(ctx.mcx(), 2, at).unwrap().expect("bytes past the offset");
        let got = r.read_to_end().unwrap();
        r.close().unwrap();
        assert_eq!(got, whole[at as usize..], "from byte {at}");
    }
    assert!(f.read_part_at(ctx.mcx(), 2, n).unwrap().is_none(), "nothing past the end");
    assert!(f.read_part_at(ctx.mcx(), 2, n + 5).unwrap().is_none(), "nothing past the end");
    assert!(f.read_part_at(ctx.mcx(), 3, 0).unwrap().is_none(), "an empty part");
}

#[test]
fn abandoned_epoch_is_not_committed_and_tail_is_overwritten() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("abandon");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("agg", 1, 0), 4);

    let mut w = f.begin_epoch(ctx.mcx()).unwrap();
    w.write_part(0, &pattern(0, 0, 500)).unwrap();
    w.finish().unwrap();
    let committed = f.spilled_bytes();

    // Abandon one epoch mid-write (unwind shape: drop without finish).
    {
        let mut w = f.begin_epoch(ctx.mcx()).unwrap();
        w.write_part(0, &pattern(9, 9, 20_000)).unwrap();
        // dropped un-finished
    }
    assert_eq!(f.spilled_bytes(), committed, "abandoned epoch commits nothing");
    assert_eq!(f.epochs(), 1);

    // The next epoch lands at the committed offset, overwriting the tail.
    let mut w = f.begin_epoch(ctx.mcx()).unwrap();
    w.write_part(1, &pattern(1, 2, 700)).unwrap();
    w.finish().unwrap();

    let mut r = f.read_part(ctx.mcx(), 0).unwrap().unwrap();
    let got0 = r.read_to_end().unwrap();
    r.close().unwrap();
    assert_eq!(got0, pattern(0, 0, 500));
    let mut r = f.read_part(ctx.mcx(), 1).unwrap().unwrap();
    let got1 = r.read_to_end().unwrap();
    r.close().unwrap();
    assert_eq!(got1, pattern(1, 2, 700));
}

/// THE load-bearing leg: files written by two "worker" threads are read by
/// a third "combine" thread that never wrote — open-by-name against each
/// thread's own VFD cache (the §6.2 hazard exercised for real), with the
/// SpillFile structs moved across threads like sink Locals through SEAL.
#[test]
fn cross_thread_write_then_read_by_name() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("xthread");

    let set = SpillSet::create().unwrap();

    // Two writer threads, one file each (single-writer law).
    let files: Vec<SpillFile> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for worker in 0..2usize {
            let set = Arc::clone(&set);
            handles.push(s.spawn(move || {
                setup_thread(); // each thread arms its own fd substrate
                let ctx = mcx::MemoryContext::new("spillset-writer");
                let mut f =
                    SpillFile::new(set, SpillSet::file_name("dst", 7, worker), 4);
                for e in 0..2u32 {
                    let mut w = f.begin_epoch(ctx.mcx()).unwrap();
                    for part in 0..4u32 {
                        w.write_part(part, &pattern(part + worker as u32 * 100, e, 5000))
                            .unwrap();
                    }
                    w.finish().unwrap();
                }
                f // moves back like a Local through SEAL
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // A combine thread that never wrote reads every (file, part).
    std::thread::scope(|s| {
        let files = &files;
        s.spawn(move || {
            setup_thread();
            let ctx = mcx::MemoryContext::new("spillset-combine");
            for (worker, f) in files.iter().enumerate() {
                for part in 0..4u32 {
                    let mut expect = pattern(part + worker as u32 * 100, 0, 5000);
                    expect.extend(pattern(part + worker as u32 * 100, 1, 5000));
                    let mut r = f.read_part(ctx.mcx(), part).unwrap().unwrap();
                    let got = r.read_to_end().unwrap();
                    r.close().unwrap();
                    assert_eq!(got, expect, "worker {worker} part {part}");
                }
            }
        })
        .join()
        .unwrap();
    });
}

/// Teardown: dropping the engagement's SpillSet deletes the fileset
/// directories with every spill file in them (payload-drop semantics).
#[test]
fn spillset_drop_deletes_files() {
    setup_process();
    setup_thread();
    let (dir, _cwd) = scratch_datadir("teardown");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("agg", 1, 0), 2);
    let mut w = f.begin_epoch(ctx.mcx()).unwrap();
    w.write_part(0, &pattern(0, 0, 4096)).unwrap();
    w.finish().unwrap();

    let tmpdir = format!("{dir}/base/pgsql_tmp");
    let count_entries = || std::fs::read_dir(&tmpdir).map(|d| d.count()).unwrap_or(0);
    assert!(count_entries() > 0, "fileset dir exists while the set lives");

    drop(f);
    drop(set); // last Arc: FileSet::drop → delete_all
    assert_eq!(count_entries(), 0, "payload drop removed every spill file");
}

/// M3.5 join batches (inc-4): per-extent claims — `part_extents` enumerates
/// exactly the committed extents and `read_extent` streams each one alone,
/// concatenating to the full partition image.
#[test]
fn extent_claims_roundtrip() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("extents");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("hj-in", 0, 1), 4);
    // Three epochs; part 2 written in epochs 0 and 2 only.
    for e in 0..3u32 {
        let mut w = f.begin_epoch(ctx.mcx()).unwrap();
        w.write_part(1, &pattern(1, e, 3000)).unwrap();
        if e != 1 {
            w.write_part(2, &pattern(2, e, 12_000)).unwrap();
        }
        w.finish().unwrap();
    }
    let xs = f.part_extents(2).unwrap();
    assert_eq!(xs.len(), 2, "one extent per contributing epoch");
    let mut got = Vec::new();
    for x in &xs {
        let mut r = f.read_extent(ctx.mcx(), *x).unwrap();
        assert_eq!(r.total_len(), x.len);
        got.extend(r.read_to_end().unwrap());
        r.close().unwrap();
    }
    let mut expect = pattern(2, 0, 12_000);
    expect.extend(pattern(2, 2, 12_000));
    assert_eq!(got, expect, "extent claims concatenate to the partition image");
    // And they agree with the whole-partition reader.
    let mut r = f.read_part(ctx.mcx(), 2).unwrap().unwrap();
    assert_eq!(r.read_to_end().unwrap(), expect);
    r.close().unwrap();
}

/// Empty file: a SpillFile that never crossed a budget creates nothing and
/// reads as empty.
#[test]
fn never_spilled_file_is_absent() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("empty");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("agg", 1, 0), 4);
    assert_eq!(f.spilled_bytes(), 0);
    for part in 0..4 {
        assert!(f.read_part(ctx.mcx(), part).unwrap().is_none());
    }
}

fn epoch(mcx: mcx::Mcx<'_>, f: &mut SpillFile, len: usize) -> types_error::PgResult<()> {
    let mut w = f.begin_epoch(mcx)?;
    w.write_part(0, &pattern(0, 0, len))?;
    w.finish()
}

/// Descriptors this process holds open on a file whose path contains `name`.
#[cfg(target_os = "linux")]
fn open_descriptors_on(name: &str) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy().contains(name))
        .count()
}

/// D2570: a spill file counts against temp_file_limit until it is deleted, not only while an epoch has it open. At W8
/// a spill is split over the workers' files, each closed between epochs, so 53400 fired only when enough epochs
/// overlapped (load made it miss). Made deterministic: the leader's file A sits closed while a claim thread's file B,
/// charged to the leader's account, grows past the limit with it. A reopened for its next epoch is not charged twice;
/// B's failed epoch closes its file (it was left open on the pool thread); the deletes credit both.
#[test]
fn temp_file_limit_counts_a_closed_spill_file_once() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("d2570");
    let ctx = mcx::MemoryContext::new("spillset-test");
    let leader = fd::vfd::temp_account();
    let base = leader.load(Ordering::Relaxed);
    let charged = || leader.load(Ordering::Relaxed) - base;
    let saved = guc_tables::vars::temp_file_limit.read();
    guc_tables::vars::temp_file_limit.write(1024); // kB

    let set = SpillSet::create().unwrap();
    let mut a = SpillFile::new(Arc::clone(&set), SpillSet::file_name("d2570", 1, 0), 1);
    let b_name = SpillSet::file_name("d2570", 1, 1);
    let mut b = SpillFile::new(Arc::clone(&set), b_name.clone(), 1);
    epoch(ctx.mcx(), &mut a, 300_000).unwrap();
    epoch(ctx.mcx(), &mut a, 300_000).unwrap();
    assert_eq!(charged(), 600_000, "closed between its epochs, A stays charged, each byte once");
    epoch(ctx.mcx(), &mut a, 100_000).unwrap();
    assert_eq!(charged(), 700_000, "reopened, A's first 600 kB are not charged again (they would pass 1 MB)");

    std::thread::scope(|s| {
        s.spawn(|| {
            setup_thread();
            // a claim runs with the leader's temp_file_limit and charges the leader's account (hosts SessionCarry)
            let own_limit = guc_tables::vars::temp_file_limit.read();
            guc_tables::vars::temp_file_limit.write(1024);
            let prev = fd::vfd::charge_temp_to(Some(Arc::clone(&leader)));
            let claim = mcx::MemoryContext::new("spillset-claim");
            epoch(claim.mcx(), &mut b, 200_000).unwrap();
            let err = epoch(claim.mcx(), &mut b, 200_000).unwrap_err();
            assert_eq!(
                err.sqlstate(),
                types_error::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED,
                "closed A's 700 kB + B's 400 kB pass 1 MB"
            );
            #[cfg(target_os = "linux")]
            assert_eq!(open_descriptors_on(&b_name), 0, "the failed epoch closed its file");
            fd::vfd::charge_temp_to(prev);
            guc_tables::vars::temp_file_limit.write(own_limit);
        });
    });
    guc_tables::vars::temp_file_limit.write(saved);

    let b_bytes = charged() - 700_000;
    assert!(b_bytes >= 200_000, "B's committed epoch and the failed one's written blocks stay charged");
    a.delete().unwrap();
    assert_eq!(charged(), b_bytes, "A's delete credits it");
    b.delete().unwrap();
    assert_eq!(charged(), 0, "B's delete credits it");
}

/// spillopt #3: spans of 32 KiB and more go to the file without the 8 KB buffer (`BufFile::write_through`) and come back
/// straight into the reader's buffer (`read_through_exact`), interleaved with small spans that take the buffer; every
/// part reads back byte-exact whatever the reader's buffer size and start offset (small, unaligned, a buffer's
/// leftover then a long read, a read crossing extents of two epochs).
#[test]
fn long_spans_bypass_the_buffer_and_read_back_exact() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("through");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("agg", 2, 1), 4);
    // per epoch, per part: spans written one after another (small, long, unaligned long, small)
    // epoch 0 by `write_part`, the others by `write_parts` (epoch 3: 450 short spans, past PG_IOV_MAX per gathered write)
    let many = [500usize; 150];
    let spans: [&[usize]; 4] = [&[100, 40_000, 70_001, 9], &[65_536, 3, 32_768], &[8191, 8193, 200_000], &many];
    let mut expect: Vec<Vec<u8>> = vec![Vec::new(); 4];
    for (e, sizes) in spans.iter().enumerate() {
        let mut w = f.begin_epoch(ctx.mcx()).unwrap();
        let mut batch: Vec<(u32, Vec<u8>)> = Vec::new();
        for part in [0u32, 1, 3] {
            for (k, &n) in sizes.iter().enumerate() {
                let b = pattern(part, (e * 1000 + k) as u32, n);
                expect[part as usize].extend_from_slice(&b);
                if e == 0 {
                    w.write_part(part, &b).unwrap();
                } else {
                    batch.push((part, b));
                }
            }
        }
        let parts: Vec<(u32, &[u8])> = batch.iter().map(|(p, b)| (*p, b.as_slice())).collect();
        w.write_parts(&parts).unwrap();
        w.finish().unwrap();
    }
    assert_eq!(f.epochs(), 4);
    for part in 0..4u32 {
        let want = &expect[part as usize];
        assert_eq!(f.part_len(part), want.len() as u64, "part {part}");
        if want.is_empty() {
            assert!(f.read_part(ctx.mcx(), part).unwrap().is_none());
            continue;
        }
        for bufsize in [7usize, 8192, 33_000, 100_000, 1 << 20] {
            for start in [0u64, 1, 8191, 40_100, 110_101] {
                let Some(mut r) = f.read_part_at(ctx.mcx(), part, start).unwrap() else { panic!("part {part} past {start}") };
                let mut got = Vec::new();
                let mut buf = vec![0u8; bufsize];
                loop {
                    let n = r.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                r.close().unwrap();
                assert!(got == want[start as usize..], "part {part} buffer {bufsize} from {start}");
            }
        }
    }
}

/// gravity D4215: `read_each` asks before every piece past the first (one per extent), a false answer stops the read at
/// the end of the extent it read, and a read continued after a stop gives the rest: the part's bytes, whatever the buffer.
#[test]
fn read_each_asks_between_extents_and_a_stop_ends_the_read_at_an_extents_end() {
    setup_process();
    setup_thread();
    let (_dir, _cwd) = scratch_datadir("each");
    let ctx = mcx::MemoryContext::new("spillset-test");

    let set = SpillSet::create().unwrap();
    let mut f = SpillFile::new(Arc::clone(&set), SpillSet::file_name("run", 1, 0), 4);
    let sizes = [9000usize, 100, 40000];
    let mut whole = Vec::new();
    for (e, size) in sizes.iter().enumerate() {
        let mut w = f.begin_epoch(ctx.mcx()).unwrap();
        w.write_part(1, &pattern(9, e as u32, 33)).unwrap();
        w.write_part(2, &pattern(2, e as u32, *size)).unwrap();
        whole.extend(pattern(2, e as u32, *size));
        w.finish().unwrap();
    }
    let mut buf = vec![0u8; 65536];

    let mut r = f.read_part(ctx.mcx(), 2).unwrap().expect("has bytes");
    let mut asks = 0;
    let got = r.read_each(&mut buf, &mut || {
        asks += 1;
        true
    });
    assert_eq!(got.unwrap(), (whole.len(), false));
    assert_eq!(asks, 2, "one ask before each extent past the first");
    assert_eq!(buf[..whole.len()], whole[..]);
    r.close().unwrap();

    // stopped at every ask: one extent a call, the part's bytes in order
    let mut r = f.read_part(ctx.mcx(), 2).unwrap().expect("has bytes");
    let (mut got, mut calls) = (Vec::new(), Vec::new());
    loop {
        let (n, stopped) = r.read_each(&mut buf, &mut || false).unwrap();
        if n == 0 {
            assert!(!stopped, "the end is no stop");
            break;
        }
        calls.push((n, stopped));
        got.extend_from_slice(&buf[..n]);
    }
    r.close().unwrap();
    assert_eq!(calls, vec![(9000, true), (100, true), (40000, false)]);
    assert_eq!(got, whole);

    // a buffer shorter than an extent: no ask inside an extent
    let mut r = f.read_part(ctx.mcx(), 2).unwrap().expect("has bytes");
    let mut asks = 0;
    let got = r.read_each(&mut buf[..5000], &mut || {
        asks += 1;
        false
    });
    assert_eq!(got.unwrap(), (5000, false));
    assert_eq!(asks, 0);
    assert_eq!(buf[..5000], whole[..5000]);
    r.close().unwrap();
}
