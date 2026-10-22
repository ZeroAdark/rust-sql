//! C1 L5a: the residency plane's memory law (`crate::mem`).
//!
//! The CLOCK core runs over test victims, deterministically. The faces run over real parts through the process
//! ring, serialized by `LOCK` because they set the cap. They assert only what concurrently running tests cannot
//! move: other tests hold images and make them sticky, but only these tests admit (Cached).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use pgrc_format::abi::{ByteArena, DecodeOut};
use pgrc_format::class::StorageClass;
use pgrc_format::part::{ExtentRecord, StreamEntry, StreamRole};

use crate::cursor::{reference_binding_leaked, Residency, StreamCursor};
use crate::mem::{self, Clock, Guard, Sweep, Victim, CACHED, FUTILE_SKIP, HELD, STICKY};
use crate::openpart::{OpenPart, SegBuf};
use crate::testpart::{build_part, seq_i64_col, text_col, BuiltPart, CellValue, ColSpec, DictSpec, PartSpec};

use super::decode_props::assert_granule;
use super::{open_built, ArenaBuf};

static LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sets the process cap for one test and restores it after.
struct Cap(u64);

impl Cap {
    fn set(bytes: u64) -> Cap {
        let old = mem::cap();
        mem::set_cap(bytes);
        Cap(old)
    }
}

impl Drop for Cap {
    fn drop(&mut self) {
        mem::set_cap(self.0);
    }
}

// ---------------------------------------------------------------------------
// the CLOCK core over test victims
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Tv {
    present: bool,
    sticky: bool,
    refbit: bool,
    pinned: bool,
    bytes: u64,
    visits: u32,
}

struct TvSlot(Arc<Mutex<Tv>>);

impl Victim for TvSlot {
    fn visit(&self) -> Sweep {
        let mut v = self.0.lock().unwrap();
        v.visits += 1;
        if !v.present {
            return Sweep::Dead;
        }
        if v.sticky {
            return Sweep::Sticky;
        }
        if std::mem::take(&mut v.refbit) {
            return Sweep::Referenced;
        }
        if v.pinned {
            return Sweep::Pinned;
        }
        v.present = false;
        Sweep::Evicted(v.bytes)
    }

    fn dead(&self) -> bool {
        let v = self.0.lock().unwrap();
        !v.present || v.sticky
    }
}

fn tvs(n: usize, bytes: u64) -> Vec<Arc<Mutex<Tv>>> {
    (0..n).map(|_| Arc::new(Mutex::new(Tv { present: true, bytes, ..Tv::default() }))).collect()
}

fn ring_of(v: &[Arc<Mutex<Tv>>]) -> Clock {
    let mut c = Clock::new();
    for t in v {
        c.register(Box::new(TvSlot(t.clone())));
    }
    c
}

fn used(v: &[Arc<Mutex<Tv>>]) -> u64 {
    v.iter().map(|t| t.lock().unwrap()).filter(|t| t.present).map(|t| t.bytes).sum()
}

fn present(v: &[Arc<Mutex<Tv>>]) -> Vec<bool> {
    v.iter().map(|t| t.lock().unwrap().present).collect()
}

#[test]
fn clock_gives_a_second_chance_and_keeps_its_hand() {
    let v = tvs(4, 10);
    let mut c = ring_of(&v[..3]);
    v[0].lock().unwrap().refbit = true;
    // 30 bytes over a cap of 20: A's bit is cleared, B goes.
    assert!(c.sweep(&|| used(&v[..3]) > 20));
    assert_eq!(present(&v), [true, false, true, true]);
    assert!(!v[0].lock().unwrap().refbit, "a referenced entry loses its bit, not its place");
    // D arrives: the hand resumes at C, not at A.
    c.register(Box::new(TvSlot(v[3].clone())));
    assert!(c.sweep(&|| used(&v) > 20));
    assert_eq!(present(&v), [true, false, false, true]);
    assert_eq!((c.evicted, c.evicted_bytes, c.slots()), (2, 20, 2));
}

