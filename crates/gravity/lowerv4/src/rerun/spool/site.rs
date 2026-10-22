//! nest.spool's pulled child's site number (D1919, D1633; M8-NEST, a helper of nest/spool.rs). Exec admit (admit.rs
//! `registry`) pairs the program's source sites, in site order, with the graph's sources in its walk order (each graph
//! pipeline's source, then its nest inners depth-first): each site takes the first unused source of its relation and
//! index (a bitmap's probe index), and an index, bitmap, TID or sample recipe takes its quals from the source it pairs.
//! Only a heap range's recipe is its site's alone (m2_admit.rs `range`), so sites over a key whose every source is a heap
//! range pair in any order. The child is numbered while its reader is made, inside its owners' makes, and nest/inner.rs
//! holds an owner's number ahead of a non-spool inner's make (pipes.rs `reserve_site`), so a sibling inner made before
//! may already sit between the owner's held number and the next. `walk_site` numbers the child where exec admit pairs
//! it with its own fill pipeline's source, and moves no held owner past a site of the owner's key; else (D3769) it pins
//! the child and every site of its key numbered before it to their own sources (`Px::site_pins`, which exec admit pairs
//! ahead of the walk order), where every such site's pipeline is known and no owner holds a number of that key; else the
//! Material refuses by D1633's name. Registers nothing.
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_program::SiteDesc;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::face::kinds::Face;
use crate::frame::px::Px;
use crate::rerun::params::NestExt;
use crate::admit::refusal::{Reason, ShapeWhy};

/// D1633's limit: no number pairs the child with its own source without moving an owner past a scan of the owner's key.
const PAIRED_SITE: &str = "a plan Material whose pulled child's site exec admit would pair with another scan of its relation and index (D1633)";

/// A source's key in exec admit's pairing (admit.rs `registry`'s `find`).
pub(crate) type Key = (u32, u32);

/// A walk entry: the source's index in the `Pipes` view, its key, and whether it is a heap range.
pub(crate) type Entry = (usize, Option<Key>, bool);

pub(crate) fn key(s: &Source<'_>) -> Option<Key> {
    let (_, relid, sd) = s.relation()?;
    Some((relid, match s {
        Source::Bitmap { plan: BitmapPlan::Index { indexid, .. }, .. } => *indexid,
        _ => sd.index,
    }))
}

/// Exec admit's walk (admit.rs `sources`), each source's view index numbered as `Pipes::inners` numbers the inners
/// (depth-first, from the graph's length).
pub(crate) fn walk(p: &PipelineNode<'_>, at: usize, next: &mut usize, out: &mut PgVec<'_, Entry>) {
    crate::stack_check(); // D3832
    out.push_in((at, key(&p.source), matches!(p.source, Source::Range { am: Am::Heap, .. })));
    for op in p.ops.iter() {
        if let Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
            *next += 1;
            walk(inner, *next - 1, next, out);
        }
    }
}

/// `pairs` after every site numbered so far, a number an owner holds (pipes.rs `reserve_site`) taken as a source of its
/// owner's key, as exec admit pairs it once the owner's make places its site there. `Err` when a held number's owner is no
/// known pipeline (fail closed).
pub(crate) fn pairs_held(px: &Px<'_, '_>, w: &[Entry], k: Key) -> Result<Option<usize>, ()> {
    pairs_held_at(px, w, k, px.sites.len())
}

/// `pairs_held` for a site numbered `at` (D3493: a number its feed held, inner_arms_index.rs `hold`): only the sites
/// before it pair ahead of it.
pub(crate) fn pairs_held_at(px: &Px<'_, '_>, w: &[Entry], k: Key, at: usize) -> Result<Option<usize>, ()> {
    let ps = px.gx.pipelines();
    let held = px.ext::<NestExt>(Face::Nest).map_or(&[][..], |x| &x.sites[..]);
    let (mut used, pins) = pinned(px, w);
    let mut pair = |k: Key| {
        let find = |used: &[bool], unused: bool| w.iter().enumerate().position(|(i, e)| (!unused || !used[i]) && e.1 == Some(k));
        let i = find(&used, true).or_else(|| find(&used, false))?;
        used[i] = true;
        Some(i)
    };
    for (s, d) in px.sites.iter().enumerate().take(at).filter(|(s, _)| !pins.contains(s)) {
        match held.iter().find(|h| h.1 == s) {
            Some(h) => {
                if let Some(q) = key(&ps.get(px.after_of(h.0)).ok_or(())?.source) {
                    let _ = pair(q);
                }
            }
            None if d.is_source() => {
                let _ = pair((d.relation, d.index));
            }
            None => {}
        }
    }
    Ok(pair(k))
}

