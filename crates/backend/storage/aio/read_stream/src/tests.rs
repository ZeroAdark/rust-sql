// A fake buffer manager behind the bufmgr seams: buffer id = block + 1,
// per-buffer pin counts, a resident set (hits), "busy" blocks that make
// AsyncReadBuffers shorten a run (forwarding the pinned tail), and an IO log.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

use bufmgr_seams::ReadBuffersOperation;
use types_core::{BlockNumber, Buffer, ForkNumber, InvalidBlockNumber, InvalidBuffer};
use types_error::PgResult;
use types_storage::{RelFileLocator, RelFileLocatorBackend};

use super::*;

#[derive(Default)]
struct Fake {
    pins: HashMap<Buffer, i32>,
    resident: HashSet<BlockNumber>,
    busy: HashSet<BlockNumber>,
    ios: Vec<(BlockNumber, i32)>,
    waits: u32,
    advice: u32,
    max_pins_seen: i32,
}

static FAKE: Mutex<Option<Fake>> = Mutex::new(None);
static TEST_LOCK: Mutex<()> = Mutex::new(());
static EIC: AtomicI32 = AtomicI32::new(2);
static COMBINE: AtomicI32 = AtomicI32::new(4);
static PIN_LIMIT: AtomicU32 = AtomicU32::new(1000);
static ADDITIONAL: AtomicU32 = AtomicU32::new(1000);
static IN_BATCH: AtomicBool = AtomicBool::new(false);

fn with<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
    f(FAKE.lock().unwrap().as_mut().unwrap())
}

fn pin(f: &mut Fake, b: Buffer) {
    *f.pins.entry(b).or_insert(0) += 1;
    let total: i32 = f.pins.values().sum();
    f.max_pins_seen = f.max_pins_seen.max(total);
}

fn fake_start(
    op: &mut ReadBuffersOperation,
    buffers: &mut [Buffer],
    blocknum: BlockNumber,
    nblocks: &mut i32,
    flags: u32,
) -> PgResult<bool> {
    with(|f| {
        let requested = *nblocks;
        let mut actual = requested;
        for i in 0..requested {
            let blk = blocknum + i as BlockNumber;
            let found;
            if buffers[i as usize] != InvalidBuffer {
                assert_eq!(buffers[i as usize], blk as Buffer + 1, "forwarded buffer out of place");
                found = f.resident.contains(&blk);
            } else {
                pin(f, blk as Buffer + 1);
                buffers[i as usize] = blk as Buffer + 1;
                found = f.resident.contains(&blk);
            }
            if found {
                if i == 0 {
                    *nblocks = 1;
                    return Ok(false);
                }
                actual = i;
                break;
            }
        }
        for i in 1..actual {
            if f.busy.remove(&(blocknum + i as BlockNumber)) {
                actual = i;
                break;
            }
        }
        if flags & READ_BUFFERS_ISSUE_ADVICE != 0 {
            f.advice += 1;
        }
        *nblocks = actual;
        op.blocknum = blocknum;
        op.nblocks = actual as i16;
        op.nblocks_done = 0;
        f.ios.push((blocknum, actual));
        Ok(true)
    })
}

fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        bufmgr_seams::start_read_buffers::set(|op, _rel, buffers, blocknum, nblocks, flags| {
            fake_start(op, buffers, blocknum, nblocks, flags)
        });
        bufmgr_seams::start_read_buffer::set(|op, _rel, buffer, blocknum, flags| {
            let mut one = [InvalidBuffer];
            let mut n = 1;
            let r = fake_start(op, &mut one, blocknum, &mut n, flags);
            *buffer = one[0];
            r
        });
        bufmgr_seams::wait_read_buffers::set(|op, _rel| {
            with(|f| {
                f.waits += 1;
                for i in 0..op.nblocks as BlockNumber {
                    f.resident.insert(op.blocknum + i);
                }
            });
            Ok(())
        });
        bufmgr_seams::release_buffer::set(|b| {
            with(|f| {
                let c = f.pins.get_mut(&b).expect("release of an unpinned buffer");
                assert!(*c > 0, "release of an unpinned buffer {b}");
                *c -= 1;
            });
            Ok(())
        });
        bufmgr_seams::io_combine_limit::set(|| COMBINE.load(Ordering::Relaxed));
        bufmgr_seams::get_pin_limit::set(|| PIN_LIMIT.load(Ordering::Relaxed));
        bufmgr_seams::get_additional_pin_limit::set(|| ADDITIONAL.load(Ordering::Relaxed));
        bufmgr_seams::get_local_pin_limit::set(|| PIN_LIMIT.load(Ordering::Relaxed));
        bufmgr_seams::get_additional_local_pin_limit::set(|| ADDITIONAL.load(Ordering::Relaxed));
        bufmgr_seams::get_access_strategy_pin_limit::set(|_| i32::MAX);
        aio_seams::pgaio_enter_batchmode::set(|| {
            if IN_BATCH.swap(true, Ordering::Relaxed) {
                return Err(Box::new(types_error::PgError::error("starting batch while batch already in progress")));
            }
            Ok(())
        });
        aio_seams::pgaio_exit_batchmode::set(|| {
            IN_BATCH.store(false, Ordering::Relaxed);
            Ok(())
        });
        guc_tables::vars::effective_io_concurrency.install_if_absent(
            guc_tables::GucVarAccessors { get: || EIC.load(Ordering::Relaxed), set: |v| EIC.store(v, Ordering::Relaxed) },
        );
    });
}

fn setup(eic: i32, combine: i32, resident: &[BlockNumber]) -> MutexGuard<'static, ()> {
    let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install();
    reset_fake(eic, combine, resident);
    g
}

fn reset_fake(eic: i32, combine: i32, resident: &[BlockNumber]) {
    EIC.store(eic, Ordering::Relaxed);
    COMBINE.store(combine, Ordering::Relaxed);
    PIN_LIMIT.store(1000, Ordering::Relaxed);
    ADDITIONAL.store(1000, Ordering::Relaxed);
    IN_BATCH.store(false, Ordering::Relaxed);
    let mut f = Fake::default();
    f.resident.extend(resident.iter().copied());
    *FAKE.lock().unwrap() = Some(f);
}

fn assert_no_pins() {
    with(|f| {
        for (b, c) in &f.pins {
            assert_eq!(*c, 0, "buffer {b} still pinned");
        }
    });
}

fn smgr() -> RelFileLocatorBackend {
    RelFileLocatorBackend {
        locator: RelFileLocator { spcOid: 1663, dbOid: 5, relNumber: 16384 },
        backend: types_core::INVALID_PROC_NUMBER,
    }
}

fn begin<D: Copy + Default>(flags: i32) -> ReadStream<'static, D> {
    let cx = mcx::MemoryContext::new_wholesale("read stream test");
    read_stream_begin_smgr_relation(
        cx.mcx(),
        flags,
        None,
        smgr(),
        types_core::RELPERSISTENCE_PERMANENT,
        ForkNumber::MAIN_FORKNUM,
    )
    .unwrap()
}

fn drain<D: Copy + Default>(stream: &mut ReadStream<'_, D>, blocks: &[BlockNumber]) -> Vec<BlockNumber> {
    let mut it = blocks.iter().copied();
    let mut cb = |_: &mut D| -> PgResult<BlockNumber> { Ok(it.next().unwrap_or(InvalidBlockNumber)) };
    let mut out = Vec::new();
    loop {
        let b = stream.next_buffer(&mut cb).unwrap();
        if b == InvalidBuffer {
            break;
        }
        out.push(b as BlockNumber - 1);
        bufmgr_seams::release_buffer::call(b).unwrap();
    }
    out
}

