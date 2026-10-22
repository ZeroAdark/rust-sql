use gravity_abi::Ctx;
use gravity_abi::breaker::{Breaker, BreakerKind, HashBuild, BUILD_PARTITION, BUILD_TABLE};
use gravity_program::{Arena, Code, End, Layout, Parallel, Pipeline, Program, Reason, SiteDesc, WritePlan};

unsafe extern "C-unwind" fn noop(_: *mut Ctx) -> i64 {
    0
}

/// Another body than `noop`'s: two identical functions may share one address (D2916), so the test tells them by result.
unsafe extern "C-unwind" fn noop2(_: *mut Ctx) -> i64 {
    1
}

fn layout(nsites: u16) -> Layout {
    Layout { nsites, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 }
}

fn stage() -> Arena {
    Arena::new(1 << 16)
}

#[test]
fn one_pipeline_programs_are_the_serial_case() {
    let st = stage();
    let p = Program::new(layout(1), &[SiteDesc::index_source(st.handle(), 1, 2, &[1], &[0], &[])], &[], WritePlan::SELECT, Code::hand(noop));
    assert_eq!(p.pipelines.len(), 1);
    assert_eq!(p.pipelines[0], Pipeline { driving_site: 0, ends_in: End::Sink, parallel: Parallel::Ineligible { site: 0, reason: Reason::IndexSource }, predicted_rows: 0, consumes: &[], consumes_from: &[], claims: gravity_program::ClaimKind::Source, owner: None, admit: None, words: None, topk: None, finish: false, store_row_bytes: 0, build_rows: 0, recheck_skip: false, lazy_for: -1, recheck_hold: false, recheck_hold_epq: false, held_sort: false, held_at: false, recheck: false, whole_read: false, ahead: None, pg_gate: -1, pg_regions: [-1; 4], quiet: false, licence: gravity_program::LicenceStamp::None, gate_tags: None, live_through: -1, rows_of: u16::MAX, build_of: u16::MAX, source_of: u16::MAX, pass_of: u16::MAX, row_width: 0, post: false, first_call: false, end_rate: gravity_program::END_RATE_NONE, end_rows: 0, end_of: u16::MAX, top_rows: 0, holds: None, refills: false, cascade: false });
    assert_eq!(p.worker_partial_bytes(), 0);
    assert_eq!(p.producers_of(0).count(), 0);
    let q = Program::new(layout(0), &[], &[], WritePlan::SELECT, Code::hand(noop));
    assert_eq!(q.pipelines[0].parallel, Parallel::Ineligible { site: -1, reason: Reason::NoDrivingSite });
}

#[test]
fn pipelines_enter_the_content_key_and_size_the_breaker_slot() {
    let st = stage();
    let sites = [SiteDesc::range_source(st.handle(), 7, &[1, 2])];
    assert_eq!((sites[0].nkeys(), sites[0].natts(), sites[0].is_source(), sites[0].is_range()), (2, 2, true, true));
    let two = |rows| Program::with_pipelines(layout(1), &sites, &[], WritePlan::SELECT, [(Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 16)), rows), Code::hand(noop)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(noop2))]);
    let a = two(10);
    let b = two(10);
    let c = two(11);
    assert_eq!(a.key, b.key);
    assert_ne!(a.key, c.key);
    assert_eq!(a.worker_partial_bytes(), 16);
    assert_eq!(a.producers_of(1).collect::<Vec<_>>(), vec![0]);
    assert_eq!(a.pipelines[1].consumes, &[0][..], "a sink pipeline declaring nothing consumes the chain's slot");
    assert_eq!((a.slots.bytes[0], a.slots.live_mask(0), a.slots.live_mask(1), a.slots.max_live, a.slots.nslots()), (16, 1, 1, 1, 1));
    // each pipeline's own entry, by what it returns (never by function address, D2916)
    assert_eq!(unsafe { (a.code_of(1).entry())(core::ptr::null_mut()) }, 1);
    assert_eq!(unsafe { (a.code().entry())(core::ptr::null_mut()) }, 0);
    let one = Program::new(layout(1), &sites, &[], WritePlan::SELECT, Code::hand(noop));
    assert_ne!(one.key, a.key);
    assert_eq!(one.pipelines[0].parallel, Parallel::Ineligible { site: 0, reason: Reason::Sink });
}

