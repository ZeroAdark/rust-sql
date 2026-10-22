//! The graph's pipelines as one indexable view (M8 L0-m; M8-plan §3.0 Decision 1). Decompose keeps a nest's inner and a
//! nested SubPlan's inside the outer op, never as a graph pipeline, so no maker could address one. Here the graph pipelines
//! come first, then every embedded inner in depth-first order: `[k]` and `get(k)` reach inner `k - len()`, so an inner's
//! makers index it as they index a pipeline (nest/inner.rs makes them under its outer's identity, `Px::making`). `len`,
//! `iter` and the other slice methods stay the graph pipelines' own, so every site that walks the graph sees what it saw
//! before. Crate-level: graph.rs (CORE) returns it. Owner M8-NEST.
#![allow(unsafe_code)]
use core::ops::{Deref, Index};

use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Op, PipelineNode};
use gravity_program::SiteDesc;
use mcx::GrowIn;

use crate::face::kinds::Face;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::rerun::params::NestExt;

pub(crate) use crate::lifetime::material::spool_fill;
pub(crate) use crate::rerun::spool::form::{spool_pull, REFILL_CHILD, LOUD_CHILD, MANY_READERS, PGRC_CHILD};

#[repr(transparent)]
pub struct Pipes<'p>([PipelineNode<'p>]);

impl<'p> Pipes<'p> {
    pub(crate) fn of<'g>(s: &'g [PipelineNode<'p>]) -> &'g Pipes<'p> {
        // SAFETY: `Pipes` is `repr(transparent)` over the slice: one layout, and the reference keeps the slice's lifetime
        unsafe { &*(s as *const [PipelineNode<'p>] as *const Pipes<'p>) }
    }

    /// Pipeline `k`: a graph pipeline, or the embedded inner `k - len()`; `None` past both.
    pub fn get(&self, k: usize) -> Option<&PipelineNode<'p>> {
        self.0.get(k).or_else(|| self.inners().into_iter().nth(k - self.0.len()).map(|x| x.1))
    }

    /// Every embedded inner in index order (from `len()`): the outer `(pipeline, op)` whose op embeds it, and the inner.
    /// D3585: a lowering's own graph walks its ops once (memo.rs); each call after the first follows the kept places.
    pub fn inners(&self) -> Vec<((usize, usize), &PipelineNode<'p>)> {
        let key = crate::memo::key_of(&self.0);
        match crate::memo::read(key, |m| m.inners.clone()) {
            Some(Some(at)) => self.inners_at(&at).unwrap_or_else(|| self.walk_inners()),
            Some(None) => {
                let out = self.walk_inners();
                let at: std::rc::Rc<[(usize, usize)]> = out.iter().map(|x| x.0).collect(); // alloc: Program
                crate::memo::write(key, |m| m.inners = Some(at));
                out
            }
            None => self.walk_inners(),
        }
    }

