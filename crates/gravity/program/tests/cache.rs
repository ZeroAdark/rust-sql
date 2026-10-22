use std::sync::Arc;

use gravity_abi::Ctx;
use gravity_program::{Arena, Cmd, Code, Generation, Handle, IndexPlan, Layout, OutCol, PlanKey, Program, ProgramCache, SiteDesc, WritePlan};

unsafe extern "C-unwind" fn fake_entry(_ctx: *mut Ctx) -> i64 {
    gravity_abi::status::DONE
}

/// D3091: a body of its own (never run), so identical code folding in an optimized build (fast-profile) cannot merge it
/// with `fake_entry`: the key test compares the two entry addresses.
unsafe extern "C-unwind" fn other_entry(_ctx: *mut Ctx) -> i64 {
    gravity_abi::status::STOP
}

struct Mapping(#[allow(dead_code)] Vec<u8>);

fn layout(nsites: u16, out_natts: u16) -> Layout {
    Layout { nsites, out_natts, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }
}

fn program(rel: u32, bytes: usize) -> Arc<Program> {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::index_source(st.handle(), rel, rel + 1, &[1, 2], &[0, 1], &[3, 4])];
    let out = [OutCol::new("a", 23, -1, 4, true), OutCol::new("b", 25, -1, -1, false)];
    let code = if bytes == 0 { Code::hand(fake_entry) } else { Code::mapped(fake_entry, bytes, Box::new(Mapping(vec![0; bytes]))) };
    Arc::new(Program::new(layout(1, 2), &sites, &out, WritePlan::SELECT, code))
}

fn pk(h: u64, g: u32) -> PlanKey {
    PlanKey { handle: Handle(h), generation: Generation(g) }
}

#[test]
fn content_key_covers_every_field() {
    let base = program(1, 0);
    assert_eq!(base.key, program(1, 0).key, "the key is a function of the content");
    assert_ne!(base.key, program(2, 0).key, "relation oid");
    assert_ne!(base.key, program(1, 10).key, "code bytes");
    let st = Arena::new(1 << 12);
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1, 2, &[1, 2], &[0, 1], &[3, 4])];
    let out = [OutCol::new("a", 23, -1, 4, true), OutCol::new("b", 25, -1, -1, false)];
    let other_code = Program::new(layout(1, 2), &sites, &out, WritePlan::SELECT, Code::hand(other_entry));
    assert_ne!(base.key, other_code.key, "entry address");
    let renamed = [OutCol::new("a", 23, -1, 4, true), OutCol::new("c", 25, -1, -1, false)];
    assert_ne!(base.key, Program::new(layout(1, 2), &sites, &renamed, WritePlan::SELECT, Code::hand(fake_entry)).key, "output column");
    let w = WritePlan::write(a, Cmd::Update, 1, &[IndexPlan::new(a, 2, &[1, 2], true)]);
    let written = Program::new(layout(1, 2), &sites, &out, w, Code::hand(fake_entry));
    assert_ne!(base.key, written.key, "write plan");
    assert_eq!(written.write.indexes().len(), 1);
    assert_eq!(written.write.indexes()[0].keys(), [1, 2]);
    assert!(written.write.cmd.writes() && !base.write.cmd.writes());
    assert_eq!(base.out[1].name_bytes(), b"b");
    assert_eq!(base.nsources(), 1);
}

#[test]
fn code_calls_and_reports_owner() {
    let p = program(1, 0);
    assert!(p.code().is_hand());
    assert_eq!(unsafe { p.code().call(std::ptr::null_mut()) }, 0);
    let m = program(2, 100);
    assert!(!m.code().is_hand());
    assert_eq!(m.code_bytes(), 100);
    assert_eq!(m.layout.ctx_size(), gravity_abi::HEADER_SIZE + m.layout.nsites as usize * gravity_abi::SITE_SIZE);
    assert_eq!(m.layout.site_off(0), gravity_abi::HEADER_SIZE);
}

#[test]
#[should_panic(expected = "layout.nsites")]
fn program_rejects_site_count_mismatch() {
    let _ = Program::new(layout(2, 0), &[], &[], WritePlan::SELECT, Code::hand(fake_entry));
}

