//! The `src.verdicts` protocol: a pgrc (or heap range) site's host verdicts for one execution, and the verdict census
//! line (PGRUST_GRAVITY_REPORT).
use gravity_hosts::ops::scan::pgrc::{granule_source, cond_cache};
use gravity_hosts::ops::scan::pgrc::granule_source::VerdictClaim;
use gravity_program::protos::{word_zone_at_of, word_zone_list_of, Proto, ProtoKind, VERDICT_COND, VERDICT_STREAM, VERDICT_WORD_ZONE};
use gravity_program::{ZoneOp, ZonePred};
use types_error::PgResult;

use crate::ops::{Live, Prepare, Protocol};
use crate::glue;

/// `src.verdicts` (V4R-4): a pgrc site's host verdicts for this execution, into its recipe's cell (BIND gave each
/// execution its own) before any source binds. Slot 0 carries the enables and the covered quals' first run of bound words,
/// slots 1 / 2 the qual shape's fingerprint (every slot-1 proto may carry one more run: `cond_words`), slot 3
/// decode.late's lanes, slot 4 decode.packed's candidates; slot 0 finishes the
/// condition fingerprint over this execution's words (a literal never rides a proto: the front key does not hash them).
/// A shape of none or an unhashable word (a compressed or external varlena) leaves the cache off for the execution; the
/// other verdicts stand. D2976: `pgrust.condition_cache` (boot ON) governs this cache as the old AM's: off, the execution
/// takes no cache (no lookup, no publish); `pgrust.condition_cache_size` stays its budget.
/// A heap range site's slot 0 carries one enable, `VERDICT_STREAM` (heap.stream, V4R-32), into its recipe.
pub(crate) struct Verdicts;

impl Protocol for Verdicts {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Verdicts
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        if p.slot != 0 {
            return Ok(None);
        }
        // a heap range site's one verdict: heap.stream (V4R-32; the bit is the program's, the recipes keep it). A program's
        // site number is not its recipe's index (a write target's site takes no recipe), so the switch goes to every range
        // recipe over the site's relation: all of them at the first slot-0 verdicts proto (`set_streams`), not a registry
        // walk per site.
        let first = || cx.program.v4.as_ref().and_then(|d| d.protos.iter().find(|q| q.kind == ProtoKind::Verdicts && q.slot == 0)).is_some_and(|q| q.site == p.site);
        if first() {
            set_streams(cx.program, &cx.admitted.registry);
        }
        if p.arg & !VERDICT_STREAM == 0 {
            return Ok(None);
        }
        let heap = usize::try_from(p.site).ok().and_then(|s| cx.program.sites.get(s)).map(|d| d.relation);
        let reg = &cx.admitted.registry;
        if (0..reg.len()).filter_map(|i| reg.range_recipe(i)).any(|r| Some(r.heap) == heap.map(|h| h as u32)) {
            return Ok(None);
        }
        let Some(r) = usize::try_from(p.site).ok().and_then(|s| cx.admitted.registry.granule_recipe(s)) else {
            return Err(glue::internal("gravity: src.verdicts on a site without a pgrc granule recipe"));
        };
        let shape_word = |slot: u8| cx.program.v4.as_ref().and_then(|d| d.protos.iter().find(|q| q.kind == ProtoKind::Verdicts && q.site == p.site && q.slot == slot)).map_or(0, |q| q.arg);
        let shape = [shape_word(1), shape_word(2)];
        let mut enables = p.arg;
        let words = (enables & VERDICT_COND != 0 && shape != [0, 0]).then(|| cond_words(cx.program, p, cx.owner)).unwrap_or_default();
        // D289: a covered word bound NULL (an extern parameter) takes no cache for the execution: it fills nothing
        let bound = words.iter().all(|&(w, _)| cx.nulls.get(w) == Some(&0));
        // an unlogged relation takes the cache only under pgrust.condition_cache_unlogged (default on)
        let mode = crate::inputs::pgrc::condition_cache_mode();
        let on = mode != guc_tables::ConditionCacheMode::Off && (!r.cond_unlogged || crate::inputs::pgrc::condition_cache_unlogged_on());
        // D3161: a zero budget keeps nothing, so its entry would be a scan-local memo nothing reads (a scan asks each
        // granule once): the execution takes no cache, and the program records no verdict words per row
        let budget = if on { crate::inputs::pgrc::condition_cache_bytes() } else { 0 };
        let (taken, fp) = cond_take(enables, budget, mode, || match shape != [0, 0] && bound {
            true => cond_cache::fingerprint(shape, cx.words, cx.nulls, &words),
            false => None,
        });
        enables = taken;
        if fp != [0, 0] {
            cond_cache::set_capacity(budget);
        }
        let census = (*REPORT.get_or_init(crate::inputs::knobs::seam::report_on) && first()).then(|| Census(census(), zone_census()));
        // D289: the cell is the Admitted's; this execution writes it only as its one live claimant (another portal's
        // live execution contests it: both read LEGACY)
        let claim = r.verdicts.claim();
        if claim.owner() {
            r.verdicts.set(1, fp[0]);
            r.verdicts.set(2, fp[1]);
            r.verdicts.set(3, shape_word(3));
            r.verdicts.set(4, shape_word(4));
            r.verdicts.set(0, enables);
            // the word-valued zone predicates (a generic plan's `lane op $n`), over this execution's words
            let wz = cx.program.v4.as_ref().into_iter().flat_map(|d| d.protos.iter()).filter(|q| q.kind == ProtoKind::Verdicts && q.site == p.site && q.slot >= VERDICT_WORD_ZONE);
            r.verdicts.set_word_zone(wz.map(|q| (q.arg, q.words.base as usize)), cx.words, cx.nulls);
            // NEW-72: the IN lists' members, walked from this execution's arrays
            let mut images = Vec::new(); // alloc: Exec
            r.verdicts.add_word_zone(&list_preds(cx.program, p.site, cx.words, cx.nulls, Some(&mut images)));
            r.verdicts.add_code_images(images);
        }
        Ok(Some(Box::new(Held { _claim: claim, _census: census }))) // alloc: Exec
    }
}