#[test]
fn clock_never_evicts_pinned_or_sticky_entries() {
    let v = tvs(3, 10);
    let mut c = ring_of(&v);
    v[0].lock().unwrap().pinned = true;
    v[1].lock().unwrap().sticky = true;
    assert!(!c.sweep(&|| used(&v) > 0), "cap 0 cannot be met while A is pinned");
    assert_eq!(present(&v), [true, true, false]);
    assert_eq!(c.slots(), 1, "the sticky slot is freed; the pinned one stays");
}

#[test]
fn a_futile_sweep_backs_off_for_64_admissions() {
    let v = tvs(2, 10);
    let mut c = ring_of(&v);
    v.iter().for_each(|t| t.lock().unwrap().pinned = true);
    let visits = || v.iter().map(|t| t.lock().unwrap().visits).sum::<u32>();
    assert!(!c.sweep(&|| true));
    assert_eq!((visits(), c.futile), (4, 1), "two turns, nothing freed");
    for _ in 0..FUTILE_SKIP {
        assert!(!c.sweep(&|| true));
    }
    assert_eq!(visits(), 4, "backed off: no visits");
    v[1].lock().unwrap().pinned = false;
    assert!(c.sweep(&|| used(&v) > 10), "the backoff spent, the sweep runs again");
    assert_eq!(present(&v), [true, false]);
}

#[test]
fn the_janitor_frees_dead_slots_on_admission() {
    let v = tvs(10, 1);
    let mut c = ring_of(&v);
    v.iter().for_each(|t| t.lock().unwrap().present = false);
    for t in tvs(5, 1) {
        c.register(Box::new(TvSlot(t)));
    }
    assert_eq!((c.slots(), c.admitted), (5, 15), "five admissions free ten dead slots");
}

// ---------------------------------------------------------------------------
// the faces over real parts
// ---------------------------------------------------------------------------

const ROWS: u64 = 5 * 8192 + 1000;

/// Six granules: an i64 column cut into five extents.
fn five_extent_part() -> BuiltPart {
    let mut words = seq_i64_col(1, ROWS);
    words.extent_cuts = vec![1, 2, 3, 4];
    build_part(&PartSpec::new(ROWS, vec![words]))
}

fn values(part: &OpenPart, attno: u32) -> (StreamEntry, Vec<ExtentRecord>) {
    let dir = part.stream_directory().expect("stream directory");
    let ps = dir.lookup(attno, 0, StreamRole::Values).expect("values stream");
    (ps.entry, ps.extents.clone())
}

fn file_bytes<'a>(b: &'a BuiltPart, r: &ExtentRecord) -> &'a [u8] {
    &b.bytes[r.file_off as usize..(r.file_off + r.len) as usize]
}

#[test]
fn each_face_puts_its_image_in_its_class() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let b = five_extent_part();
    let part = open_built(&b, 90, 1);
    let (e, x) = values(&part, 1);
    let r0 = part.resident();
    let shared = part.extent_bytes(&e, &x[0], 0).unwrap();
    let cached = part.extent_bytes_cached(&e, &x[1], 1).unwrap();
    let private = part.extent_bytes_uncached(&e, &x[2], 2).unwrap();
    assert_eq!((shared.class(), cached.class(), private.class()), (STICKY, CACHED, HELD));
    assert_eq!(part.resident(), r0 + x[0].len + x[1].len, "a Private read admits nothing");
    for (buf, r) in [(&shared, &x[0]), (&cached, &x[1]), (&private, &x[2])] {
        assert_eq!(buf.bytes(), file_bytes(&b, r));
    }
    // Hits: Cached and Private faces leave a sticky image sticky; a Shared face makes a cached image sticky.
    assert!(part.extent_bytes_cached(&e, &x[0], 0).unwrap().same(&shared));
    assert_eq!(shared.class(), STICKY);
    assert!(part.extent_bytes_uncached(&e, &x[1], 1).unwrap().same(&cached));
    assert_eq!(cached.class(), CACHED, "a Private hit changes nothing");
    assert!(part.extent_bytes(&e, &x[1], 1).unwrap().same(&cached));
    assert_eq!(cached.class(), STICKY);
    // Let go by its map, an image is held by whoever still has it.
    assert_eq!(part.evict_extent_images(&[(x[0].file_off, x[0].len)]).0, 1);
    assert_eq!(shared.class(), HELD);
    let late = part.extent_bytes_cached(&e, &x[3], 3).unwrap();
    drop(part);
    assert_eq!(late.class(), HELD, "a dropped part's images are held by their holders");
}