/// KEY-PREDICTED-ROWS road (b): an estimate that names its graph pipeline (`rows_of`) is bound per statement, so it is in
/// `key` and not in `bind_key`; one the lowering derived (`rows_of` none) is in both, and so is every fold (`with_out_region`).
#[test]
fn bind_key_leaves_out_the_estimates_an_admission_binds() {
    let st = stage();
    let sites = [SiteDesc::range_source(st.handle(), 7, &[1, 2])];
    let one = |rows, rows_of| {
        let p = Pipeline { rows_of, ..Pipeline::eligible(0, End::Sink, rows) };
        Program::with_pipelines(layout(1), &sites, &[], WritePlan::SELECT, [(p, Code::hand(noop))])
    };
    let (a, b) = (one(10, 0), one(5000, 0));
    assert_ne!(a.key, b.key, "the estimate is in the key");
    assert_eq!(a.bind_key, b.bind_key, "and not in the bind key");
    let (c, d) = (one(10, u16::MAX), one(5000, u16::MAX));
    assert_ne!(c.bind_key, d.bind_key, "a derived estimate is bound by no statement");
    assert_ne!(a.bind_key, c.bind_key, "rows_of is the program's");
    let (e, f) = (one(10, 0).with_out_region(8), one(5000, 0).with_out_region(16));
    assert_eq!(e.bind_key, one(10, 0).with_out_region(8).bind_key);
    assert_ne!(e.bind_key, f.bind_key, "a fold re-keys the bind key too");
    assert_ne!(e.bind_key, a.bind_key);
}

/// D3205: a pipeline's `end_rows` (the planner's rows at its end, a breaker's size under `pgrust.gravity_exact_size`) is in
/// `key` and not in `bind_key`: the content key leaves it out, so a shared program lowered for one literal serves another
/// (`generate_series(1, 100)` then `(1, 200)`), and the progcache check compares `bind_key`.
#[test]
fn bind_key_leaves_out_the_end_rows() {
    let st = stage();
    let sites = [SiteDesc::range_source(st.handle(), 7, &[1, 2])];
    let one = |end_rows| {
        let p = Pipeline { end_rows, ..Pipeline::serial(0, End::Sink, Reason::IndexSource) };
        Program::with_pipelines(layout(1), &sites, &[], WritePlan::SELECT, [(p, Code::hand(noop))])
    };
    let (a, b) = (one(100), one(200));
    assert_ne!(a.key, b.key, "the end rows are in the key");
    assert_eq!(a.bind_key, b.bind_key, "and not in the bind key");
}

/// D4002: the progcache check names what parts two programs' `bind_key`s (here a pipeline's `lazy_for`, not the bindable
/// estimate beside it), and `bind_key` leaves out every estimate the content key does (`estimates_out`): `end_rate`, which
/// the lowering decides on only through a guard (two plans one unit of rate apart raised the check on unnest.sql), and the
/// row width, which only sizes.
#[test]
fn bind_key_leaves_out_what_the_content_key_does_and_bind_diff_names_the_rest() {
    let st = stage();
    let sites = [SiteDesc::range_source(st.handle(), 7, &[1, 2])];
    let one = |rows, end_rate, row_width, lazy_for| {
        let p = Pipeline { rows_of: 0, end_rate, row_width, lazy_for, ..Pipeline::eligible(0, End::Sink, rows) };
        Program::with_pipelines(layout(1), &sites, &[], WritePlan::SELECT, [(p, Code::hand(noop))])
    };
    let (a, b) = (one(10, 111_111, 24, -1), one(5000, 111_110, 40, -1));
    assert_ne!(a.key, b.key, "the estimates are in the program's key");
    assert_eq!(a.bind_key, b.bind_key, "and none is in its bind key");
    let full = Pipeline { rows_of: 0, end_rate: 7, row_width: 9, end_rows: 11, build_rows: 13, ..Pipeline::eligible(0, End::Sink, 5) };
    assert_eq!(gravity_program::estimates_out(&full), Pipeline { rows_of: 0, end_rate: 0, ..Pipeline::eligible(0, End::Sink, 0) });
    let c = one(10, 111_111, 24, 0);
    assert_ne!(a.bind_key, c.bind_key);
    assert_eq!(a.bind_diff(&c), "pipeline 0's lazy_for (-1 / 0)");
    assert_eq!(one(10, 3, 0, -1).with_out_region(8).bind_diff(&one(10, 3, 0, -1).with_out_region(16)), "the out region (8 / 16)");
}