/// NEW-72: site `site`'s IN-list predicates for an execution of words `words` / `nulls` (its `WORD_ZONE_LIST` slots,
/// lowerv4 verdict_zone.rs `list_of`): each array's non-NULL members' span (`lane >= min`, `lane <= max`, inexact) and
/// its distinct members as the lane's `InMember` predicates (the bloom's one disjunction per lane), what the lowering
/// baked from the image before. A NULL array, one of NULLs alone, or one the walk cannot open adds none.
/// sortpath: `images` takes a prefix list's least and greatest members' text (`CodeImage`) for the parts' code zones.
pub(crate) fn list_preds(program: &gravity_program::Program, site: i16, words: &[u64], nulls: &[u8], images: Option<&mut Vec<granule_source::CodeImage>>) -> Vec<ZonePred> {
    list_preds_of(word_zone_slots(program, site as i64), words, nulls, images)
}

/// `list_preds` over a site's word-zone slots `slots` (`word_zone_slots`, or `ZoneSlots::of`).
fn list_preds_of<'q>(slots: impl Iterator<Item = &'q Proto>, words: &[u64], nulls: &[u8], mut images: Option<&mut Vec<granule_source::CodeImage>>) -> Vec<ZonePred> {
    let mut out = Vec::new(); // alloc: Exec
    for q in slots {
        let Some(facts) = word_zone_list_of(q.arg) else { continue };
        let w = q.words.base as usize;
        let (Some(&x), Some(0)) = (words.get(w), nulls.get(w)) else { continue };
        let ef = gravity_expr::nodes::array::ElemFacts::from_word(facts as i64);
        let mut scratch = [0u8; 256];
        let mut region = gravity_expr::Region::over(&mut scratch);
        let Ok(mut walk) = gravity_expr::nodes::array::Walk::over(gravity_expr::Datum::from_u64(x), ef, &mut region) else { continue };
        let mut members = Vec::new(); // alloc: Exec
        // a probe-word list (`WORD_ZONE_LIST_HASH`): each member's probe word, made from its image as the lowering makes a
        // constant's (P5-11 / P5-13 `HashEq`); a member the host does not read in place gives the list none
        let hash = gravity_program::protos::word_zone_list_hash(q.arg).then(|| if ef.len == -1 { gravity_program::BOUND_TEXT_HASH } else { gravity_program::BOUND_FIXED_HASH | ef.len as u8 });
        // sortpath (4): a path lane's list: each member's prefix key (`WORD_ZONE_LIST_PREFIX`), a text image's as the
        // lowering makes a constant's (SRC-09); a member it cannot read gives the list none
        let prefix = gravity_program::protos::word_zone_list_prefix(q.arg) && ef.len == -1;
        let hash = if prefix { Some(gravity_program::BOUND_TEXT_PREFIX) } else { hash };
        let mut whole = true;
        // sortpath: a prefix list's least and greatest member by bytes (the code zones' span ends)
        let mut ends: Option<(Box<[u8]>, Box<[u8]>)> = None;
        while let Some(e) = walk.next() {
            if e.isnull {
                continue;
            }
            if prefix && images.is_some() {
                match (granule_source::text_image(e.value.as_u64()), ends.as_mut()) {
                    (Some(t), Some((lo, hi))) => {
                        if t < *lo {
                            *lo = t;
                        } else if t > *hi {
                            *hi = t;
                        }
                    }
                    (Some(t), None) => ends = Some((t.clone(), t)),
                    (None, _) => whole = false,
                }
            }
            let v = match hash {
                Some(form) => granule_source::image_key(e.value.as_u64(), form),
                None => gravity_decompose::ops::shared::scan::zone_word_of(ef.typ, ef.len as i32, e.value.as_u64()),
            };
            match v {
                Some(v) => members.push(v), // alloc: Exec
                None => whole &= hash.is_none(),
            }
        }
        members.sort_unstable();
        members.dedup();
        if !whole {
            continue;
        }
        if prefix {
            if let (Some(&lo), Some(&hi)) = (members.first(), members.last()) {
                out.push(ZonePred::prefix(q.peer, ZoneOp::Ge, lo)); // alloc: Exec
                out.push(ZonePred::prefix(q.peer, ZoneOp::Le, hi)); // alloc: Exec
                if let (Some(v), Some((a, b))) = (images.as_deref_mut(), ends.take()) {
                    v.push(granule_source::CodeImage { lane: q.peer, op: ZoneOp::Ge, key: lo, image: a }); // alloc: Exec
                    v.push(granule_source::CodeImage { lane: q.peer, op: ZoneOp::Le, key: hi, image: b }); // alloc: Exec
                }
            }
            continue;
        }
        if hash.is_some() {
            // a list past `MAX_LIST` distinct members (one class to the keys) probes no bloom: each member is a chance that a
            // granule holds one, so such a test rarely skips a granule, and it costs a probe per member per granule (NEW-72
            // entry B's perf pass: 1000 text or uuid members, +3 to +9 % over the whole scan); without it the scan reads as
            // the base did (no list probe)
            if members.len() <= gravity_program::cost::expr::MAX_LIST {
                out.extend(members.iter().map(|&v| ZonePred::hashed(q.peer, ZoneOp::HashIn, v))); // alloc: Exec
            }
            continue;
        }
        let (Some(&lo), Some(&hi)) = (members.first(), members.last()) else { continue };
        out.push(ZonePred::word(q.peer, ZoneOp::Ge, lo)); // alloc: Exec
        out.push(ZonePred::word(q.peer, ZoneOp::Le, hi)); // alloc: Exec
        out.extend(members.iter().map(|&v| ZonePred::word(q.peer, ZoneOp::InMember, v))); // alloc: Exec
    }
    out
}