/// D3769: the walk entries the pinned sites take (exec admit pairs them first), and those sites: a pinned site pairs by
/// no order, so the order simulations here skip it and count its entry as taken.
fn pinned<'g>(px: &Px<'g, '_>, w: &[Entry]) -> (PgVec<'g, bool>, PgVec<'g, usize>) {
    let mcx = px.mcx();
    let mut used = mcx::pg_vec![in mcx; false; w.len()];
    let mut sites = mcx::reserved_in(px.site_pins.len(), mcx);
    for &(s, q) in px.site_pins.iter() {
        if let Some(i) = w.iter().position(|e| e.0 == q) {
            used[i] = true;
        }
        sites.push_in(s);
    }
    (used, sites)
}

/// Draft `d` (key `k`, walk entry `own`) numbered at the first number an owner holds, each holding owner moved to the next
/// held number and the last to a fresh one (walk_site's shift for an index arm whose feed exec admit walks before the
/// owner, so the owner's held number would take the feed's source first). Only where exec admit then pairs `d` with `own`
/// and every moved owner with its own source, and no other site of a moved site's key lies between its old and new
/// numbers (those keep their pairs); else `None`, nothing changed. Fail closed: every holder a known pipeline with a keyed
/// source the walk holds.
pub(crate) fn shift_held(px: &mut Px<'_, '_>, w: &[Entry], k: Key, own: usize, d: SiteDesc) -> Option<usize> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let held = px.ext::<NestExt>(Face::Nest).map_or(&[][..], |x| &x.sites[..]);
    // (held number, its place in the held list, the owner's key, the owner's walk entry), by number
    let mut hs: PgVec<'_, (usize, usize, Key, usize)> = mcx::reserved_in(held.len(), px.mcx());
    for (i, h) in held.iter().enumerate() {
        let at = px.after_of(h.0);
        let hk = key(&ps.get(at)?.source)?;
        hs.push_in((h.1, i, hk, w.iter().position(|e| e.0 == at)?));
    }
    hs.sort_unstable();
    let len = px.sites.len();
    let first = hs.first()?.0;
    let to = |j: usize| hs.get(j + 1).map_or(len, |x| x.0);
    let (mut used, pins) = pinned(px, w);
    let free = |s: usize| !hs.iter().any(|h| h.0 == s) && px.sites[s].is_source() && !pins.contains(&s);
    let crosses = |lo: usize, hi: usize, key: Key| (lo + 1..hi.min(len)).any(|s| free(s) && (px.sites[s].relation, px.sites[s].index) == key);
    if crosses(first, len, k) || hs.iter().enumerate().any(|(j, h)| crosses(h.0, to(j), h.2)) {
        return None;
    }
    let mut pair = |k: Key| {
        let find = |used: &[bool], unused: bool| w.iter().enumerate().position(|(i, e)| (!unused || !used[i]) && e.1 == Some(k));
        let i = find(&used, true).or_else(|| find(&used, false))?;
        used[i] = true;
        Some(i)
    };
    for s in 0..=len {
        let (sk, want) = match (s == first, (0..hs.len()).find(|&j| to(j) == s)) {
            (true, _) => (k, Some(own)),
            (false, Some(j)) => (hs[j].2, Some(hs[j].3)),
            (false, None) if s < len && free(s) => ((px.sites[s].relation, px.sites[s].index), None),
            (false, None) => continue,
        };
        let got = pair(sk);
        if want.is_some_and(|x| got != Some(x)) {
            return None;
        }
    }
    let fresh = px.add_site(SiteDesc::none_in(px.arena));
    let held = &mut px.ext_mut::<NestExt>(Face::Nest).sites;
    for (j, h) in hs.iter().enumerate() {
        let n = to(j);
        held[h.1].1 = if n == len { fresh } else { n };
    }
    px.sites[first] = d;
    Some(first)
}