/// The length of the image whose releases [`watch_account`] counts (0: none), and the count.
static WATCHED: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);

/// The part cache's account hook (`partcache::set_account_hook`, where Gravity wires the memory governor): counts the
/// releases of exactly the watched image's bytes.
fn watch_account(n: u64, charged: bool, _rec: &mut crate::partcache::Charged) {
    if !charged && n != 0 && n == WATCHED.load(Ordering::SeqCst) {
        RELEASED.fetch_add(1, Ordering::SeqCst);
    }
}

/// D3113 on the line's eviction class: a key eviction lets an image another clone holds go to HELD in the chunk census,
/// but the part cache's census and its account keep it charged until that last holder drops it, which releases it once.
#[test]
fn a_key_evicted_image_stays_charged_until_its_last_holder_drops_it() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    // one extent of a length no other test's image has, so a concurrent test's release cannot count here
    let vals = (0..1009u64).map(|r| Some(CellValue::Bytes(format!("d3113-owed-{r:05}-{}", "y".repeat((r % 29) as usize)).into_bytes())));
    let b = build_part(&PartSpec::new(1009, vec![ColSpec::new(1, StorageClass::VarlenaVerbatim, vals.collect())]));
    let part = open_built(&b, 91, 1);
    let (e, x) = values(&part, 1);
    crate::partcache::set_account_hook(watch_account);
    let held = part.extent_bytes(&e, &x[0], 0).unwrap();
    let len = held.len() as u64;
    let resident = part.resident();
    RELEASED.store(0, Ordering::SeqCst);
    WATCHED.store(len, Ordering::SeqCst);
    assert_eq!(part.evict_extent_images(&[(x[0].file_off, x[0].len)]), (1, len), "the map lets it go");
    assert_eq!(held.class(), HELD, "held by its holder (the chunk census)");
    assert_eq!(part.resident(), resident - len, "the part holds it no more");
    assert_eq!(RELEASED.load(Ordering::SeqCst), 0, "still charged to the account while a holder keeps it");
    drop(held);
    assert_eq!(RELEASED.load(Ordering::SeqCst), 1, "released at its last holder's drop");
    drop(part);
    assert_eq!(RELEASED.load(Ordering::SeqCst), 1, "once: the part's drop does not release it again");
    WATCHED.store(0, Ordering::SeqCst);
}

/// The same law when the part itself lets go of its maps: a dropped part's image another clone holds stays charged to
/// the part cache's census and account until that holder drops it, which releases it once.
#[test]
fn a_dropped_parts_image_stays_charged_until_its_last_holder_drops_it() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let vals = (0..1013u64).map(|r| Some(CellValue::Bytes(format!("d3113-part-{r:05}-{}", "z".repeat((r % 31) as usize)).into_bytes())));
    let b = build_part(&PartSpec::new(1013, vec![ColSpec::new(1, StorageClass::VarlenaVerbatim, vals.collect())]));
    let part = open_built(&b, 91, 2);
    let (e, x) = values(&part, 1);
    crate::partcache::set_account_hook(watch_account);
    let held = part.extent_bytes(&e, &x[0], 0).unwrap();
    let len = held.len() as u64;
    RELEASED.store(0, Ordering::SeqCst);
    WATCHED.store(len, Ordering::SeqCst);
    drop(part);
    assert_eq!(held.class(), HELD, "held by its holder (the chunk census)");
    assert_eq!(RELEASED.load(Ordering::SeqCst), 0, "still charged to the account while a holder keeps it");
    drop(held);
    assert_eq!(RELEASED.load(Ordering::SeqCst), 1, "released at its last holder's drop, once");
    WATCHED.store(0, Ordering::SeqCst);
}

