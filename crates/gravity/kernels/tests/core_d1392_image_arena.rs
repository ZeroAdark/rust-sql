//! D1392 (a): a keyed table's image arena (a kept varlena key's copy, a text extreme's: `agg_image_put`, lowerv4's
//! `key_copy` through `GroupRec::images`) lives with its worker's T1 (`T1::img_words`), as long as its chunks. `agg_open`
//! runs at every claim over a claim-lived state slot; the cursor lived in that state and was dropped each time, so every
//! claim's first new record pinned a fresh 64 KiB chunk (TPC-H Q12 SF10 at W8: 88,807 claims, 17 GB of statement pins,
//! the pool refused). Each claim here opens over a FRESH zeroed state, as the server's frame gives it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::thread::ThreadId;

use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind, T1};
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::partial;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::table::entries::{agg_locate, agg_open_grown};
use gravity_kernels::host_abi::{agg_copies, agg_image_put, agg_open, AGG_STATE_BYTES};

const SHARE: u32 = 1 << 20;
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
const CHUNK: usize = 64 << 10;

/// The pins by test thread: a process static keyed by thread (no thread-local block: the session TLS census counts those).
static PINS: Mutex<Option<HashMap<ThreadId, Vec<Vec<u64>>>>> = Mutex::new(None);

/// The program's pin: a zeroed region kept to the thread's end (a statement's pins), counted.
unsafe extern "C" fn test_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
    let mut v = vec![0u64; (bytes as usize).div_ceil(8)];
    let d = v.as_mut_ptr() as *mut u8;
    PINS.lock().unwrap().get_or_insert_with(HashMap::new).entry(std::thread::current().id()).or_default().push(v);
    d
}

fn pins() -> usize {
    PINS.lock().unwrap().as_ref().and_then(|m| m.get(&std::thread::current().id())).map_or(0, Vec::len)
}

/// A bound ctx over one armed share region (a worker's own partial), its program pinning through `test_pin`.
struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    own: Vec<u64>,
}

impl Bound {
    fn new() -> Bound {
        let mut own = vec![0u64; SHARE as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        program.pin = Some(test_pin);
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, own }
    }

    fn ctx(&mut self) -> *mut Ctx {
        self.ctx.as_mut_ptr() as *mut Ctx
    }

    fn t1(&mut self) -> *mut T1 {
        unsafe { partial(self.ctx()) as *mut T1 }
    }

    /// The next section over the same region (a rescan, a re-execution): its header zeroed and armed again, as the
    /// driver arms a share.
    fn next_section(&mut self) {
        self.own.fill(0);
        unsafe { HashAgg::arm(self.own.as_mut_ptr() as *mut u8, SHARE) };
    }
}

/// One claim: its open over a fresh state slot, one image copied (a new group's key); the image's address.
unsafe fn claim(b: &mut Bound, text: &[u8]) -> usize {
    claim_with(b, text, false)
}

/// `claim`, opened as table.sized opens (`agg_open_grown`: laid out at a small bound with room to grow in place) when `sized`.
unsafe fn claim_with(b: &mut Bound, text: &[u8], sized: bool) -> usize {
    let mut st = vec![0u64; AGG_STATE_BYTES as usize / 8 + 2];
    let ctx = b.ctx();
    let r = if sized { agg_open_grown(ctx, st.as_mut_ptr() as *mut u8, &LAYOUT, 0, 16) } else { agg_open(ctx, st.as_mut_ptr() as *mut u8, &LAYOUT, 0, 0) };
    assert_eq!(r, 0);
    let d = agg_image_put(ctx, st.as_mut_ptr() as *mut u8, text.as_ptr(), text.len() as u64);
    assert_ne!(d, 0, "the image is copied");
    d as usize
}

fn text(i: usize) -> &'static [u8] {
    if i % 2 == 0 { b"MAIL" } else { b"SHIP" }
}

#[test]
fn the_claims_of_one_section_share_the_image_arena() {
    let mut b = Bound::new();
    let before = pins();
    let addrs: Vec<usize> = (0..1000).map(|i| unsafe { claim(&mut b, text(i)) }).collect();
    assert_eq!(pins() - before, 1, "1000 claims of one section copy 1000 images into one 64 KiB chunk, not a chunk per claim");
    assert!(addrs.windows(2).all(|w| w[1] == w[0] + 8), "each image follows the last in the one chunk");
    let w = unsafe { T1::img_words(b.t1()) };
    assert_eq!(unsafe { *w.add(2) }, CHUNK as u64, "the table's words hold the open chunk's size");
}

#[test]
fn a_full_chunk_doubles_and_the_next_claims_keep_the_doubled_one() {
    let mut b = Bound::new();
    let before = pins();
    // a 64 KiB chunk holds 8,060 images of 8 bytes past its header and start map (D3312); the next chunk is 128 KiB
    for i in 0..8192 + 100 {
        unsafe { claim(&mut b, text(i)) };
    }
    assert_eq!(pins() - before, 2);
    assert_eq!(unsafe { *T1::img_words(b.t1()).add(2) }, 2 * CHUNK as u64);
}

#[test]
fn a_grown_table_keeps_its_image_cursor() {
    let mut b = Bound::new();
    let before = pins();
    let first = unsafe { claim_with(&mut b, b"MAIL", true) };
    let t = b.t1();
    let words = unsafe { (*T1::img_words(t), *T1::img_words(t).add(1), *T1::img_words(t).add(2)) };
    let cap = unsafe { (*t).cap };
    // table.sized's in-place grow (a full home pair doubles the slots): the header's words untouched
    assert!(unsafe { T1::grow(t) }, "a table laid out at 16 groups has room to grow");
    assert_eq!(unsafe { (*t).cap }, 2 * cap);
    assert_eq!(unsafe { (*T1::img_words(t), *T1::img_words(t).add(1), *T1::img_words(t).add(2)) }, words, "the grow keeps the cursor");
    let next = unsafe { claim_with(&mut b, b"SHIP", true) };
    assert_eq!((pins() - before, next), (1, first + 8), "the claim after the grow bumps the same chunk");
}

