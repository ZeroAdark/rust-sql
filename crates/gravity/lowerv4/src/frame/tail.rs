//! M7-DRAIN (V4R-378; main 05:48Z tail direction, 07:29Z this file): a pipeline's tail, the steps another graph pipeline's
//! makers make into it after its own (join.drain's fused form, M7-plan §3.6: the probe pipeline runs the drain pipeline's ops
//! after the Drain op into the drain's end), and the slots a program pipeline consumes when not its graph pipeline's (a drain
//! that never reads the probe's spool; a probe that runs a drain's ops). frame.rs's `resolve` appends a pipeline's tail
//! before its end; nothing declared, nothing changes. And renumber_seeded (D1188) and merge_arms (D1187). M1-CORE scope, owner M7-DRAIN. Budget 96.
use gravity_kernels::ops::shared::object::entries::SUBHASH_LAZY;
use gravity_program::protos::{Proto, ProtoKind, NO_SLOT};
use gravity_program::{ArenaRef, Pipeline};
use mcx::{PgBox, PgVec};

use crate::face::step::StepMech;
use crate::frame::frame::rename_slot;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};

/// A pipeline's tail and consumed slots (see the file's head).
pub struct Tail<'g> {
    pub ident: usize,
    pub shape: usize,
    /// The graph pipeline whose makers declared `consumes` (a fused drain's probe: its own slots lead the list).
    pub of: usize,
    pub consumes: Vec<u8>,
    pub steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
}

impl<'g> Px<'g, '_> {
    /// The slots the pipeline being made consumes (`consumed` indexes them), declared by a maker that runs before the
    /// makers that read a slot's region (a source; an end, for its tail's steps).
    pub fn set_consumes(&mut self, consumes: Vec<u8>) {
        let (ident, mcx) = (self.ident(self.making.0), self.mcx());
        match self.tails.iter_mut().find(|t| t.ident == ident) {
            Some(t) => t.consumes = consumes,
            None => self.tails.push(Tail { ident, shape: ident, of: self.making.0, consumes, steps: PgVec::new_in(mcx) }), // alloc: Program
        }
    }

    /// Opens a tail of graph pipeline `shape`'s ops for the pipeline being made (its end's maker): the steps are made next
    /// under `making(shape, ident)` and handed in by `set_tail`; `resolve` appends them before the end.
    pub fn open_tail(&mut self, shape: usize, consumes: Vec<u8>) {
        self.set_consumes(consumes);
        let ident = self.ident(self.making.0);
        if let Some(t) = self.tails.iter_mut().find(|t| t.ident == ident && t.shape == ident) {
            t.shape = shape;
        } else {
            self.bug("two tails for one pipeline");
        }
    }

    pub fn set_tail(&mut self, ident: usize, steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>) {
        match self.tails.iter_mut().find(|t| t.ident == ident && t.shape != ident) {
            Some(t) => t.steps = steps,
            None => self.bug("a tail's steps without its open"),
        }
    }

    pub(crate) fn take_tail(&mut self, ident: usize) -> PgVec<'g, PgBox<'g, dyn StepMech + 'g>> {
        match self.tails.iter().position(|t| t.ident == ident) {
            Some(i) => self.tails.remove(i).steps,
            None => PgVec::new_in(self.mcx()),
        }
    }

    /// Whether the makers running are a tail's (`making` = its shape, its ident): its steps share their union words
    /// with the shape's own (step/limit.rs `Scope::Union`).
    pub(crate) fn tailing(&self) -> bool {
        self.making.0 != self.making.1 && self.tails.iter().any(|t| (t.shape, t.ident) == self.making)
    }

    /// The index of `slot` among the slots pipeline `k`'s program descriptor consumes (a probe's `region`): the declared
    /// list of the pipeline `k`'s makers declare under, else graph pipeline `k`'s.
    /// D2325: a LAZY hashed SubPlan's set slot is not among them: frame/program.rs drops it from every reader's program
    /// descriptor (no reader waits on it; the probe reads the set through `m8_subhash_run`), so the indices past it shift
    /// down, and this index skips it (its SubHash proto is declared at the set's end, before any reader is made).
    /// D3527: a declared list skips them too (frame/program.rs drops the set's slot from that call's descriptor as well):
    /// a fused drain's tail read the next build's region one past the descriptor's list and the probe crashed. Each slot
    /// of the list is read from the life its declaring pipeline reads, the drain's for the tail's own slots.
    pub fn consumed(&self, k: usize, slot: u8) -> Option<usize> {
        let ps = self.gx.pipelines();
        match self.tails.iter().find(|t| t.ident == self.ident(k)) {
            Some(t) => {
                let from = |s: u8| if t.of < ps.len() && ps[t.of].desc.consumes.contains(&s) { t.of } else { t.shape };
                t.consumes.iter().filter(|&&s| s == slot || !self.lazy_set(from(s), s)).position(|&s| s == slot)
            }
            None => {
                // an embedded inner runs in its owner's call, whose descriptor lists its inners' slots (Pipes::consumed_at)
                let mut k = k;
                while k >= ps.len() {
                    k = ps.inners()[k - ps.len()].0 .0;
                }
                ps[k].desc.consumes.iter().filter(|&&s| s == slot || !self.lazy_set(k, s)).position(|&s| s == slot)
            }
        }
    }

    /// Whether graph pipeline `k` reads slot `s` from a LAZY hashed SubPlan's set (D2325).
    fn lazy_set(&self, k: usize, s: u8) -> bool {
        if k >= self.gx.pipelines().len() {
            return false;
        }
        let life = self.gx.life_of(k, s).map(|l| l.0 as usize);
        self.protos.iter().any(|p| p.kind == ProtoKind::SubHash && p.arg & SUBHASH_LAZY != 0 && Some(p.pipeline as usize) == life)
    }
}