/// D4834: an image another holder keeps as its part lets go of it (a key eviction, the part's drop) takes its bytes and
/// block off the part's charge into itself, and its last holder's drop gives them back. Left on the part's charge, each
/// eviction and re-fault of a shared image grew the part cache's account by the image.
#[test]
fn d4834_an_owed_image_takes_its_bytes_off_the_parts_charge() {
    use crate::openpart::IMG_BLOCK;
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let vals = (0..1019u64).map(|r| Some(CellValue::Bytes(format!("d4834-owed-{r:05}-{}", "w".repeat((r % 23) as usize)).into_bytes())));
    let b = build_part(&PartSpec::new(1019, vec![ColSpec::new(1, StorageClass::VarlenaVerbatim, vals.collect())]));
    let part = open_built(&b, 91, 4);
    let (e, x) = values(&part, 1);
    let h0 = part.held_and_images().0;
    let kept = part.extent_bytes(&e, &x[0], 0).unwrap();
    let len = kept.len() as u64;
    assert!(part.held_and_images().0 > h0 + len, "cached: on the part's charge");
    assert_eq!(part.evict_extent_images(&[(x[0].file_off, x[0].len)]), (1, len));
    assert_eq!(part.held_and_images().0, h0, "evicted while a holder keeps it: off the part's charge");
    assert_eq!(kept.owed_bytes(), len + IMG_BLOCK, "its bytes and block go with the image");
    let again = part.extent_bytes(&e, &x[0], 0).unwrap();
    let h1 = part.held_and_images().0;
    drop(kept);
    assert_eq!(part.held_and_images().0, h1, "the evicted copy's drop leaves the part's charge alone");
    drop(part);
    assert_eq!(again.owed_bytes(), len + IMG_BLOCK, "the part's drop: a kept image takes its bytes and block too");
    drop(again);
}

#[test]
fn the_sweep_spares_pinned_and_sticky_images() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let b = five_extent_part();
    let part = open_built(&b, 91, 1);
    let (e, x) = values(&part, 1);
    let pinned = part.extent_bytes_cached(&e, &x[0], 0).unwrap();
    drop(part.extent_bytes_cached(&e, &x[1], 1).unwrap());
    drop(part.extent_bytes_cached(&e, &x[2], 2).unwrap());
    drop(part.extent_bytes(&e, &x[2], 2).unwrap());
    let (r, ev) = (part.resident(), mem::stats().evicted);
    mem::sweep_all_for_tests();
    assert_eq!(part.resident(), r - x[1].len, "only the unpinned cached image goes");
    assert_eq!(mem::stats().evicted, ev + 1);
    assert!(part.extent_bytes_cached(&e, &x[0], 0).unwrap().same(&pinned), "the pinned image stayed resident");
    drop(pinned);
    mem::sweep_all_for_tests();
    assert_eq!(part.resident(), r - x[1].len - x[0].len, "its holder gone, the next sweep takes it");
}

#[test]
fn cap_zero_admits_nothing() {
    let _s = serial();
    let _cap = Cap::set(0);
    let b = five_extent_part();
    let part = open_built(&b, 92, 1);
    let (e, x) = values(&part, 1);
    let (r0, slots0) = (part.resident(), mem::stats().slots);
    let a = part.extent_bytes_cached(&e, &x[0], 0).unwrap();
    let a2 = part.extent_bytes_cached(&e, &x[0], 0).unwrap();
    assert_eq!(a.class(), HELD);
    assert!(!a.same(&a2), "nothing resident: each read is its caller's own");
    assert_eq!((part.resident(), mem::stats().slots), (r0, slots0));
}

