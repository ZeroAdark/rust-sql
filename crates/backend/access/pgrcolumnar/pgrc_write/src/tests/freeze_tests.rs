//! AUTOVAC FREEZE (spec §13.1 "Freeze"; the delta-v2 port of main's d8502dd97e4): VACUUM republishes the newest
//! generation every snapshot sees under `FROZEN_PUBLISHER_FXID` when readers would otherwise look its publisher up in
//! pg_xact; the scan's floor bounds `relfrozenxid` when it cannot.

use std::sync::Arc;

use pgsync::{lock, Mutex};

use super::*;
use crate::plan::{redo_publish, PlanBuilder};
use crate::publish::{freeze_scan, publish_ext, publish_freeze, FreezeOutcome, FreezeScan, FROZEN_PUBLISHER_FXID};
use crate::seal::PartSpec;
use crate::walmodel::{RecordingSink, WalLog, WalRecord};
use crate::walsink::NoWal;
use pgrc_format::manifest::CommitPointer;
use pgrc_format::wal::PublishKind;
use pgrc_read::{resolve_effective, CommitCheck, MemTableDir, TableExpect};

fn load(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> crate::publish::PublishOutcome {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, 500, |i| Some((fxid * 1000 + i) as i64));
    finish_and_publish(&mut w, vfs, &mut kit, probe)
}

fn spec(m: &Manifest) -> PartSpec {
    PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: m.header.schema_fingerprint }
}

fn freeze(vfs: &mut MemVfs, probe: &Probe, cutoff: u64) -> (FreezeOutcome, Option<u64>) {
    let head = crate::publish::effective_manifest(vfs, DIR, probe, None).expect("walk");
    let s = head.as_ref().map_or(PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: 0 }, spec);
    publish_freeze(vfs, &mut NoWal, DIR, &s, probe, (None, None), cutoff).expect("freeze")
}

fn current(vfs: &mut MemVfs) -> u64 {
    CommitPointer::decode(&vfs.read_full(&format!("{DIR}/CURRENT")).unwrap()).unwrap().gen
}

fn manifest(vfs: &mut MemVfs, gen: u64) -> Manifest {
    crate::plan::read_manifest(vfs, DIR, gen).expect("manifest")
}

/// A reader whose pg_xact holds no normal xid any more (truncated below every publisher).
struct FrozenOnly;
impl CommitCheck for FrozenOnly {
    fn committed(&self, fxid: u64) -> bool {
        !crate::publish::clog_bound(fxid)
    }
}

fn reader(vfs: &mut MemVfs) -> MemTableDir {
    let mut dir = MemTableDir::default();
    for name in vfs.list_dir(DIR).unwrap() {
        dir.put(&name, vfs.read_full(&format!("{DIR}/{name}")).unwrap());
    }
    dir
}

#[test]
fn freezes_the_committed_head_below_the_cutoff() {
    let mut vfs = mem_with_dir();
    let probe = Probe::new(TxnVerdict::Committed);
    assert_eq!(load(&mut vfs, 7, &probe).gen, 1);
    let (out, floor) = freeze(&mut vfs, &probe, 100);
    let FreezeOutcome::Frozen(o) = out else { panic!("expected Frozen, got {out:?}") };
    let base = manifest(&mut vfs, 1);
    assert_eq!((o.gen, o.kind, o.manifest.header.prev_gen, o.manifest.header.publisher_fxid), (2, PublishKind::Freeze, 1, FROZEN_PUBLISHER_FXID));
    assert_eq!(o.manifest.parts, base.parts, "the base's parts");
    assert_eq!(o.manifest.ext, base.ext, "the base's extension block");
    assert_eq!(o.manifest.header.next_part_no, base.header.next_part_no);
    assert!(o.part_nos.is_empty() && o.runs_written == 0, "nothing installed or staged");
    assert_eq!(floor, None, "no reader looks a publisher up any more");
    assert_eq!(base.header.publisher_fxid, 7, "the old manifest is chain history");
    assert_eq!(current(&mut vfs), 2);
    // again: nothing due
    let (again, floor) = freeze(&mut vfs, &probe, 100);
    assert!(matches!(again, FreezeOutcome::Skipped(None)), "{again:?}");
    assert_eq!((floor, current(&mut vfs)), (None, 2));
}

