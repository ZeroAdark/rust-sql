//! P2 W1 (`PGRUST_GRAVITY_PARK_INDEX`): a park's take, put and eviction find their slot through an index beside the slots
//! (a key -> newest-entry table, the LRU list, the free-slot bits), and its release pass passes the parked Nodes whose
//! sources read none of the released relations. Indexed or not, a park must take the same Node, fill the same slot,
//! evict the same entry and release the same handles as the linear park did. Each test holds `SERIAL`: the switch, its
//! check and the release log are process-wide, so this file is its own test binary. A debug build checks every take,
//! put, eviction and release against the walk inside the park too (`index_mismatches`, a panic on a mismatch).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use common::*;
use gravity_driver::park::{index_mismatches, set_park_index};
use gravity_driver::slots::note_released;
use gravity_driver::{Call, Entry, Node, Park, Step};
use gravity_program::cost::runtime::PARK_INDEX_FROM;
use gravity_program::{ContentKey, Generation, Handle, PlanKey, Program};

static SERIAL: Mutex<()> = Mutex::new(());

/// The switch set for one test, the others waiting.
fn serial(on: bool) -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    set_park_index(on, true);
    g
}

fn key(k: u64) -> PlanKey {
    PlanKey { handle: Handle(0x9100 + k / 3), generation: Generation(1 + (k % 3) as u32) }
}

/// A Node over its own `Program` of shape `shape` (three shapes: three sizes, so the byte cap has more than one size to
/// pick from; Nodes of one shape tie on bytes, so its slot-order ties are exercised too).
fn node(shape: u64) -> (Box<Node>, *const Program) {
    let program = Arc::new(shape_program(shape));
    let ptr = Arc::as_ptr(&program);
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    (Box::new(Node::build(program, &mut derive)), ptr)
}