#[test]
fn the_fault_log_is_capped_for_refaults() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let b = five_extent_part();
    let part = open_built(&b, 93, 1);
    let (e, x) = values(&part, 1);
    let opened = part.faults().len() as u64;
    let cap = 2 * part.sections().len() + 64;
    let rounds = cap / x.len() + 2;
    for _ in 0..rounds {
        for (i, r) in x.iter().enumerate() {
            assert_eq!(part.extent_bytes_cached(&e, r, i as u32).unwrap().bytes(), file_bytes(&b, r));
        }
        mem::sweep_all_for_tests();
    }
    assert!(part.faults().len() <= cap, "{} entries over a cap of {cap}", part.faults().len());
    assert!(part.faults_unlogged() > 0);
    assert_eq!(part.faults().len() as u64 + part.faults_unlogged(), opened + (rounds * x.len()) as u64);
}

/// Six granules: an i64 column in five extents, a text column with NULLs (a validity stream) in three, and a
/// dictionary text column.
fn wide_part() -> BuiltPart {
    let mut words = seq_i64_col(1, ROWS);
    words.extent_cuts = vec![1, 2, 3, 4];
    let mut text = text_col(2, ROWS, Some(9));
    text.extent_cuts = vec![2, 4];
    let mut dict = text_col(3, ROWS, Some(5));
    dict.dict = Some(DictSpec { entries: (0..300).map(|i| format!("entry-{i:05}").into_bytes()).collect() });
    dict.dict_exec_flag = true;
    build_part(&PartSpec::new(ROWS, vec![words, text, dict]))
}

/// Decode granule `g` of `cur` into `datums` (backed by `ab`) and check it against the builder's expectation.
fn decode_checked(b: &BuiltPart, cur: &mut StreamCursor<'_>, attno: u32, g: u32, base: usize, ab: &mut ArenaBuf) -> Vec<u64> {
    let exp = b.expected.iter().find(|e| e.attno == attno && e.path_ord == 0).expect("expected col");
    let n = cur.values_in_granule(g).expect("values") as usize;
    let mut datums = vec![0u64; n.max(1)];
    let mut out = DecodeOut { datums: &mut datums, arena: ByteArena::new(ab.bytes_mut()) };
    assert_eq!(cur.decode_full(g, &mut out).expect("decode_full") as usize, n);
    assert_granule(exp.class, &datums, &exp.valid, &exp.canon, base, n);
    datums.truncate(n);
    datums
}

#[test]
fn cached_cursors_decode_like_shared_while_the_ring_sweeps() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let b = wide_part();
    let binding = reference_binding_leaked();
    for attno in 1..=3u32 {
        let part = open_built(&b, 94, attno as u64);
        let mut cur = StreamCursor::open(part.clone(), binding, attno, 0).unwrap().with_residency(Residency::Cached);
        let mut base = 0usize;
        for g in 0..cur.granule_count() {
            let mut ab = ArenaBuf::new(4 << 20);
            base += decode_checked(&b, &mut cur, attno, g, base, &mut ab).len();
            let mut vw = vec![0u64; 8192usize.div_ceil(64)];
            cur.validity(g, &mut vw).expect("validity");
            let (v, val) = cur.loaded_extents();
            assert!(v <= 1 && val <= 1, "attno {attno}: one extent per stream, held {v} / {val}");
            // Everything nothing holds goes; the cursor's own extents are pinned.
            mem::sweep_all_for_tests();
        }
    }
}

/// The guard's mechanics (the dict-datum guard test needs a `DictCodes` part, which only the writer builds: L5b,
/// beside the UAF test in `pgrc_am`).
#[test]
fn a_guard_pins_what_it_holds_until_release() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    let b = five_extent_part();
    let part = open_built(&b, 95, 1);
    let (e, x) = values(&part, 1);
    let mut g = Guard::new();
    let a = part.extent_bytes_cached(&e, &x[0], 0).unwrap();
    g.hold(&a);
    g.hold(&a);
    drop(a);
    let mut worker = Guard::new();
    worker.hold(&part.extent_bytes_cached(&e, &x[1], 1).unwrap());
    g.absorb(worker);
    assert_eq!(g.bytes(), x[0].len + x[1].len, "each image held once");
    let r = part.resident();
    mem::sweep_all_for_tests();
    assert_eq!(part.resident(), r, "a guard pins what it holds");
    let cur = StreamCursor::open(part.clone(), reference_binding_leaked(), 1, 0).unwrap().with_residency(Residency::Cached);
    let mut h = Guard::new();
    cur.hold_datums(&mut h);
    assert_eq!(h.bytes(), 0, "a word column's datums alias nothing");
    g.release();
    assert_eq!(g.bytes(), 0);
    mem::sweep_all_for_tests();
    assert_eq!(part.resident(), r - x[0].len - x[1].len, "released, both go");
}