    /// D3585: `inners` from the kept places `at` (each outer `(pipeline, op)`, an outer past `len()` an earlier inner);
    /// `None` when an op there embeds no inner (the walk answers then).
    fn inners_at(&self, at: &[(usize, usize)]) -> Option<Vec<((usize, usize), &PipelineNode<'p>)>> {
        let n = self.0.len();
        let mut out: Vec<((usize, usize), &PipelineNode<'p>)> = Vec::with_capacity(at.len()); // alloc: Program
        for &(k, j) in at {
            let outer: &PipelineNode<'p> = if k < n { &self.0[k] } else { out.get(k - n)?.1 };
            let (Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. })) = outer.ops.get(j)? else { return None };
            out.push(((k, j), &**inner)); // alloc: Program
        }
        Some(out)
    }

    /// `inners` by a walk of every pipeline's ops.
    fn walk_inners(&self) -> Vec<((usize, usize), &PipelineNode<'p>)> {
        fn ops<'a, 'p>(k: usize, p: &'a PipelineNode<'p>, n: usize, out: &mut Vec<((usize, usize), &'a PipelineNode<'p>)>) {
            crate::stack_check(); // D3832
            for (j, op) in p.ops.iter().enumerate() {
                let (Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. })) = op else { continue };
                let at = n + out.len();
                out.push(((k, j), &**inner)); // alloc: Program
                ops(at, inner, n, out);
            }
        }
        let mut out = Vec::new(); // alloc: Program
        self.0.iter().enumerate().for_each(|(k, p)| ops(k, p, self.0.len(), &mut out));
        out
    }

    /// D1018 (regress aggregates#527): each streamed group (`Op::GroupRun(i)`) an embedded inner runs, with that inner's
    /// index: decompose names a stream's producer when it pushes a graph pipeline (graph.rs `push_into`), never for an inner
    /// kept inside its outer op (the producer stays `u16::MAX`). ops/group/regroup.rs re-declares those streams with this
    /// index, so their readers (keyed/stream.rs `own`, the record's representatives) find the inner.
    pub(crate) fn stream_producers<'m>(&self, streams: &[gravity_decompose::ops::shared::agg::GroupDecl<'_>], mcx: mcx::Mcx<'m>) -> mcx::PgVec<'m, (usize, usize)> {
        let mut out = mcx::PgVec::new_in(mcx);
        for (n, (_, p)) in self.inners().into_iter().enumerate() {
            for op in p.ops.iter() {
                let Op::GroupRun(i) = op else { continue };
                if streams.get(*i as usize).is_some_and(|g| g.producer.0 == u16::MAX) {
                    out.push_in((*i as usize, self.0.len() + n));
                }
            }
        }
        out
    }

    /// D3810: the pipeline whose op embeds inner `k` (a graph pipeline or another inner); `None` for a graph pipeline.
    pub fn owner_of(&self, k: usize) -> Option<usize> {
        k.checked_sub(self.0.len()).and_then(|i| self.inners().get(i).map(|x| x.0 .0))
    }

    /// The index of the inner that op `j` of pipeline `k` embeds (`k` a graph pipeline or an inner).
    pub fn inner_of(&self, k: usize, j: usize) -> Option<usize> {
        self.inners().iter().position(|x| x.0 == (k, j)).map(|i| self.0.len() + i)
    }

    /// D1339 (M8-RERUN rows B / D): `slot`'s index among the consumed regions of the graph pipeline whose call runs `k`: an
    /// embedded inner runs in its owner's call, whose regions list its inners' slots.
    pub fn consumed_at(&self, k: usize, slot: u8) -> Option<usize> {
        crate::stack_check();
        k.checked_sub(self.0.len()).map_or_else(|| self.0[k].desc.consumes.iter().position(|&s| s == slot), |i| self.consumed_at(self.inners()[i].0 .0, slot))
    }

    /// One past the last inner: a two-pass HAVING's virtual pass ids start here (frame/program.rs `pass_id`), so no index
    /// names both an inner and a pass.
    pub fn end(&self) -> usize {
        fn count(p: &PipelineNode<'_>) -> usize {
            crate::stack_check(); // D3832
            let one = |op: &Op<'_>| match op {
                Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => 1 + count(inner),
                _ => 0,
            };
            p.ops.iter().map(one).sum()
        }
        self.0.len() + self.0.iter().map(count).sum::<usize>()
    }
}

impl<'p> Deref for Pipes<'p> {
    type Target = [PipelineNode<'p>];

    fn deref(&self) -> &[PipelineNode<'p>] {
        &self.0
    }
}

impl<'p> Index<usize> for Pipes<'p> {
    type Output = PipelineNode<'p>;

    /// Past the graph and its inners the slice's own panic, as before.
    fn index(&self, k: usize) -> &PipelineNode<'p> {
        self.get(k).unwrap_or_else(|| &self.0[k])
    }
}