/// D1188 (found pre-landing in DRAIN-1, main 11:59Z): the seeded form makes the probe pipeline the first producer of the
/// drain end's slot (its `Arms`, the drain `Follows` it). Decompose frees a slot at its last reader, and that may be the
/// probe (a HashAgg its outer side reads, then the drain's sort numbered 1 again): the probe would read the slot it
/// produces. Such a slot takes the lowest number no pipeline uses: the probe's end and Arms, then the drain's life.
/// D1331: join.drain's seeded form (the probe the first arm of a breaker end the drain feeds) is not built; it refuses by this
/// Shape, never by NotBuilt of join.drain, a kind that is built (tests/m1.rs no_refusal_names_a_built_variant).
pub(crate) const SEEDED_DRAIN: &str = "join.drain's seeded form (a probe under a breaker end)";

pub(crate) fn seeded_drain(node: i32) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(SEEDED_DRAIN) })
}

pub(crate) fn renumber_seeded(arena: ArenaRef, pipelines: &mut [Pipeline], protos: &mut [Proto], node: i32) {
    let follows = |a: &Proto| protos.iter().find(|f| f.kind == ProtoKind::Follows && f.peer == a.pipeline).map(|f| f.pipeline as usize);
    let seeded: Vec<(usize, usize, u8)> = protos.iter().filter(|a| a.kind == ProtoKind::Arms).filter_map(|a| Some((a.pipeline as usize, follows(a)?, a.slot))).collect(); // alloc: Program
    for (k, q, s) in seeded {
        if q >= pipelines.len() {
            continue;
        }
        // B4d: decompose reuses a number at its last reader, so a life between the probe and the drain (another partition's
        // build or spool) may hold the drain end's number inside the run's span, where the driver's `arms_of` would take it
        // for an arm: it takes a number no pipeline uses (drain.rs `between` let it through: no arm of the run reads it)
        let kind = pipelines[q].breaker().map(|b| b.kind);
        let mut x = k + 1;
        while x < q {
            if pipelines[x].produces() == Some(s) && pipelines[x].breaker().map(|b| b.kind) != kind {
                let Some(to) = (0..NO_SLOT).find(|&t| !pipelines.iter().any(|p| p.produces() == Some(t) || p.consumes.contains(&t))) else {
                    crate::refuse!(node, Reason::Width { needed: NO_SLOT as u32 + 1, have: NO_SLOT as u32 })
                };
                // D3514: the arms of an Append between (an aggregate over a partitioned table) fold into one production,
                // so they take one number together (each its own left the reader the last partition's fold alone)
                let arms = between_arms(pipelines, protos, (k, q), x, s);
                for &a in &arms {
                    rename_slot(arena, pipelines, protos, a, s, to);
                }
                x = arms.last().map_or(x, |&a| a.max(x)) + 1;
                continue;
            }
            x += 1;
        }
        // D1188: the probe, or a pipeline between (B4d), reads a life before the probe under the number
        let read = |p: &Pipeline| p.consumes.contains(&s);
        if !pipelines.get(k).is_some_and(|p| p.produces() == Some(s) && (read(p) || pipelines[k + 1..q].iter().any(read))) {
            continue;
        }
        // D3822: a probe union's arms (V4R-533) are each a seeded run of this drain on its slot: one production, renamed together
        if protos.iter().any(|a| a.kind == ProtoKind::Arms && a.slot == s && overlap(a, k..q + 1) && !seeded_probe(pipelines, protos, a.pipeline as usize, q, s)) {
            seeded_drain(node)
        }
        let Some(to) = (0..NO_SLOT).find(|&t| !pipelines.iter().any(|p| p.produces() == Some(t) || p.consumes.contains(&t))) else {
            crate::refuse!(node, Reason::Width { needed: NO_SLOT as u32 + 1, have: NO_SLOT as u32 })
        };
        for p in 0..q {
            if seeded_probe(pipelines, protos, p, q, s) {
                if let Some(b) = crate::frame::iface::breaker_of_mut(&mut pipelines[p]) {
                    *b = b.in_slot(to);
                }
            }
        }
        let renamed = |a: &Proto| a.pipeline as usize == k || (a.slot == s && pipelines.get(a.pipeline as usize).is_some_and(|x| x.produces() == Some(to)));
        protos.iter_mut().filter(|a| a.kind == ProtoKind::Arms && renamed(a)).for_each(|a| a.slot = to);
        rename_slot(arena, pipelines, protos, q, s, to);
    }
    merge_arms(protos);
}

