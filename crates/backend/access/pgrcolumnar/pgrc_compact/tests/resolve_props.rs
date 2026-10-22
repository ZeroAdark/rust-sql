mod common;

use std::collections::{BTreeSet, HashMap, HashSet};

use common::{cases, random_decl, ref_cmp, Rng};
use pgrc_compact::ident::{self, OutMap};
use pgrc_compact::order::{concat_order, sorted_order, Input};
use pgrc_compact::{JobKind, RowMapView};
use pgrc_format::manifest::{check_row_ids, with_next_row_id, with_ranges, IdRanges, Manifest, ManifestHeader, PartRecord};
use pgrc_format::rowid::{align_up, ROWID_FIRST, SEG_EXTENT_IDS};
use pgrc_format::rowmap::GranMap;
use pgrc_write::seal::cmp::KeySlot;

enum Map {
    Implicit(u64),
    Granules(GranMap),
}

struct Part {
    part_no: u32,
    map: Map,
    ranges: IdRanges,
    payload: Vec<Option<i64>>,
    dv: BTreeSet<u32>,
}

impl Part {
    fn view(&self) -> RowMapView<'_> {
        match &self.map {
            Map::Implicit(b) => RowMapView::Implicit(*b),
            Map::Granules(m) => RowMapView::Granules(m),
        }
    }
}

#[derive(Default)]
struct Table {
    parts: Vec<Part>,
    next: u32,
    next_id: u64,
    live: HashMap<u64, Option<i64>>,
    dropped: HashSet<u64>,
    jobs: [u64; 3],
}

fn value(r: &mut Rng) -> Option<i64> {
    (!r.chance(15)).then(|| r.below(9) as i64 - 4)
}

impl Table {
    fn reserve(&mut self, n: u64) -> u64 {
        let lo = self.next_id.max(ROWID_FIRST);
        self.next_id = lo + align_up(n);
        lo
    }

    fn load(&mut self, r: &mut Rng, sealed: bool) {
        let n = 1 + r.below(40) as u32;
        let part_no = self.next;
        self.next += 1 + r.below(3) as u32;
        // the counter is shared: some IDs between parts are burned (another table writer, a tail)
        self.reserve(1 + r.below(600));
        let (map, ranges) = if sealed {
            let k = 1 + r.below(2);
            let bases: Vec<u64> = (0..k).map(|_| self.reserve(SEG_EXTENT_IDS)).collect();
            let mut p = r.below(SEG_EXTENT_IDS * k - 40 * 10) as u32;
            let pos: Vec<u32> = (0..n).map(|_| { let gap = if r.chance(50) { 1 } else { 9 }; p += 1 + r.below(gap) as u32; p }).collect();
            let ranges = bases.iter().map(|&b| (b, b + SEG_EXTENT_IDS)).collect();
            let runs: Vec<(u64, u32)> = pos.iter().map(|&l| (pgrc_format::rowid::seg_id(&bases, l).expect("in its extents"), 1)).collect();
            let body = pgrc_format::rowmap::encode_runs(&runs, pgrc_format::rowmap::Opts::default()).expect("encodes");
            (Map::Granules(GranMap::decode(&body, u64::from(n)).expect("decodes")), ranges)
        } else {
            let base = self.reserve(u64::from(n));
            (Map::Implicit(base), vec![(base, base + align_up(u64::from(n)))])
        };
        let part = Part { part_no, map, ranges, payload: (0..n).map(|_| value(r)).collect(), dv: BTreeSet::new() };
        for o in 0..n {
            self.live.insert(part.view().ident(o), part.payload[o as usize]);
        }
        self.parts.push(part);
    }

    fn delete(&mut self, r: &mut Rng) {
        let Some(p) = (!self.parts.is_empty()).then(|| r.below(self.parts.len() as u64) as usize) else { return };
        let part = &mut self.parts[p];
        for _ in 0..1 + r.below(part.payload.len() as u64 / 2 + 1) {
            let o = r.below(part.payload.len() as u64) as u32;
            if part.dv.insert(o) {
                let id = part.view().ident(o);
                assert!(self.live.remove(&id).is_some());
                self.dropped.insert(id);
            }
        }
    }