/// NEW-72: site `site`'s zone predicates for an execution of words `words` / `nulls`, as its source binds them: the
/// static ones with each bound one's value (`WORD_ZONE_AT` slots, `granule_source::bind_statics`), then the IN lists'
/// (`list_preds`). `None` when a bound one has no value: the execution takes no zone verdict over the site, so a reader
/// of the census or the answers must take none either.
pub(crate) fn site_zone<'a>(program: &'a gravity_program::Program, site: usize, words: &[u64], nulls: &[u8]) -> Option<std::borrow::Cow<'a, [ZonePred]>> {
    zone_of(program, site, || word_zone_slots(program, site as i64), words, nulls)
}

/// Site `site`'s word-zone slots (`src.verdicts` slots `VERDICT_WORD_ZONE..`) in declaration order: a walk of every proto.
fn word_zone_slots(program: &gravity_program::Program, site: i64) -> impl Iterator<Item = &Proto> + '_ {
    program.v4.iter().flat_map(|d| d.protos.iter()).filter(move |q| q.kind == ProtoKind::Verdicts && q.site as i64 == site && q.slot >= VERDICT_WORD_ZONE)
}

/// D3586: the program's word-zone slots by site, in declaration order within a site, gathered in one walk of the protos:
/// a reader of every site's (`runner::plan`'s prepare) takes each site's from here, not a walk of every proto per site.
/// That walk was quadratic in an Append's arms: a pruned generic EXECUTE over 2000 partitions spent 5 of its 29 ms in it.
pub(crate) struct ZoneSlots<'p> {
    by_site: Vec<&'p Proto>,
}