/// D3514: the arms of the `Arms` run inside the seeded run `(k, q)` (exclusive) that holds `x` on slot `s`, in program
/// order (`protos::arms_of`'s rule, never the seeded run itself, which spans `k ..= q`); `[x]` when `x` is no arm of one.
fn between_arms(pipelines: &[Pipeline], protos: &[Proto], (k, q): (usize, usize), x: usize, s: u8) -> Vec<usize> {
    let produces = |y: usize| pipelines.get(y).and_then(|p| p.produces()) == Some(s);
    let run = protos
        .iter()
        .filter(|a| a.kind == ProtoKind::Arms && a.slot == s && (a.pipeline as usize) > k && (a.pipeline as usize).saturating_add(a.arg as usize) <= q)
        .map(|a| a.pipeline as usize..(a.pipeline as usize).saturating_add(a.arg as usize))
        .find(|r| r.contains(&x) && produces(r.start) && produces(r.end - 1));
    match run {
        Some(r) => r.filter(|&y| produces(y)).collect(), // alloc: Program
        None => vec![x], // alloc: Program
    }
}

/// D3822: pipeline `p` is a probe of drain `q` seeding the end's slot `s` (its `Arms` run on `s`, the drain `Follows` it).
fn seeded_probe(pipelines: &[Pipeline], protos: &[Proto], p: usize, q: usize, s: u8) -> bool {
    let follows = |a: &Proto| protos.iter().any(|f| f.kind == ProtoKind::Follows && f.peer == a.pipeline && f.pipeline as usize == q);
    pipelines.get(p).is_some_and(|x| x.produces() == Some(s)) && protos.iter().any(|a| a.kind == ProtoKind::Arms && a.pipeline as usize == p && a.slot == s && follows(a))
}

fn overlap(a: &Proto, r: core::ops::Range<usize>) -> bool {
    (a.pipeline as usize) < r.end && r.start < a.pipeline as usize + a.arg as usize
}