    fn job(&mut self, r: &mut Rng) {
        if self.parts.is_empty() {
            return;
        }
        let s = r.below(self.parts.len() as u64) as usize;
        let e = (s + 1 + r.below(4) as usize).min(self.parts.len());
        let kind = [JobKind::Concat, JobKind::SortedMerge, JobKind::DvRewrite][r.below(3) as usize];
        self.jobs[kind as usize] += 1;
        let (out, payload) = {
            let dels: Vec<Box<dyn Fn(u32) -> bool + '_>> = self.parts[s..e].iter().map(|p| Box::new(move |o| p.dv.contains(&o)) as Box<dyn Fn(u32) -> bool + '_>).collect();
            let views: Vec<RowMapView<'_>> = self.parts[s..e].iter().map(Part::view).collect();
            let inputs: Vec<Input<'_>> = self.parts[s..e].iter().zip(&dels).zip(&views).map(|((p, d), v)| Input { part_no: p.part_no, rows: p.payload.len() as u32, deleted: &**d, map: v }).collect();
            let order = if kind == JobKind::SortedMerge {
                let decl = random_decl(r, 1);
                let base = concat_order(&inputs).unwrap();
                let cell = |&(n, o): &(u16, u32)| self.parts[s + n as usize].payload[o as usize];
                let keys = vec![base.iter().map(|x| cell(x).map_or(KeySlot::Null, KeySlot::I)).collect::<Vec<_>>()];
                let order = sorted_order(&inputs, &keys, std::slice::from_ref(&decl)).unwrap();
                let mut want = base.clone();
                want.sort_by(|a, b| ref_cmp(cell(a), cell(b), &decl).then(inputs[a.0 as usize].ident(a.1).cmp(&inputs[b.0 as usize].ident(b.1))));
                assert_eq!(order, want, "the sorted merge's order");
                order
            } else {
                concat_order(&inputs).unwrap()
            };
            let map = ident::build_map(&kind, &inputs, &order).expect("the output map encodes");
            let OutMap::Granules(body) = &map;
            let rows = order.len() as u64;
            let ids = ident::identities(&inputs, &order).unwrap();
            let back = GranMap::decode(body, rows).expect("granules decode");
            let rev = back.needs_rev().then(|| back.build_frameix().expect("frameix"));
            let mut got = vec![0u64; ids.len()];
            back.decode_ids(0, &mut got);
            assert_eq!(got, ids, "the decoded map's IDs");
            for (o, &id) in ids.iter().enumerate() {
                assert_eq!(back.ordinal_of(id, rev.as_ref()), Some(o as u32), "ID {id}'s ordinal ({:?})", back.rev_form());
            }
            (back, order.iter().map(|&(n, o)| self.parts[s + n as usize].payload[o as usize]).collect::<Vec<_>>())
        };
        let gone: Vec<Part> = self.parts.drain(s..e).collect();
        if payload.is_empty() {
            return;
        }
        let part_no = self.next;
        self.next += 1;
        let ranges = ident::union_ranges(gone.iter().map(|p| &p.ranges[..]));
        let map = Map::Granules(out);
        self.parts.push(Part { part_no, map, ranges, payload, dv: BTreeSet::new() });
    }

    fn check(&self) {
        let rec = |p: &Part| PartRecord { rows: p.payload.len() as u64, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no: p.part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        let header = ManifestHeader { gen: 1, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: self.parts.len() as u32, next_part_no: self.next, flags: 0, reserved: 0 };
        let mut by_no: Vec<&Part> = self.parts.iter().collect();
        by_no.sort_by_key(|p| p.part_no);
        let table: Vec<(u32, IdRanges)> = by_no.iter().map(|p| (p.part_no, p.ranges.clone())).collect();
        let ext = with_next_row_id(&with_ranges(&[], &table).unwrap(), self.next_id).unwrap();
        let mut recs: Vec<PartRecord> = self.parts.iter().map(rec).collect();
        recs.sort_by_key(|r| r.part_no);
        check_row_ids(&Manifest::new(header, recs).with_ext(ext)).expect("the one-home law");
        let home_of = |id: u64| self.parts.iter().position(|p| p.ranges.iter().any(|&(lo, hi)| (lo..hi).contains(&id)));
        let mut at: HashMap<u64, (usize, u32)> = HashMap::new();
        for (i, p) in self.parts.iter().enumerate() {
            for o in 0..p.payload.len() as u32 {
                let id = p.view().ident(o);
                assert_eq!(home_of(id), Some(i), "part {}'s row {o}: ID {id} in its ranges", p.part_no);
                assert!(at.insert(id, (i, o)).is_none(), "ID {id} in two places");
            }
        }
        for (&id, want) in &self.live {
            let &(i, o) = at.get(&id).unwrap_or_else(|| panic!("live {id} lost"));
            let p = &self.parts[i];
            assert!(!p.dv.contains(&o), "live {id} under a DV");
            assert_eq!(p.payload[o as usize], *want, "live {id}'s payload");
        }
        for id in &self.dropped {
            if let Some(&(i, o)) = at.get(id) {
                assert!(self.parts[i].dv.contains(&o), "dropped {id} resolves");
            }
        }
    }
}

#[test]
fn identities_resolve_across_random_ladders() {
    let n = cases(10_000);
    let mut totals = [0u64; 3];
    for case in 0..n {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ (case + 1).wrapping_mul(0x2545_f491_4f6c_dd1d));
        let mut t = Table::default();
        for _ in 0..4 + r.below(20) {
            match r.below(10) {
                0..=1 => t.load(&mut r, false),
                2..=3 => t.load(&mut r, true),
                4..=5 => t.delete(&mut r),
                _ => t.job(&mut r),
            }
            t.check();
        }
        totals.iter_mut().zip(t.jobs).for_each(|(a, b)| *a += b);
    }
    assert!(totals.iter().all(|&j| j >= n / 2), "every job kind ran: {totals:?}");
}