impl<'p> ZoneSlots<'p> {
    pub(crate) fn new(program: &'p gravity_program::Program) -> ZoneSlots<'p> {
        let mut by_site: Vec<&'p Proto> = program.v4.iter().flat_map(|d| d.protos.iter()).filter(|q| q.kind == ProtoKind::Verdicts && q.slot >= VERDICT_WORD_ZONE).collect(); // alloc: Exec
        // stable: a site's slots keep their declaration order (`list_preds` adds its lists' predicates in it)
        by_site.sort_by_key(|q| q.site);
        ZoneSlots { by_site }
    }

    /// Site `site`'s slots, in declaration order.
    pub(crate) fn of(&self, site: usize) -> impl Iterator<Item = &'p Proto> + Clone + '_ {
        let lo = self.by_site.partition_point(|q| (q.site as i64) < site as i64);
        let hi = self.by_site.partition_point(|q| q.site as i64 <= site as i64);
        self.by_site[lo..hi].iter().copied()
    }

    /// `site_zone` over this index.
    pub(crate) fn site_zone(&self, program: &'p gravity_program::Program, site: usize, words: &[u64], nulls: &[u8]) -> Option<std::borrow::Cow<'p, [ZonePred]>> {
        zone_of(program, site, || self.of(site), words, nulls)
    }
}

/// `site_zone` over site `site`'s word-zone slots, `slots()`.
fn zone_of<'a, 'q, I: Iterator<Item = &'q Proto>>(program: &'a gravity_program::Program, site: usize, slots: impl Fn() -> I, words: &[u64], nulls: &[u8]) -> Option<std::borrow::Cow<'a, [ZonePred]>> {
    let statics = program.sites.get(site)?.zone_preds();
    // a site with no bound predicate and no IN list reads its static predicates as they are (no copy per execution)
    if !slots().any(|q| word_zone_at_of(q.arg).is_some() || word_zone_list_of(q.arg).is_some()) {
        return Some(std::borrow::Cow::Borrowed(statics));
    }
    let mut zone = statics.to_vec(); // alloc: Exec
    let bound: Vec<(u16, i64)> = slots() // alloc: Exec
        .filter_map(|q| {
            let at = word_zone_at_of(q.arg)?;
            let w = q.words.base as usize;
            let (Some(&x), Some(0)) = (words.get(w), nulls.get(w)) else { return None };
            Some((at, granule_source::word_value(x, gravity_program::protos::word_zone_of(q.arg).2)?))
        })
        .collect();
    granule_source::bind_statics(&mut zone, &bound)?;
    zone.extend(list_preds_of(slots(), words, nulls, None)); // alloc: Exec
    Some(std::borrow::Cow::Owned(zone))
}

