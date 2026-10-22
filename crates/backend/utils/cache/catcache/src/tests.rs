use datum::Datum;

use crate::compute::*;
use crate::search::*;
use crate::testing;
use crate::{with_state, NONE};

use std::sync::atomic::{AtomicI32, Ordering};

static NEXT_ID: AtomicI32 = AtomicI32::new(50);

fn fresh_id() -> i32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn oid_key(v: u32) -> CatCKey<'static> {
    CatCKey::Value(Datum::from_oid(v))
}

const KINDS1: [CCFastKind; 4] = [CCFastKind::Int4; 4];

#[test]
fn hash_combine_matches_c_shape() {
    let ks = [oid_key(7), oid_key(11), oid_key(13), oid_key(17)];
    let h1 = int4_hash(Datum::from_oid(7));
    let h2 = int4_hash(Datum::from_oid(11));
    let h3 = int4_hash(Datum::from_oid(13));
    let h4 = int4_hash(Datum::from_oid(17));
    let expect = h4.rotate_left(24) ^ h3.rotate_left(16) ^ h2.rotate_left(8) ^ h1;
    assert_eq!(compute_hash_value(&KINDS1, 4, &ks), expect);
    assert_eq!(compute_hash_value(&KINDS1, 1, &ks), h1);
    assert_eq!(int4_hash(Datum::from_oid(1259)), hashfn::murmurhash32(1259));
}

#[test]
fn name_semantics_match_strncmp() {
    assert!(name_eq(b"pg_class", b"pg_class"));
    assert!(!name_eq(b"pg_class", b"pg_klass"));
    assert!(name_eq(b"abc\0zzzz", b"abc"));
    let long_a = [b'a'; 80];
    let long_b = [b'a'; 70];
    assert!(name_eq(&long_a, &long_b));
    assert_eq!(name_hash(b"abc\0zzz"), hashfn::hash_bytes(b"abc"));
}

#[test]
fn get_cc_hash_eq_funcs_table() {
    assert_eq!(get_cc_hash_eq_funcs(16), (CCFastKind::Char, F_BOOLEQ));
    assert_eq!(get_cc_hash_eq_funcs(19), (CCFastKind::Name, F_NAMEEQ));
    assert_eq!(get_cc_hash_eq_funcs(21), (CCFastKind::Int2, F_INT2EQ));
    assert_eq!(get_cc_hash_eq_funcs(23), (CCFastKind::Int4, F_INT4EQ));
    assert_eq!(get_cc_hash_eq_funcs(25), (CCFastKind::Text, F_TEXTEQ));
    assert_eq!(get_cc_hash_eq_funcs(26), (CCFastKind::Int4, F_OIDEQ));
    assert_eq!(get_cc_hash_eq_funcs(2206), (CCFastKind::Int4, F_OIDEQ));
    assert_eq!(get_cc_hash_eq_funcs(30), (CCFastKind::OidVector, F_OIDVECTOREQ));
}

#[test]
#[should_panic(expected = "not supported as catcache key")]
fn get_cc_hash_eq_funcs_rejects_unknown() {
    let _ = get_cc_hash_eq_funcs(700);
}

fn tiny_image() -> Vec<u8> {
    let mut img = vec![0u8; 32];
    img[22] = 24;
    img
}

#[test]
fn oid_hit_negative_and_miss_shape() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    let img = tiny_image();
    testing::insert_positive(id, &[oid_key(1259), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED], &img);
    testing::insert_negative(id, &[oid_key(4444), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED]);

    let t = SearchCatCache1(id, oid_key(1259)).unwrap().expect("hit");
    assert_eq!(t.tuple().t_len, img.len() as u32);
    assert_eq!(t.tuple().t_data().t_hoff, 24);
    ReleaseCatCache(t);

    assert!(SearchCatCache1(id, oid_key(4444)).unwrap().is_none());

    let a = SearchCatCache1(id, oid_key(1259)).unwrap().unwrap();
    let b = SearchCatCache1(id, oid_key(1259)).unwrap().unwrap();
    ReleaseCatCache(a);
    ReleaseCatCache(b);
    assert_eq!(testing::cache_ntup(id), 2);
}