#[test]
fn all_cached_takes_the_fast_path_without_io() {
    let blocks: Vec<BlockNumber> = (0..50).collect();
    let _g = setup(2, 4, &blocks);
    let mut s = begin::<()>(READ_STREAM_DEFAULT);
    let mut it = blocks.iter().copied();
    let mut cb = |_: &mut ()| -> PgResult<BlockNumber> { Ok(it.next().unwrap_or(InvalidBlockNumber)) };
    let first = s.next_buffer(&mut cb).unwrap();
    assert_eq!(first, 1);
    bufmgr_seams::release_buffer::call(first).unwrap();
    assert_eq!(s.state(), (1, 1, 0, 0, true));
    let mut n = 1;
    loop {
        let b = s.next_buffer(&mut cb).unwrap();
        if b == InvalidBuffer {
            break;
        }
        assert_eq!(b, n + 1);
        n += 1;
        bufmgr_seams::release_buffer::call(b).unwrap();
    }
    assert_eq!(n, 50);
    assert!(with(|f| f.ios.is_empty()));
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn sequential_misses_ramp_up_and_combine() {
    let _g = setup(2, 4, &[]);
    let blocks: Vec<BlockNumber> = (0..40).collect();
    let mut s = begin::<()>(READ_STREAM_DEFAULT);
    assert_eq!(s.max_pinned(), (2 + 1) * 4);
    assert_eq!(drain(&mut s, &blocks), blocks);
    let ios = with(|f| f.ios.clone());
    assert_eq!(ios[0], (0, 1));
    assert_eq!(ios[1], (1, 2));
    assert!(ios.iter().all(|&(_, n)| n <= 4));
    assert!(ios.iter().filter(|&&(_, n)| n == 4).count() >= 7, "{ios:?}");
    let mut next = 0;
    for &(b, n) in &ios {
        assert_eq!(b, next);
        next += n as BlockNumber;
    }
    assert_eq!(next, 40);
    assert!(with(|f| f.max_pins_seen) <= 12 + 1);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn full_flag_starts_with_full_size_reads() {
    let _g = setup(2, 8, &[]);
    let blocks: Vec<BlockNumber> = (0..24).collect();
    let mut s = begin::<()>(READ_STREAM_FULL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    assert_eq!(with(|f| f.ios[0]), (0, 8));
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn hit_splits_a_run_and_is_forwarded() {
    let _g = setup(4, 8, &[2]);
    let blocks: Vec<BlockNumber> = (0..16).collect();
    let mut s = begin::<()>(READ_STREAM_FULL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    let ios = with(|f| f.ios.clone());
    assert_eq!(ios[0], (0, 2));
    assert_eq!(ios[1], (3, 8));
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn busy_block_forwards_pinned_tail() {
    let _g = setup(4, 8, &[]);
    with(|f| {
        f.busy.insert(3);
    });
    let blocks: Vec<BlockNumber> = (0..16).collect();
    let mut s = begin::<()>(READ_STREAM_FULL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    let ios = with(|f| f.ios.clone());
    assert_eq!(ios[0], (0, 3));
    assert_eq!(ios[1], (3, 8));
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn per_buffer_data_follows_its_block() {
    let _g = setup(2, 4, &[5, 6, 7]);
    let blocks: Vec<BlockNumber> = vec![9, 1, 2, 3, 5, 6, 7, 30, 31, 12, 13, 14, 15, 16, 40];
    let mut s = begin::<u64>(READ_STREAM_DEFAULT);
    let mut it = blocks.iter().copied();
    let mut cb = |d: &mut u64| -> PgResult<BlockNumber> {
        let b = it.next().unwrap_or(InvalidBlockNumber);
        *d = b as u64 * 10 + 7;
        Ok(b)
    };
    let mut got = Vec::new();
    loop {
        let (b, d) = s.next_buffer_with_data(&mut cb).unwrap();
        if b == InvalidBuffer {
            break;
        }
        let blk = b as BlockNumber - 1;
        assert_eq!(d, blk as u64 * 10 + 7);
        got.push(blk);
        bufmgr_seams::release_buffer::call(b).unwrap();
    }
    assert_eq!(got, blocks);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn random_order_is_preserved_and_limited_by_max_ios() {
    let _g = setup(1, 4, &[]);
    let blocks: Vec<BlockNumber> = vec![50, 3, 90, 4, 5, 6, 7, 8, 9, 100, 2, 70, 71, 72];
    let mut s = begin::<()>(READ_STREAM_FULL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn queue_wraps_through_the_overflow_zone() {
    let _g = setup(1, 5, &[17, 44]);
    let blocks: Vec<BlockNumber> = (0..97).collect();
    let mut s = begin::<u32>(READ_STREAM_FULL);
    let mut it = blocks.iter().copied();
    let mut cb = |d: &mut u32| -> PgResult<BlockNumber> {
        let b = it.next().unwrap_or(InvalidBlockNumber);
        *d = b;
        Ok(b)
    };
    let mut got = Vec::new();
    loop {
        let (b, d) = s.next_buffer_with_data(&mut cb).unwrap();
        if b == InvalidBuffer {
            break;
        }
        assert_eq!(d, b as u32 - 1);
        got.push(b as BlockNumber - 1);
        bufmgr_seams::release_buffer::call(b).unwrap();
    }
    assert_eq!(got, blocks);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn zero_additional_pins_still_makes_progress() {
    let _g = setup(4, 8, &[]);
    ADDITIONAL.store(0, Ordering::Relaxed);
    let blocks: Vec<BlockNumber> = (0..20).collect();
    let mut s = begin::<()>(READ_STREAM_FULL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    assert!(with(|f| f.ios.iter().all(|&(_, n)| n == 1)));
    assert!(with(|f| f.max_pins_seen) <= 2);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn reset_releases_lookahead_and_restarts() {
    let _g = setup(4, 4, &[]);
    let mut s = begin::<()>(READ_STREAM_FULL);
    let mut it = 0..;
    let mut cb = |_: &mut ()| -> PgResult<BlockNumber> { Ok(it.next().unwrap()) };
    for want in 0..6 {
        let b = s.next_buffer(&mut cb).unwrap();
        assert_eq!(b as BlockNumber - 1, want);
        bufmgr_seams::release_buffer::call(b).unwrap();
    }
    s.reset().unwrap();
    assert_no_pins();
    assert_eq!(s.state().0, 1);
    let blocks: Vec<BlockNumber> = vec![200, 201, 202];
    assert_eq!(drain(&mut s, &blocks), blocks);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn drop_mid_stream_releases_pins() {
    let _g = setup(4, 4, &[3]);
    let mut s = begin::<()>(READ_STREAM_FULL);
    let mut it = 0..;
    let mut cb = |_: &mut ()| -> PgResult<BlockNumber> { Ok(it.next().unwrap()) };
    let b = s.next_buffer(&mut cb).unwrap();
    bufmgr_seams::release_buffer::call(b).unwrap();
    assert!(with(|f| f.pins.values().sum::<i32>()) > 0);
    drop(s);
    assert_no_pins();
}

#[test]
fn callback_error_leaves_batch_mode_and_end_drains() {
    let _g = setup(4, 4, &[]);
    let mut s = begin::<()>(READ_STREAM_FULL | READ_STREAM_USE_BATCHING);
    let mut it = [Ok(0), Ok(10)].into_iter();
    let mut cb = |_: &mut ()| -> PgResult<BlockNumber> {
        it.next().unwrap_or_else(|| Err(Box::new(types_error::PgError::error("canceling statement due to user request"))))
    };
    assert!(s.next_buffer(&mut cb).is_err());
    assert!(!IN_BATCH.load(Ordering::Relaxed), "the failed look-ahead left batch mode on");
    assert!(s.state().1 > 0);
    s.end().unwrap();
    assert_no_pins();
}

#[test]
fn advice_only_for_random_jumps_under_sync() {
    let _g = setup(4, 4, &[]);
    let blocks: Vec<BlockNumber> = vec![0, 1, 2, 3, 100, 101, 102, 103, 500, 900];
    let mut s = begin::<()>(READ_STREAM_FULL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    assert!(with(|f| f.advice) >= 2);
    s.end().unwrap();
    assert_no_pins();

    reset_fake(4, 4, &[]);
    let mut s = begin::<()>(READ_STREAM_FULL | READ_STREAM_SEQUENTIAL);
    assert_eq!(drain(&mut s, &blocks), blocks);
    assert_eq!(with(|f| f.advice), 0);
    s.end().unwrap();
}

#[test]
fn block_range_callback() {
    let mut p = BlockRangeReadStreamPrivate::new(3, 6);
    assert_eq!(
        [p.next_block(), p.next_block(), p.next_block(), p.next_block()],
        [3, 4, 5, InvalidBlockNumber]
    );
}
