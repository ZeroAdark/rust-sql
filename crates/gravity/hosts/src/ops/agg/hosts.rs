//! M1's host implementations (plan §4.3, the end-to-end slice). M1's sources run on the hosts that exist: `src.pgrc` on
//! `GranuleRangeSource` and `src.index` on `IndexSource`, both derived by `v3_r1` / `v3_r1b` in registry.rs's table (the
//! recipes exec/src/v4/m1.rs admits), so the index host's `substitute` + key recheck serve EvalPlanQual unchanged. M1's
//! verdict, claim and condition-cache hosts are prerequisites in granule_source.rs (plan §5.2). Here: what M1's runtime
//! protocols build from a finished breaker region, each over its reader (`RegionFn`'s emit), and a site's bind facts.
//! Created and closed by M1 (owner M1-HOSTS).
use gravity_abi::hash::{Depth, KeyHash};
use gravity_driver::{ArenaRef, SourceBox};
use gravity_fn::arena::{AVec, Kind, OpArena};
use gravity_kernels::table::entries::{admit_filter_add, admit_filter_bits, ADMIT_ALL, ADMIT_EMPTY};
use gravity_program::{SiteDesc, ZonePred};
use memledger::{ChargedVec, Owner};
use types_error::{PgError, PgResult};
use types_rel::Relation;

use crate::ops::scan::pgrc::granule_source::{part_extrema, zone_census, GranuleRecipe, PartList, RtFilter, ROWS};
use crate::ops::Recipe;

/// No recipe kind of M1's own: every M1 site derives through registry.rs's earlier rows.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

/// A region reader run with the emit it feeds: `None` = the region is not the reader's.
pub type Reading<'a> = &'a mut dyn FnMut(&mut dyn FnMut(u64, u64, u64)) -> Option<usize>;

/// `keyed.twopass`'s admitted set in host_abi v4 m1's `AdmitHdr` layout, with its prefilter (V4R-209).
/// CONVERT (wave 2): `set` and `filter` are charged to the read region's slot owner by their capacity (ChargedVec).
pub struct AdmitSet {
    pub set: ChargedVec<u64>,
    pub mask: u64,
    pub count: u64,
    pub empty_key: u64,
    pub filter: ChargedVec<u64>,
    pub filter_bits: u64,
}

impl AdmitSet {
    /// The set that admits nothing (`every` false: count 0, an all-clear filter) or every key (`ADMIT_ALL` over one empty
    /// slot, an all-set one-word filter: D459, a spilled pass 1 with no owner result).
    pub fn whole(every: bool) -> AdmitSet {
        let (mut filter, mut set) = (ChargedVec::default(), ChargedVec::default());
        filter.with_vec(|v| v.push(if every { u64::MAX } else { 0 })); // alloc: Exec
        set.with_vec(|v| v.extend([ADMIT_EMPTY; 2])); // alloc: Exec
        AdmitSet { set, mask: 0, count: every as u64, empty_key: if every { ADMIT_ALL } else { 0 }, filter, filter_bits: 6 }
    }
}