/// The walk entry exec admit pairs a site of key `k` with after `sites`: the first unused source of that key, else the
/// first (a second site over a used source); `pin` (`pinned`): the entries pinned sites take first, and those sites.
pub(crate) fn pairs(w: &[Entry], sites: &[SiteDesc], k: Key, pin: &(PgVec<'_, bool>, PgVec<'_, usize>), mcx: mcx::Mcx<'_>) -> Option<usize> {
    let mut used = mcx::to_vec_in(&pin.0[..], mcx);
    let mut pair = |k: Key| {
        let find = |used: &[bool], unused: bool| w.iter().enumerate().position(|(i, e)| (!unused || !used[i]) && e.1 == Some(k));
        let i = find(&used, true).or_else(|| find(&used, false))?;
        used[i] = true;
        Some(i)
    };
    for (_, s) in sites.iter().enumerate().filter(|(i, s)| s.is_source() && !pin.1.contains(i)) {
        let _ = pair((s.relation, s.index));
    }
    pair(k)
}

/// The pulled child's site number for reader `ik` over fill pipeline `kf`. The candidates: the next number (`add`, the
/// number D1633's row gives where it serves), or the chain's first held number with each held owner moved to the next
/// (`shift`; the last a fresh one). One level down the next number comes first, so every Material D1633's row serves
/// keeps its IR; two or more levels down the shift does (each link above held its owner's number before its make). A
/// candidate holds when the child pairs its own source there and no owner ends up ahead of a site of its key that exec
/// admit walks after it (a key over heap ranges only pairs freely). Fail closed, the shift needs every held number on
/// the chain (an off-chain one below the chain's first becomes a source the replay did not count) and of a known
/// pipeline: twopass.having's pass 2 number while its remake runs (`add_pass` registers it after) counts as one of every
/// key, so the Material refuses by D1633's name there unless its child's key is heap ranges only. Where B-R1 serves (two
/// or more levels down, or the shift), a child index key that is an expression refuses by D1637's name (spool_form.rs),
/// and an inner of the graph whose walk binds at bind by D1630's (spool_readers.rs `walk_bound`).
pub(crate) fn walk_site(px: &mut Px<'_, '_>, ik: usize, kf: usize, draft: SiteDesc) -> usize {
    let (gx, ident) = (px.gx, px.ident(ik));
    let ps = gx.pipelines();
    let (n, inners) = (ps.len(), ps.inners());
    let chain: PgVec<'_, usize> = core::iter::successors(Some(inners[ik - n].0 .0), |&o| (o >= n).then(|| inners[o - n].0 .0)).collect_in(px.mcx());
    let (mut w, mut next) = (PgVec::new_in(px.mcx()), n);
    ps.iter().enumerate().for_each(|(k, p)| walk(p, k, &mut next, &mut w));
    let (ck, kf_at) = ((draft.relation, draft.index), w.iter().position(|e| e.0 == kf));
    let loose = |k: Key| w.iter().all(|e| e.1 != Some(k) || e.2);
    // a held number's owner key: its pipeline's (a pass's graph pipeline's) source key, unknown (`None`) for no pipeline
    let owner = |h: usize| ps.get(px.after_of(h)).map(|p| key(&p.source));
    let of_key = |o: Option<Option<Key>>, k: Key| !loose(k) && o.is_none_or(|x| x == Some(k));
    // the chain's held numbers in order (an inner holds under its own index, the graph pipeline under `ident`), each with
    // its entry in the held list and its owner's key
    let holder = |o: usize| if o >= n { o } else { ident };
    let held = px.ext::<NestExt>(Face::Nest).map_or(&[][..], |x| &x.sites[..]);
    let mut own: PgVec<'_, (usize, usize, Option<Option<Key>>)> = held.iter().enumerate().filter(|(_, h)| chain.iter().any(|&o| holder(o) == h.0)).map(|(i, h)| (h.1, i, owner(h.0))).collect_in(px.mcx());
    own.sort_unstable();
    let pin = pinned(px, &w);
    let pairs_own = |at: usize| loose(ck) || (kf_at.is_some() && pairs(&w, &px.sites[..at], ck, &pin, px.mcx()) == kf_at);
    // the next number: every held number goes ahead of the child
    let held_key = held.iter().any(|h| of_key(owner(h.0), ck));
    let add = pairs_own(px.sites.len()) && !held_key;
    // the shift moves each held owner past the sites numbered after its held number and before the next held one (only
    // with every held number on the chain and of a known pipeline)
    let crossed = |s: usize| own.iter().rev().find(|o| o.0 < s).map(|o| o.2);
    let known = own.len() == held.len() && own.iter().all(|o| o.2.is_some());
    let shift = known && own.first().is_some_and(|f| pairs_own(f.0))
        && px.sites.iter().enumerate().filter(|(_, d)| d.is_source()).all(|(s, d)| crossed(s).is_none_or(|o| !of_key(o, (d.relation, d.index))));
    if add && chain.len() == 1 {
        return px.add_site(draft);
    }
    // D3769: neither pairs it by the walk order (a scan of its key numbered before it, a SubPlan's nest inner, takes its
    // source first); with no owner holding a number of its key, the child and every site of its key numbered before it
    // are pinned to their own sources (`pin_set`), each such site's pipeline known
    let mut pins = (!add && !shift && !held_key).then(|| pin_set(px, &w, ck, kf)).flatten();
    if !add && !shift && pins.is_none() {
        crate::refuse!(ps[ik].node_id, Reason::Shape { why: ShapeWhy(PAIRED_SITE) });
    }
    if let Some(p) = pins.take_if(|_| chain.len() == 1) {
        return pin_site(px, draft, p, kf);
    }
    // B-R1 serves the rest: D1637's key class and D1630's refuse there (one level down at D1633's number they are
    // trunk's, DEFERRED)
    if crate::rerun::spool::form::expr_key(&ps[kf].source) {
        crate::refuse!(ps[ik].node_id, Reason::Shape { why: ShapeWhy(crate::rerun::spool::form::KEY_AT_FIRST_READ) });
    }
    if crate::rerun::spool::readers::walk_bound(gx) {
        crate::refuse!(ps[ik].node_id, Reason::Shape { why: ShapeWhy(crate::rerun::spool::form::WALK_BOUND_AT_BIND) });
    }
    if let Some(p) = pins.take() {
        return pin_site(px, draft, p, kf);
    }
    if !shift {
        return px.add_site(draft);
    }
    let first = own[0].0;
    let fresh = px.add_site(SiteDesc::none_in(px.arena));
    let held = &mut px.ext_mut::<NestExt>(Face::Nest).sites;
    for (j, &(_, i, _)) in own.iter().enumerate() {
        held[i].1 = own.get(j + 1).map_or(fresh, |h| h.0);
    }
    px.sites[first] = draft;
    first
}

