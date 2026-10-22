//! M3's host implementations (M3-plan.md §6, cards/M3.md L3): the extent residency the walk's width reads
//! (`extent_resident`, cachestat(2) through vfs over a claim's stream extents), the non-base code select over a code frame (widths
//! 1-16, code_sparse's kernel side) and the codefront light / pair flush helpers. L1 stub (M3-L1, V4R-107): M3 derives
//! no source of its own yet. Created by M3 (owner M3-HOSTS).
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::Recipe;

/// No recipe kind of M3's own yet: every site an M3 statement opens derives through the earlier milestones' rows.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

/// `extent_resident` (road width.walk, runs/walk_width.rs): the page-cache residency of granules `[g_lo, g_hi)` of one
/// column's cursor, `(pages cached, pages)` over its values and validity extents, file-contiguous extents probed as one
/// span (one cachestat each, never per granule); `None` when the part cannot tell (an in-memory image, a kernel
/// without cachestat): the walk then takes its cold width.
pub fn extent_resident(cur: &pgrc_read::StreamCursor<'_>, g_lo: u32, g_hi: u32) -> Option<(u64, u64)> {
    spans_resident(&mut |f| cur.granule_extents(g_lo, g_hi, f), &|off, len| cur.page_resident(off, len))
}

/// `extents`' file-contiguous runs each probed once by `probe`, the answers summed; `None` once one cannot tell.
fn spans_resident(extents: &mut dyn FnMut(&mut dyn FnMut(u64, u64)), probe: &dyn Fn(u64, u64) -> Option<(u64, u64)>) -> Option<(u64, u64)> {
    let (mut span, mut acc) = ((0u64, 0u64), Some((0u64, 0u64)));
    let add = |s: (u64, u64), acc: &mut Option<(u64, u64)>| {
        if let (Some((c, p)), true) = (*acc, s.1 != 0) {
            *acc = probe(s.0, s.1).map(|(c2, p2)| (c + c2, p + p2));
        }
    };
    extents(&mut |off, len| {
        if span.1 != 0 && off == span.0 + span.1 {
            span.1 += len;
        } else {
            add(span, &mut acc);
            span = (off, len);
        }
    });
    add(span, &mut acc);
    acc
}

#[cfg(test)]
mod tests {
    use super::spans_resident;

    /// The probe's cost (main 09-25: measured before WALK depends on it): `M3_RESIDENT_DIR=<pgrc table dir>
    /// M3_RESIDENT_ATTNOS=5,.. cargo test -p gravity_hosts --lib resident_probe_cost -- --ignored --nocapture` opens every
    /// part-N.pgrc and probes each granule (a walk claim) of each named column, printing claims, ns per claim (p50 /
    /// p99 / mean) and the cached share.
    #[test]
    #[ignore]
    fn resident_probe_cost() {
        let dir = std::env::var("M3_RESIDENT_DIR").expect("M3_RESIDENT_DIR");
        let attnos: Vec<u32> = std::env::var("M3_RESIDENT_ATTNOS").unwrap_or("5".into()).split(',').map(|a| a.parse().unwrap()).collect();
        let mut parts: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pgrc")).collect();
        parts.sort();
        for attno in attnos {
            let (mut ns, mut cached, mut pages, mut unknown) = (Vec::new(), 0u64, 0u64, 0u64); // alloc: Cold
            for path in parts.iter() {
                let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
                let part = std::sync::Arc::new(pgrc_read::OpenPart::open(Box::new(pgrc_read::VfsPartIo::open(&c).unwrap()), &pgrc_read::PartExpect::none()).unwrap()); // alloc: Cold
                let Ok(cur) = pgrc_read::StreamCursor::open(part, pgrc_am::scan::codec_binding(), attno, 0) else { continue };
                for g in 0..cur.granule_count() {
                    let t = std::time::Instant::now();
                    let r = super::extent_resident(&cur, g, g + 1);
                    ns.push(t.elapsed().as_nanos() as u64);
                    match r {
                        Some((c, p)) => (cached, pages) = (cached + c, pages + p),
                        None => unknown += 1,
                    }
                }
            }
            ns.sort_unstable();
            let n = ns.len().max(1);
            println!("attno {attno}: {} parts, {} claims, ns/claim p50 {} p99 {} mean {}, cached {cached} of {pages} pages, unknown {unknown}", parts.len(), ns.len(), ns[n / 2], ns[n * 99 / 100], ns.iter().sum::<u64>() / n as u64);
        }
    }

    /// Contiguous extents are one probe; a gap starts another; an unknown answer is unknown for the claim.
    #[test]
    fn contiguous_extents_are_one_probe_and_unknown_is_unknown() {
        let calls = core::cell::RefCell::new(Vec::new()); // alloc: Cold
        let probe = |off: u64, len: u64| {
            calls.borrow_mut().push((off, len));
            Some((len / 4096 / 2, len / 4096))
        };
        let ext = [(0u64, 8192u64), (8192, 8192), (32768, 4096), (36864, 12288)];
        let got = spans_resident(&mut |f| ext.iter().for_each(|&(o, l)| f(o, l)), &probe);
        assert_eq!(*calls.borrow(), [(0, 16384), (32768, 16384)]);
        assert_eq!(got, Some((4, 8)));
        assert_eq!(spans_resident(&mut |_| {}, &probe), Some((0, 0)), "no extents: nothing to probe");
        let unknown = |off: u64, _| (off == 0).then_some((1, 1));
        assert_eq!(spans_resident(&mut |f| ext.iter().for_each(|&(o, l)| f(o, l)), &unknown), None);
    }
}