#[test]
#[should_panic(expected = "last pipeline must end in the sink")]
fn a_trailing_breaker_is_refused() {
    Program::with_pipelines(layout(0), &[], &[], WritePlan::SELECT, [(Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 16)), Reason::NoDrivingSite), Code::hand(noop))]);
}

#[test]
fn hash_build_partitions_and_tables() {
    let mut part = vec![0u64; HashBuild::bytes(4) as usize / 8];
    let p = unsafe { HashBuild::at(part.as_mut_ptr() as *mut u8, 4, BUILD_PARTITION) };
    for (k, v) in [(1, 10), (2, 20), (1, 11), (3, 30)] {
        assert!(p.insert(k, v));
    }
    assert!(!p.insert(4, 40));
    assert_eq!(p.entries().map(|e| (e.key, e.payload)).collect::<Vec<_>>(), vec![(1, 10), (2, 20), (1, 11), (3, 30)]);
    let mut tab = vec![0u64; HashBuild::bytes(16) as usize / 8];
    let t = unsafe { HashBuild::at(tab.as_mut_ptr() as *mut u8, 16, BUILD_TABLE) };
    for e in p.entries() {
        assert!(t.insert(e.key, e.payload));
    }
    let mut hits: Vec<i64> = t.probe(1).map(|e| e.payload).collect();
    hits.sort();
    assert_eq!(hits, vec![10, 11]);
    assert_eq!(t.probe(9).count(), 0);
    assert_eq!(t.len, 4);
}

#[test]
fn site_descriptors_list_their_lanes() {
    use gravity_abi::ctx::lane;
    use gravity_program::{LaneDesc, LaneId, ZoneOp, ZonePred};
    let st = stage();
    let a = st.handle();
    let r = SiteDesc::range_source(a, 7, &[3, 1]);
    assert_eq!(r.nlanes(), 3, "two attributes and the offset lane");
    assert_eq!(r.lanes()[0], LaneDesc::attr(3));
    assert_eq!(r.lanes()[2].id, LaneId::Offset);
    assert!(r.driving_lanes.eq_mask(0b11), "the attributes drive; the offset lane is derived");
    assert_eq!(r.lane_index(LaneId::Offset), Some(2));
    let i = SiteDesc::index_source(a, 1, 2, &[1], &[0], &[5]);
    assert!(i.nlanes() == 1 && i.driving_lanes.eq_mask(1));
    assert_eq!(SiteDesc::write(1, 4).nlanes(), 0);
    let z = r.clone().with_lane(LaneDesc::rowid(), true).with_zone(ZonePred::word(0, ZoneOp::Lt, 100));
    assert!(z.nlanes() == 4 && z.driving_lanes.eq_mask(0b1011) && z.zone_preds().len() == 1);
    assert_ne!(z, r, "lanes and zone predicates are part of the descriptor (and so of the content key)");
    let wide: Vec<i16> = (1..=300).collect();
    let w = SiteDesc::range_source(a, 7, &wide).filter_only(299).answered(70);
    assert!(w.nlanes() == 301 && w.driving_lanes.count() == 300 && w.filter_lanes.contains(299) && w.answer_lanes.contains(70) && !w.answer_lanes.contains(71));
    let p = Program::new(Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 }, &[w.clone()], &[], WritePlan::SELECT, Code::hand(noop));
    assert_eq!(p.sites[0], w, "the program's copy is the descriptor");
    assert!(p.arena_used() >= 301 * core::mem::size_of::<LaneDesc>() + 600, "the lists live in the program's arena");
    for id in [LaneId::Attr(-1), LaneId::Attr(7), LaneId::RowId, LaneId::Offset, LaneId::Verdict(3), LaneId::Projection(9), LaneId::JsonbPath(2)] {
        assert_eq!(LaneId::decode(id.encode()), Some(id));
    }
    assert_eq!(lane::id_kind(LaneId::Offset.encode()), lane::ID_OFFSET);
}

mod dag {
    use super::*;
    use gravity_abi::breaker::{Budget, Spill};
    fn produce(slot: u8) -> End {
        End::Breaker(Breaker::sized(BreakerKind::HashAgg, 64).in_slot(slot))
    }

