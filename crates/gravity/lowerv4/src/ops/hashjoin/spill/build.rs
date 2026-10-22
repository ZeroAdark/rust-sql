//! The Grace spill of a join.inner build past its limit (M7-SPILL SPILL-1, V4R-359; M7-plan §3.10, decision 5 (a)). The
//! build end grows only within its owner's limit (W; the leader's G too: driver `region_grow_in`). Refused, it writes the
//! records of the upper half of its resident partitions (at last of 0 too: `wider`) to its owner's file of the build
//! slot's stream (S5, driver `hostimpl::spill::v4`) and keeps the rest; each later row of a spilled partition goes after
//! them. The merge unions the
//! owners' spilled partitions and writes the leader's records of each the same way (`merge_into`, `finish_room`). The
//! lay-out takes only the resident records. The probe keeps the rows of spilled partitions, and the pair pipeline joins
//! each spilled partition with its kept rows (join/spill_pair.rs). There is one partition level: a partition that still
//! does not fit grows by name. A partition is `route`'s bits of the KeyHash, remixed, so a partition's records still
//! spread over the whole directory. The face decides whether a build may spill (`SpillMech::build_bits`, kind
//! spill.breakers: keyed/spill.rs delegates to this file's row). Owner M7-SPILL (M1 scope).
//! Budget 620 lines.
//!
//! Header bytes (agreed with M7-DRAIN): `SPILL` holds the partition bits once the build spilled (0: never).
//! `PBITS` holds the bits a build that may spill uses (0: it never spills). Bit p of `SPILLED` is set when partition p
//! is in the files. A spilled record in a file is the record's bytes, with each image word (a by-distance payload,
//! `DESC`) replaced by the image's length, then the images, each padded to 8 bytes.
//!
//! SAFETY (parallel builds, W >= 2; memory.md S5): a worker's grow, route and flush write only
//! its own index of the slot's stream (the driver's `spill_index`: its own stage and its own file), never another
//! owner's; the leader's merge writes index 0 after the section, when no worker writes; the pair pipeline reads every
//! owner's parts only after the build's and the probe's sections flushed and ended. The W16 pass of e2e m7_spill_inner
//! runs it.
use gravity_abi::status::raise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::spill::NoSpill;
use mcx::CollectIn;

use crate::face::kinds::KindId;
use crate::face::spill::{SpillDecl, SpillImpl, SpillMech};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;

pub(crate) use crate::ops::hashjoin::spill::raw::region::{any_spilled, finish_room, grow_leader, image, merge_into, put_record, serialized_len, spilling};
use crate::ops::hashjoin::spill::raw::region::{spill_flush, spill_grow, spill_route};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::JoinInner, &SpillImpl(&BUILD))], ..crate::ops::NONE };

pub(crate) const SPILL: usize = 112; // u8
pub(crate) const PBITS: usize = 113; // u8
pub(crate) const SPILLED: usize = 120; // u64
/// D4904 (strategies/dedup.rs `compact`): u8, 1 once a keys-only build's pass kept most of its records (no further pass
/// this open: the driver zeroes it at every open, as the spill words).
pub(crate) const KEYS_DONE: usize = 115;
// A build that never spills stores none of its spill words (`init`): the driver zeroes them at every open (V4R-359).
const _: () = assert!(SPILL < SPILLED && PBITS < SPILLED && KEYS_DONE < SPILLED && SPILLED + 8 <= gravity_driver::node::ZERO_PREFIX);
/// A spilling build routes at the finest partition count its `SPILLED` mask holds (64), and a spill sends out only the
/// share of them its grant needs (`wider`), so the share written follows the build's bytes over its grant (JOIN-12). One
/// level, no recursive repartition: measured, 64 partitions cost nothing at W0 against 2 and spread the pair pass better at
/// W8; a partition past its region cap sub-splits (D2715), and one past the grant grows by name.
pub(crate) const BITS: u8 = 6;

/// The partition of KeyHash `h` among `2^bits` (`bits` 1..=6): the top bits of the hash remixed by splitmix64's
/// finalizer, so no partition bit is a directory home bit or a bloom tag bit. The IR's `emit_route` is the same function.
pub(crate) fn route(h: u64, bits: u8) -> u32 {
    (remix(h) >> (64 - bits as u32)) as u32
}

/// splitmix64's finalizer of KeyHash `h`: `route` reads its top bits, D2715's split (spill/load.rs `fine`) the next ones.
pub(crate) fn remix(h: u64) -> u64 {
    let mut z = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `route` of KeyHash word `h` (`I64`) at `bits` (`I64`, 1..=6), as IR.
pub(crate) fn emit_route(b: &mut FunctionBuilder<'_>, h: Value, bits: Value) -> Value {
    let mix = |b: &mut FunctionBuilder<'_>, z: Value, s: i64, m: u64| {
        let sc = b.iconst(Type::I64, s);
        let x = b.int_bin(IntBinOp::Ushr, z, sc);
        let x = b.int_bin(IntBinOp::Xor, z, x);
        let mc = b.iconst(Type::I64, m as i64);
        b.int_bin(IntBinOp::Mul, x, mc)
    };
    let z = mix(b, h, 30, 0xbf58_476d_1ce4_e5b9);
    let z = mix(b, z, 27, 0x94d0_49bb_1331_11eb);
    let s31 = b.iconst(Type::I64, 31);
    let t = b.int_bin(IntBinOp::Ushr, z, s31);
    let z = b.int_bin(IntBinOp::Xor, z, t);
    let sixty4 = b.iconst(Type::I64, 64);
    let sh = b.int_bin(IntBinOp::Sub, sixty4, bits);
    b.int_bin(IntBinOp::Ushr, z, sh)
}