#[test]
fn two_key_hit_general_lane() {
    let id = fresh_id();
    let kinds = [CCFastKind::Int4, CCFastKind::Int2, CCFastKind::Int4, CCFastKind::Int4];
    testing::init_cache_bare(id, 2, kinds, 4, None);
    let img = tiny_image();
    let k = [oid_key(1259), CatCKey::Value(Datum::from_i16(3)), CatCKey::UNUSED, CatCKey::UNUSED];
    testing::insert_positive(id, &k, &img);

    let t = SearchCatCache2(id, oid_key(1259), CatCKey::Value(Datum::from_i16(3)))
        .unwrap()
        .expect("2-key hit");
    ReleaseCatCache(t);
    assert!(std::panic::catch_unwind(|| {
        SearchCatCache2(id, oid_key(1259), CatCKey::Value(Datum::from_i16(4)))
    })
    .is_err());
}

#[test]
fn name_key_hit() {
    let id = fresh_id();
    let kinds = [CCFastKind::Name, CCFastKind::Int4, CCFastKind::Int4, CCFastKind::Int4];
    testing::init_cache_bare(id, 1, kinds, 4, None);
    let img = tiny_image();
    testing::insert_positive(
        id,
        &[CatCKey::Str("pg_class"), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED],
        &img,
    );
    let t = SearchCatCache1(id, CatCKey::Str("pg_class")).unwrap().expect("name hit");
    ReleaseCatCache(t);
    assert!(std::panic::catch_unwind(|| SearchCatCache1(id, CatCKey::Str("pg_clasz"))).is_err());
}

#[test]
fn move_to_front_on_hit() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 1, None);
    let img = tiny_image();
    for oid in [10u32, 11, 12] {
        testing::insert_positive(id, &[oid_key(oid), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED], &img);
    }
    let t = SearchCatCache1(id, oid_key(10)).unwrap().unwrap();
    ReleaseCatCache(t);
    with_state(|st| {
        let c = st.cache(id);
        let head = c.cc_bucket[0];
        assert_eq!(c.tuples[head as usize].keys[0].as_u32(), 10);
        let mut n = 0;
        let mut cur = head;
        let mut prev = NONE;
        while cur != NONE {
            assert_eq!(c.tuples[cur as usize].prev, prev);
            prev = cur;
            cur = c.tuples[cur as usize].next;
            n += 1;
        }
        assert_eq!(n, 3);
    });
}

#[test]
fn invalidate_and_reset() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    let img = tiny_image();
    testing::insert_positive(id, &[oid_key(77), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED], &img);
    testing::insert_negative(id, &[oid_key(78), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED]);
    assert_eq!(testing::cache_ntup(id), 2);

    let hv = with_state(|st| {
        let c = st.cache(id);
        compute_hash_value(&c.cc_kind, 1, &[oid_key(77), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED])
    });
    crate::CatCacheInvalidate(id, hv);
    assert_eq!(testing::cache_ntup(id), 1);

    testing::insert_positive(id, &[oid_key(77), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED], &img);
    let t = SearchCatCache1(id, oid_key(77)).unwrap().unwrap();
    crate::CatCacheInvalidate(id, hv);
    assert_eq!(testing::cache_ntup(id), 2);
    with_state(|st| assert!(st.cache(id).tuples[t.slot as usize].dead));
    ReleaseCatCache(t);
    assert_eq!(testing::cache_ntup(id), 1);

    crate::ResetCatalogCachesExt(false).unwrap();
    assert_eq!(testing::cache_ntup(id), 0);
}

#[test]
fn rehash_preserves_entries() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 2, None);
    let img = tiny_image();
    for oid in 0..40u32 {
        testing::insert_negative(id, &[oid_key(oid), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED]);
        let _ = img;
    }
    with_state(|st| {
        crate::graph::maybe_rehash(st, id);
        assert!(st.cache(id).cc_nbuckets > 2);
    });
    for oid in 0..40u32 {
        assert!(SearchCatCache1(id, oid_key(oid)).unwrap().is_none());
    }
}

#[test]
fn packed_byref_key_roundtrip() {
    let buf = [0u8, 1, 2, 3, 4, 5, 6, 7];
    let k = crate::pack_ref(2, 4);
    // SAFETY: off+len within buf.
    let s = unsafe { crate::stored_bytes(buf.as_ptr(), k) };
    assert_eq!(s, &[2, 3, 4, 5]);
}

mod state_kernel {
    use super::*;

