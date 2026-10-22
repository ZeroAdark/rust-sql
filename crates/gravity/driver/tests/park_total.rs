//! H16 (b) B2 (FRAME's design, main 09-24): the park keeps its bytes as a running total (never summed per put) and looks
//! at the last put's slot first. Random puts, takes and drops against a model of the LRU park: the count and the total
//! always equal the entries' (`Park::bytes` also asserts the total against a fresh sum in a debug build), and a take hands
//! back the newest entry of its key, as the linear scan did. The byte cap still evicts the largest older entries and
//! keeps the one just parked; a content mismatch still drops every entry of the handle.

use gravity_driver_rig as common;

use std::sync::Arc;

use common::*;
use gravity_driver::{Node, Park};
use gravity_program::{Generation, Handle, PlanKey, Program};

fn key(k: u64) -> PlanKey {
    PlanKey { handle: Handle(0x7000 + k), generation: Generation(1) }
}

/// A Node over its own `Program` (the same content as every other: the park tells the entries apart by the Arc).
fn node() -> (Box<Node>, *const Program) {
    let program = Arc::new(p_rows(64));
    let ptr = Arc::as_ptr(&program);
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    (Box::new(Node::build(program, &mut derive)), ptr)
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

#[test]
fn the_running_total_and_the_hint_match_the_linear_park_over_random_puts_takes_and_drops() {
    const CAP: usize = 8;
    const KEYS: u64 = 5;
    let mut park = Park::with_bytes(CAP, usize::MAX);
    let content = Arc::new(p_rows(64)).key;
    // the model: (key, stamp, program, bytes) per entry, the LRU as the park keeps it
    let mut model: Vec<(u64, u64, *const Program, usize)> = Vec::new();
    let (mut rng, mut stamp, mut hits, mut misses) = (Rng(0x9e37_79b9_7f4a_7c15), 0u64, 0, 0);
    for step in 0..600 {
        let k = rng.next(KEYS);
        match rng.next(10) {
            0..=4 => {
                let (n, ptr) = node();
                let bytes = n.bytes();
                assert!(bytes > 0, "a built Node holds bytes");
                let room = model.len() < CAP;
                if !room {
                    let oldest = (0..model.len()).min_by_key(|&i| model[i].1).unwrap();
                    model.remove(oldest);
                }
                stamp += 1;
                model.push((k, stamp, ptr, bytes));
                assert_eq!(park.put(key(k), n), room, "step {step}: a free slot iff the model had room");
            }
            5..=8 => {
                let want = model.iter().enumerate().filter(|(_, e)| e.0 == k).max_by_key(|(_, e)| e.1).map(|(i, _)| i);
                match (park.take(key(k), content), want) {
                    (Some(n), Some(i)) => {
                        assert_eq!(Arc::as_ptr(n.program()), model[i].2, "step {step}: the newest entry of key {k}");
                        model.remove(i);
                        n.teardown();
                        hits += 1;
                    }
                    (None, None) => misses += 1,
                    (got, want) => panic!("step {step}: take of key {k} got {} want {}", got.is_some(), want.is_some()),
                }
            }
            _ => {
                let before = model.len();
                model.retain(|e| e.0 != k);
                assert_eq!(park.drop_for(key(k).handle), before - model.len(), "step {step}: the handle's entries dropped");
            }
        }
        assert_eq!(park.len(), model.len(), "step {step}: the entry count");
        assert_eq!(park.bytes(), model.iter().map(|e| e.3).sum::<usize>(), "step {step}: the running byte total");
    }
    assert!(hits > 50 && misses > 10, "the walk took both ways ({hits} hits, {misses} misses)");
    park.clear();
    assert_eq!((park.len(), park.bytes()), (0, 0));
}

#[test]
fn a_take_after_two_puts_of_one_key_gets_the_newer_then_the_older() {
    let mut park = Park::with_bytes(4, usize::MAX);
    let content = Arc::new(p_rows(64)).key;
    let (a, pa) = node();
    let (b, pb) = node();
    let (c, _) = node();
    park.put(key(1), a);
    park.put(key(1), b);
    park.put(key(2), c);
    // the last put's slot (key 2) is the hint: key 1's take scans, and gets the newer
    assert_eq!(Arc::as_ptr(park.take(key(1), content).expect("key 1").program()), pb);
    assert_eq!(Arc::as_ptr(park.take(key(1), content).expect("key 1 again").program()), pa);
    assert!(park.take(key(1), content).is_none());
    assert_eq!(park.len(), 1);
}

#[test]
fn the_byte_cap_evicts_older_entries_and_keeps_the_one_just_parked() {
    let (probe, _) = node();
    let b = probe.bytes();
    probe.teardown();
    let cap = 2 * b + b / 2;
    let mut park = Park::with_bytes(8, cap);
    let content = Arc::new(p_rows(64)).key;
    let mut last = core::ptr::null();
    for k in 0..20 {
        let (n, ptr) = node();
        park.put(key(k), n);
        last = ptr;
        assert!(park.len() <= 2 && park.bytes() <= cap, "put {k}: {} entries, {} of {cap} bytes", park.len(), park.bytes());
    }
    assert!(park.evictions() >= 18, "{} evictions", park.evictions());
    assert_eq!(Arc::as_ptr(park.take(key(19), content).expect("the last put stays").program()), last);
    assert_eq!(park.bytes(), park.len() * b);
}

#[test]
fn a_content_mismatch_drops_every_entry_of_the_handle() {
    let mut park = Park::with_bytes(4, usize::MAX);
    let (a, _) = node();
    let (b, _) = node();
    park.put(key(3), a);
    park.put(key(3), b);
    let other = Arc::new(p_rows(32)).key;
    assert_ne!(other, Arc::new(p_rows(64)).key, "another content");
    assert!(park.take(key(3), other).is_none());
    assert_eq!((park.len(), park.bytes()), (0, 0));
}

/// D3620: a park holds no slot until a Node is put and grows one slot a put up to its capacity (each slot holds its Node
/// inline, ~1 KB): a deep statement level's park is empty for as long as its one statement runs.
#[test]
fn a_park_grows_its_slots_with_its_puts() {
    let mut park = Park::with_bytes(4, usize::MAX);
    assert_eq!((park.held_slots(), park.capacity()), (0, 4), "a new park holds no slot");
    let content = Arc::new(p_rows(64)).key;
    assert!(park.take(key(1), content).is_none(), "a take from an empty park misses");
    for k in 0..6u64 {
        let (n, _) = node();
        park.put(key(k), n);
        assert_eq!(park.held_slots(), (k as usize + 1).min(4), "one slot a put, up to the capacity");
    }
    assert_eq!((park.len(), park.evictions()), (4, 2), "past the capacity a put evicts, as before");
    assert!(park.take(key(5), content).is_some(), "the newest put is taken back");
}