/// Eight threads under a cap below their working set: Cached faults and hits (plain and unwrapped), Private reads, Cached
/// cursor decodes, and holds that come and go. Every image a thread holds is checked against the part file, and
/// the plane's accounting balances once everyone lets go.
#[test]
fn stress_eight_threads_under_a_tiny_cap() {
    let _s = serial();
    let mut words = seq_i64_col(1, 16 * 8192);
    words.extent_cuts = (1..16).collect();
    let b = Arc::new(build_part(&PartSpec::new(16 * 8192, vec![words])));
    let parts: Vec<Arc<OpenPart>> = (0..2).map(|i| open_built(&b, 96, i)).collect();
    let (e, x) = values(&parts[0], 1);
    let _ = values(&parts[1], 1);
    let max = x.iter().map(|r| r.len).max().unwrap();
    assert_eq!(mem::stats().cached, 0, "no other test admits");
    // A cap under what the threads hold (over_cap and the backoff), then room for the holds (8 × 4 images) plus a
    // few admissions (admission and eviction); the working set is 64 images.
    for cap in [4 * max, 40 * max] {
        let s0 = mem::stats();
        let _cap = Cap::set(cap);
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let (b, parts, e, x) = (b.clone(), parts.clone(), e, x.clone());
                std::thread::spawn(move || {
                    let binding = reference_binding_leaked();
                    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ (t + 1).wrapping_mul(0xD1B5_4A32_D192_ED03);
                    let mut next = || {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng
                    };
                    let mut held: Vec<(SegBuf, ExtentRecord)> = Vec::new();
                    let mut peak_cached = 0u64;
                    for _ in 0..400 {
                        let p = &parts[(next() % 2) as usize];
                        let i = (next() % x.len() as u64) as usize;
                        let r = x[i];
                        let got = match next() % 8 {
                            0..=3 => Some(p.extent_bytes_cached(&e, &r, i as u32).unwrap()),
                            4 => Some(p.unwrapped_extent_bytes_cached(&e, &r, i as u32, &mut |raw| Ok(raw.to_vec())).unwrap()),
                            5 => Some(p.extent_bytes_uncached(&e, &r, i as u32).unwrap()),
                            6 => {
                                if !held.is_empty() {
                                    held.swap_remove((next() % held.len() as u64) as usize);
                                }
                                None
                            }
                            _ => {
                                let mut cur = StreamCursor::open(p.clone(), binding, 1, 0).unwrap().with_residency(Residency::Cached);
                                let g = (next() % u64::from(cur.granule_count())) as u32;
                                let mut ab = ArenaBuf::new(1 << 20);
                                decode_checked(&b, &mut cur, 1, g, g as usize * 8192, &mut ab);
                                None
                            }
                        };
                        if let Some(buf) = got {
                            assert_eq!(buf.bytes(), file_bytes(&b, &r), "thread {t}: extent {i}");
                            if held.len() < 4 {
                                held.push((buf, r));
                            }
                        }
                        peak_cached = peak_cached.max(mem::stats().cached);
                    }
                    for (buf, r) in &held {
                        assert_eq!(buf.bytes(), file_bytes(&b, r), "thread {t}: a held image moved");
                    }
                    peak_cached
                })
            })
            .collect();
        let peak = threads.into_iter().map(|h| h.join().expect("stress thread")).max().unwrap();
        let s1 = mem::stats();
        eprintln!(
            "mem stress: cap {cap} peak cached {peak} admitted {} evicted {} ({} B) over_cap {} futile {}",
            s1.admitted - s0.admitted,
            s1.evicted - s0.evicted,
            s1.evicted_bytes - s0.evicted_bytes,
            s1.over_cap - s0.over_cap,
            s1.futile - s0.futile
        );
        assert!(peak <= cap + 8 * max, "cached {peak} over cap {cap} by more than one admission per thread");
        assert!(s1.evicted + s1.over_cap > s0.evicted + s0.over_cap, "the cap was reached");
    }
    drop(parts);
    mem::sweep_all_for_tests();
    let s2 = mem::stats();
    assert_eq!((s2.cached, s2.slots), (0, 0), "every admitted byte and slot is gone with its parts");
}