    #[test]
    fn sequential_borrows_roundtrip() {
        let id = fresh_id();
        testing::init_cache_bare(id, 1, KINDS1, 4, None);
        with_state(|st| st.cache_mut(id).cc_ntup += 5);
        assert_eq!(with_state(|st| st.cache(id).cc_ntup), 5);
        with_state(|st| st.cache_mut(id).cc_ntup = 0);
        assert_eq!(with_state(|st| st.cache(id).cc_ntup), 0);
    }

    #[test]
    fn pinned_image_survives_state_mutation() {
        let id = fresh_id();
        testing::init_cache_bare(id, 1, KINDS1, 4, None);
        let img = super::tiny_image();
        testing::insert_positive(id, &[oid_key(9), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED], &img);
        let t = SearchCatCache1(id, oid_key(9)).unwrap().unwrap();
        for oid in 100..164u32 {
            testing::insert_negative(id, &[oid_key(oid), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED]);
        }
        assert_eq!(t.tuple().t_len, img.len() as u32);
        assert_eq!(t.tuple().t_data().t_hoff, 24);
        ReleaseCatCache(t);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn reentrant_access_panics_in_debug() {
        let nested = std::panic::catch_unwind(|| with_state(|_outer| with_state(|_inner| 0u8)));
        assert!(nested.is_err(), "re-entrancy guard failed to fire");
        let _ = with_state(|st| st.caches.len());
    }
}

fn k1(v: u32) -> [CatCKey<'static>; 4] {
    [oid_key(v), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED]
}

fn probe_has(id: i32, v: u32) -> bool {
    match SearchCatCache1(id, oid_key(v)) {
        Ok(Some(t)) => {
            ReleaseCatCache(t);
            true
        }
        _ => false,
    }
}

#[test]
fn cap_evicts_down_to_cap_and_spares_pinned() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    let img = tiny_image();
    for v in 1..=8u32 {
        testing::insert_positive(id, &k1(v), &img);
    }
    let base = with_state(|st| st.ch_ntup);

    let p1 = SearchCatCache1(id, oid_key(3)).unwrap().expect("hit");
    let p2 = SearchCatCache1(id, oid_key(7)).unwrap().expect("hit");

    let target = base - 4;
    with_state(|st| crate::graph::enforce_cap_at(st, target, -1, NONE));
    assert_eq!(with_state(|st| st.ch_ntup), target);
    assert_eq!(with_state(|st| st.cache(id).cc_ntup), 4);

    assert!(probe_has(id, 3));
    assert!(probe_has(id, 7));
    ReleaseCatCache(p1);
    ReleaseCatCache(p2);
}

#[test]
fn cap_prefers_cold_entries_over_hot() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 8, None);
    let img = tiny_image();
    for v in 1..=6u32 {
        testing::insert_positive(id, &k1(v), &img);
    }
    with_state(|st| {
        let c = st.cache_mut(id);
        for ct in c.tuples.iter_mut() {
            if !ct.payload.is_null() && ct.keys[0].as_u32() >= 3 {
                ct.hot = false;
            }
        }
    });
    let target = with_state(|st| st.ch_ntup) - 2;
    with_state(|st| crate::graph::enforce_cap_at(st, target, -1, NONE));
    assert_eq!(with_state(|st| st.ch_ntup), target);
    assert!(probe_has(id, 1), "hot entry evicted before cold ones");
    assert!(probe_has(id, 2), "hot entry evicted before cold ones");
}

#[test]
fn cap_gives_up_when_everything_is_pinned() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    let img = tiny_image();
    let mut pins = Vec::new();
    for v in 1..=5u32 {
        testing::insert_positive(id, &k1(v), &img);
        pins.push(SearchCatCache1(id, oid_key(v)).unwrap().expect("hit"));
    }
    with_state(|st| crate::graph::enforce_cap_at(st, 1, -1, NONE));
    assert_eq!(with_state(|st| st.cache(id).cc_ntup), 5);
    for p in pins {
        ReleaseCatCache(p);
    }
}

#[test]
fn cap_protects_the_just_created_slot() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    let img = tiny_image();
    for v in 1..=4u32 {
        testing::insert_positive(id, &k1(v), &img);
    }
    let slot4 = with_state(|st| {
        let c = st.cache(id);
        (0..c.tuples.len() as u32)
            .find(|&s| {
                let ct = &c.tuples[s as usize];
                !ct.payload.is_null() && ct.keys[0].as_u32() == 4
            })
            .expect("slot of key 4")
    });
    let target = with_state(|st| st.ch_ntup) - 3;
    with_state(|st| crate::graph::enforce_cap_at(st, target, id, slot4));
    assert_eq!(with_state(|st| st.cache(id).cc_ntup), 1);
    assert!(probe_has(id, 4), "protected slot was evicted");
}