#[test]
fn a_spilled_table_keeps_its_image_cursor() {
    let mut b = Bound::new();
    let before = pins();
    let first = unsafe { claim(&mut b, b"MAIL") };
    // a flush empties the pool (reset_pool), a spill clears the table (clear): the records' images stay pinned and the
    // cursor stays with the table
    unsafe {
        (*b.t1()).reset_pool();
        (*b.t1()).clear();
    }
    let next = unsafe { claim(&mut b, b"SHIP") };
    assert_eq!((pins() - before, next), (1, first + 8), "the claim after the spill bumps the same chunk");
}

#[test]
fn the_next_section_over_the_same_region_starts_a_fresh_arena() {
    let mut b = Bound::new();
    let before = pins();
    let first = unsafe { claim(&mut b, b"MAIL") };
    unsafe { claim(&mut b, b"SHIP") };
    assert_eq!(pins() - before, 1);
    // a rescan / re-execution: the same program and region, a new section (the table laid out again)
    b.next_section();
    let again = unsafe { claim(&mut b, b"MAIL") };
    assert_eq!(pins() - before, 2, "the new section's table opens its own arena, never the last section's cursor");
    assert!(again < first || again >= first + CHUNK, "the image is in the new chunk, not past the old cursor");
}

/// D3312: a record of one bigint key, its text kept in its MinCode slot (as lowerv4 `keep_datums` keeps a key's copy), a count.
const COPY_LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinCode, SlotKind::Count]);

fn key_text(k: u64) -> Vec<u8> {
    format!("key-{k:08}{}", "x".repeat(24)).into_bytes()
}

fn pinned_bytes() -> usize {
    PINS.lock().unwrap().as_ref().and_then(|m| m.get(&std::thread::current().id())).map_or(0, |v| v.iter().map(|p| 8 * p.len()).sum())
}

/// D3312: 100 turns of 8,000 keys through one claim's table opened at 100 groups (1,024 slots: nearly every row evicts and
/// re-opens its key), each fresh record copying its key's text into the arena (`agg_image_put`) and keeping the copy in
/// its MinCode slot; with `named` the slot is named (`agg_copies`). Every record's copy is then checked against its key,
/// and the counts against the rows. The bytes pinned.
fn cycle(named: bool) -> usize {
    use gravity_abi::breaker::{IMG_SLOTS_KNOWN, IMG_SLOT_PLAIN, MINCODE_NONE};
    let mut b = Bound::new();
    let before = pinned_bytes();
    let mut st = vec![0u64; AGG_STATE_BYTES as usize / 8 + 2];
    let (ctx, sp) = (b.ctx(), st.as_mut_ptr() as *mut u8);
    let (rep, count) = (COPY_LAYOUT.slot_off(0) as usize, COPY_LAYOUT.slot_off(1) as usize);
    unsafe {
        assert_eq!(agg_open(ctx, sp, &COPY_LAYOUT, 0, 100), 0);
        if named {
            agg_copies(sp, IMG_SLOTS_KNOWN | IMG_SLOT_PLAIN);
        }
        for _ in 0..100 {
            for i in 0..8000u64 {
                let key = i * 7919 % 8000;
                let r = agg_locate(ctx, sp, &key);
                assert!(r > 0, "located: {r}");
                let r = r as usize as *mut u8;
                if (r.add(rep) as *const u64).read_unaligned() == MINCODE_NONE {
                    let t = key_text(key);
                    let d = agg_image_put(ctx, sp, t.as_ptr(), t.len() as u64);
                    assert_ne!(d, 0, "the image is copied");
                    (r.add(rep) as *mut u64).write_unaligned(d as u64);
                }
                *(r.add(count) as *mut u64) += 1;
            }
        }
        let t = &*b.t1();
        let rec = COPY_LAYOUT.size() as usize;
        let mut recs: Vec<*mut u8> = t.records().map(|r| r.as_ptr()).collect();
        for part in 0..t.parts() as u32 {
            for blk in t.chain(part) {
                recs.extend((0..blk.recs).map(|i| blk.ptr.add(i * rec) as *mut u8));
            }
        }
        let mut rows = 0u64;
        for &r in &recs {
            let key = (r as *const u64).read_unaligned();
            let d = (r.add(rep) as *const u64).read_unaligned() as usize as *const u8;
            let want = key_text(key);
            assert_eq!((d as *const u32).read_unaligned() as usize >> 2, want.len() + 4, "key {key}: the copy's header (named {named})");
            assert_eq!(std::slice::from_raw_parts(d.add(4), want.len()), want.as_slice(), "key {key}: the copy's bytes (named {named})");
            rows += (r.add(count) as *const u64).read_unaligned();
        }
        assert_eq!(rows, 800_000, "every row counted once (named {named})");
    }
    pinned_bytes() - before
}

/// D3312: an in-memory compact keeps one record a key; the copies of the folded ones are dead, and with the copy slot named
/// the reclaim slides the live ones down the arena, which then reuses its chunks: the pins stay flat in the rows. Without
/// it (the control) the arena grows with every re-open.
#[test]
fn an_in_memory_compact_reclaims_the_dead_key_copies() {
    let (named, control) = (cycle(true), cycle(false));
    assert!(named <= 6 << 20, "the named copy slot's arena: {named} bytes pinned (control {control})");
    assert!(control >= 4 * named, "the control's arena grows with the rows: {control} bytes pinned (named {named})");
}