/// The condition cache's covered bind words of slot-0 proto `p`'s site in need order, each with its varlena bit: the runs
/// of `p` and of every slot-1 proto of the site in declaration order (lowerv4 verdict_cond.rs `Run`: `n` words from
/// `words.base`, word `i`'s bit `peer >> min(i, 15) & 1`).
fn cond_words(program: &gravity_program::Program, p: &Proto, owner: Option<(memledger::Owner, memledger::Kind)>) -> memledger::ChargedVec<(usize, bool)> {
    let more = program.v4.iter().flat_map(|d| d.protos.iter()).filter(|q| q.kind == ProtoKind::Verdicts && q.site == p.site && q.slot == 1);
    let runs = core::iter::once(p).chain(more).map(|q| (q.words.base as usize, q.words.n as usize, q.peer));
    let mut words = memledger::ChargedVec::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1)); // alloc: Exec charged owner.map(|o| o.0)
    words.with_vec(|v| v.extend(runs.flat_map(|(base, n, mask)| (0..n).map(move |i| (base + i, mask >> i.min(15) & 1 != 0))))); // alloc: Exec charged owner
    words
}

/// heap.stream for every range recipe of the execution: each takes the `VERDICT_STREAM` bit of the last slot-0
/// verdicts proto, in declaration order, over a site of its relation (the bit a proto-by-proto pass leaves last); a recipe
/// no such proto names keeps its bit. One pass over the protos and one over the registry.
fn set_streams(program: &gravity_program::Program, reg: &gravity_hosts::ops::HostRegistry) {
    let mut bit: std::collections::HashMap<u32, bool> = std::collections::HashMap::new(); // alloc: Exec
    for q in program.v4.iter().flat_map(|d| d.protos.iter()).filter(|q| q.kind == ProtoKind::Verdicts && q.slot == 0) {
        if let Some(d) = usize::try_from(q.site).ok().and_then(|s| program.sites.get(s)) {
            bit.insert(d.relation as u32, q.arg & VERDICT_STREAM != 0);
        }
    }
    for r in (0..reg.len()).filter_map(|i| reg.range_recipe(i)) {
        if let Some(&on) = bit.get(&r.heap) {
            r.set_stream(on);
        }
    }
}

/// The execution's condition-cache enables and fingerprint from the program's `enables`, the cache's `budget` (0 under
/// `pgrust.condition_cache = off`, or a zero size) and the covered quals' fingerprint `fp`. No budget or no fingerprint:
/// VERDICT_COND off and the fingerprint [0, 0], so the execution looks up and publishes nothing, has no hit, and so no
/// zero-read count answer either (VERDICT_UNREAD acts only on a hit): every window decodes its columns.
fn cond_take(mut enables: u64, budget: u64, mode: guc_tables::ConditionCacheMode, fp: impl FnOnce() -> Option<[u64; 2]>) -> (u64, [u64; 2]) {
    if mode != guc_tables::ConditionCacheMode::Full {
        enables &= !gravity_program::protos::VERDICT_UNREAD;
    }
    match (mode != guc_tables::ConditionCacheMode::Off && budget > 0 && enables & VERDICT_COND != 0).then(fp).flatten() {
        Some(fp) => (enables, fp),
        None => (enables & !VERDICT_COND, [0, 0]),
    }
}

static REPORT: pgsync::OnceLock<bool> = pgsync::OnceLock::new(); // PGRUST_GRAVITY_REPORT, once per backend: the census line