#[test]
fn idle_trim_defers_eviction_to_the_idle_point() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 8, None);
    let img = tiny_image();
    let base = with_state(|st| st.ch_ntup - st.ch_nlisted);
    let cap = base + 4;
    crate::CAP_OVERRIDE.with(|c| c.set(Some(cap)));
    crate::SetCatCacheTrimAtIdle(true);
    for v in 1..=4u32 {
        testing::insert_positive(id, &k1(v), &img);
    }
    // Negative entries go through create_entry_negative, an insert site that runs the cap check.
    for v in 101..=108u32 {
        testing::insert_negative(id, &k1(v));
    }
    assert_eq!(testing::cache_ntup(id), 12, "an insertion past the cap evicted with the idle trim on");
    let pin = SearchCatCache1(id, oid_key(3)).unwrap().expect("hit");
    crate::TrimCatCacheAtIdle();
    assert_eq!(with_state(|st| st.ch_ntup - st.ch_nlisted), cap, "the idle trim did not reach the cap");
    assert!(probe_has(id, 3), "pinned entry evicted at the idle point");
    ReleaseCatCache(pin);
    crate::TrimCatCacheAtIdle();
    assert_eq!(with_state(|st| st.ch_ntup - st.ch_nlisted), cap, "a second idle trim under the cap evicted");
    crate::SetCatCacheTrimAtIdle(false);
    for v in 201..=204u32 {
        testing::insert_negative(id, &k1(v));
    }
    assert!(with_state(|st| st.ch_ntup - st.ch_nlisted) <= cap, "insert-time trim not back with the flag off");
    crate::CAP_OVERRIDE.with(|c| c.set(None));
}

#[test]
fn l2_negative_build_match_and_install() {
    let id = fresh_id();
    let kinds = [CCFastKind::Name, CCFastKind::Int4, CCFastKind::Int4, CCFastKind::Int4];
    testing::init_cache_bare(id, 2, kinds, 4, None);

    let keys = [CatCKey::Bytes(b"some_rel"), oid_key(2200), CatCKey::UNUSED, CatCKey::UNUSED];
    let other = [CatCKey::Bytes(b"other_rel"), oid_key(2200), CatCKey::UNUSED, CatCKey::UNUSED];
    let ent = crate::l2::build_negative(id, &keys).unwrap();
    assert!(ent.negative);

    // Full logical-key matching against the shared body (byref + word keys).
    assert!(crate::l2::entry_matches(ent.as_ref(), &kinds, 2, &keys));
    assert!(!crate::l2::entry_matches(ent.as_ref(), &kinds, 2, &other));

    // probe_keys reproduces the entry's own logical keys.
    let probes = ent.probe_keys(&kinds, 2);
    assert!(crate::l2::entry_matches(ent.as_ref(), &kinds, 2, &probes));

    // Install as an L1 entry aliasing the shared payload; a probe now sees a
    // negative hit through the completely unchanged L1 walk.
    let hash = compute_hash_value(&kinds, 2, &keys);
    with_state(|st| crate::graph::install_from_l2(st, id, hash, &ent));
    assert_eq!(std::sync::Arc::strong_count(&ent), 2, "L1 holds the Arc");
    let r = SearchCatCache2(id, keys[0], keys[1]).unwrap();
    assert!(r.is_none(), "negative entry answers without a miss");

    // Invalidation removes the L1 alias and releases the Arc.
    crate::graph::CatCacheInvalidate(id, hash);
    assert_eq!(std::sync::Arc::strong_count(&ent), 1, "eviction drops the Arc");
}