    fn share(slot: u8) -> End {
        End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None })
    }

    fn build(pipes: &[Pipeline]) -> Program {
        let st = stage();
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        Program::with_pipelines(layout(1), &sites, &[], WritePlan::SELECT, pipes.iter().map(|p| (*p, Code::hand(noop))))
    }

    /// CAP-34: kernel words are u16 slots up to `limits::KERNEL_WORDS` (the word plan's bound); the one-owner-per-word check
    /// covers all of them. Words 5 and 37 are different words (a 32-bit mask folded 37 onto 5), and so are 300 and 44 (a u8
    /// slot space folded 300 onto 44).
    #[test]
    fn kernel_words_past_32_are_their_own_words() {
        use gravity_program::WordsDesc;
        let p = build(&[Pipeline::eligible(0, produce(0), 10).words(WordsDesc::bits_only(5, 6)), Pipeline::eligible(0, produce(1), 10).words(WordsDesc::bits_only(37, 38)), Pipeline::eligible(0, produce(2), 10).words(WordsDesc::bits_only(200, 201)), Pipeline::eligible(0, produce(3), 10).words(WordsDesc::bits_only(44, 45)), Pipeline::eligible(0, produce(4), 10).words(WordsDesc::bits_only(300, 301)), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1, 2, 3, 4])]);
        assert_eq!(p.pipelines.iter().filter_map(|q| q.words).map(|d| d.bits).collect::<Vec<_>>(), vec![5, 37, 200, 44, 300]);
    }

    #[test]
    #[should_panic(expected = "word 37 taken by pipeline 0")]
    fn one_kernel_word_past_32_in_two_pipelines_is_refused() {
        use gravity_program::WordsDesc;
        build(&[Pipeline::eligible(0, produce(0), 10).words(WordsDesc::bits_only(37, 38)), Pipeline::eligible(0, produce(1), 10).words(WordsDesc::bits_only(37, 38)), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1])]);
    }

    #[test]
    fn two_producers_into_one_consumer_and_the_lifetimes() {
        let p = build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, share(1), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1])]);
        assert_eq!(p.pipelines[1].consumes, &[][..], "a producer declaring nothing consumes nothing");
        assert_eq!(p.producers_of(2).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!((p.last_consumer(0), p.last_consumer(1), p.last_consumer(2)), (2, 2, 2));
        assert_eq!((p.slots.live_mask(0), p.slots.live_mask(1), p.slots.live_mask(2), p.slots.max_live), (0b01, 0b11, 0b11, 2));
        assert_eq!((p.slots.bytes[0], p.slots.bytes[1], p.slots.shared[0], p.slots.shared[1]), (64, 0, false, true));
        assert_eq!(p.worker_partial_bytes(), 64, "the share-budgeted slot needs no arena region");
        assert_eq!(p.slots.live_slots(1).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn a_slot_is_reused_after_its_last_consumer_and_a_chain_infers_the_nearest_producer() {
        let p = build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, produce(1), 10).consuming(&[0]), Pipeline::eligible(0, produce(0), 10).consuming(&[1]), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)]);
        assert_eq!(p.pipelines[3].consumes, &[0][..], "the chain: the nearest producer's slot");
        assert_eq!(p.producers_of(3).collect::<Vec<_>>(), vec![2], "slot 0's producer for pipeline 3 is pipeline 2, not 0");
        assert_eq!((0..4).map(|q| p.slots.live_mask(q)).collect::<Vec<_>>(), vec![0b01, 0b11, 0b11, 0b01]);
        assert_eq!(p.slots.max_live, 2);
        assert_eq!((p.last_consumer(0), p.last_consumer(1), p.last_consumer(2)), (1, 2, 3));
    }

    #[test]
    fn pipelines_and_slots_are_sized_per_program() {
        let n = 300;
        let mut pipes: Vec<Pipeline> = (0..n).map(|i| Pipeline::eligible(0, produce((i % 200) as u8), 10)).collect();
        for i in 200..n {
            pipes[i] = pipes[i].consuming(Box::leak(Box::new([((i + 1) % 200) as u8])));
        }
        pipes.push(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        let p = build(&pipes);
        assert_eq!((p.pipelines.len(), p.slots.nslots()), (n + 1, 200));
        assert!(p.slots.is_live(250, 51) && !p.slots.is_live(250, 101), "slot 51 lives until pipeline 250 consumes it; slot 101 has no consumer");
        assert_eq!((p.slots.live_slots(250).count(), p.slots.max_live), (51, 101));
        assert_eq!(p.producers_of(n).collect::<Vec<_>>(), vec![n - 1]);
    }

    #[test]
    #[should_panic(expected = "consumes slot 1 before any pipeline produced it")]
    fn a_consumed_slot_must_be_produced_earlier() {
        build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1])]);
    }

    #[test]
    fn producing_a_slot_again_ends_the_earlier_production() {
        let p = build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, produce(0), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0])]);
        assert_eq!(p.producers_of(2).collect::<Vec<_>>(), vec![1], "the consumer reads the nearest production");
        assert_eq!((p.last_consumer(0), p.last_consumer(1)), (0, 2), "pipeline 0's production has no consumer");
        assert_eq!((0..3).map(|q| p.slots.live_mask(q)).collect::<Vec<_>>(), vec![0b1, 0b1, 0b1]);
    }

    #[test]
    #[should_panic(expected = "consumes the slot it produces")]
    fn a_pipeline_never_consumes_its_own_slot() {
        build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, produce(0), 10).consuming(&[0]), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)]);
    }

    #[test]
    fn a_breaker_freed_before_the_peak_divides_the_grant_by_its_own_peak() {
        let p = build(&[Pipeline::eligible(0, share(0), 10), Pipeline::eligible(0, share(1), 10).consuming(&[0]), Pipeline::eligible(0, share(2), 10), Pipeline::eligible(0, share(3), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1, 2, 3])]);
        assert_eq!(p.slots.max_live, 3);
        assert_eq!((0..4).map(|s| p.slots.peak[s]).collect::<Vec<_>>(), vec![2, 3, 3, 3], "D150: slot 0 is freed before three are live");
        assert_eq!((p.slots.share_live(Some(0)), p.slots.share_live(Some(3)), p.slots.share_live(None)), (2, 3, 3));
    }

    /// D3034: a fixed slot live beside two share slots takes no part of the grant: each share divides it by the two share
    /// slots live at once (never by three: a third of the grant left unused, the share tables spilling), and never by
    /// fewer (two shares at half each hold the grant, no more).
    #[test]
    fn a_fixed_slot_live_beside_shares_takes_no_part_of_the_grant() {
        let p = build(&[Pipeline::eligible(0, share(0), 10), Pipeline::eligible(0, share(1), 10), Pipeline::eligible(0, produce(2), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1, 2])]);
        assert_eq!(p.slots.max_live, 3, "three slots live at the sink");
        assert_eq!((0..3).map(|s| p.slots.peak[s]).collect::<Vec<_>>(), vec![2, 2, 2]);
        assert_eq!((p.slots.share_live(Some(0)), p.slots.share_live(Some(1))), (2, 2));
        // a share alone beside a fixed slot (a group table under a count-ordered top's owner) holds the whole grant
        let q = build(&[Pipeline::eligible(0, share(0), 10), Pipeline::eligible(0, produce(1), 10).consuming(&[0]), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1])]);
        assert_eq!((q.slots.max_live, q.slots.share_live(Some(0))), (2, 1));
    }

    #[test]
    fn a_high_slot_sizes_the_plan() {
        let p = build(&[Pipeline::eligible(0, produce(200), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)]);
        assert_eq!((p.slots.nslots(), p.slots.bytes[200], p.slots.max_live), (201, 64, 1));
        assert_eq!(p.slots.live_slots(1).collect::<Vec<_>>(), vec![200]);
    }
}


