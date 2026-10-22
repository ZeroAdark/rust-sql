//! One test: the cap and the ring are process-wide.

use std::cell::RefCell;
use std::sync::Arc;

use pgrc_colcache::image::{verify, CompanionImage, Images, NoImage};
use pgrc_colcache::{Kind, PageSource, TxnOracle};
use pgrc_marks::{Locator, Registry, WriterCache};
use pgrc_read::mem;
use types_core::{TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageMut, PAI_IS_HEAP};

const FROZEN: u16 = 0x0100 | 0x0200;
const XMAX_INVALID: u16 = 0x0800;
const SEG: Kind = Kind::Segment { slot: 1, part_no: 1 };

struct Heap(RefCell<Vec<Box<[u64]>>>);

impl Heap {
    fn new(nblocks: usize, rows: usize) -> Heap {
        let h = Heap(RefCell::new((0..nblocks).map(|_| vec![0u64; BLCKSZ / 8].into_boxed_slice()).collect()));
        for b in 0..nblocks {
            h.page(b).init(0);
            for _ in 0..rows {
                h.add(b);
            }
        }
        h
    }

    fn page(&self, b: usize) -> PageMut<'_> {
        // SAFETY: an 8-aligned BLCKSZ buffer the heap owns, alive while the heap is.
        unsafe { PageMut::from_raw(std::ptr::NonNull::new(self.0.borrow_mut()[b].as_mut_ptr() as *mut u8).unwrap()) }
    }

    fn add(&self, b: usize) {
        let mut t = [0u8; 32];
        t[0..4].copy_from_slice(&3u32.to_ne_bytes());
        t[18..20].copy_from_slice(&1u16.to_ne_bytes());
        t[20..22].copy_from_slice(&(FROZEN | XMAX_INVALID).to_ne_bytes());
        t[22] = 24;
        t[24..32].copy_from_slice(&(b as i64).to_ne_bytes());
        self.page(b).add_item(&t, 0, PAI_IS_HEAP).expect("room");
    }
}

impl PageSource for Heap {
    fn nblocks(&self) -> PgResult<u32> {
        Ok(self.0.borrow().len() as u32)
    }
    fn read(&self, block: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        let p = &self.0.borrow()[block as usize];
        // SAFETY: BLCKSZ bytes of an 8-aligned buffer.
        f(unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, BLCKSZ) })
    }
}

struct Committed;

impl TxnOracle for Committed {
    fn is_current(&self, _: TransactionId) -> bool {
        false
    }
    fn in_progress(&self, _: TransactionId) -> PgResult<bool> {
        Ok(false)
    }
    fn committed(&self, _: TransactionId) -> PgResult<bool> {
        Ok(true)
    }
    fn horizon(&self) -> PgResult<TransactionId> {
        Ok(1_000_000)
    }
    fn oldest_clog(&self) -> TransactionId {
        3
    }
}

fn loc(n: u32) -> Locator {
    (1663, 5, 80_000 + n)
}

fn image(imgs: &Images, reg: &Registry, n: u32, h: &Heap) -> Result<Arc<CompanionImage>, NoImage> {
    imgs.current(reg, loc(n), n, SEG, h, &Committed).expect("current")
}

#[test]
fn images_under_the_cap() {
    let (reg, imgs, mut cache) = (Registry::new(), Images::new(), WriterCache::new());
    let s0 = mem::stats();
    let delta = |f: fn(&mem::Stats) -> u64| f(&mem::stats()) as i64 - f(&s0) as i64;
    mem::set_cap(4 << 20);
    let (a, b, c) = (Heap::new(8, 100), Heap::new(16, 200), Heap::new(16, 200));

    // Admitted: published, charged cached, nothing held once the callers drop.
    let ia = image(&imgs, &reg, 1, &a).unwrap();
    let (bytes_a, nblocks_a) = (ia.bytes as i64, ia.nblocks);
    assert!(bytes_a > 0 && imgs.len() == 1);
    assert_eq!((delta(|s| s.cached), delta(|s| s.held), delta(|s| s.image_bytes)), (bytes_a, 0, bytes_a));

    // Pinned (a view holds it): the sweep passes it; unpinned: evicted, its bytes credited.
    mem::sweep_all_for_tests();
    assert!(imgs.get(loc(1)).is_some(), "a held image survives the sweep");
    drop(ia);
    mem::sweep_all_for_tests();
    assert!(imgs.get(loc(1)).is_none(), "an unheld image goes");
    assert_eq!((delta(|s| s.cached), delta(|s| s.image_bytes)), (0, 0));

    // After an eviction the next build reads every block (base = None), and is right.
    let ia = image(&imgs, &reg, 1, &a).unwrap();
    assert_eq!(ia.redecoded, nblocks_a);
    assert!(verify(&ia, &a, &Committed).unwrap().is_empty());

    // Over the cap: B and C pinned, a write to A, then a cap the refresh cannot get under. The view reads the refresh
    // privately and A leaves the map; the next build (with room) is a full one and sees the write.
    let (ib, ic) = (image(&imgs, &reg, 2, &b).unwrap(), image(&imgs, &reg, 3, &c).unwrap());
    a.add(3);
    reg.written(&mut cache, loc(1), 3);
    let over0 = mem::stats().over_cap;
    mem::set_cap(384 << 10);
    let private = image(&imgs, &reg, 1, &a).unwrap();
    assert!(mem::stats().over_cap > over0 && imgs.get(loc(1)).is_none(), "over the cap: not published");
    assert!(verify(&private, &a, &Committed).unwrap().is_empty());
    assert!(delta(|s| s.held) > 0, "a private image is held");
    drop((ia, private, ib, ic));
    assert_eq!(delta(|s| s.held), 0);
    // No write since: kept, the base would look refreshable, and block 3 (armed by the private refresh) would stay
    // stale in it.
    mem::set_cap(4 << 20);
    let ia = image(&imgs, &reg, 1, &a).unwrap();
    assert!(verify(&ia, &a, &Committed).unwrap().is_empty(), "the build after an unadmitted refresh sees the write");
    assert_eq!(ia.redecoded, nblocks_a, "and is a full one");
    drop(ia);

    // Past the page bound (nblocks × BLCKSZ × 2 > cap / 2) once A changes: no image, and the published one goes. At
    // cap 0 (a boot setting: the map starts empty), none is built.
    mem::set_cap(128 << 10);
    a.add(4);
    reg.written(&mut cache, loc(1), 4);
    assert_eq!(image(&imgs, &reg, 1, &a).err(), Some(NoImage::OverCap));
    assert!(imgs.get(loc(1)).is_none());
    mem::set_cap(0);
    assert_eq!(image(&imgs, &reg, 4, &Heap::new(1, 10)).err(), Some(NoImage::OverCap));

    // Everything credited.
    for n in 1..=3 {
        imgs.drop_image(&reg, loc(n));
    }
    mem::sweep_all_for_tests();
    assert_eq!((delta(|s| s.cached), delta(|s| s.held), delta(|s| s.image_bytes)), (0, 0, 0));
    mem::set_cap(mem::DEFAULT_CAP);
}