#[test]
fn negative_name_keys_are_clipped_like_namestrcpy() {
    let id = fresh_id();
    let kinds = [CCFastKind::Name, CCFastKind::Int4, CCFastKind::Int4, CCFastKind::Int4];
    testing::init_cache_bare(id, 2, kinds, 4, None);
    let long = [b'a'; 100_000];
    let keys = [CatCKey::Bytes(&long), oid_key(2200), CatCKey::UNUSED, CatCKey::UNUSED];

    // L2 shape: the stored key is namestrcpy's 63 bytes, not the probe's 100000.
    let ent = crate::l2::build_negative(id, &keys).unwrap();
    let stored = ent.probe_keys(&kinds, 2);
    assert_eq!(stored[0].bytes().len(), NAMEDATALEN - 1);
    assert!(crate::l2::entry_matches(ent.as_ref(), &kinds, 2, &stored));
    // nameeq's strncmp(NAMEDATALEN) sees the clipped NUL at byte 63: no match.
    assert!(!crate::l2::entry_matches(ent.as_ref(), &kinds, 2, &keys));

    // L1 shape (create_entry_negative) and a 63-byte probe round-trips.
    testing::insert_negative(id, &keys);
    with_state(|st| {
        let ct = st.cache(id).tuples.iter().find(|ct| ct.negative).expect("L1 negative entry");
        // SAFETY: by-ref keys were packed against this entry's payload.
        let b = unsafe { crate::stored_bytes(ct.payload, ct.keys[0]) };
        assert_eq!(b.len(), NAMEDATALEN - 1);
    });
    let exact = [b'a'; NAMEDATALEN - 1];
    testing::insert_negative(id, &[CatCKey::Bytes(&exact), oid_key(2200), CatCKey::UNUSED, CatCKey::UNUSED]);
    let r = SearchCatCache2(id, CatCKey::Bytes(&exact), oid_key(2200)).unwrap();
    assert!(r.is_none(), "63-byte negative entry answers without a miss");
}

#[test]
fn l2_shared_entry_survives_l1_eviction_while_pinned_elsewhere() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    // A fake shared positive body: negative:false with a prefixed image.
    let img = tiny_image();
    let keys = [oid_key(777), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED];
    let hash = compute_hash_value(&KINDS1, 1, &keys);
    let ent = {
        let buf = crate::l2::AlignedBytes::new_zeroed(crate::IMG_PREFIX + img.len()).unwrap();
        // SAFETY: fresh buffer of IMG_PREFIX + img.len() bytes.
        unsafe {
            core::ptr::write(buf.as_ptr().add(12).cast::<u32>(), img.len() as u32);
            core::ptr::copy_nonoverlapping(img.as_ptr(), buf.as_ptr().add(crate::IMG_PREFIX), img.len());
        }
        std::sync::Arc::new(crate::l2::CatL2Entry {
            keys: [Datum::from_oid(777), Datum::null(), Datum::null(), Datum::null()],
            negative: false,
            t_len: img.len() as u32,
            t_self: types_tuple::ItemPointerData::new(0, 1),
            t_tableoid: 1,
            payload: buf,
        })
    };
    with_state(|st| crate::graph::install_from_l2(st, id, hash, &ent));

    // Pin it (C's refcount), then invalidate: the entry goes dead-but-pinned,
    // and the shared body must stay alive until the pin releases.
    let pin = SearchCatCache1(id, oid_key(777)).unwrap().expect("hit");
    assert_eq!(pin.tuple().t_len, img.len() as u32);
    crate::graph::CatCacheInvalidate(id, hash);
    assert_eq!(std::sync::Arc::strong_count(&ent), 2, "pinned: Arc still held");
    ReleaseCatCache(pin);
    assert_eq!(std::sync::Arc::strong_count(&ent), 1, "unpin frees the alias");
}

// C palloc raises ERRCODE_OUT_OF_MEMORY on failure; the catcache's image and
// scan-key buffers must surface that as a catchable error, not a panic.
#[test]
fn payload_alloc_failure_is_a_catchable_error() {
    let cx = mcx::MemoryContext::new("t");
    // Over C's MaxAllocSize: the allocator refuses it deterministically with
    // mcxt.c's ERROR (an unwind recovered at the statement boundary).
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::payload_alloc(cx.mcx(), 1usize << 31).map(|_| ())
    }));
    let payload = r.expect_err("over-ceiling request must unwind");
    let err = types_error::pg_error_from_panic(payload).unwrap_or_else(|_| panic!("a PgError payload"));
    assert!(err.message().contains("invalid memory alloc request size"), "{}", err.message());
}

