// Breaker lives: every consumer names the life it reads, validate_dag checks it, Program::lives lists them.
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::Ctx;
use gravity_program::{Arena, Code, End, Layout, LifeBytes, LifeId, Pipeline, Program, Reason, SiteDesc, WritePlan};

unsafe extern "C-unwind" fn noop(_: *mut Ctx) -> i64 {
    0
}

fn fixed(slot: u8) -> End {
    End::Breaker(Breaker::sized(BreakerKind::HashAgg, 64).in_slot(slot))
}

fn share(slot: u8) -> End {
    End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None })
}

fn build(pipes: &[Pipeline]) -> Program {
    let st = Arena::new(1 << 16);
    let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
    let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
    Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.iter().map(|p| (*p, Code::hand(noop))))
}

fn sink() -> Pipeline {
    Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)
}

#[test]
fn a_reused_slot_has_two_lives_and_each_consumer_reads_its_own() {
    let a = Arena::new(1 << 16);
    let h = a.handle();
    let p = build(&[
        Pipeline::eligible(0, share(0), 10),
        sink().consuming_from_in(h, 0, LifeId(0)),
        Pipeline::eligible(0, fixed(0), 10),
        Pipeline::serial(-1, share(1), Reason::NoDrivingSite).consuming_from_in(h, 0, LifeId(2)),
        sink().consuming_from_in(h, 1, LifeId(3)),
    ]);
    drop(a);
    let lives: Vec<_> = p.lives().collect();
    assert_eq!(lives.iter().map(|l| (l.id, l.slot, l.producer, l.last_consumer)).collect::<Vec<_>>(), vec![(LifeId(0), 0, 0, 1), (LifeId(2), 0, 2, 3), (LifeId(3), 1, 3, 4)]);
    assert_eq!((lives[0].bytes, lives[1].bytes), (LifeBytes::Share, LifeBytes::Fixed(64)));
    assert!(lives[0].parallel && !lives[2].parallel);
    assert_eq!(p.consumers_of(LifeId(2)).collect::<Vec<_>>(), vec![3]);
    assert_eq!((p.life_of(3, 0), p.life_of(4, 1)), (Some(LifeId(2)), Some(LifeId(3))));
    assert_eq!(p.pipelines[3].consumes_from, &[LifeId(2)][..], "the program holds its own copy");
}

#[test]
fn a_pipeline_without_lives_reads_the_nearest_producer() {
    let p = build(&[Pipeline::eligible(0, share(0), 10), sink().consuming(&[0])]);
    assert_eq!(p.life_of(1, 0), Some(LifeId(0)));
    assert_eq!(p.lives().next().unwrap().last_consumer, 1);
}

#[test]
#[should_panic(expected = "past the slot's next producer")]
fn reading_a_life_past_its_slots_next_producer_is_refused() {
    let a = Arena::new(1 << 16);
    build(&[Pipeline::eligible(0, share(0), 10), Pipeline::eligible(0, share(0), 10), sink().consuming_from_in(a.handle(), 0, LifeId(0))]);
}

#[test]
#[should_panic(expected = "which does not produce it")]
fn a_life_must_produce_the_slot() {
    let a = Arena::new(1 << 16);
    build(&[Pipeline::eligible(0, share(0), 10), Pipeline::eligible(0, share(1), 10), sink().consuming_from_in(a.handle(), 0, LifeId(1))]);
}

/// S6: a reader of a slot declares it. The hash join's partition pipe reads the spilled outer rows, so the outer slot's
/// life ends at that pipe, not at the probe that produced it.
#[cfg(feature = "hand")]
#[test]
fn the_partition_pipe_consumes_the_spilled_outer_slot() {
    use gravity_kernels::par_hashjoin::{SLOT_BUILD, SLOT_OUTER};
    let p = gravity_program::hand::par_hashjoin(7, 8);
    let part = p.pipelines.iter().position(|q| matches!(q.claims, gravity_program::ClaimKind::SpillPartitions { .. })).unwrap();
    let probe = p.pipelines.iter().position(|q| q.produces() == Some(SLOT_OUTER)).unwrap();
    assert!(p.pipelines[part].consumes.contains(&SLOT_BUILD) && p.pipelines[part].consumes.contains(&SLOT_OUTER));
    assert_eq!(p.last_consumer(probe), part);
}