/// hosts granule_source's granule / verdict / packed / elided / contested / unread / constant-key (V4R-19 A) counters, the
/// condition cache's hits / misses / published granules, agg_percode_const's windows (V4R-71: folded / folded on a
/// miss / declined on a miss / declined otherwise), then `REUSE` words: each kind of cross-statement reuse
/// (`cond_cache::reuse::census`), the condition tables built and the sidecar files written (always 0 now).
fn census() -> [u64; N_CENSUS] {
    let (g, v, p, c) = (granule_source::granule_census(), granule_source::verdict_census(), granule_source::packed_census(), granule_source::const_key_census());
    let (cc, pk, r) = (cond_cache::census(), gravity_kernels::dict::strategies::percode_const_census(), cond_cache::reuse::census());
    let built = gravity_hosts::dict::text_hosts::cond_tables_built();
    let written = 0; // the sidecar files written: none since the Fp128 sidecars are gone
    [g[0], g[1], g[2], g[3], g[4], v[0], v[1], v[2], v[3], v[4], p[0], p[1], granule_source::lane_elided_census(), granule_source::contested_census(), granule_source::unread_census(), c[0], c[1], cc[0], cc[1], cc[2], pk[0], pk[1], pk[2], pk[3], r[0], r[1], r[2], r[3], built, written]
}

/// The census words: `census`'s, in the line's order; the last `REUSE` are the reuse line's.
const N_CENSUS: usize = 30;
const REUSE: usize = 6;

/// The execution's claim on a site's verdict cell (D289: the cell is the Admitted's; the last live claim's end clears it)
/// and the first site's census.
struct Held {
    _claim: VerdictClaim,
    _census: Option<Census>,
}
impl Live for Held {
    fn answers(&self) -> u16 {
        crate::ops::ask::NONE
    }
}

/// The census at prepare, one per execution (the first site's verdicts); dropped with the execution's protocols, it logs
/// the deltas (none moved: no line). The counters are process-wide statics: a concurrent statement in another backend
/// thread can inflate a delta (as lib.rs's granule census deltas).
struct Census([u64; N_CENSUS], [u64; 3]);

/// The zone store's `[hits, built, built and not kept under pgrust.zone_store = off]` so far in this process.
fn zone_census() -> [u64; 3] {
    let [hits, built] = granule_source::zone_store_counts();
    [hits, built, granule_source::zone_store_skipped()]
}