// C SearchCatCacheMiss copies the caller's raw NAME datum into the scan key
// (catcache.c:1561 cur_skey[0].sk_argument = v1); nameeq/btnamecmp then
// compare NAMEDATALEN bytes, so a 64-byte probe keeps its 64th byte and can
// never equal a 63-byte catalog name (audit FP-catcache-1: has_schema_privilege
// of repeat('a', 64) must not find the 63-'a' schema).
#[test]
fn name_scan_key_keeps_the_64th_byte() {
    let cx = mcx::MemoryContext::new("t");
    let probe = "a".repeat(64);
    let d = frame_scan_arg(cx.mcx(), CCFastKind::Name, &CatCKey::Str(&probe)).unwrap();
    // SAFETY: frame_scan_arg returns a NAMEDATALEN-byte buffer in `cx`.
    let buf = unsafe { std::slice::from_raw_parts(d.as_usize() as *const u8, NAMEDATALEN) };
    assert_eq!(buf[62], b'a');
    assert_eq!(buf[63], b'a', "the 64th probe byte must survive framing (C: raw datum)");
    // A 63-byte probe frames as the NUL-terminated name it is.
    let probe63 = "a".repeat(63);
    let d = frame_scan_arg(cx.mcx(), CCFastKind::Name, &CatCKey::Str(&probe63)).unwrap();
    // SAFETY: as above.
    let buf = unsafe { std::slice::from_raw_parts(d.as_usize() as *const u8, NAMEDATALEN) };
    assert_eq!(buf[62], b'a');
    assert_eq!(buf[63], 0);
    // Longer probes are capped at NAMEDATALEN (nameeq never reads past it).
    let probe65 = "a".repeat(65);
    let d = frame_scan_arg(cx.mcx(), CCFastKind::Name, &CatCKey::Str(&probe65)).unwrap();
    // SAFETY: as above.
    let buf = unsafe { std::slice::from_raw_parts(d.as_usize() as *const u8, NAMEDATALEN) };
    assert_eq!(buf[63], b'a');
}

// RehashCatCache announces the bucket doubling at DEBUG1 (catcache.c:1002):
// "rehashing catalog cache id %d for %s; %d tups, %d buckets" -- visible to
// a client running with client_min_messages = debug1 (audit-18.6 b169,
// row catcache-f7333dc6).
static REHASH_LOGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn capture_rehash_log(e: &types_error::PgError, _output_to_server: &mut bool) {
    if e.level == types_error::DEBUG1 {
        REHASH_LOGS.lock().unwrap_or_else(|e| e.into_inner()).push(e.message().to_string());
    }
}

#[test]
fn rehash_announces_at_debug1_like_c() {
    elog::init_seams();
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 2, None);
    with_state(|st| {
        let mcx = st.mcx;
        st.cache_mut(id).cc_relname =
            Some(mcx::PgString::from_str_in("pg_class", mcx).unwrap());
    });
    // Every insert runs C's post-insert check (catcache.c:2282); the first
    // doubling fires at the 5th tuple of a 2-bucket cache and reports the
    // count and bucket number BEFORE the rehash.
    let prior_min = elog::config::log_min_messages();
    elog::config::set_log_min_messages(types_error::DEBUG1);
    REHASH_LOGS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let prior = elog::sink::set_emit_log_hook(Some(capture_rehash_log));
    for oid in 0..40u32 {
        testing::insert_negative(id, &[oid_key(oid), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED]);
    }
    elog::sink::set_emit_log_hook(prior);
    elog::config::set_log_min_messages(prior_min);
    let logs = REHASH_LOGS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        logs.contains(&format!("rehashing catalog cache id {id} for pg_class; 5 tups, 2 buckets")),
        "catcache.c:1002 DEBUG1 line missing: {logs:?}"
    );
    with_state(|st| assert!(st.cache(id).cc_nbuckets > 2));
}

// catcache.c:2216 palloc in CacheMemoryContext: an unsatisfiable shared-entry
// body request is the catchable out-of-memory ERROR, never a process abort.
#[test]
fn l2_body_alloc_failure_is_a_catchable_error() {
    let Err(e) = crate::l2::AlignedBytes::new_zeroed(1usize << 62) else {
        panic!("unsatisfiable request succeeded");
    };
    assert!(e.message().contains("out of memory"), "{}", e.message());
}