/// The mask a spill widens `m` to at `bits` (JOIN-12): the upper half of the partitions still resident goes out, so the
/// resident ones stay a prefix `0 .. r` (an owners' union of such masks is one too) and each spill halves `r`. The share
/// spilled then follows the build's bytes over its grant at run time: half of a build that passes it once, three quarters
/// of one that passes it twice, as PostgreSQL's ExecHashIncreaseNumBatches doubles nbatch, where a fixed share sent
/// every partition but 0 out of a build just past its grant. Partition 0 goes last; when all are out, `m`.
pub(crate) fn wider(m: u64, bits: u8) -> u64 {
    let all = if bits >= 6 { u64::MAX } else { (1u64 << (1u32 << bits)) - 1 };
    match (m & all).trailing_zeros().min(all.count_ones()) {
        0 => m | all,
        1 => all,
        r => m | (all & !((1u64 << (r / 2)) - 1)),
    }
}

struct Build;
static BUILD: Build = Build;

impl SpillMech for Build {
    fn spill_row(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _d: &SpillDecl, _h: Value, _row: Value, _bytes: Value) {
        crate::refuse!(env.node, Reason::Bug { payload: "a kind-7 build spills through its grow, not spill_row" })
    }

    fn reload(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _d: &SpillDecl, _x: &Exits) {
        crate::refuse!(env.node, Reason::Bug { payload: "a kind-7 build reloads in its pair pipeline, not reload" })
    }

