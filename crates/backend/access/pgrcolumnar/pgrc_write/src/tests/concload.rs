//! CONCLOAD (pgrc-plans/CONCLOAD.done): bulk loads publish over each other's in-progress generations (the stacking
//! probe) instead of waiting. A stacked generation names the creator of every part a transaction still in progress
//! added, whichever loader published last; a stacking walk passes a rolled-back loader's generation and drops its parts.

use super::*;
use crate::publish::{effective_manifest, publish_ext, PublishOutcome};
use crate::plan::PlanBuilder;
use crate::walsink::NoWal;
use crate::WriteResult;
use pgrc_format::manifest::{ranges, under, Manifest};

fn load(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> WriteResult<PublishOutcome> {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, 10, |i| Some((fxid * 1000 + i) as i64));
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    w.publish(vfs, probe)
}

fn part(o: &PublishOutcome) -> u32 {
    *o.part_nos.first().expect("one part")
}

fn tags(m: &Manifest) -> Vec<(u32, u64)> {
    under(m).unwrap().iter().map(|e| (e.part_no, e.fxid)).collect()
}

fn lists(m: &Manifest, part_no: u32) -> bool {
    m.parts.iter().any(|p| p.part_no == part_no)
}

/// Two loaders publishing in turn: each publish tags every part of the other one, the parts the other one tagged
/// below its own generations included (A3 names b1, which only B1's own generations list untagged and A2 tagged).
#[test]
fn loaders_publishing_in_turn_tag_every_part_of_the_other() {
    let mut vfs = mem_with_dir();
    let h = load(&mut vfs, 100, &Probe::new(TxnVerdict::Committed)).unwrap();
    let open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking();
    let a1 = load(&mut vfs, 200, &open()).unwrap();
    let b1 = load(&mut vfs, 300, &open()).expect("B stacks over A");
    assert_eq!((b1.manifest.header.prev_gen, tags(&b1.manifest), b1.stacked_over.clone()), (a1.gen, vec![(part(&a1), 200)], vec![200]));
    let a2 = load(&mut vfs, 200, &open()).expect("A stacks over B");
    assert_eq!(tags(&a2.manifest), vec![(part(&b1), 300)]);
    let b2 = load(&mut vfs, 300, &open()).unwrap();
    assert_eq!(tags(&b2.manifest), vec![(part(&a1), 200), (part(&a2), 200)], "a1 too: B's generation below lists it tagged");
    let a3 = load(&mut vfs, 200, &open()).unwrap();
    assert_eq!(tags(&a3.manifest), vec![(part(&b1), 300), (part(&b2), 300)], "b1 too: A2 tagged it, B2 lists it untagged");
    let gens = [h.gen, a1.gen, b1.gen, a2.gen, b2.gen, a3.gen];
    assert!(gens.windows(2).all(|w| w[0] < w[1]), "numbers climb along the chain: {gens:?}");
    let parts: Vec<u32> = [&h, &a1, &b1, &a2, &b2, &a3].iter().map(|o| part(o)).collect();
    assert!(parts.windows(2).all(|w| w[0] < w[1]), "no part number twice: {parts:?}");
    assert_eq!(a3.manifest.parts.len(), 6);
    // B rolls back: A's next publish (on its own head) drops both of B's parts
    let b_gone = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::Aborted).stacking();
    let a4 = load(&mut vfs, 200, &b_gone).unwrap();
    assert_eq!(a4.dropped, vec![part(&b1), part(&b2)]);
    assert!(!lists(&a4.manifest, part(&b1)) && !lists(&a4.manifest, part(&b2)) && tags(&a4.manifest).is_empty());
    assert!(a4.stats.check(&a4.manifest).is_ok(), "the stats follow the part set");
}