#[test]
#[should_panic(expected = "layout.out_natts")]
fn program_rejects_output_count_mismatch() {
    let _ = Program::new(layout(0, 1), &[], &[], WritePlan::SELECT, Code::hand(fake_entry));
}

#[test]
fn l1_then_l2_then_miss() {
    let mut c = ProgramCache::new(1 << 20, 16);
    assert!(c.l1(pk(1, 1)).is_none());
    let p = program(7, 10);
    assert!(c.l2(p.key).is_none());
    c.insert(Some(pk(1, 1)), p.clone());
    assert!(Arc::ptr_eq(&c.l1(pk(1, 1)).unwrap(), &p));
    assert!(c.l1(pk(1, 2)).is_none());
    assert!(Arc::ptr_eq(&c.l2(p.key).unwrap(), &p));
    c.insert(Some(pk(2, 1)), p.clone());
    assert_eq!(c.l1_len(), 2);
    assert_eq!(c.l2_len(), 1);
    let s = c.stats();
    assert_eq!((s.l1_hits, s.l2_hits, s.misses), (1, 1, 1));
}

#[test]
fn drop_for_is_the_one_invalidation_channel() {
    let mut c = ProgramCache::new(1 << 20, 16);
    c.insert(Some(pk(1, 1)), program(1, 1));
    c.insert(Some(pk(1, 2)), program(2, 1));
    c.insert(Some(pk(2, 1)), program(3, 1));
    assert_eq!(c.drop_for(Handle(1)), 2);
    assert!(c.l1(pk(1, 1)).is_none());
    assert!(c.l1(pk(1, 2)).is_none());
    assert!(c.l1(pk(2, 1)).is_some());
    assert_eq!(c.l2_len(), 3);
    c.reset();
    assert_eq!((c.l1_len(), c.l2_len(), c.l2_bytes()), (0, 0, 0));
}

#[test]
fn l2_evicts_lru_by_bytes_and_count() {
    let mut c = ProgramCache::new(250, 16);
    let (p1, p2, p3) = (program(1, 100), program(2, 100), program(3, 100));
    c.insert(None, p1.clone());
    c.insert(None, p2.clone());
    assert!(c.l2(p1.key).is_some());
    c.insert(None, p3.clone());
    assert_eq!(c.l2_len(), 2);
    assert!(c.l2(p2.key).is_none());
    assert!(c.l2(p1.key).is_some());
    assert!(c.l2(p3.key).is_some());
    assert_eq!(c.stats().evictions, 1);
    let held = program(9, 1000);
    c.insert(None, held.clone());
    assert_eq!(c.l2_len(), 1);
    assert_eq!(c.l2_bytes(), 1000);
    let mut small = ProgramCache::new(1 << 20, 2);
    let last = program(4, 1);
    for k in 0..4 {
        small.insert(None, program(k, 1));
    }
    small.insert(None, last.clone());
    assert_eq!(small.l2_len(), 2);
    assert!(small.l2(last.key).is_some());
    assert_eq!(Arc::strong_count(&held), 2);
}

/// D2948 (D1360's debug W8 sweep: cr_cap34_kernel_words_wide, cr_cap36_setop_slots): a tier-0 program is filed with the
/// interpreter's 0 code bytes and grows when it is promoted; its eviction subtracts what was counted at filing, not its
/// size now. Before, the subtraction underflowed (a debug panic; a release build wrapped l2_bytes past the budget and every
/// later filing evicted the whole L2).
#[test]
fn d2948_an_entry_promoted_after_filing_is_evicted_by_the_bytes_it_was_counted_at() {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::index_source(st.handle(), 7, 8, &[1, 2], &[0, 1], &[3, 4])];
    let out = [OutCol::new("a", 23, -1, 4, true), OutCol::new("b", 25, -1, -1, false)];
    let up = Box::new(|| Ok(Code::mapped(fake_entry, 4096, Box::new(Mapping(vec![0; 4096])))));
    let tiered = Arc::new(Program::new(layout(1, 2), &sites, &out, WritePlan::SELECT, Code::tiered(Code::hand(fake_entry), u64::MAX, up)));
    let mut c = ProgramCache::new(1 << 20, 1);
    c.insert(None, tiered.clone());
    assert_eq!(c.l2_bytes(), 0, "filed while it interprets");
    assert!(tiered.code().tier0().expect("tier-0 code").promote());
    assert_eq!(tiered.code_bytes(), 4096, "promoted");
    c.insert(None, program(1, 100));
    assert_eq!(c.l2_bytes(), 100, "the tier-0 entry evicted by what it was counted at");
    let front = tiered.key;
    c.install(front, tiered.clone());
    c.install(front, program(2, 0));
    assert_eq!(c.l2_bytes(), 0, "the promoted entry, re-filed at its new size, evicted by that size");
}