/// D1187: a seeded drain that is an arm of an append.arms run (a RIGHT JOIN per partition): its probe's run (the probe ..
/// the drain) and the append's overlap on one slot, one production. Every such proto takes the union, so the run holding
/// any of its pipelines is the same one (program `arms_of`, the driver's, `ArmSort`); a build between is no arm.
pub(crate) fn merge_arms(protos: &mut [Proto]) {
    let follows = |p: u16| protos.iter().any(|f| f.kind == ProtoKind::Follows && f.peer == p);
    let seeded: Vec<usize> = (0..protos.len()).filter(|&i| protos[i].kind == ProtoKind::Arms && follows(protos[i].pipeline)).collect(); // alloc: Program
    let mut again = true;
    while again {
        again = false;
        for &i in &seeded {
            for j in 0..protos.len() {
                let (a, b) = (protos[i], protos[j]);
                let r = b.pipeline as usize..b.pipeline as usize + b.arg as usize;
                if j != i && b.kind == ProtoKind::Arms && b.slot == a.slot && overlap(&a, r.clone()) && (a.pipeline, a.arg) != (b.pipeline, b.arg) {
                    let (lo, hi) = (r.start.min(a.pipeline as usize), r.end.max(a.pipeline as usize + a.arg as usize));
                    for x in [i, j] {
                        (protos[x].pipeline, protos[x].arg) = (lo as u16, (hi - lo) as u64);
                    }
                    again = true;
                }
            }
        }
    }
}

/// B4c-2 (V4R-414), to_program before its remap: a seeded drain's Arms (join/drain.rs: the probe k .. the drain d, graph
/// indices; `Follows(d)` names k) ends at the drain wherever `map` puts it (a spill pair spliced after k lengthens the run).
/// Other Arms count program pipelines from their first (spill_pair's (q, 2), an Append's arms), so only their start moves.
pub(crate) fn seeded_arms(protos: &mut [Proto], map: impl Fn(u16) -> u16) {
    for i in 0..protos.len() {
        let p = protos[i];
        if p.kind != ProtoKind::Arms || p.arg < 1 || !protos.iter().any(|f| f.kind == ProtoKind::Follows && f.peer == p.pipeline) {
            continue;
        }
        let (a, z) = (map(p.pipeline), map(p.pipeline + p.arg as u16 - 1));
        protos[i].arg = (z.max(a) - a + 1) as u64;
    }
}