fn shape_program(shape: u64) -> Program {
    match shape % 3 {
        0 => p_rows(64),
        1 => p_rows(8),
        _ => p_count(4),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Today's linear park, slot for slot (`Park::put_parked` / `take` / `drop_for` / `take_parked_into` before W1): the lowest
/// free slot, else a slot grown up to the cap, else the least recently put entry's; over the byte cap the largest other
/// entry goes (the last slot of a tie); a take gets its key's newest entry.
struct Model {
    slots: Vec<Option<(PlanKey, u64, *const Program, usize, ContentKey)>>,
    cap: usize,
    cap_bytes: usize,
    clock: u64,
    evictions: u64,
}

impl Model {
    fn live(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    fn total(&self) -> usize {
        self.slots.iter().flatten().map(|e| e.3).sum()
    }

    fn put(&mut self, key: PlanKey, ptr: *const Program, bytes: usize, content: ContentKey) -> bool {
        self.clock += 1;
        let free = self.slots.iter().position(Option::is_none).or_else(|| {
            (self.slots.len() < self.cap).then(|| {
                self.slots.push(None);
                self.slots.len() - 1
            })
        });
        let idx = free.unwrap_or_else(|| {
            let victim = (0..self.slots.len()).min_by_key(|&i| self.slots[i].map_or(0, |e| e.1)).unwrap();
            self.slots[victim] = None;
            self.evictions += 1;
            victim
        });
        self.slots[idx] = Some((key, self.clock, ptr, bytes, content));
        while self.live() > 1 && self.total() > self.cap_bytes {
            let victim = (0..self.slots.len()).filter(|&i| i != idx && self.slots[i].is_some()).max_by_key(|&i| self.slots[i].map_or(0, |e| e.3)).unwrap();
            self.slots[victim] = None;
            self.evictions += 1;
        }
        free.is_some()
    }

    /// The key's newest entry's content, if any.
    fn peek(&self, key: PlanKey) -> Option<ContentKey> {
        let i = (0..self.slots.len()).filter(|&i| self.slots[i].is_some_and(|e| e.0 == key)).max_by_key(|&i| self.slots[i].map_or(0, |e| e.1))?;
        self.slots[i].map(|e| e.4)
    }

    /// `take` with `content`: the key's newest entry, or (another content) none and every entry of its handle dropped.
    fn take(&mut self, key: PlanKey, content: ContentKey) -> Option<(*const Program, ContentKey)> {
        let i = (0..self.slots.len()).filter(|&i| self.slots[i].is_some_and(|e| e.0 == key)).max_by_key(|&i| self.slots[i].map_or(0, |e| e.1))?;
        let e = self.slots[i].take()?;
        if e.4 != content {
            self.drop_for(key.handle);
            return None;
        }
        Some((e.2, e.4))
    }

    fn drop_for(&mut self, handle: Handle) -> usize {
        let mut n = 0;
        for s in self.slots.iter_mut() {
            if s.is_some_and(|e| e.0.handle == handle) {
                *s = None;
                n += 1;
            }
        }
        n
    }

    /// `take_parked_into`: the key's entries in slot order.
    fn take_all(&mut self, key: PlanKey) -> Vec<(*const Program, usize, ContentKey)> {
        let mut out = Vec::new();
        for s in self.slots.iter_mut() {
            if s.is_some_and(|e| e.0 == key) {
                let e = s.take().unwrap();
                out.push((e.2, e.3, e.4));
            }
        }
        out
    }
}

/// `steps` random puts, takes, drops, kept-pool round trips, clears and switch flips over `keys` keys against the model.
fn walk(cap: usize, keys: u64, cap_bytes: usize, steps: usize, seed: u64, flips: bool) -> (u64, u64) {
    let contents: Vec<ContentKey> = (0..3).map(|s| shape_program(s).key).collect();
    let other = Arc::new(p_rows(9)).key;
    let mut park = Park::with_bytes(cap, cap_bytes);
    let mut model = Model { slots: Vec::new(), cap, cap_bytes, clock: 0, evictions: 0 };
    let mut on = gravity_driver::park::park_index();
    let (mut rng, mut hits, mut misses) = (Rng(seed), 0u64, 0u64);
    for step in 0..steps {
        let k = key(rng.next(keys));
        match rng.next(100) {
            0..=44 => {
                let shape = rng.next(3);
                let (n, ptr) = node(shape);
                let bytes = n.bytes();
                let want = model.put(k, ptr, bytes, contents[shape as usize]);
                assert_eq!(park.put(k, n), want, "step {step}: a put fills a free or grown slot iff the linear park's did");
            }
            45..=84 => match {
                // now and then another content under the key: its take drops every entry of the handle
                let content = if rng.next(30) == 0 { other } else { model.peek(k).unwrap_or(contents[0]) };
                (park.take(k, content), model.take(k, content))
            } {
                (Some(n), Some((ptr, content))) => {
                    assert_eq!(Arc::as_ptr(n.program()), ptr, "step {step}: the take of {k:?} got the linear park's Node");
                    // a taken Node goes back as a statement's end puts it, under a fresh stamp
                    let bytes = n.bytes();
                    let want = model.put(k, ptr, bytes, content);
                    assert_eq!(park.put(k, n), want, "step {step}: the put back");
                    hits += 1;
                }
                (None, None) => misses += 1,
                (got, want) => panic!("step {step}: take of {k:?}: index {} linear {}", got.is_some(), want.is_some()),
            },
            85..=89 => assert_eq!(park.drop_for(k.handle), model.drop_for(k.handle), "step {step}: drop_for {:?}", k.handle),
            90..=94 => {
                // D3881: a freed level's Nodes out to the thread's pool and back, in the order they came out
                let mut out = Vec::new();
                park.take_parked_into(k, &mut out);
                let want = model.take_all(k);
                assert_eq!(out.iter().map(|(_, n)| Arc::as_ptr(n.program())).collect::<Vec<_>>(), want.iter().map(|w| w.0).collect::<Vec<_>>(), "step {step}: the key's Nodes in slot order");
                for ((kk, n), (ptr, bytes, content)) in out.into_iter().zip(want) {
                    let w = model.put(kk, ptr, bytes, content);
                    assert_eq!(park.put_parked(kk, n), w, "step {step}: put_parked");
                }
            }
            95 if rng.next(10) == 0 => {
                park.clear();
                model.slots.iter_mut().for_each(|s| *s = None);
            }
            96..=97 if flips => {
                // the switch read again at the park's next call (a drop of no entry)
                on = !on;
                set_park_index(on, true);
                assert_eq!(park.drop_for(Handle(u64::MAX)), 0);
            }
            _ => {}
        }
        assert_eq!(park.len(), model.live(), "step {step}: the entry count");
        assert_eq!(park.bytes(), model.total(), "step {step}: the byte total");
        assert_eq!(park.evictions(), model.evictions, "step {step}: the evictions");
        assert_eq!(park.held_slots(), model.slots.len(), "step {step}: the slots grown");
        assert_eq!(park.indexed(), on && park.held_slots() >= PARK_INDEX_FROM, "step {step}: the park follows the switch from PARK_INDEX_FROM slots");
    }
    set_park_index(true, true);
    (hits, misses)
}

#[test]
fn the_indexed_park_takes_puts_and_evicts_as_the_linear_park_over_random_steps() {
    let _g = serial(true);
    let before = index_mismatches();
    let (probe, _) = node(0);
    let b = probe.bytes();
    probe.teardown();
    // cap 4: under PARK_INDEX_FROM, the walks; cap 16: indexed from its 16th slot
    for (cap, keys, cap_bytes, seed) in [(4, 9, usize::MAX, 0x9e37_79b9_7f4a_7c15), (16, 30, usize::MAX, 0x2468_ace0_1357_9bdf), (64, 100, usize::MAX, 0x1234_5678_9abc_def1), (256, 600, usize::MAX, 0x0bad_cafe_f00d_d00d), (64, 80, 20 * b, 0x5eed_0001), (256, 300, 40 * b, 0x5eed_0002)] {
        let (hits, misses) = walk(cap, keys, cap_bytes, 4000, seed, false);
        assert!(hits > 30 && misses > 30, "cap {cap} bytes {cap_bytes}: the walk took both ways ({hits} hits, {misses} misses)");
    }
    assert_eq!(index_mismatches(), before, "the check found the index and the walks agreeing");
}

#[test]
fn the_linear_park_matches_the_model_too() {
    let _g = serial(false);
    let (hits, misses) = walk(64, 100, usize::MAX, 3000, 0x7777_0001, false);
    assert!(hits > 100 && misses > 100, "({hits} hits, {misses} misses)");
}

#[test]
fn a_park_rebuilds_its_index_when_the_switch_turns_on_and_drops_it_when_off() {
    let _g = serial(true);
    let before = index_mismatches();
    let (hits, _) = walk(64, 90, usize::MAX, 4000, 0xf11b_0001, true);
    assert!(hits > 100);
    assert_eq!(index_mismatches(), before);
}

#[test]
fn a_full_park_evicts_its_least_recently_put_entry_and_refills_the_lowest_free_slot() {
    let _g = serial(true);
    let content = shape_program(0).key;
    const N: u64 = PARK_INDEX_FROM as u64;
    let mut park = Park::with_bytes(PARK_INDEX_FROM, usize::MAX);
    let mut ptrs = Vec::new();
    for k in 0..N {
        let (n, ptr) = node(0);
        assert!(park.put(key(k), n), "put {k}: a free slot");
        ptrs.push(ptr);
    }
    assert!(park.indexed(), "a full park of PARK_INDEX_FROM slots is indexed");
    // key 0 taken and put back: key 1 is the least recently put now
    let n0 = park.take(key(0), content).expect("key 0");
    assert!(park.put(key(0), n0), "the taken slot is free again");
    let (nn, pn) = node(0);
    assert!(!park.put(key(N), nn), "full: an eviction");
    assert_eq!((park.len(), park.evictions(), park.held_slots()), (PARK_INDEX_FROM, 1, PARK_INDEX_FROM));
    assert!(park.take(key(1), content).is_none(), "the least recently put entry (key 1) went");
    for k in (0..N).filter(|&k| k != 1) {
        assert_eq!(Arc::as_ptr(park.take(key(k), content).expect("kept").program()), ptrs[k as usize], "key {k} kept");
    }
    assert_eq!(Arc::as_ptr(park.take(key(N), content).expect("the last put").program()), pn);
    assert!(park.is_empty());
    // two entries of one key: the newer first, then the older; a content mismatch drops what is left of the handle
    let (a, pa) = node(0);
    let (b, pb) = node(0);
    park.put(key(7), a);
    park.put(key(7), b);
    assert_eq!(Arc::as_ptr(park.take(key(7), content).expect("newer").program()), pb);
    park.put(key(7), node(0).0);
    let (c, _) = node(0);
    park.put(key(9), c);
    assert!(park.take(key(7), Arc::new(p_rows(9)).key).is_none(), "another content: the handle's entries drop");
    assert_eq!(park.len(), 1, "key 9 stays (another handle)");
    let _ = pa;
}

#[test]
fn shed_entries_keep_their_place_in_the_index() {
    let _g = serial(true);
    let before = index_mismatches();
    let content = shape_program(0).key;
    const N: u64 = PARK_INDEX_FROM as u64;
    let mut park = Park::with_bytes(PARK_INDEX_FROM, usize::MAX);
    let mut ptrs = Vec::new();
    for k in 0..N {
        let (n, ptr) = node(0);
        park.put(key(k), n);
        ptrs.push(ptr);
    }
    assert!(park.indexed());
    park.shed_all();
    // full of shed entries: the next puts still evict the least recently put first, and takes find their keys
    for k in N..N + 3 {
        assert!(!park.put(key(k), node(0).0), "full");
    }
    assert_eq!(park.evictions(), 3);
    for k in 0..3 {
        assert!(park.take(key(k), content).is_none(), "key {k} was the oldest: evicted");
    }
    for k in 3..N {
        assert_eq!(Arc::as_ptr(park.take(key(k), content).expect("kept").program()), ptrs[k as usize]);
    }
    park.shed_all();
    assert_eq!(park.len(), 3);
    park.clear();
    assert_eq!((park.len(), park.bytes()), (0, 0));
    assert!(park.put(key(1), node(0).0), "a cleared park fills slot 0 again");
    assert!(park.indexed(), "a cleared park keeps its slots and its index");
    assert_eq!(index_mismatches(), before);
}

const NULLS: [u8; 1] = [0];

/// One serial statement of `program` under `key`: its Node parks at the end, its source keeping its table's handle.
fn statement(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, key: PlanKey, flag: &AtomicU8) {
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, flag);
    let mut exec = entry.start(Some(key), program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    while entry.run(&mut exec, &mut b, Call::batch(64)).unwrap() != Step::Done {}
    entry.end(exec, true);
}

/// The handles the sources keep across unbind (parked or not).
fn kept(table: &RangeTable) -> usize {
    Arc::strong_count(&table.token) - 1
}

/// Two parked Nodes over two relations, each keeping its table's handle; a record of one relation releases that one, and
/// (indexed) the pass visits that Node alone; a reset (relid 0) reaches both.
fn release_pass(on: bool) -> (u64, u64) {
    const R1: u32 = 7101;
    const R2: u32 = 7102;
    let (p1, p2, p3) = (Arc::new(p_rows_on(R1, 64)), Arc::new(p_rows_on(R2, 64)), Arc::new(p_rows_on(R1 + 50, 64)));
    let (t1, t2) = (RangeTable::new(24, 10), RangeTable::new(24, 10));
    t1.keep.store(true, Ordering::Relaxed);
    t2.keep.store(true, Ordering::Relaxed);
    let (mut d1, mut d2, mut d3) = (FakeDerive::ranges(t1.clone()), FakeDerive::ranges(t2.clone()), FakeDerive::ranges(RangeTable::new(24, 10)));
    let mut entry = Entry::new(8);
    let flag = AtomicU8::new(0);
    let (k1, k2, k3) = (key(301), key(302), key(303));
    statement(&mut entry, &p1, &mut d1, k1, &flag);
    statement(&mut entry, &p2, &mut d2, k2, &flag);
    assert_eq!((kept(&t1), kept(&t2)), (1, 1), "each parked source keeps its handle");
    let v0 = entry.park().release_visits();
    note_released(R2);
    // the next statement's acquire releases what was recorded from the park
    statement(&mut entry, &p3, &mut d3, k3, &flag);
    assert_eq!((kept(&t1), kept(&t2)), (1, 0), "the record of R2 released R2's handle alone");
    let one = entry.park().release_visits() - v0;
    note_released(0);
    statement(&mut entry, &p3, &mut d3, k3, &flag);
    assert_eq!((kept(&t1), kept(&t2)), (0, 0), "a reset releases every relation's");
    let all = entry.park().release_visits() - v0 - one;
    drop(entry);
    (one, all)
}

#[test]
fn a_release_record_visits_only_the_parked_nodes_of_its_relations() {
    let _g = serial(true);
    let before = index_mismatches();
    let (one, all) = release_pass(true);
    assert_eq!(one, 1, "indexed: the R2 record's pass visited R2's Node alone");
    assert!(all >= 2, "a reset visits every parked Node ({all})");
    assert_eq!(index_mismatches(), before, "no Node passed kept anything of the released relation");
    drop(_g);
    let _g = serial(false);
    let (one, all) = release_pass(false);
    assert!(one >= 2 && all >= 2, "linear: every pass visits every parked Node ({one}, {all})");
}