mod wide {
    use super::*;
    use gravity_abi::breaker::{Budget, KeyKind, SlotKind, Spill, ORDER_NONE};
    use gravity_program::{record_layout, OwnerDesc};

    fn produce(slot: u8) -> End {
        End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None })
    }

    fn build(pipes: &[Pipeline]) -> Program {
        let st = stage();
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        Program::with_pipelines(layout(1), &sites, &[], WritePlan::SELECT, pipes.iter().map(|p| (*p, Code::hand(noop))))
    }

    /// Decision 81: a pipeline consumes any number of slots; the DAG rules hold over all of them.
    #[test]
    fn a_pipeline_consumes_three_slots() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let emit = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(a, 0).consuming_in(a, 1).consuming_in(a, 2);
        assert_eq!((emit.consumes, emit.nconsumed()), (&[0, 1, 2][..], 3));
        let p = build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, produce(1), 10), Pipeline::eligible(0, produce(2), 10), emit]);
        assert_eq!(p.producers_of(3).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!((p.last_consumer(0), p.last_consumer(1), p.last_consumer(2)), (3, 3, 3));
        assert!(p.slots.is_live(3, 0) && p.slots.is_live(3, 1) && p.slots.is_live(3, 2), "all three live into the emit");
    }

    /// D2495: a lazy build's pipelines (`lazy_for` = q: the build and a producer fed into it) run at q's first row, beside
    /// the slot q produces, so the plan counts their slots live through q: q's share divides the grant three ways, not two.
    #[test]
    fn a_lazy_builds_slots_are_live_through_its_probe() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut fed = Pipeline::eligible(0, produce(0), 10);
        fed.lazy_for = 2;
        let mut lazy = Pipeline::eligible(0, produce(1), 10).consuming_in(a, 0);
        lazy.lazy_for = 2;
        let probe = Pipeline::eligible(0, produce(2), 10).consuming_in(a, 1);
        let emit = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(a, 2);
        let p = build(&[fed, lazy, probe, emit]);
        assert!(p.slots.is_live(2, 0) && p.slots.is_live(2, 1) && p.slots.is_live(2, 2), "the fed slot is live at the probe");
        assert!(!p.slots.is_live(3, 0), "and dead after it");
        assert_eq!((p.slots.share_live(Some(0)), p.slots.share_live(Some(2)), p.slots.max_live), (3, 3, 3));
    }

    /// D2495: a producer fed into a lazy build that is itself lazy for an outer probe runs at that probe's first row too, so its
    /// slot is live through the outermost probe and the four slots open there split the grant four ways.
    #[test]
    fn a_nested_lazy_builds_slots_are_live_through_the_outer_probe() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut fed = Pipeline::eligible(0, produce(2), 10);
        fed.lazy_for = 2;
        let mut fed2 = Pipeline::eligible(0, produce(3), 10);
        fed2.lazy_for = 2;
        let mut inner = Pipeline::eligible(0, produce(0), 10).consuming_in(a, 2).consuming_in(a, 3);
        inner.lazy_for = 3;
        let probe = Pipeline::eligible(0, produce(1), 10).consuming_in(a, 0);
        let emit = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(a, 1);
        let p = build(&[fed, fed2, inner, probe, emit]);
        assert!((0..4).all(|s| p.slots.is_live(3, s)), "all four live at the outer probe");
        assert_eq!((p.slots.share_live(Some(2)), p.slots.share_live(Some(1)), p.slots.max_live), (4, 4, 4));
    }

    /// Each copied consumed-slot slice takes whole arena words: two multi-slot consumers (2 + 3 slots, one word summed, two taken)
    /// once left the slot plan's live words past the arena (a 40-byte allocation failure aborted the server: L4b's staged Grace form).
    #[test]
    fn two_multi_slot_consumers_fit_the_program_arena() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mid = Pipeline::eligible(0, produce(2), 10).consuming_in(a, 0).consuming_in(a, 1);
        let emit = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(a, 0).consuming_in(a, 1).consuming_in(a, 2);
        let p = build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, produce(1), 10), mid, emit]);
        assert_eq!((p.pipelines[2].consumes, p.pipelines[3].consumes), (&[0, 1][..], &[0, 1, 2][..]));
        assert!(p.slots.is_live(3, 0) && p.slots.is_live(3, 2));
    }

    /// The consumed-slot slice a caller built in its own arena is copied into the program's: the lowerer's arena is
    /// reset after `with_pipelines` (rung 9 need 6 read `[0, 0]` through a dangling slice).
    #[test]
    fn a_consumed_slice_built_elsewhere_is_owned_by_the_program() {
        let arena = Arena::new(1 << 16);
        let emit = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(arena.handle(), 0).consuming_in(arena.handle(), 1);
        let p = build(&[Pipeline::eligible(0, produce(0), 10), Pipeline::eligible(0, produce(1), 10), emit]);
        assert_ne!(p.pipelines[2].consumes.as_ptr() as usize, emit.consumes.as_ptr() as usize, "the program holds its own copy");
        arena.handle().zero(0, arena.handle().used());
        drop(arena);
        assert_eq!(p.pipelines[2].consumes, &[0, 1][..]);
    }

    #[test]
    #[should_panic(expected = "consumes one slot twice")]
    fn a_pipeline_never_consumes_a_slot_twice() {
        let arena = Arena::new(1 << 12);
        let _ = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(arena.handle(), 0).consuming_in(arena.handle(), 0);
    }

    /// The content key covers the wide tier: two programs differing only past the eighth key word differ.
    #[test]
    fn the_content_key_covers_the_wide_layout() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let keys = |last: KeyKind| -> Vec<KeyKind> {
            let mut k = vec![KeyKind::I64; 9];
            k[8] = last;
            k
        };
        let slots = [SlotKind::SumI64; 9];
        let (l1, l2) = (record_layout(a, &keys(KeyKind::I64), &slots, true), record_layout(a, &keys(KeyKind::I32), &slots, true));
        assert!(l1.is_wide() && l2.is_wide() && l1 != l2);
        let small = record_layout(a, &keys(KeyKind::I64)[..2], &slots[..2], true);
        assert!(!small.is_wide() && small == gravity_abi::breaker::RecordLayout::new(&[KeyKind::I64; 2], &[SlotKind::SumI64; 2]));
        let program = |l| build(&[Pipeline::eligible(0, produce(0), 10).owner(OwnerDesc::generic(l, ORDER_NONE, 0)), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)]);
        assert_ne!(program(l1).key, program(l2).key);
        assert_eq!(program(l1).key, program(record_layout(a, &keys(KeyKind::I64), &slots, true)).key, "equal by content");
    }
}