/// The distinct keys `(w0, w1, nwords)` read emits, in `mask + 1` two-word slots (at least 64, at most half full) homed by
/// `KeyHash` over the key's `nwords` words and probed linearly; a one-word key is `(w0, 0)`; the empty pair is
/// `empty_key`. Every key, the empty pair too, sets its prefilter bits in a filter sized from the key count (B3-14). `None` when the region is not the reader's.
/// The statement's cancel raises between keys (PostgreSQL's CHECK_FOR_INTERRUPTS: the leader lays the set out).
/// CONVERT (wave 2): `owner` = the read region's slot owner and kind (pass 1's HashAgg, a build's HashBuild: the live
/// slot): the keys read are a scratch arena of the call on it, the kept set and filter charged to it (None: uncharged).
pub fn admit_set(read: Reading<'_>, owner: Option<(Owner, Kind)>) -> PgResult<Option<AdmitSet>> {
    let (o, k) = (owner.map(|o| o.0), owner.map_or(Kind::Region, |o| o.1));
    let a = OpArena::scratch_of(o, k);
    let (mut keys, mut refused) = (AVec::<([u64; 2], usize)>::new(), None);
    let got = read(&mut |w0, w1, nw| {
        if refused.is_none() {
            refused = keys.push(&a, ([w0, if nw == 2 { w1 } else { 0 }], nw.clamp(1, 2) as usize)).err();
        }
    });
    if got.is_none() {
        return Ok(None);
    }
    if let Some(r) = refused {
        return Err(out_of_memory(r.bytes));
    }
    let keys = keys.as_slice(&a);
    let bits = (2 * keys.len()).next_power_of_two().max(64).trailing_zeros();
    let mask = (1usize << bits) - 1;
    let mut set = ChargedVec::new(o, k); // alloc: Exec charged o
    set.resize(2 * (mask + 1), ADMIT_EMPTY).map_err(|_| out_of_memory(16 * (mask as u64 + 1)))?;
    let (mut count, mut empty_key) = (0u64, 0u64);
    let fbits = admit_filter_bits(keys.len());
    let mut filter = ChargedVec::new(o, k); // alloc: Exec charged o
    filter.resize((1usize << fbits) / 64, 0).map_err(|_| out_of_memory((1u64 << fbits) / 8))?;
    for (k, nw) in keys.iter() {
        cancelpoll::interrupts::check()?;
        admit_filter_add(&mut filter, fbits, KeyHash::of(&k[..*nw]));
        if *k == [ADMIT_EMPTY; 2] {
            count += 1 - empty_key;
            empty_key = 1;
            continue;
        }
        let mut i = KeyHash::of(&k[..*nw]).home(bits, Depth::TOP);
        // bounded("other: a linear probe of the admit table, which holds two slots or more per key (`bits` from twice the key count): a probe ends at the key or at one of the free slots, half the table or more")
        while [set[2 * i], set[2 * i + 1]] != *k {
            if set[2 * i] == ADMIT_EMPTY && set[2 * i + 1] == ADMIT_EMPTY {
                (set[2 * i], set[2 * i + 1]) = (k[0], k[1]);
                count += 1;
                break;
            }
            i = (i + 1) & mask;
        }
    }
    Ok(Some(AdmitSet { set, mask: mask as u64, count, empty_key, filter, filter_bits: fbits as u64 }))
}

/// PG's "out of memory" (53200) for `bytes` a scratch arena or a charged list was refused.
#[cold]
fn out_of_memory(bytes: u64) -> Box<PgError> {
    let e = PgError::error("out of memory").with_detail(format!("Failed on request of size {bytes} in a Gravity scratch region.")); // alloc: Cold
    Box::<PgError>::new(e.with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)) // alloc: Cold
}

/// `s`'s elements `keep` takes moved to its front, in order (`Vec::retain`'s); their count.
fn kept_len<T: Copy>(s: &mut [T], keep: impl Fn(&T) -> bool) -> usize {
    let mut w = 0;
    // bounded("other: its callers' lists: rt_filter's keys (at most the larger of its cap and 2^15)")
    for r in 0..s.len() {
        if keep(&s[r]) {
            s[w] = s[r];
            w += 1;
        }
    }
    w
}

/// `s` without its consecutive repeats at its front (`Vec::dedup`'s); their count.
fn dedup_len<T: Copy + PartialEq>(s: &mut [T]) -> usize {
    let mut w = s.len().min(1);
    // bounded("other: its callers' lists: rt_filter's keys and images (at most the larger of its cap and 2^15)")
    for r in 1..s.len() {
        if s[r] != s[w - 1] {
            s[w] = s[r];
            w += 1;
        }
    }
    w
}

/// A granule site's `facts` for this execution's snapshot `list`: its surviving rows (the granules the zone predicates
/// keep, at most the list's rows, at most an entries window's entries) and lane `lane`'s dense domain, the parts' exact
/// extrema when the lane's key is its datum word (`None` otherwise).
pub fn site_facts(rel: &Relation<'_>, list: &PartList, r: &GranuleRecipe, preds: &[ZonePred], lane: Option<usize>) -> PgResult<(u64, Option<(i64, i64)>)> {
    let (alive, _) = zone_census(rel, list, &r.attnos[..r.natts], preds)?;
    let rows = list.rows().min(alive.saturating_mul(ROWS as u64)).min(entries_bound(rel, r)?);
    let Some(k) = lane.filter(|&k| k < r.natts) else { return Ok((rows, None)) };
    let word = |v: i64| r.profiles.get(k).copied().flatten().is_some_and(|p| p.key.exact_key_to_datum_word(v) == Some(v as u64));
    Ok((rows, part_extrema(list, r.attnos[k] as u32)?.filter(|&(lo, hi)| word(lo) && word(hi))))
}