    /// B3: the build may spill when all of these hold. Its probe is op `j` of `q`, an INNER, LEFT, SEMI or ANTI
    /// probe that is not lazy, deferred, catalog or swapped. Only filters and projections follow the probe, into a sink or
    /// (B4b-1) a plain fold / full sort end (`end_takes_pair`). No rerun set
    /// holds either pipeline or the slot. The program has no write or lock.
    fn build_bits(&self, px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> Option<u8> {
        eligible(px, k, q, j).then_some(BITS)
    }
}

/// The partition bits of build `k` probed at op `j` of `q` when the spill face lets it spill (kind spill.breakers).
pub(crate) fn build_bits(px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> Option<u8> {
    use crate::face::spill::SpillForm;
    if !SpillForm::Build.on() {
        return None;
    }
    let node = px.gx.pipelines()[k].node_id;
    SpillForm::Build.need::<SpillImpl>(&node).0.build_bits(px, k, q, j)
}

/// C10: why build `k` probed at op `j` of `q` keeps growing past its grant when `build_bits` gives it no bits: the probe's
/// shape (`eligible`'s reason), else the face (off, or a spill form with no build road).
pub(crate) fn no_bits(px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> NoSpill {
    eligible_why(px, k, q, j).err().unwrap_or(NoSpill::BuildFaceOff)
}

/// D2084: a probe under a union's arm spills when every arm of its one union (`arm_run`: consecutive pipelines, no inner
/// union) probes build `k` and may spill on its own: each arm keeps its rows into the one stream, and the pair follows the
/// run's last arm (pair.rs `pair`). A drained end's arms (V4R-533) keep named growth.
fn eligible(px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> bool {
    eligible_why(px, k, q, j).is_ok()
}

/// `eligible`, with the reason it is not (C10: `spill::NoSpill`, one lint/SPILL-DEBT row each).
fn eligible_why(px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> Result<(), NoSpill> {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::Op;
    if !under_arm(px.gx, q, j) {
        return alone(px, k, q, j);
    }
    let gx = px.gx;
    let Some((first, n)) = arm_run(gx, q) else { return Err(NoSpill::ArmsNotOneRun) };
    let Some(Op::Join(JoinDesc::Probe { slot, .. })) = gx.pipelines()[q].ops.get(j) else { return Err(NoSpill::ArmsOpNotProbe) };
    let probe_at = |a: usize| gx.pipelines()[a].ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Probe { slot: s, .. }) if s == slot));
    if gx.end_maker(q) == KindId::JoinDrain {
        return Err(NoSpill::ArmsDrainEnd);
    }
    for a in first..first + n {
        match probe_at(a) {
            Some(ja) if gx.life_of(a, *slot) == Some(gravity_program::LifeId(k as u16)) => alone(px, k, a, ja)?,
            _ => return Err(NoSpill::ArmsApart),
        }
    }
    Ok(())
}

/// D4275: whether probe `j` of `q` sits under a union's arm, read from the graph (an `Arm` marker before it), never from
/// the unions lowering has registered so far (`Px::union_of`). The build is made before its probe's pipelines, so it saw
/// no union and spilled on its first arm's `alone` while each arm's `pair` saw the union and kept nothing when a later arm
/// failed (a constant, VALUES, SRF or aggregated arm): the spilled partitions had no reader (the runner's D133 error). A
/// join's Drain op registers a union too, but it is none here: its tail's probe answers `alone` at the build and at `pair`.
fn under_arm(gx: &crate::graph::Gx<'_, '_>, q: usize, j: usize) -> bool {
    gx.pipelines().get(q).is_some_and(|p| p.ops.iter().take(j).any(|o| crate::graph::arm_at(o).is_some()))
}

/// D2084: the run of the streamed union pipeline `q` is an arm of (`Gx::run_arms`, the arms writing its slot; a sink end's:
/// `Gx::stream_arms`), as `(first, len)`, when the arms are consecutive pipelines, arm `i` the run's `i`-th, every arm of
/// the union present, and each holds exactly one `Arm` marker (no inner union); `None` otherwise.
pub(crate) fn arm_run(gx: &crate::graph::Gx<'_, '_>, q: usize) -> Option<(usize, usize)> {
    let run = gx.run_arms(q);
    let run = if run.is_empty() { gx.stream_arms(q) } else { run };
    if run.is_empty() {
        return None;
    }
    let mut arms = run.into_iter().chain([q]).collect_in(gx.mcx);
    arms.sort_unstable();
    let ps = gx.pipelines();
    let n = arms.len();
    let marker = |p: usize| {
        let mut m = ps[p].ops.iter().filter_map(crate::graph::arm_at);
        match (m.next(), m.next()) {
            (Some(a), None) => Some(a),
            _ => None,
        }
    };
    (arms.windows(2).all(|w| w[1] == w[0] + 1) && arms.iter().enumerate().all(|(x, &p)| marker(p) == Some((x as u16, n as u16)))).then_some((arms[0], n))
}

/// The B3 rule (`build_bits`) for probe `j` of `q` alone, with the reason it does not hold (C10).
fn alone(px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> Result<(), NoSpill> {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::Op;
    use types_nodes::jointype::JoinType;
    let gx = px.gx;
    let (Some(b), Some(p)) = (gx.pipelines().get(k), gx.pipelines().get(q)) else { return Err(NoSpill::ProbeShape) };
    if gx.spooled_drain(q).is_some() {
        return Err(NoSpill::SpooledDrain); // A1b N2 (join/drain.rs): a pair would remake the spooled probe's end, a second spool of one probe
    }
    let Some(br) = crate::frame::iface::breaker_of(&b.desc) else { return Err(NoSpill::ProbeShape) };
    let Some(Op::Join(JoinDesc::Probe { slot, jointype, catalog: false, keys, .. })) = p.ops.get(j) else { return Err(NoSpill::ProbeShape) };
    // D1899: a key whose domain CHECK calls VOLATILE runs once per row (PG keeps a batch's outer hash values), where the
    // pair pipeline would run a kept row's key again
    if keys.iter().any(|n| crate::effects::volatile_check(gx, n)) {
        return Err(NoSpill::VolatileDomainKey);
    }
    // B4a: LEFT, SEMI and ANTI keep as INNER does; B4c: FULL and the RIGHT forms spill, their drain sending
    // each partition's drain part (spill_load.rs, drain_src.rs phase 2)
    if !matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI) {
        return Err(NoSpill::JoinTypeOutside);
    }
    if *slot != br.slot {
        return Err(NoSpill::ProbeSlotApart);
    }
    pair_end(gx, q, false)?;
    // D2717: PG's outer-first path over a build that may raise or call spills too. A lazy build (D824) runs whole at the
    // probe's first row (the driver's width-1 out-of-order arm), then spills and is probed as an eager one is; op 14's
    // deferred build spills as it runs and holds its error, which the probe's first row raises before any row is kept
    // (rt.rs `defer_error` clears the spill words of the FAILED region, so the pair pass reads no partition of it)
    // the pair pipeline's copy of the consumed table (join/spill_pair.rs `MORE`); its nominal driving site is the probe's
    if p.desc.consumes.len() > crate::ops::hashjoin::spill::pair::MORE || p.desc.driving_site < 0 {
        return Err(NoSpill::ConsumesMore);
    }
    // D3918: under a full sort end the pipeline's last op is the sort's own step, a pass-through (ops/sort/lower.rs
    // `Pass`: the end takes the row as it is). This rule kept every probe under a plan Sort from B4b-1's end
    // (`end_takes_pair`), its build growing by name. `P'` makes that step again with the end. A form-G sort
    // (`run_holds_rows`: it orders a drained group's records), a top and a presorted run are other kinds' steps.
    let sort = |i: usize| i + 1 == p.ops.len() && gx.step_need(q, i) == KindId::RunsSort && gx.end_maker(q) == KindId::RunsSort && crate::frame::iface::run_holds_rows(gx, q);
    if !(j + 1..p.ops.len()).all(|i| matches!(gx.step_need(q, i), KindId::StepFilter | KindId::StepProject) || sort(i)) {
        return Err(NoSpill::ChainTail);
    }
    if crate::lifetime::hashjoin::resident_in_rerun(gx, k, q, br.slot) {
        return Err(NoSpill::ResidentInRerun);
    }
    if gx.pipelines().iter().any(|x| x.ops.iter().any(|o| matches!(o, Op::Lock(_)) || matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select))) {
        return Err(NoSpill::ProgramWrites);
    }
    match remade_set(gx, q, j) {
        true => Err(NoSpill::RemadeSet),
        false => Ok(()),
    }
}

/// Whether an op the pair pipeline makes again (pair.rs `pair`: from the SubPlan steps right before probe `j` of `q` to the
/// end) embeds, at any depth, an inner whose re-open runs a rerun set (rerun/sets.rs `owner_set`: a correlated SubPlan over a
/// plain aggregate, a sort run, a hash build of its set). `P'` would run that set under its own virtual id, and the set's
/// record is not proven to be `P'`'s region: a SubPlan over avg() in an inner join's qual read no row of a spilled build's
/// kept partitions right (gen sp-0111 / 0114, witness subplan_spill_pass_region). The build keeps named growth instead.
fn remade_set(gx: &crate::graph::Gx<'_, '_>, q: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    let j0 = (0..j).rev().take_while(|&i| matches!(ps[q].ops[i], gravity_decompose::Op::SubPlan(_))).last().unwrap_or(j);
    let inners = ps.inners();
    let under = |mut at: (usize, usize)| {
        while let Some(x) = at.0.checked_sub(ps.len()) {
            at = inners[x].0;
        }
        at.0 == q && at.1 >= j0
    };
    inners.iter().enumerate().any(|(x, i)| under(i.0) && crate::rerun::sets::owner_set(gx, ps.len() + x).is_some())
}

/// B4b-1 (V4R-404; the B4b split, PlainAgg + Sort first): the probe pipeline `q`'s end takes the pair pipeline's rows: a sink, or a plain fold / full sort end
/// whose arms continue one slot (source/append.rs `continues`, the Arms road on), `P'` its second arm (spill_pair.rs
/// `pair`). `P'`'s cells are datums: a fold with a scan-form register (`scan_register`) keeps named growth. A full sort
/// keys each cell by its own rep (none `P'` hands on in another form: ops/shared/sort.rs `unlike_pair`). D3057: a keyed group's end (owner phase, partitioned spill, registers as `regs_take_pair` allows)
/// takes the pair too; PS1-16's ops/hashjoin/outer_kept.rs reads this, so a key past such a probe keeps its value.
pub(crate) fn end_takes_pair(gx: &crate::graph::Gx<'_, '_>, q: usize) -> bool {
    pair_end(gx, q, false).is_ok()
}

/// `end_takes_pair`, with the reason the end takes no pair (C10), of `q` reached through a seeded drain when `via_drain` (`q`
/// is a drain pipeline whose end the probe's rows reach, D4053's G1).
fn pair_end(gx: &crate::graph::Gx<'_, '_>, q: usize, via_drain: bool) -> Result<(), NoSpill> {
    crate::stack_check();
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::Op;
    use gravity_abi::breaker::BreakerKind;
    use crate::frame::iface::breaker_of;
    let d = gx.pipelines()[q].desc;
    // B4c-2: a RIGHT / FULL probe's end is the drain's spool `m`; the end the rows reach is the drain pipeline's (join/drain.rs
    // fuses a sink onto the probe or seeds a fold / full sort), and P' takes it the same way. A drain end another pipeline
    // also feeds (an Append's arms: a partitionwise join's seeded drain per partition, D1187) keeps named growth: the pair
    // would splice into that run of arms (partition_join-007 / -079)
    if gx.end_maker(q) == KindId::JoinDrain {
        let Some(m) = breaker_of(&d) else { return Err(NoSpill::DrainShape) };
        let drain = |p: &gravity_decompose::graph::PipelineNode<'_>| p.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Drain { matches, .. }) if *matches == m.slot));
        let Some((x, _)) = gx.pipelines().iter().enumerate().skip(q + 1).find(|(x, p)| drain(p) && gx.life_of(*x, m.slot) == Some(gravity_program::LifeId(q as u16))) else { return Err(NoSpill::DrainShape) };
        let shared = breaker_of(&gx.pipelines()[x].desc)
            .is_some_and(|e| gx.pipelines().iter().enumerate().any(|(y, p)| y != x && breaker_of(&p.desc).is_some_and(|o| o.slot == e.slot)));
        return if shared { Err(NoSpill::DrainShared) } else { pair_end(gx, x, true) };
    }
    let ok = |t: bool, w: NoSpill| if t { Ok(()) } else { Err(w) };
    match (gx.end_kind(q), breaker_of(&d)) {
        (crate::graph::EndKind::Sink, _) => ok(gx.end_maker(q) == KindId::EndSink, NoSpill::SinkNotEndSink),
        (crate::graph::EndKind::Breaker(_), Some(_)) if !crate::face::end::Parallel::Arms.on() => Err(NoSpill::EndArmsFaceOff),
        (crate::graph::EndKind::Breaker(_), Some(b)) => match b.kind {
            BreakerKind::Sort => ok(gx.end_maker(q) == KindId::RunsSort && !crate::ops::shared::sort::unlike_pair(gx, q), NoSpill::SortNotPair),
            BreakerKind::PlainAgg => {
                ok(gx.end_maker(q) == KindId::FoldPlain && gx.group_at(q).is_some_and(|g| regs_take_pair(gx, q, g, via_drain)), NoSpill::PlainScanRegister)
            }
            // D3057 (JOIN-12b) S1: a keyed group's end takes `P'` as the group's next arm, as a seeded drain's arm continues
            // it (drain/lower.rs `continues`: an owner phase, a partitioned spill). A key past the probe keeps its value
            // (outer_kept declines whenever this answers yes), so `q` and `P'` key alike; a scan-form register keeps named growth
            BreakerKind::HashAgg => {
                use gravity_abi::breaker::Spill;
                let pair = matches!(gx.end_maker(q), KindId::KeyedHashagg | KindId::KeyedEmitGroup)
                    && d.owner.is_some()
                    && matches!(b.spill, Spill::Partitioned { .. })
                    && gx.group_at(q).is_some_and(|g| regs_take_pair(gx, q, g, via_drain));
                ok(pair, NoSpill::HashAggNotPair)
            }
            // D3057: another hash build's end keeps named growth. `P'` would be another arm of that build's records, and its
            // datum cells would settle against the probe arm's code words (D2854's one form per column, ops/hashjoin/build.rs)
            BreakerKind::HashBuild => Err(NoSpill::EndHashBuild),
            _ => Err(NoSpill::EndOther),
        },
        _ => Err(NoSpill::EndRowOp),
    }
}

/// Whether the fold of group `g` (pipeline `q`'s end) takes `P'`. A code register (MinCode / MaxCode) never does: its cells
/// are a scan lane's codes, which `P'` does not carry. D4053 (D3119-L): a numeric register (SumNum / AvgNum / MinNum / MaxNum)
/// does, its kept rows' datums folded through their mantissa at the register's scale (pair.rs, num_word.rs `word`), unless
/// G1 the end is reached through a seeded drain (`via_drain`, dsx-0065), G2 the probe consumes a rerun set's slot (rnd-1062)
/// or G3 an Fp128 group key past the probe reaches a scan column (PS1-16's outer_kept keys it by its code; without G3 such
/// a fold measured 11 % more instructions at W8).
fn regs_take_pair(gx: &crate::graph::Gx<'_, '_>, q: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, via_drain: bool) -> bool {
    use gravity_decompose::ops::shared::agg::Register;
    if g.aggs.iter().any(|a| matches!(a.reg, Register::MinCode | Register::MaxCode)) {
        return false;
    }
    !g.aggs.iter().any(|a| scan_register(a.reg)) || (!via_drain && !g2_rerun_slot(gx, q) && !g3_fp_key(gx, q, g))
}

/// G2 (D4053): probe pipeline `q` consumes a slot a rerun set holds (rnd-1062: `P'` copies `q`'s consumed slots and would open
/// over the set's slot, which only its owner may: driver node/mod.rs RT-2e).
fn g2_rerun_slot(gx: &crate::graph::Gx<'_, '_>, q: usize) -> bool {
    gx.pipelines()[q].desc.consumes.iter().any(|&s| gx.graph.reruns.iter().any(|r| gravity_decompose::graph::slot_has(&r.slots, s)))
}

/// G3 (D4053): an Fp128 group key of `g` that walks back through `q`'s projections, filters and probes to a scan column
/// (emit_group.rs `past_var` with every probe taken as outer_kept): the key PS1-16 keys by its code when the end takes no
/// pair (outer_kept declines whenever `end_takes_pair` answers yes).
fn g3_fp_key(gx: &crate::graph::Gx<'_, '_>, q: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    use gravity_abi::breaker::KeyKind;
    use gravity_decompose::Op;
    use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
    let ops = &gx.pipelines()[q].ops;
    let Some(i0) = ops.iter().rposition(|op| matches!(op, Op::Project(_))) else { return false };
    let reaches = |mut pos: usize, mut a: i16| -> bool {
        loop {
            let Some(i) = ops[..pos].iter().rposition(|op| !matches!(op, Op::Filter(_))) else { return false };
            match &ops[i] {
                Op::Project(l) => {
                    let Some(v) = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == a).and_then(|te| crate::table::emit_group::bare(te.expr).as_var()) else { return false };
                    if v.varlevelsup != 0 || v.varno == INNER_VAR {
                        return false;
                    }
                    if v.varno != OUTER_VAR {
                        return true;
                    }
                    (pos, a) = (i, v.varattno);
                }
                Op::Join(gravity_decompose::join::JoinDesc::Probe { .. }) => pos = i,
                _ => return false,
            }
        }
    };
    g.keys.iter().filter(|k| k.kind == KeyKind::Fp128 && k.resno > 0).filter_map(|k| k.expr.node().as_var()).filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0).any(|v| reaches(i0, v.varattno))
}