/// D2189: a freeze changes only the publisher. VACUUM's spec carries the live schema's fingerprint; a head left
/// under another stamp (a column dropped by a dependency's cascade, with no ALTER publish) froze under the live one and
/// was refused ("manifest base identity mismatch"); it now keeps the fingerprint of the generation it copies.
#[test]
fn a_freeze_keeps_the_fingerprint_of_the_generation_it_copies() {
    let mut vfs = mem_with_dir();
    let probe = Probe::new(TxnVerdict::Committed);
    load(&mut vfs, 7, &probe);
    let base = manifest(&mut vfs, 1);
    let live = PartSpec { schema_fingerprint: base.header.schema_fingerprint ^ 0x5a5a_5a5a, ..spec(&base) };
    let (out, floor) = publish_freeze(&mut vfs, &mut NoWal, DIR, &live, &probe, (None, None), 100).expect("the freeze under another stamp");
    let FreezeOutcome::Frozen(o) = out else { panic!("expected Frozen, got {out:?}") };
    assert_eq!((o.gen, o.manifest.header.publisher_fxid), (2, FROZEN_PUBLISHER_FXID));
    assert_eq!(o.manifest.header.schema_fingerprint, base.header.schema_fingerprint, "the copied generation's fingerprint");
    assert_eq!(o.manifest.header.flags, base.header.flags, "no schema listed");
    assert_eq!(o.manifest.parts, base.parts, "the base's parts");
    assert_eq!(o.manifest.ext, base.ext, "the base's extension block");
    assert_eq!((floor, current(&mut vfs)), (None, 2));
}

#[test]
fn a_frozen_head_resolves_without_its_base_publisher() {
    let mut vfs = mem_with_dir();
    let probe = Probe::new(TxnVerdict::Committed);
    load(&mut vfs, 7, &probe);
    assert!(resolve_effective(&reader(&mut vfs), &FrozenOnly, &TableExpect::default()).unwrap().is_none(), "before: xid 7 cannot be adjudicated");
    freeze(&mut vfs, &probe, 100);
    let eff = resolve_effective(&reader(&mut vfs), &FrozenOnly, &TableExpect::default()).unwrap().expect("the frozen head is effective");
    assert_eq!((eff.manifest.header.gen, eff.walked_past, eff.manifest.parts.len()), (2, 0, 1));
}

#[test]
fn a_young_or_busy_chain_is_not_frozen_and_bounds_relfrozenxid() {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    let (out, floor) = freeze(&mut vfs, &committed, 100);
    assert!(matches!(out, FreezeOutcome::Skipped(None)) && floor.is_none(), "an empty table");
    load(&mut vfs, 7, &committed);
    // the publisher is not before the cutoff: nothing due, but readers look 7 up
    let (out, floor) = freeze(&mut vfs, &committed, 7);
    assert!(matches!(out, FreezeOutcome::Skipped(None)), "{out:?}");
    assert_eq!((floor, current(&mut vfs)), (Some(7), 1));
    // a committed generation above the cutoff heads it: a snapshot that does not see 150 walks down to 7
    load(&mut vfs, 150, &committed);
    let (out, floor) = freeze(&mut vfs, &committed, 100);
    assert!(matches!(out, FreezeOutcome::Skipped(Some("publisher not below the cutoff"))), "{out:?}");
    assert_eq!((floor, current(&mut vfs)), (Some(7), 2));
    // once 150 is before the cutoff too, the head freezes and the floor goes
    let (out, floor) = freeze(&mut vfs, &committed, 200);
    assert!(matches!(&out, FreezeOutcome::Frozen(o) if o.manifest.header.prev_gen == 2), "{out:?}");
    assert_eq!(floor, None);
    // a foreign publish in progress over the frozen head: nothing to freeze, its xid is in the floor
    let busy = Probe::new(TxnVerdict::Committed).set(300, TxnVerdict::InProgress);
    load(&mut vfs, 300, &busy);
    let (out, floor) = freeze(&mut vfs, &busy, 400);
    assert!(matches!(out, FreezeOutcome::Skipped(None)), "the head below it is frozen: {out:?}");
    assert_eq!(floor, Some(300));
}

#[test]
fn a_busy_head_over_an_old_base_skips_and_keeps_the_base_publisher() {
    let mut vfs = mem_with_dir();
    load(&mut vfs, 7, &Probe::new(TxnVerdict::Committed));
    let busy = Probe::new(TxnVerdict::InProgress).set(7, TxnVerdict::Committed);
    load(&mut vfs, 9, &busy);
    let (out, floor) = freeze(&mut vfs, &busy, 100);
    assert!(matches!(out, FreezeOutcome::Skipped(Some("publish in progress"))), "{out:?}");
    assert_eq!((floor, current(&mut vfs)), (Some(7), 2));
}