/// A maintenance step (X) stacks over A and commits; A publishes again on X's generation and drops its own tag; B then
/// stacks on A's generation. The walk stops at X's (committed) generation, whose tag still names a1.
#[test]
fn a_stack_names_what_the_committed_generation_under_the_run_tags() {
    let mut vfs = mem_with_dir();
    load(&mut vfs, 100, &Probe::new(TxnVerdict::Committed)).unwrap();
    let a_open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress);
    let a1 = load(&mut vfs, 200, &a_open()).unwrap();
    let spec = crate::seal::PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: a1.manifest.header.schema_fingerprint };
    let x = publish_ext(&mut vfs, &mut NoWal, DIR, &spec, 300, &a_open().set(300, TxnVerdict::InProgress).stacking(), (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(())).unwrap();
    assert_eq!(tags(&x.manifest), vec![(part(&a1), 200)]);
    let a2 = load(&mut vfs, 200, &a_open().stacking()).unwrap();
    assert!(tags(&a2.manifest).is_empty(), "A's own parts need no tag in A's generation");
    let b1 = load(&mut vfs, 400, &a_open().set(400, TxnVerdict::InProgress).stacking()).unwrap();
    assert_eq!(tags(&b1.manifest), vec![(part(&a1), 200), (part(&a2), 200)], "a1 is named by X's tag");
}

/// A rolls back under B's stacked generation: C's publish walks through A's generation instead of waiting for B, drops
/// A's part and tags B's; D then stacks on C's generation, which retired a rolled-back creator's part only.
#[test]
fn a_stack_walks_through_a_rolled_back_loader() {
    let mut vfs = mem_with_dir();
    load(&mut vfs, 100, &Probe::new(TxnVerdict::Committed)).unwrap();
    let a1 = load(&mut vfs, 200, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress)).unwrap();
    let b1 = load(&mut vfs, 300, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking()).unwrap();
    let after = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::Aborted).set(300, TxnVerdict::InProgress).set(400, TxnVerdict::InProgress).set(500, TxnVerdict::InProgress);
    let c1 = load(&mut vfs, 400, &after().stacking()).expect("C stacks over B and A's rolled-back generation");
    assert_eq!((c1.manifest.header.prev_gen, c1.dropped.clone(), tags(&c1.manifest)), (b1.gen, vec![part(&a1)], vec![(part(&b1), 300)]));
    assert!(!lists(&c1.manifest, part(&a1)));
    let d1 = load(&mut vfs, 500, &after().stacking()).expect("D stacks over C, which dropped a dead part");
    assert_eq!(tags(&d1.manifest), vec![(part(&b1), 300), (part(&c1), 400)]);
    // a publisher that does not stack still waits for the head
    assert!(matches!(load(&mut vfs, 600, &after().set(600, TxnVerdict::InProgress)), Err(crate::WriteError::PublishBusy { holder_fxid: 500 })));
    let all_done = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::Aborted);
    let disk = effective_manifest(&mut vfs, DIR, &all_done, None).unwrap().unwrap();
    assert_eq!(disk.header.gen, d1.gen);
}