/// An aggregate register decompose chose from a scan lane's code or packed-numeric form (MinCode / MaxCode, SumNum / AvgNum /
/// MinNum / MaxNum). D3119 excluded them all from `P'` after dsx-0065 and rnd-1062 went SERVED to REFUSED; D4053 lets the
/// numeric ones take it again behind those two statements' guards (`regs_take_pair`: G1, G2, plus G3).
fn scan_register(r: gravity_decompose::ops::shared::agg::Register) -> bool {
    use gravity_decompose::ops::shared::agg::Register;
    matches!(r, Register::MinCode | Register::MaxCode | Register::SumNum | Register::AvgNum | Register::MinNum | Register::MaxNum)
}

// ---- the build end's IR (join/build.rs) ----

fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], f: usize) -> gravity_ir::HostFn {
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    b.declare_host_once(name, params, Some(Type::I64), f, flags)
}

/// A negative status of a host call raises its held error (the HOST raise).
pub(crate) fn raise_neg(b: &mut FunctionBuilder<'_>, env: &Env, st: Value) {
    let failed = b.icmp(IntCond::Slt, st, env.zero);
    let (e, l_ok) = (b.iconst(Type::I64, raise::bit(raise::HOST) as i64), b.create_block(&[]));
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
}

/// The spill words of a fresh region (`open`): never spilled, spilling at `bits`. A build that never spills (`bits` 0) stores
/// nothing: every open zeroes the region's first `ZERO_PREFIX` bytes (driver node.rs `open_slot`; a worker's partial and an
/// owner's cut the same, section.rs), so its words read 0 (the assertion above keeps them inside that prefix).
pub(crate) fn init(b: &mut FunctionBuilder<'_>, r: Value, bits: u8) {
    if bits == 0 {
        return;
    }
    let (z8, pb, z) = (b.iconst(Type::I8, 0), b.iconst(Type::I8, bits as i64), b.iconst(Type::I64, 0));
    (b.store(r, SPILL as i32, z8), b.store(r, PBITS as i32, pb), b.store(r, SPILLED as i32, z));
}