/// An entries window's rows (`GranuleRecipe::entries`, road dict.entries; its parts' entry runs): when every
/// part of the column is dictionary-coded, the footer's exact entry count over the generation (no part opened; the whole
/// generation, so at most a larger bound than the alive parts'), which sizes the table and its radix bits (8 M
/// entries against 100 M rows: P = 9, not 11). A bound on the table's groups, not a row count: a part with deletes
/// publishes its rows, each value among its entries. Any other column, or no committed publish: no bound (`u64::MAX`).
fn entries_bound(rel: &Relation<'_>, r: &GranuleRecipe) -> PgResult<u64> {
    let Some(a) = r.entries.and_then(|k| r.attnos.get(k)).and_then(|&a| usize::try_from(a).ok()?.checked_sub(1)) else { return Ok(u64::MAX) };
    let Some(c) = pgrc_am::footer::footer_codecs(rel)? else { return Ok(u64::MAX) };
    Ok(match (c.codec.get(a), c.ncodes.get(a)) {
        (Some(&pgrc_am::footer::CODEC_DICT), Some(&n)) if n > 0 => n,
        _ => u64::MAX,
    })
}

/// `join.rtfilter`'s filter on column `attno` from a finished build's reading: `(0, lo, hi)` the key range, `(1, key, 0)`
/// per key of a build within the reader's cap; no item at all = an empty build (`lo > hi`). PS1-22: `(2, lo word, hi word)`
/// per key of a small text build (its `Fp128` images, no range item). D3245: keys past `cap` (the probe's granules:
/// `rtfilter_set_max`) are first clipped to `clip`, the probe column's key range over its parts (asked only then), then
/// fold into runs of consecutive keys, kept when at most `cap`, else the range alone. `None` = the region is not the
/// reader's, or no filter.
/// CONVERT (wave 2): the keys and images read are a scratch arena of the call on `owner` (the build's slot owner and
/// kind; a refused carve is no filter: the probe reads every row). D4533: the kept filter's own runs and images are
/// copies charged to `owner` too (`RtFilter::charged`; a refused copy is no filter).
pub fn rt_filter(attno: i16, cap: usize, clip: &dyn Fn() -> Option<(i64, i64)>, read: Reading<'_>, owner: Option<(Owner, Kind)>) -> Option<RtFilter> {
    let ar = OpArena::scratch_of(owner.map(|o| o.0), owner.map_or(Kind::Region, |o| o.1));
    let (mut range, mut keys, mut fps, mut ok) = (None, AVec::<i64>::new(), AVec::<u128>::new(), true);
    let n = read(&mut |item, a, b| {
        ok &= match item {
            0 => {
                range = Some((a as i64, b as i64));
                true
            }
            2 => fps.push(&ar, a as u128 | (b as u128) << 64).is_ok(),
            _ => keys.push(&ar, a as i64).is_ok(),
        }
    })?;
    if !ok {
        return None;
    }
    let (lo, hi) = match range {
        _ if !fps.is_empty() && (range.is_some() || !keys.is_empty()) => return None,
        Some(r) => r,
        None if n == 0 => (i64::MAX, i64::MIN),
        None if !fps.is_empty() => (i64::MIN, i64::MAX),
        None => return None,
    };
    // bounded("other: the keys a finished build's reader hands on: at most the larger of its set cap (cost rtfilter_set_max: the probe's granules, at least RTFILTER_SET_MAX) and RTFILTER_RUNS_READ (2^15) by lowerv4 rtfilter `export`, about 22 ns a key by the cost home's measure")
    keys.as_mut_slice(&ar).sort_unstable();
    let n = dedup_len(keys.as_mut_slice(&ar));
    keys.truncate(n);
    if keys.len() > cap {
        if let Some((min, max)) = clip() {
            let n = kept_len(keys.as_mut_slice(&ar), |k| (min..=max).contains(k));
            keys.truncate(n);
            if keys.len() <= cap {
                fires::fire_shared("rtfilter.clip"); // shapecheck fire signal (D3245): keys past the cap within it once clipped to the probe's range
            }
        }
    }
    // a cancel publishes no filter (the scan reads every row; the build's next tick stops it)
    let Ok(mut runs) = RtFilter::runs_of(&mut cancelpoll::step("rt_filter: a build's key runs"), keys.as_slice(&ar)) else { return None };
    if runs.len() > cap {
        runs.clear();
    } else if keys.len() > cap {
        fires::fire_shared("rtfilter.runs"); // shapecheck fire signal (D3245): keys past the cap kept as runs
    }
    // bounded("other: the text images a small build's reader hands on: at most its set cap (cost rtfilter_set_max: the probe's granules, at least RTFILTER_SET_MAX) by lowerv4 rtfilter `export`")
    fps.as_mut_slice(&ar).sort_unstable();
    let n = dedup_len(fps.as_mut_slice(&ar));
    fps.truncate(n);
    RtFilter::charged(attno, lo, hi, &runs, fps.as_slice(&ar), owner)
}