/// D3769: the sites of key `k` numbered so far that exec admit would pair by its walk order, each with its pipeline (the
/// view index `Px::pipeline_site` names it under, its source of key `k` in the walk), and the fill pipeline `kf`'s walk
/// entry: `None` (fail closed) when one has no known pipeline of that key, or `kf` none.
fn pin_set<'g>(px: &Px<'g, '_>, w: &[Entry], k: Key, kf: usize) -> Option<PgVec<'g, (usize, usize)>> {
    let pinned = pinned(px, w).1;
    w.iter().find(|e| e.0 == kf && e.1 == Some(k))?;
    let mut out: PgVec<'g, (usize, usize)> = PgVec::new_in(px.mcx());
    for (s, _) in px.sites.iter().enumerate().filter(|(s, d)| d.is_source() && (d.relation, d.index) == k && !pinned.contains(s)) {
        let q = (0..px.pipe_sites_len()).find(|&q| px.pipeline_site(q) == Some(s))?;
        w.iter().find(|e| e.0 == q && e.1 == Some(k))?;
        out.push_in((s, q));
    }
    Some(out)
}

/// D3769: `draft` numbered next and pinned to fill pipeline `kf`'s source, the sites `pins` names to theirs.
fn pin_site(px: &mut Px<'_, '_>, draft: SiteDesc, pins: PgVec<'_, (usize, usize)>, kf: usize) -> usize {
    let site = px.add_site(draft);
    px.site_pins.extend(pins); // alloc: Program
    px.site_pins.push((site, kf)); // alloc: Program
    site
}