impl Drop for Census {
    fn drop(&mut self) {
        let d: [u64; N_CENSUS] = { let n = census(); core::array::from_fn(|i| n[i].wrapping_sub(self.0[i])) };
        // every execution with a census logs its reuse line
        let r = &d[N_CENSUS - REUSE..];
        let z: [u64; 3] = { let n = zone_census(); core::array::from_fn(|i| n[i].wrapping_sub(self.1[i])) };
        let line = elog::with_debug_query_string(|q| format!("gravity: reuse verdicts={} codetabs={}/{} feedback={} sidecars={}/{} unread={} zones={}/{}/{} :: {}", r[0], r[1], r[4], r[2], r[3], r[5], d[14], z[0], z[1], z[2], q.unwrap_or("").replace('\n', " "))); // alloc: Cold
        let _ = elog::ereport(types_error::LOG).errmsg(line).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "reuse"));
        if d[..N_CENSUS - REUSE] != [0; N_CENSUS - REUSE] {
            let line = elog::with_debug_query_string(|q| format!("gravity: verdicts granules={}/{}/{}/{} parts_answered={} skipped=bloom:{}/psma:{}/cond:{}/rt:{} narrowed={} packed={}/{} elided={} contested={} unread={} constkey={}/{} cond={}/{}/{} pkconst={}/{}/{}/{} :: {}", d[0], d[1], d[2], d[4], d[3], d[5], d[6], d[8], d[9], d[7], d[10], d[11], d[12], d[13], d[14], d[15], d[16], d[17], d[18], d[19], d[20], d[21], d[22], d[23], q.unwrap_or("").replace('\n', " "))); // alloc: Cold
            let _ = elog::ereport(types_error::LOG).errmsg(line).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "verdicts"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::protos::{word_zone_arg, word_zone_at_arg, word_zone_list_arg, V4Descs};
    use gravity_program::{Arena, Code, End, Layout, Pipeline, Reason, SiteDesc, WritePlan};

    /// `pgrust.condition_cache = off` (a zero budget): no fingerprint and no VERDICT_COND, whatever the program enables, so
    /// the execution neither looks up nor publishes, and the zero-read count answer (VERDICT_UNREAD, which acts only on a
    /// hit) never fires; on, the program's enables and the quals' fingerprint stand.
    #[test]
    fn the_switch_off_takes_no_condition_cache_and_so_no_zero_read_answer() {
        use gravity_program::protos::VERDICT_UNREAD;
        let enables = VERDICT_COND | VERDICT_UNREAD;
        assert_eq!(cond_take(enables, 0, guc_tables::ConditionCacheMode::Full, || panic!("off: no fingerprint taken")), (VERDICT_UNREAD, [0, 0]));
        assert_eq!(cond_take(enables, 1 << 20, guc_tables::ConditionCacheMode::Full, || Some([7, 9])), (enables, [7, 9]));
        assert_eq!(cond_take(enables, 1 << 20, guc_tables::ConditionCacheMode::Full, || None), (VERDICT_UNREAD, [0, 0]), "no fingerprint: no cache");
    }

    #[test]
    fn only_full_mode_allows_unread_counts() {
        use guc_tables::ConditionCacheMode;
        use gravity_program::protos::VERDICT_UNREAD;
        let enables = VERDICT_COND | VERDICT_UNREAD;
        assert_eq!(cond_take(enables, 1 << 20, ConditionCacheMode::Off, || panic!("off takes no fingerprint")), (0, [0, 0]));
        for mode in [ConditionCacheMode::Granule, ConditionCacheMode::Rows] {
            assert_eq!(cond_take(enables, 1 << 20, mode, || Some([7, 9])), (VERDICT_COND, [7, 9]));
        }
        assert_eq!(cond_take(enables, 1 << 20, ConditionCacheMode::Full, || Some([7, 9])), (enables, [7, 9]));
    }

    /// A program of `nsites` range sites and the protos `protos` (no pipeline reads them here).
    fn program(nsites: usize, protos: Vec<Proto>) -> gravity_program::Program {
        let st = Arena::new(1 << 16);
        let sites: Vec<SiteDesc> = (0..nsites).map(|_| SiteDesc::range_source(st.handle(), 7, &[1])).collect(); // alloc: Cold
        let layout = Layout { nsites: nsites as u16, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let pipes = [Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)];
        gravity_program::Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop)))).with_v4(V4Descs::new(protos))
    }

    /// D3586: `ZoneSlots` gives each site the word-zone slots a walk of every proto gives it, in declaration order (the order
    /// `list_preds` adds its lists' predicates in), over sites out of order, other kinds and slots, and a site-less proto.
    #[test]
    fn zone_slots_equal_the_walk_of_every_proto() {
        let mut protos = Vec::new(); // alloc: Cold
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for i in 0..600u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let site = (x % 41) as i16 - 1;
            let kind = if x >> 8 & 3 == 0 { ProtoKind::Arms } else { ProtoKind::Verdicts };
            let slot = ((x >> 12) % 8) as u8;
            let arg = match x >> 20 & 3 {
                0 => word_zone_arg(i as u16, 1, 4),
                1 => word_zone_at_arg(i as u16, 1, 4, 2),
                2 => word_zone_list_arg(i),
                _ => i,
            };
            protos.push(Proto::new(kind, 0).site(site).slot(slot).words(i as u16, 1).arg(arg)); // alloc: Cold
        }
        let p = program(40, protos);
        let slots = ZoneSlots::new(&p);
        let mut seen = 0;
        // usize::MAX reads as site -1 in both (a site-less proto's), as `site_zone`'s walk read it
        for site in (0..42usize).chain([usize::MAX]) {
            let walk: Vec<(u16, u64)> = word_zone_slots(&p, site as i64).map(|q| (q.words.base, q.arg)).collect(); // alloc: Cold
            let index: Vec<(u16, u64)> = slots.of(site).map(|q| (q.words.base, q.arg)).collect(); // alloc: Cold
            assert_eq!(index, walk, "site {site}");
            seen += walk.len();
        }
        assert!(seen > 100, "the program has word-zone slots to compare ({seen})");
    }
}