/// A row that found no room in region `r` (`n` records of `rec` bytes, its images undone to `top0`, the header stamped):
/// the region grown within its limit, else spilled (`spill_grow`); the base and the records kept (`I64`, `I64`).
pub(crate) fn grow(b: &mut FunctionBuilder<'_>, env: &Env, r: Value, n: Value, rec: usize, top0: Value) -> (Value, Value) {
    let h = host(b, "m7_spill_grow", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr], gravity_abi::host_words!(spill_grow(ctx, r, n, rec, top0, out)));
    let slot = b.declare_stack_slot(8, 8);
    let out = b.stack_addr(slot);
    let recc = b.iconst(Type::I64, rec as i64);
    let base = b.call_host(h, &[env.ctx, r, n, recc, top0, out], Some(Type::I64)).expect("m7_spill_grow returns the base");
    let failed = b.icmp(IntCond::Eq, base, env.zero);
    let (e, l_ok) = (b.iconst(Type::I64, raise::bit(raise::HOST) as i64), b.create_block(&[]));
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    (base, b.load(Type::I64, out, 0))
}

/// After record `n` of region `r` is stored (its images from `top0` down): 1 when it stays, 0 when its partition spilled
/// and it went to the file (`spill_route`). The host is called only while `SPILL` is set; otherwise it is one branch per row.
pub(crate) fn route_row(b: &mut FunctionBuilder<'_>, env: &Env, r: Value, n: Value, top0: Value) -> Value {
    let sp = b.load_zext(Type::I8, r, SPILL as i32);
    let on = b.icmp(IntCond::Ne, sp, env.zero);
    let (l_route, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(on, (l_route, &[]), (l_join, &[env.one]));
    b.switch_to(l_route);
    let h = host(b, "m7_spill_route", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(spill_route(ctx, r, n, top0)));
    let kept = b.call_host(h, &[env.ctx, r, n, top0], Some(Type::I64)).expect("m7_spill_route returns its verdict");
    raise_neg(b, env, kept);
    b.br(l_join, &[kept]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// When region `r` spilled, the owner's staged bytes of `stream` are written: 0 names its own slot (the build end's
/// `finish`), `slot + 1` the kept rows (the probe's `epilogue`).
pub(crate) fn flush(b: &mut FunctionBuilder<'_>, env: &Env, r: Value, stream: u64) {
    let sp = b.load_zext(Type::I8, r, SPILL as i32);
    let on = b.icmp(IntCond::Ne, sp, env.zero);
    let (l_f, l_go) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(on, (l_f, &[]), (l_go, &[]));
    b.switch_to(l_f);
    let h = host(b, "m7_spill_flush", &[Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(spill_flush(ctx, stream, bits)));
    let s = b.iconst(Type::I64, stream as i64);
    let st = b.call_host(h, &[env.ctx, s, sp], Some(Type::I64)).expect("m7_spill_flush returns its status");
    raise_neg(b, env, st);
    b.br(l_go, &[]);
    b.switch_to(l_go);
}


/// A record or kept row of at most these bytes is laid out on its host's stack on its way to a file (`spill_route`, load.rs
/// `kept_put`): one per row of a spilled partition.
pub(crate) const ROW_STACK: usize = 1024;


#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::ops::hashjoin::build::{D_VARLENA, END, FLAGS, HDR, LIMIT, N, PHASE, REC, TOP, WORDS};
    use crate::ops::hashjoin::rt::{rd, wr, Rt};
    use crate::ops::hashjoin::spill::raw::region::scatter;
    use crate::ops::hashjoin::build::DESC;
    use gravity_abi::hash::KeyHash;

    /// `route` spreads a run of consecutive keys over every partition, and the spread does not depend on the directory's
    /// home bits (the top bits of the KeyHash): each partition's keys still reach most homes.
    #[test]
    fn route_spreads_consecutive_keys_over_every_partition_and_home() {
        let mut parts = [0u32; 1 << BITS];
        let mut homes = std::collections::HashSet::new();
        for k in 0..(1000u64 << BITS) {
            let h = KeyHash::of(&[k]).to_abi();
            let p = route(h, BITS);
            parts[p as usize] += 1;
            if p == 3 {
                homes.insert(h >> (64 - 10));
            }
        }
        assert!(parts.iter().all(|&c| c > 700 && c < 1300), "{parts:?}");
        assert!(homes.len() > 600, "partition 3's keys reach {} of 1024 homes", homes.len());
    }

    /// A region of one-word keys `keys`, each with a text payload `"v<key>"` repeated `key % 7 + 1` times (a varlena
    /// image kept by distance from `END`); `len` bytes.
    fn region(keys: &[i64], len: usize) -> Vec<u64> {
        let mut r = vec![0u64; len / 8];
        let d = r.as_mut_ptr() as *mut u8;
        // SAFETY: every offset below is inside `r`'s `len` bytes.
        unsafe {
            (wr::<u8>(d, WORDS, 1), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u32>(d, REC, 24), wr::<u16>(d, DESC, D_VARLENA), wr::<u8>(d, PHASE, 1));
            (wr::<u64>(d, END, len as u64), wr::<u32>(d, LIMIT, len as u32));
            let mut top = len;
            for (i, &k) in keys.iter().enumerate() {
                let text = format!("v{k}").repeat(k as usize % 7 + 1);
                let n = 4 + text.len();
                top -= n.next_multiple_of(8);
                wr::<u32>(d, top, (n as u32) << 2);
                core::ptr::copy_nonoverlapping(text.as_ptr(), d.add(top + 4), text.len());
                let rec = d.add(HDR + 24 * i);
                (wr::<i64>(rec, 0, k), wr::<u64>(rec, 8, 0), wr::<u64>(rec, 16, (len - top) as u64));
            }
            (wr::<u64>(d, TOP, top as u64), wr::<u64>(d, N, keys.len() as u64));
        }
        r
    }

    /// Each record's key and text (read through its distance from `END`).
    unsafe fn records(d: *mut u8) -> Vec<(i64, String)> {
        let t = Rt::of(d, usize::MAX);
        (0..t.n).map(|i| {
            let (off, len) = image(&t, i, 0).expect("a text image");
            (t.key(i), String::from_utf8(core::slice::from_raw_parts(d.add(off + 4), len - 4).to_vec()).unwrap())
        }).collect()
    }

    /// `scatter` hands out exactly the records of the masked partitions and keeps the rest with their images compacted up
    /// to `END`; `put_record` of what it handed out rebuilds them in another region.
    #[test]
    fn scattered_records_round_trip_and_the_kept_images_stay_intact() {
        let keys: Vec<i64> = (0..400).map(|k| k * 37 % 1000).collect();
        let len = 64 << 10;
        let mut r = region(&keys, len);
        let d = r.as_mut_ptr() as *mut u8;
        // SAFETY: `d` is `region`'s region of `len` bytes; the second region below is laid the same way.
        unsafe {
            let before = records(d);
            let mask = wider(0, BITS) & 0x5555_5555_5555_5554;
            let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
            let a = gravity_fn::arena::OpArena::scratch_of(None, gravity_fn::arena::Kind::HashBuild);
            let kept = scatter(&a, d, len, keys.len(), mask, BITS, &mut |p, b| Ok(out.push((p, b.to_vec())))).unwrap();
            let t = Rt::of(d, len);
            let routed = |k: i64| route(KeyHash::of(&[k as u64]).to_abi(), BITS);
            assert!(out.iter().all(|(p, _)| mask >> p & 1 == 1) && (0..t.n).all(|i| mask >> routed(t.key(i)) & 1 == 0));
            assert_eq!(kept + out.len(), keys.len());
            let after = records(d);
            let expect: Vec<_> = before.iter().filter(|(k, _)| mask >> routed(*k) & 1 == 0).cloned().collect();
            assert_eq!(after, expect, "the kept records in order, their texts intact");
            assert!(rd::<u64>(d, TOP) as usize >= HDR + 24 * kept);
            let mut r2 = region(&[], len);
            let d2 = r2.as_mut_ptr() as *mut u8;
            for (_, b) in out.iter() {
                assert_eq!((put_record(d2, b), serialized_len(d2, b.as_ptr())), (b.len(), b.len()));
            }
            let mut back = records(d2);
            let mut gone: Vec<_> = before.iter().filter(|(k, _)| mask >> routed(*k) & 1 == 1).cloned().collect();
            (back.sort(), gone.sort());
            assert_eq!(back, gone);
        }
    }

    /// B4c: a keeping, marking build (RIGHT / FULL / RIGHT ANTI) scatters its NULL-key records with the rest: each keeps its
    /// NULLKEY link through the file, and the resident region and a reloaded partition each split them past their keyed
    /// records at the lay-out, with one zeroed mark per record; together they hold every record once.
    #[test]
    fn a_keeping_build_scatters_its_null_key_records_and_both_sides_split_them() {
        use crate::ops::hashjoin::build::{ALLOW, A_KEEP, A_MARKS, A_TAGS, F_RANGE, KEPT, LOW, NULLKEY};
        let keys: Vec<i64> = (0..300).map(|k| k * 37 % 1000).collect();
        let is_null = |i: usize| i % 5 == 0;
        let len = 64 << 10;
        let (mut r, mut r2) = (region(&keys, len), region(&[], len));
        let (d, d2) = (r.as_mut_ptr() as *mut u8, r2.as_mut_ptr() as *mut u8);
        // SAFETY: both regions are `region`'s, `len` bytes each, ours alone.
        unsafe {
            for d in [d, d2] {
                (wr::<u8>(d, ALLOW, A_TAGS | A_KEEP | A_MARKS), wr::<u8>(d, FLAGS, F_RANGE));
            }
            for i in (0..keys.len()).filter(|&i| is_null(i)) {
                // a NULL-key row's image word is 0 (the build stores no key), its link says NULLKEY
                (wr::<i64>(d.add(HDR + 24 * i), 0, 0), wr::<u64>(d.add(HDR + 24 * i), 8, NULLKEY));
            }
            wr::<u64>(d, KEPT, keys.len().div_ceil(5) as u64);
            let null_part = route(KeyHash::of(&[0]).to_abi(), BITS);
            let mask = 1u64 << null_part;
            let mut out: Vec<Vec<u8>> = Vec::new();
            let a = gravity_fn::arena::OpArena::scratch_of(None, gravity_fn::arena::Kind::HashBuild);
            let kept = scatter(&a, d, len, keys.len(), mask, BITS, &mut |_, b| Ok(out.push(b.to_vec()))).unwrap();
            let nulls_out = out.iter().filter(|b| rd::<u64>(b.as_ptr(), 8) & LOW == NULLKEY).count();
            assert_eq!(nulls_out, keys.len().div_ceil(5), "every NULL-key record went with its partition, NULLKEY intact");
            for b in &out {
                put_record(d2, b);
            }
            let split = |d: *mut u8| {
                crate::ops::hashjoin::rt::lay_out(d, len).unwrap();
                let t = Rt::of(d, len);
                assert!((t.n..t.n + t.kept).all(|i| rd::<u64>(t.rec(i), 8) & LOW == 0), "a kept record's link is cleared");
                assert!((0..t.n + t.kept).all(|i| *d.add(t.marks_at() + i) == 0), "one zeroed mark per record");
                (t.n, t.kept)
            };
            let ((n1, k1), (n2, k2)) = (split(d), split(d2));
            assert_eq!((k1, k2), (0, nulls_out), "the resident region keeps none of the moved NULL-key records");
            assert_eq!((n1 + k1, n1 + n2 + k1 + k2), (kept, keys.len()), "every record once");
        }
    }

    /// JOIN-12: each spill sends the upper half of the resident partitions out, partition 0 last; then the mask stays.
    #[test]
    fn a_spill_halves_the_resident_partitions_then_takes_partition_0() {
        let mut m = 0;
        for want in [0xFFFF_FFFF_0000_0000u64, 0xFFFF_FFFF_FFFF_0000, 0xFFFF_FFFF_FFFF_FF00, u64::MAX << 4, u64::MAX << 2, u64::MAX << 1, u64::MAX, u64::MAX] {
            m = wider(m, 6);
            assert_eq!(m, want);
        }
        assert_eq!((wider(0, 5), wider(0xFFFF_0000, 5), wider(0xFFFF_FFFE, 5), wider(0xFFFF_FFFF, 5)), (0xFFFF_0000, 0xFFFF_FF00, 0xFFFF_FFFF, 0xFFFF_FFFF));
        assert_eq!((wider(0, 1), wider(0b10, 1), wider(0b11, 1)), (0b10, 0b11, 0b11));
        // two owners' masks (resident prefixes 0 .. 16 and 0 .. 8) unite to the smaller prefix, which widens to 0 .. 4
        assert_eq!(wider(0xFFFF_FFFF_FFFF_0000 | 0xFFFF_FFFF_FFFF_FF00, 6), u64::MAX << 4);
    }

    /// D4275: build B0 (p0) probed through a UNION ALL's two arms, a scan (p1) and a constant (p2, no driving site), each
    /// into a sink; p3 a scan probe of B0 under no arm. The build is made before its probe's arms register their unions,
    /// so the arm is read from the graph: the build and each arm's `pair` answer alike, and the constant arm's reason keeps
    /// B0 resident, though the scan arm alone may spill. A registered union with no `Arm` marker (a join's Drain op) is no
    /// union here.
    #[test]
    fn a_union_probed_build_decides_from_every_arm_before_the_arms_are_made() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_decompose::ops::shared::append::AppendDesc;
        use gravity_decompose::join::JoinDesc;
        use gravity_decompose::{Am, NoFacts, Op, PipelineGraph, PipelineNode, Source};
        use gravity_program::{Arena, ArenaVec, End, Pipeline, SiteDesc};
        use types_nodes::jointype::JoinType;
        use types_nodes::list::NodeList;
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 1, relid: 16384 };
        let why = gravity_program::Reason::NoDrivingSite;
        let mut b0 = PipelineNode::new(a, 2, scan(), Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 4096).in_slot(0)), why));
        (b0.push(Op::Project(nil)), b0.push(Op::Join(JoinDesc::Build { keys: nil, decls: ArenaVec::new_in(a), pg_tuple: u32::MAX })));
        let probe = || Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 2, outer_first: false, catalog: false });
        let arm = |i: u16| Op::Append(AppendDesc::Arm { i, n: 2, ordered: false, prune: None });
        let pipe = |source, site: i16, ops: Vec<_>| {
            let mut p = PipelineNode::new(a, 1, source, Pipeline::serial(site, End::Sink, why).consuming(&[0]));
            ops.into_iter().for_each(|o| p.push(o));
            p
        };
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(b0);
        g.pipelines.push(pipe(scan(), 0, vec![arm(0), probe(), Op::Project(nil)]));
        g.pipelines.push(pipe(Source::Const, -1, vec![arm(1), probe(), Op::Project(nil)]));
        g.pipelines.push(pipe(scan(), 0, vec![probe(), Op::Project(nil)]));
        let gx = crate::graph::Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = gravity_lower::Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        assert_eq!((super::alone(&px, 0, 1, 1), super::alone(&px, 0, 2, 1)), (Ok(()), Err(NoSpill::ConsumesMore)), "the scan arm alone may spill");
        assert_eq!(super::arm_run(px.gx, 1), Some((1, 2)));
        // the build's question (no union registered yet), then each arm's after its marker registered the union
        let at_build = [super::eligible_why(&px, 0, 1, 1), super::eligible_why(&px, 0, 2, 1)];
        assert_eq!(at_build, [Err(NoSpill::ConsumesMore), Err(NoSpill::ConsumesMore)], "every arm answers at the build");
        // the resident build's declared debt (build.rs `make`: no bits, so `holds` answers Grows(no_bits)) is the failing
        // arm's own reason, ConsumesMore, the row its closing (a driving site for the arm) retires
        assert_eq!((super::build_bits(&px, 0, 1, 1), super::no_bits(&px, 0, 1, 1)), (None, NoSpill::ConsumesMore));
        (px.set_union(1, 1, 0), px.set_union(2, 1, 0));
        assert_eq!([super::eligible_why(&px, 0, 1, 1), super::eligible_why(&px, 0, 2, 1)], at_build, "pair answers as the build did");
        // a union a Drain op registers is no arm: p3's probe answers alone
        assert_eq!(super::eligible_why(&px, 0, 3, 0), Ok(()));
        px.set_union(3, 1, 0);
        assert_eq!(super::eligible_why(&px, 0, 3, 0), Ok(()), "a Drain's union is none");
    }
}