/// The L5a decode microbench (`cargo test --release -p pgrc_read --lib tests::mem::microbench -- --ignored
/// --nocapture`): ns per face call for a Shared (sticky) hit, a Cached hit and a fault, and ns per row for a warm
/// `decode_full` walk through Shared and Cached cursors. Best of seven; the base tree's copy drops the Cached rows.
#[test]
#[ignore]
fn microbench() {
    let _s = serial();
    let _cap = Cap::set(1 << 40);
    const G: u64 = 64;
    let mut words = seq_i64_col(1, G * 8192);
    words.extent_cuts = (1..G as u32).collect();
    let b = build_part(&PartSpec::new(G * 8192, vec![words]));
    let best = |reps: u32, f: &mut dyn FnMut()| -> f64 {
        (0..7)
            .map(|_| {
                let t = std::time::Instant::now();
                for _ in 0..reps {
                    f();
                }
                t.elapsed().as_nanos() as f64 / reps as f64
            })
            .fold(f64::MAX, f64::min)
    };
    let mut ino = 0u64;
    let fault = best(20, &mut || {
        ino += 1;
        let p = open_built(&b, 98, ino);
        let (e, x) = values(&p, 1);
        for (i, r) in x.iter().enumerate() {
            std::hint::black_box(p.extent_bytes(&e, r, i as u32).unwrap());
        }
    }) / G as f64;
    let part = open_built(&b, 97, 1);
    let (e, x) = values(&part, 1);
    let hit = |part: &Arc<OpenPart>, cached: bool| {
        let (part, x) = (part.clone(), x.clone());
        move || {
            for (i, r) in x.iter().enumerate() {
                let img = if cached { part.extent_bytes_cached(&e, r, i as u32) } else { part.extent_bytes(&e, r, i as u32) };
                std::hint::black_box(img.unwrap().bytes()[0]);
            }
        }
    };
    let shared_hit = best(2000, &mut hit(&part, false)) / G as f64;
    let cpart = open_built(&b, 97, 2);
    let _ = values(&cpart, 1);
    let cached_hit = best(2000, &mut hit(&cpart, true)) / G as f64;
    let binding = reference_binding_leaked();
    let walk = |part: &Arc<OpenPart>, res: Residency| {
        let mut cur = StreamCursor::open(part.clone(), binding, 1, 0).unwrap().with_residency(res);
        let mut datums = vec![0u64; 8192];
        let mut ab = ArenaBuf::new(1 << 16);
        move || {
            for g in 0..G as u32 {
                let mut out = DecodeOut { datums: &mut datums, arena: ByteArena::new(ab.bytes_mut()) };
                std::hint::black_box(cur.decode_full(g, &mut out).unwrap());
            }
        }
    };
    let rows = (G * 8192) as f64;
    let shared_walk = best(50, &mut walk(&part, Residency::Shared)) / rows;
    let cached_walk = best(50, &mut walk(&cpart, Residency::Cached)) / rows;
    eprintln!(
        "microbench: fault {fault:.0} ns/extent; shared hit {shared_hit:.1} ns; cached hit {cached_hit:.1} ns; \
         shared walk {shared_walk:.3} ns/row; cached walk {cached_walk:.3} ns/row"
    );
}