/// B4c-2: Arms runs `(first, last, slot)` from the same first on one slot (a seeded drain's run and a spill pair's from
/// its probe) are one production (merge_arms unions their protos later), so a life read in either is the union's last
/// arm. Runs from different firsts (an Append's arms around a partition's seeded run, D1187) keep their own lives.
pub(crate) fn union_runs(runs: &mut Vec<(u16, u16, u8)>) {
    runs.sort_unstable_by_key(|r| (r.2, r.0));
    runs.dedup_by(|b, a| {
        let one = a.2 == b.2 && b.0 == a.0;
        if one {
            a.1 = a.1.max(b.1);
        }
        one
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D1187 (regress partition_join-007's shape): probes 1 and 3 seed their drains 4 and 5, arms of the Append run 4..7
    /// with 6; one run 1..7 on slot 4 (2, a build, is no arm), the unrelated slot's run kept.
    #[test]
    fn seeded_drains_join_the_append_run() {
        let mut protos = [
            Proto::new(ProtoKind::Arms, 1).slot(4).arg(4),
            Proto::new(ProtoKind::Follows, 4).peer(1),
            Proto::new(ProtoKind::Arms, 3).slot(4).arg(3),
            Proto::new(ProtoKind::Follows, 5).peer(3),
            Proto::new(ProtoKind::Arms, 4).slot(4).arg(3),
            Proto::new(ProtoKind::Arms, 8).slot(4).arg(2),
        ];
        merge_arms(&mut protos);
        let runs: Vec<(u16, u64)> = protos.iter().filter(|a| a.kind == ProtoKind::Arms).map(|a| (a.pipeline, a.arg)).collect();
        assert_eq!(runs, [(1, 6), (1, 6), (1, 6), (8, 2)]);
    }

    /// D3514: a seeded run (probe 0 .. drain 5, a UNION's Distinct on slot 2) holds an aggregate over a partitioned table:
    /// its Append's arms 1-3 fold into a plain fold that decompose numbered 2 as well, read by 4. The arms take one free
    /// number together, as do their run and their reader; the seeded run keeps its own.
    #[test]
    fn an_append_run_inside_a_seeded_run_takes_one_number() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::{Arena, End};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let end = |kind| End::Breaker(Breaker { kind, slot: 2, bytes: 64, budget: Budget::Fixed(64), spill: Spill::None });
        let (distinct, fold) = (Pipeline::eligible(-1, end(BreakerKind::Distinct), 0), Pipeline::eligible(-1, end(BreakerKind::PlainAgg), 0));
        let sink = Pipeline::eligible(-1, End::Sink, 0);
        let mut pipes = [distinct, fold, fold, fold, sink.consuming_in(a, 2), distinct, sink.consuming_in(a, 2)];
        let mut protos = [Proto::new(ProtoKind::Arms, 0).slot(2).arg(6), Proto::new(ProtoKind::Follows, 5).peer(0), Proto::new(ProtoKind::Arms, 1).slot(2).arg(3)];
        renumber_seeded(a, &mut pipes, &mut protos, -1);
        let t = pipes[1].produces().expect("the fold's slot");
        assert_ne!(t, 2, "the fold leaves the seeded run's number");
        assert_eq!((pipes[2].produces(), pipes[3].produces()), (Some(t), Some(t)), "every arm folds into one production");
        assert_eq!((pipes[4].consumes, protos[2].slot), (&[t][..], t), "its reader and its run follow it");
        assert_eq!((pipes[0].produces(), pipes[5].produces(), pipes[6].consumes), (Some(2), Some(2), &[2u8][..]), "the seeded run keeps its number");
    }

    /// D3822: a drain's probe is a union of two arms (1, 2), each a seeded run of drain 3 on the Sort's slot 2; arm 1 reads
    /// a build (0) decompose numbered 2 too. Both arms, the drain and the Sort's reader (4) take one free number; the build
    /// keeps 2. An Arms run on 2 from a pipeline that is no probe of the drain (0, overlapping) still refuses.
    #[test]
    fn a_probe_unions_arms_take_one_number() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::{Arena, End};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let end = |kind| End::Breaker(Breaker { kind, slot: 2, bytes: 64, budget: Budget::Fixed(64), spill: Spill::None });
        let (build, sort) = (Pipeline::eligible(-1, end(BreakerKind::HashBuild), 0), Pipeline::eligible(-1, end(BreakerKind::Sort), 0));
        let sink = Pipeline::eligible(-1, End::Sink, 0);
        let pipes = [build, sort.consuming_in(a, 2), sort, sort, sink.consuming_in(a, 2)];
        let protos = [
            Proto::new(ProtoKind::Arms, 1).slot(2).arg(3),
            Proto::new(ProtoKind::Follows, 3).peer(1),
            Proto::new(ProtoKind::Arms, 2).slot(2).arg(2),
            Proto::new(ProtoKind::Follows, 3).peer(2),
        ];
        let (mut p, mut q) = (pipes, protos);
        renumber_seeded(a, &mut p, &mut q, -1);
        let t = p[1].produces().expect("the first arm's slot");
        assert_ne!(t, 2, "the union's arms leave the build's number");
        assert_eq!((p[2].produces(), p[3].produces(), p[4].consumes), (Some(t), Some(t), &[t][..]), "one production, its reader follows");
        assert_eq!((p[0].produces(), p[1].consumes), (Some(2), &[2u8][..]), "the build keeps its number");
        assert_eq!(q.iter().filter(|x| x.kind == ProtoKind::Arms).map(|x| (x.pipeline, x.arg, x.slot)).collect::<Vec<_>>(), [(1, 3, t), (1, 3, t)]);
        let mut p = pipes;
        let mut q = [protos[0], protos[1], protos[2], protos[3], Proto::new(ProtoKind::Arms, 0).slot(2).arg(2)];
        let e = crate::catch(-1, || renumber_seeded(a, &mut p, &mut q, -1)).err().map(|e| format!("{e:?}")).unwrap_or_default();
        assert!(e.contains(SEEDED_DRAIN), "{e}");
    }

    /// D1331: the seeded drain form refuses by its Shape, not by NotBuilt of the built join.drain.
    #[test]
    fn a_seeded_drain_refuses_by_shape() {
        let e = crate::catch(7, || -> () { seeded_drain(7) }).err().map(|e| format!("{e:?}")).unwrap_or_default();
        assert!(e.contains("Shape") && e.contains(SEEDED_DRAIN) && !e.contains("NotBuilt"), "{e}");
    }
}
