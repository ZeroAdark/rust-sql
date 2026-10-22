//! D262: the steps every producer takes after it ran (D133's wall for a spilled hash build no pipeline reads, the partition
//! phase's claim total) run before the v4 protocols, so a protocol that acts after a producer it reads (TwoPass's pass 1,
//! Marks, RtFilter's build) never skips them; only the shape table's own steps are skipped when one acts.

use std::cell::Cell;

use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_driver::Error;
use gravity_exec::runner::producer_ran;
use gravity_program::{ClaimKind, End, Pipeline, Reason};

/// The execution the steps see: each slot's spill partitions, and whether a v4 protocol ran after the producer.
struct Ran {
    parts: [u32; 4],
    v4: Cell<bool>,
}

fn breaker(kind: BreakerKind, slot: u8) -> End {
    End::Breaker(Breaker { kind, slot, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } })
}

/// A producer ending in `kind` at slot 0, a pipeline over slot 0's spill partitions (when `read`), and the sink.
fn pipes(kind: BreakerKind, read: bool) -> Vec<Pipeline> {
    let mut over = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
    if read {
        over.claims = ClaimKind::SpillPartitions { slot: 0 };
    }
    vec![Pipeline::eligible(0, breaker(kind, 0), 1000), over]
}

/// The steps after pipeline 0 with a v4 protocol that acts (`Some(Ok)`, as TwoPass / Marks / RtFilter do after the producer
/// they read): the totals set, and the result.
fn after(pipes: &[Pipeline], parts: u32, acts: Option<Result<(), Error>>) -> (Vec<(u8, u64)>, Option<Result<(), Error>>, bool) {
    let mut ran = Ran { parts: [parts, 0, 0, 0], v4: Cell::new(false) };
    let mut totals = Vec::new();
    let r = producer_ran(pipes, 0, &mut ran, |e: &Ran, s| e.parts[s as usize], &mut |s, n| totals.push((s, n)), |e: &mut Ran| {
        e.v4.set(true);
        acts
    });
    (totals, r, ran.v4.get())
}

#[test]
fn a_spilled_build_a_protocol_reads_still_hands_its_partition_phase_the_claim_total() {
    for kind in [BreakerKind::HashBuild, BreakerKind::HashAgg] {
        let (totals, r, v4) = after(&pipes(kind, true), 8, Some(Ok(())));
        assert_eq!(totals, vec![(0, 8)], "{kind:?}: the partition phase's total before the protocol acted");
        assert!(matches!(r, Some(Ok(()))) && v4, "{kind:?}: the protocol ran and acted");
    }
}

#[test]
fn a_spilled_build_no_pipeline_reads_meets_the_d133_wall_even_when_a_protocol_reads_it() {
    let (_, r, v4) = after(&pipes(BreakerKind::HashBuild, false), 8, Some(Ok(())));
    match r {
        Some(Err(Error::Host(m))) => assert!(m.contains("D133"), "the D133 wall: {m}"),
        other => panic!("a spilled unread build probed an emptied table: {other:?}"),
    }
    assert!(!v4, "the wall stops before the protocols");
}

#[test]
fn an_unspilled_producer_runs_the_protocols_and_a_silent_one_leaves_the_shape_steps() {
    let (totals, r, v4) = after(&pipes(BreakerKind::HashBuild, false), 0, Some(Ok(())));
    assert!(totals == vec![(0, 0)] && matches!(r, Some(Ok(()))) && v4);
    let (_, r, v4) = after(&pipes(BreakerKind::HashAgg, true), 0, None);
    assert!(r.is_none() && v4, "no protocol acted: the shape table's steps run");
    let (_, r, _) = after(&pipes(BreakerKind::HashAgg, true), 8, Some(Err(Error::host("pass 1 spilled"))));
    assert!(matches!(r, Some(Err(Error::Host(_)))), "a protocol's error is the result");
}