#[test]
fn the_entries_declaration_is_additive_and_enters_the_content_key() {
    let st = stage();
    let rows = SiteDesc::range_source(st.handle(), 7, &[1]).codes_lane(0);
    assert_eq!(rows.entries, None, "off unless declared");
    let entries = rows.copy_in(st.handle()).entries_lane(0);
    assert_eq!((entries.entries, entries.copy_in(st.handle()).entries), (Some(0), Some(0)));
    let key = |s: &SiteDesc| Program::new(layout(1), core::slice::from_ref(s), &[], WritePlan::SELECT, Code::hand(noop)).key;
    assert_ne!(key(&rows), key(&entries), "a rows program is not an entries program");
    assert_eq!(key(&entries), key(&rows.copy_in(st.handle()).entries_lane(0)));
}

#[test]
fn a_shred_lane_is_an_attribute_lane_before_the_offset_and_enters_the_content_key() {
    use gravity_program::{shred_kind, LaneId};
    let st = stage();
    let mut s = SiteDesc::range_source(st.handle(), 7, &[1, 3]);
    let plain = s.copy_in(st.handle());
    assert_eq!(s.add_shred(3, b"k", shred_kind::TEXT, 0), Some(2), "after the attributes");
    assert_eq!(s.add_shred(3, b"k", shred_kind::TEXT, 0), Some(2), "one lane per declaration");
    assert_eq!(s.add_shred(3, b"a.b", shred_kind::TEXT, 0), Some(3));
    assert_eq!(s.natts(), 4);
    assert_eq!(&s.out_attnos[..], &[1, 3, 3, 3]);
    assert_eq!(s.lanes()[2].id, LaneId::JsonbPath(0));
    assert_eq!(s.lanes()[3].id, LaneId::JsonbPath(1));
    assert_eq!(s.lanes()[4].id, LaneId::Offset, "the offset lane stays last");
    assert!(s.driving_lanes.contains(2) && s.driving_lanes.contains(3));
    assert_eq!(s.shred_of(3).map(|d| s.table_arg_at(d.path)), Some(&b"a.b"[..]));
    assert_eq!(s.add_shred(3, b"n", shred_kind::NUMERIC_FS, 2), None, "D2a builds Text lanes only");
    let copy = s.copy_in(st.handle());
    assert_eq!(copy.shreds.len(), 2);
    let key = |s: &SiteDesc| Program::new(layout(1), core::slice::from_ref(s), &[], WritePlan::SELECT, Code::hand(noop)).key;
    assert_ne!(key(&plain), key(&s), "a lane program is not the value form's");
    assert_eq!(key(&s), key(&copy));
    let mut other = plain.copy_in(st.handle());
    let _ = other.add_shred(3, b"j", shred_kind::TEXT, 0);
    let _ = other.add_shred(3, b"a.b", shred_kind::TEXT, 0);
    assert_ne!(key(&other), key(&s), "the path enters the key");
}

#[test]
#[should_panic(expected = "the entries lane is the site's one attribute")]
fn the_entries_lane_is_the_sites_one_code_attribute() {
    let st = stage();
    let _ = SiteDesc::range_source(st.handle(), 7, &[1, 2]).codes_lane(0).entries_lane(0);
}