/// D3615: a code that is one entry of `m`'s mapping (as the backend's `codes_of` makes them).
fn of_mapping(m: &Arc<Mapping>) -> Code {
    Code::mapped(fake_entry, m.0.len(), Box::new(m.clone())).of_mapping(Arc::as_ptr(m) as usize)
}

/// D3615: a program of one pipeline per code, every one but the last ending in a breaker.
fn piped(codes: Vec<Code>) -> Program {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_program::{End, Pipeline, Reason};
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::index_source(st.handle(), 1, 2, &[1, 2], &[0, 1], &[3, 4])];
    let out = [OutCol::new("a", 23, -1, 4, true), OutCol::new("b", 25, -1, -1, false)];
    let n = codes.len();
    let end = |i| if i + 1 < n { End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 16)) } else { End::Sink };
    Program::with_pipelines(layout(1, 2), &sites, &out, WritePlan::SELECT, codes.into_iter().enumerate().map(|(i, c)| (Pipeline::serial(-1, end(i), Reason::NoDrivingSite), c)))
}

/// D3615: one compile maps every pipeline's entry, and each of those codes reports the whole mapping's bytes; a program
/// is charged a mapping once (it was charged once per pipeline: a 101-arm Append read as 17 MB of code, past the
/// process-wide tier's shard budget, and was never filed), a second mapping adds its own, and a code with bytes of its
/// own (no mapping named) counts as before.
#[test]
fn d3615_a_program_is_charged_each_mapping_once() {
    let (m1, m2) = (Arc::new(Mapping(vec![0; 1000])), Arc::new(Mapping(vec![0; 300])));
    let one = piped(vec![of_mapping(&m1), of_mapping(&m1), of_mapping(&m1)]);
    assert_eq!(one.code_bytes(), 1000, "three pipelines of one mapping: its bytes once");
    let own = Code::mapped(fake_entry, 50, Box::new(Mapping(vec![0; 50])));
    let two = piped(vec![of_mapping(&m1), of_mapping(&m2), of_mapping(&m1), own]);
    assert_eq!(two.code_bytes(), 1000 + 300 + 50, "each mapping once, and the code of its own");
}

/// D3615: the process-wide tier still refuses a program that is genuinely over a shard's share: many pipelines of one
/// mapping under the share are filed, two mappings whose bytes together pass it are not.
#[test]
fn d3615_the_shared_tier_still_refuses_a_large_program() {
    use gravity_program::cache::{SharedCache, SHARDS};
    let tier: SharedCache<Arc<Program>> = SharedCache::new(SHARDS * 4, SHARDS * 1000);
    let (m1, m2) = (Arc::new(Mapping(vec![0; 600])), Arc::new(Mapping(vec![0; 600])));
    let many = Arc::new(piped((0..20).map(|_| of_mapping(&m1)).collect()));
    tier.insert(1, many.clone(), many.code_bytes());
    assert!(tier.contains(1), "20 pipelines of one 600-byte mapping fit a 1000-byte share");
    let large = Arc::new(piped(vec![of_mapping(&m1), of_mapping(&m2)]));
    assert_eq!(large.code_bytes(), 1200);
    tier.insert(2, large.clone(), large.code_bytes());
    assert!(!tier.contains(2), "1200 bytes of code over a 1000-byte share is not filed");
}