// SearchCatCacheList PG_CATCH: when the list's key-payload palloc fails after
// the scan adopted its members, their temp refs are released instead of
// pinning them for the session.
#[test]
fn list_finalization_failure_releases_temp_member_refs() {
    let id = fresh_id();
    let kinds = [CCFastKind::Name, CCFastKind::Int4, CCFastKind::Int4, CCFastKind::Int4];
    testing::init_cache_bare(id, 1, kinds, 4, None);
    let keys = [CatCKey::Str("pg_class"), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED];
    testing::insert_positive(id, &keys, &tiny_image());
    let t = SearchCatCache1(id, CatCKey::Str("pg_class")).unwrap().expect("hit");
    let slot = t.slot;
    ReleaseCatCache(t);
    with_state(|st| st.cache_mut(id).tuples[slot as usize].refcount += 1);
    // The state is per test thread: the limited context stays installed.
    let full = Box::leak(Box::new(mcx::MemoryContext::new("full").with_limit(1))).mcx();
    with_state(|st| st.mcx = full);
    let r = crate::list::adopt_members(id, 1, 7, &keys, &[slot], true);
    assert!(r.is_err(), "the limited context must fail the key-payload alloc");
    let (refcount, c_list) =
        with_state(|st| (st.cache(id).tuples[slot as usize].refcount, st.cache(id).tuples[slot as usize].c_list));
    assert_eq!(refcount, 0, "the scan's temp ref is released on the error path");
    assert_eq!(c_list, NONE);
}

// ExecGrant_common: a lookup, then an ereport before ReleaseSysCache; the dropped handle must unpin.
#[test]
fn abandoned_pin_releases_on_drop() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 4, None);
    let img = tiny_image();
    let keys = [oid_key(1259), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED];
    testing::insert_positive(id, &keys, &img);
    let refcount = |slot: u32| with_state(|st| st.cache(id).tuples[slot as usize].refcount);

    let mut slot = 0u32;
    let r: types_error::PgResult<()> = (|| {
        let t = SearchCatCache1(id, oid_key(1259))?.expect("hit");
        slot = t.slot;
        assert_eq!(refcount(slot), 1);
        Err(Box::new(types_error::PgError::error("check failed".to_string())))?;
        drop(t);
        Ok(())
    })();
    assert!(r.is_err());
    assert_eq!(refcount(slot), 0, "the early return released the pin");

    let hash = compute_hash_value(&KINDS1, 1, &keys);
    let t = SearchCatCache1(id, oid_key(1259)).unwrap().expect("hit");
    crate::graph::CatCacheInvalidate(id, hash);
    assert_eq!(testing::cache_ntup(id), 1, "dead-but-pinned entry stays");
    drop(t);
    assert_eq!(testing::cache_ntup(id), 0, "dropping the last pin removes the dead entry");
}

#[test]
fn compressed_text_key_hashes_and_stores_detoasted() {
    use core::mem::MaybeUninit;
    ::detoast::init_seams();
    let cx = mcx::MemoryContext::new("t");
    let mcx = cx.mcx();
    let name: Vec<u8> = (0..2400).map(|i| b"abc."[i % 4]).collect();
    let mut dest = vec![MaybeUninit::<u8>::uninit(); pglz::pglz_max_output(name.len())];
    let n = pglz::pglz_compress_into(&name, &mut dest, &pglz::PGLZ_STRATEGY_ALWAYS).unwrap();
    let total = 8 + n;
    let mut img = Vec::with_capacity(total);
    img.extend_from_slice(&(((total as u32) << 2) | 0x02).to_ne_bytes());
    img.extend_from_slice(&(name.len() as u32).to_ne_bytes());
    img.extend(dest[..n].iter().map(|b| unsafe { b.assume_init() }));

    let mut td = ::tupdesc::CreateTemplateTupleDesc(mcx, 1).unwrap();
    ::tupdesc::TupleDescInitBuiltinEntry(&mut td, 1, "k", 25, -1, 0).unwrap();
    let tup = ::heaptuple::heap_form_tuple(
        mcx,
        &td,
        &[datum::Datum::from_usize(img.as_ptr() as usize)],
        &[false],
    )
    .unwrap();
    let kinds = [CCFastKind::Text, CCFastKind::Int4, CCFastKind::Int4, CCFastKind::Int4];
    let keyno = [1, 0, 0, 0];
    let probe = [CatCKey::Bytes(&name), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED];
    let hv = crate::graph::compute_tuple_hash_value(&kinds, 1, &keyno, &td, tup.as_tuple()).unwrap();
    assert_eq!(hv, crate::compute::compute_hash_value(&kinds, 1, &probe));

    let (buf, keys, len) = crate::graph::build_positive_payload(
        &kinds,
        1,
        &keyno,
        &td,
        tup.as_tuple(),
        crate::l2::AlignedBytes::new_zeroed,
        |b| b.as_ptr(),
    )
    .unwrap();
    assert_eq!(len, crate::IMG_PREFIX + tup.as_tuple().t_len as usize + name.len());
    assert!(crate::eq_stored(CCFastKind::Text, keys[0], buf.as_ptr(), &probe[0]));
}