#[test]
fn rolled_back_residue_over_a_frozen_base_is_frozen_past() {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    load(&mut vfs, 7, &committed);
    freeze(&mut vfs, &committed, 100);
    // 9 publishes gen 3 and rolls back: CURRENT names it, and readers look 9 up on their way to gen 2
    load(&mut vfs, 9, &Probe::new(TxnVerdict::InProgress).set(7, TxnVerdict::Committed).set(FROZEN_PUBLISHER_FXID, TxnVerdict::Committed));
    assert_eq!(current(&mut vfs), 3);
    let aborted = Probe::new(TxnVerdict::Committed).set(9, TxnVerdict::Aborted).used(3, 0);
    let scan = freeze_scan(&mut vfs, DIR, &aborted, 100).unwrap();
    assert_eq!(scan, FreezeScan { floor: Some(9), base: Some(manifest(&mut vfs, 2)), due: true, skip: None });
    let (out, floor) = freeze(&mut vfs, &aborted, 100);
    let FreezeOutcome::Frozen(o) = out else { panic!("expected Frozen, got {out:?}") };
    assert_eq!((o.gen, o.manifest.header.prev_gen, floor), (4, 2, None), "numbered above the residue, chained on the frozen base");
    assert_eq!(manifest(&mut vfs, 3).header.publisher_fxid, 9, "the residue stays");
}

#[test]
fn tagged_parts_in_the_head_skip_and_put_their_creator_in_the_floor() {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    let first = load(&mut vfs, 100, &committed);
    // A (200) publishes and stays open; X (300), a maintenance step, stacks on it and tags A's part (SEAL-OFFHEAD)
    load(&mut vfs, 200, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress));
    let open = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking();
    let x = publish_ext(&mut vfs, &mut NoWal, DIR, &spec(&first.manifest), 300, &open, (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(())).expect("stacked");
    assert_eq!(x.stacked_over, vec![200]);
    // X committed, A still open: readers resolve the tag's creator in pg_xact
    let probe = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress);
    let scan = freeze_scan(&mut vfs, DIR, &probe, 1000).unwrap();
    assert_eq!((scan.floor, scan.due, scan.skip), (Some(200), true, Some("tagged parts in the head")));
    let (out, floor) = freeze(&mut vfs, &probe, 1000);
    assert!(matches!(out, FreezeOutcome::Skipped(Some("tagged parts in the head"))), "{out:?}");
    assert_eq!((floor, current(&mut vfs)), (Some(200), 3));
}

#[test]
fn the_freeze_is_one_logged_publish_that_redo_reapplies() {
    let mut vfs = mem_with_dir();
    let probe = Probe::new(TxnVerdict::Committed);
    let first = load(&mut vfs, 7, &probe);
    let log = Arc::new(Mutex::new(WalLog::default()));
    let mut sink = RecordingSink::new(DIR, log.clone());
    let (out, _) = publish_freeze(&mut vfs, &mut sink, DIR, &spec(&first.manifest), &probe, (None, None), 100).expect("freeze");
    let FreezeOutcome::Frozen(o) = out else { panic!("expected Frozen, got {out:?}") };
    let recs: Vec<_> = lock(&log).records.iter().filter_map(|r| match r {
        WalRecord::Publish { rec, .. } => Some(rec.clone()),
        _ => None,
    }).collect();
    assert_eq!(recs.len(), 1, "one PUBLISH");
    let rec = &recs[0];
    assert_eq!((rec.kind, rec.base_gen, rec.header.publisher_fxid), (PublishKind::Freeze, 1, FROZEN_PUBLISHER_FXID));
    assert!(rec.installs.is_empty() && rec.upsert.is_empty() && rec.retire.is_empty(), "no part moves");
    let bytes = vfs.read_full(&format!("{DIR}/manifest-2.pgrcm")).expect("frozen manifest");
    let redone = redo_publish(&mut vfs, DIR, rec).expect("redo").expect("the base is there");
    assert_eq!(redone, o.manifest, "redo rebuilds the frozen generation");
    assert_eq!(vfs.read_full(&format!("{DIR}/manifest-2.pgrcm")).unwrap(), bytes, "idempotent");
    assert_eq!(current(&mut vfs), 2);
}