impl<'a, 'p> IntoIterator for &'a Pipes<'p> {
    type Item = &'a PipelineNode<'p>;
    type IntoIter = core::slice::Iter<'a, PipelineNode<'p>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Exec admit pairs the program's source sites with the graph's sources in this view's walk order (exec admit.rs `sources`:
/// a pipeline's source, then its nest inners depth-first) where two share a relation and index, so an owner's site takes
/// its number before its inner's: the inner's make (a step's) runs before the owner's `Mechs::new` numbers the owner's.
/// `reserve_site` holds a number for pipeline `key` (a program pipeline, or an inner's view index); `place_site` fills it.
pub(crate) fn reserve_site(px: &mut Px<'_, '_>, key: usize) {
    if !px.ext::<NestExt>(Face::Nest).is_some_and(|x| x.sites.iter().any(|s| s.0 == key)) {
        let n = px.add_site(SiteDesc::none_in(px.arena));
        px.ext_mut::<NestExt>(Face::Nest).sites.push((key, n)); // alloc: Program
    }
}

/// Pipeline `key`'s site `d` numbered: the number `reserve_site` held for it, else the next.
pub(crate) fn place_site(px: &mut Px<'_, '_>, key: usize, d: SiteDesc) -> usize {
    match px.ext::<NestExt>(Face::Nest).and_then(|x| x.sites.iter().position(|s| s.0 == key)) {
        Some(i) => {
            let n = px.ext_mut::<NestExt>(Face::Nest).sites.swap_remove(i).1;
            px.sites[n] = d;
            n
        }
        None => px.add_site(d),
    }
}

/// M8-NEST S2b (nest/inner_arms.rs): the settled union pipeline `k` feeds (its `Feed`) or is the emit of, when a nest or
/// nested SubPlan inner is that emit (its first op `Union { slot, children }`, its life its last feed's: decompose append.rs
/// `settle_arms`): `(emit, slot, the feeds in arm order)`. Graph.rs names such a feed's source nest.loop (it reads nothing).
/// Not a union the nest's rerun set refills at every re-open (S2r, rerun/union_arms.rs `refilled`), nor one no set re-runs
/// under a nest with no parameters (S2k, `kept`, D3782), nor one filled once (D3773, `filled_once`): the eager form's.
pub(crate) fn inner_union(gx: &Gx<'_, '_>, k: usize) -> Option<(usize, u8, Vec<usize>)> {
    union_in_inner(gx, k).filter(|u| !crate::rerun::union_arms::refilled(gx, u) && !crate::rerun::union_arms::kept(gx, u) && !crate::rerun::union_arms::filled_once(gx, u))
}

/// `inner_union`'s search: the settled union a nest or nested SubPlan inner emits that pipeline `k` feeds or emits.
pub(crate) fn union_in_inner(gx: &Gx<'_, '_>, k: usize) -> Option<(usize, u8, Vec<usize>)> {
    let ps = gx.pipelines();
    let feed = |q: usize, s: u8| ps[q].ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Feed { slot, .. }) if *slot == s));
    if k < ps.len() && !ps[k].ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))) {
        return None;
    }
    let arms = |e: usize, p: &PipelineNode<'_>| {
        let Some(Op::Append(AppendDesc::Union { slot, children, .. })) = p.ops.first() else { return None };
        let last = gx.life_of(e, *slot).map(|l| l.0 as usize).filter(|&l| l < ps.len())?;
        let mut f: Vec<usize> = (0..=last).rev().filter(|&q| feed(q, *slot)).take(*children as usize).collect(); // alloc: Program
        f.reverse();
        (f.len() == *children as usize && f.last() == Some(&last)).then_some((e, *slot, f))
    };
    let inners = ps.inners();
    inners.iter().enumerate().map(|(i, x)| (ps.len() + i, x.1)).filter(|&(e, _)| k < ps.len() || e == k).find_map(|(e, p)| arms(e, p).filter(|a| a.0 == k || a.2.contains(&k)))
}