fn slot_of(id: i32, v: u32) -> u32 {
    with_state(|st| {
        let c = st.cache(id);
        (0..c.tuples.len() as u32)
            .find(|&s| {
                let ct = &c.tuples[s as usize];
                !ct.payload.is_null() && ct.keys[0].as_u32() == v
            })
            .expect("slot of key")
    })
}

#[test]
fn listed_members_do_not_count_against_the_cap() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 8, None);
    let img = tiny_image();
    for v in 1..=6u32 {
        testing::insert_positive(id, &k1(v), &img);
    }
    let members: Vec<u32> = (1..=4u32).map(|v| slot_of(id, v)).collect();
    with_state(|st| {
        let c = st.cache_mut(id);
        c.cc_nlbuckets = 16;
        c.cc_lbucket.resize(16, NONE);
        for &m in &members {
            c.tuples[m as usize].refcount += 1;
        }
    });
    let keys = [oid_key(99), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED];
    let (lslot, _) = crate::list::adopt_members(id, 1, 7, &keys, &members, true).unwrap();
    with_state(|st| st.cache_mut(id).lists[lslot as usize].refcount = 0);
    assert_eq!(with_state(|st| st.ch_nlisted), 4);
    let target = with_state(|st| st.ch_ntup - st.ch_nlisted);
    with_state(|st| crate::graph::enforce_cap_at(st, target, -1, NONE));
    assert_eq!(testing::cache_ntup(id), 6, "nothing evictable exceeds the cap");
    with_state(|st| crate::graph::remove_cl(st, id, lslot));
    assert_eq!(with_state(|st| st.ch_nlisted), 0);
}

#[test]
fn futile_sweep_holds_off_until_growth() {
    let id = fresh_id();
    testing::init_cache_bare(id, 1, KINDS1, 8, None);
    let img = tiny_image();
    let mut pins = Vec::new();
    for v in 1..=8u32 {
        testing::insert_positive(id, &k1(v), &img);
        pins.push(SearchCatCache1(id, oid_key(v)).unwrap().expect("hit"));
    }
    let n = with_state(|st| st.ch_ntup - st.ch_nlisted);
    with_state(|st| crate::graph::enforce_cap_at(st, n - 4, -1, NONE));
    assert_eq!(testing::cache_ntup(id), 8);
    let retry = with_state(|st| st.cap_retry_at);
    assert!(retry > n, "futile sweep sets a hold-off above the current count");
    with_state(|st| {
        st.clock_cache = 0;
        st.clock_slot = 0;
    });
    with_state(|st| crate::graph::enforce_cap_at(st, n - 4, -1, NONE));
    assert_eq!(with_state(|st| (st.clock_cache, st.clock_slot)), (0, 0), "held off: no walk");
    for p in pins {
        ReleaseCatCache(p);
    }
    with_state(|st| crate::graph::enforce_cap_at(st, n - 4, -1, NONE));
    assert_eq!(testing::cache_ntup(id), 8, "still held off below the retry mark");
    with_state(|st| crate::graph::enforce_cap_at(st, n, -1, NONE));
    assert_eq!(with_state(|st| st.cap_retry_at), 0, "under the cap clears the hold-off");
    with_state(|st| crate::graph::enforce_cap_at(st, n - 4, -1, NONE));
    assert_eq!(testing::cache_ntup(id), 4, "unpinned entries evict again");
}

/// V4R-453: a search, list search or hash of a cache this thread never registered (a gravity pool worker has no
/// catcache) is a named ERROR, not the "not registered" panic that took the server down.
#[test]
fn an_unregistered_cache_is_a_named_error() {
    let id = fresh_id() + 500; // never registered on any thread
    std::thread::spawn(move || {
        let e = SearchCatCache1(id, oid_key(1259)).err().expect("no cache on this thread");
        assert!(e.message().contains("not registered") && e.message().contains("D1409"), "{}", e.message());
        let e = GetCatCacheHashValue(id, oid_key(1), CatCKey::UNUSED, CatCKey::UNUSED, CatCKey::UNUSED).expect_err("hash");
        assert!(e.message().contains("D1409"));
        let e = crate::list::SearchCatCacheList(id, 1, oid_key(1), CatCKey::UNUSED, CatCKey::UNUSED).err().expect("list");
        assert!(e.message().contains("D1409"));
    })
    .join()
    .unwrap();
}