/// IX0: loaders whose parts home row IDs (a grant each, from `ROWID_FIRST`) still stack: a part's `EXT_RANGES` entry and the raised
/// `EXT_ROWID` floor are part of adding it. B's generation lists both loaders' ranges; when A rolls back, C's walk
/// drops A's part and its range.
#[test]
fn loaders_with_row_ids_stack_over_each_other() {
    let load_ids = |vfs: &mut MemVfs, fxid: u64, lo: u64, probe: &Probe| -> WriteResult<PublishOutcome> {
        let mut kit = Kit::new();
        let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
        w.grant_ids(lo, lo + 291).expect("grant");
        append_int8_rows(&mut w, vfs, &mut kit, 10, |i| Some((fxid * 1000 + i) as i64));
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
        w.publish(vfs, probe)
    };
    let mut vfs = mem_with_dir();
    let h = load_ids(&mut vfs, 100, 291, &Probe::new(TxnVerdict::Committed)).unwrap();
    let open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).set(400, TxnVerdict::InProgress).stacking();
    let a1 = load_ids(&mut vfs, 200, 582, &open()).unwrap();
    let b1 = load_ids(&mut vfs, 300, 873, &open()).expect("B stacks over A, whose generation added a range");
    assert_eq!((b1.manifest.header.prev_gen, tags(&b1.manifest)), (a1.gen, vec![(part(&a1), 200)]));
    let want = |o: &PublishOutcome, lo: u64| (part(o), vec![(lo, lo + 291)]);
    assert_eq!(ranges(&b1.manifest).unwrap(), vec![want(&h, 291), want(&a1, 582), want(&b1, 873)], "parts h a1 b1 = {:?}", [part(&h), part(&a1), part(&b1)]);
    let a_gone = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::Aborted).set(300, TxnVerdict::InProgress).set(400, TxnVerdict::InProgress).stacking();
    let c1 = load_ids(&mut vfs, 400, 1164, &a_gone()).expect("C stacks over B and A's rolled-back generation");
    assert_eq!(c1.dropped, vec![part(&a1)]);
    assert_eq!(ranges(&c1.manifest).unwrap(), vec![want(&h, 291), want(&b1, 873), want(&c1, 1164)], "A's range goes with its part");
}

/// IXCOPYPATH: a stacking walk compares the row-ID entries of the parts two generations both list by one merge of
/// their part lists (it scanned one list per part of the other, O(parts²) per walked generation): the same answer as
/// that scan on random pairs (parts kept, retired or added; a kept part's ranges changed or not; no ranges at all).
#[test]
fn adds_only_compares_the_shared_parts_ranges_as_the_scan_did() {
    use pgrc_format::manifest::{with_next_row_id, with_ranges, IdRanges, ManifestHeader, PartRecord};
    let scan = |p: &Manifest, g: &Manifest| -> Option<bool> {
        let before = ranges(p).ok()?;
        Some(!ranges(g).ok()?.iter().any(|(no, r)| before.iter().any(|(pn, pr)| pn == no && pr != r)))
    };
    let mut s = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n.max(1)
    };
    let rec = |part_no| PartRecord { rows: 3, file_len: 64, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
    let mk = |table: &[(u32, IdRanges)], nos: &[u32]| {
        let header = ManifestHeader { gen: 4, prev_gen: 3, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: nos.len() as u32, next_part_no: 1000, flags: 0, reserved: 0 };
        Manifest::new(header, nos.iter().map(|&n| rec(n)).collect()).with_ext(with_next_row_id(&with_ranges(&[], table).unwrap(), 1 << 30).unwrap())
    };
    let (mut same, mut differ) = (0, 0);
    for case in 0..2_000 {
        let nos: Vec<u32> = (0..rnd(30) as u32).map(|i| i * 3).collect();
        let ids = |no: u32, salt: u64| vec![(291 * (1 + 10 * u64::from(no) + salt), 291 * (2 + 10 * u64::from(no) + salt))];
        let with_ids = rnd(4) != 0;
        let table: Vec<(u32, IdRanges)> = nos.iter().filter(|_| with_ids).map(|&no| (no, ids(no, 0))).collect();
        let mut gnos: Vec<u32> = nos.iter().copied().filter(|_| rnd(5) != 0).collect();
        gnos.extend((0..rnd(3) as u32).map(|k| 100 + k));
        let changed = gnos.first().copied().filter(|_| rnd(3) == 0 && with_ids);
        let gtable: Vec<(u32, IdRanges)> = gnos.iter().filter(|&&no| with_ids || no >= 100).map(|&no| (no, ids(no, if Some(no) == changed { 5 } else { 0 }))).collect();
        let (p, g) = (mk(&table, &nos), mk(&gtable, &gnos));
        let want = scan(&p, &g).expect("both read");
        assert_eq!(crate::publish::adds_only(Some(&p), &g).is_some(), want, "case {case}");
        match want {
            true => same += 1,
            false => differ += 1,
        }
    }
    assert!(same > 200 && differ > 200, "{same} kept their ranges, {differ} changed one");
}
