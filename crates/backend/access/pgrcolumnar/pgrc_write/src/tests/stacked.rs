//! SEAL-OFFHEAD (pgrc-plans/SEAL-OFFHEAD.md): a maintenance publish chains on a foreign in-progress head whose run only
//! added parts, tagging each such part with its creator (`EXT_UNDER`); a publish on a tagged base drops the tags of
//! committed creators and the parts of rolled-back ones; a foreground publish waits for a tagged creator in progress
//! as for the head it was; a run that changed anything but its parts is never stacked on.

use super::*;
use crate::plan::PlanBuilder;
use crate::publish::{effective_manifest, publish_ext, PublishOutcome};
use crate::walsink::NoWal;
use crate::{WriteError, WriteResult};
use pgrc_format::manifest::{under, with_tombs, Manifest, TombsRecord, MANIFESTF_UNDER};

fn bulk(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> PublishOutcome {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, 10, |i| Some((fxid * 1000 + i) as i64));
    finish_and_publish(&mut w, vfs, &mut kit, probe)
}

fn noop(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> WriteResult<PublishOutcome> {
    let spec = crate::seal::PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: effective_manifest(vfs, DIR, probe, None)?.expect("a generation").header.schema_fingerprint };
    publish_ext(vfs, &mut NoWal, DIR, &spec, fxid, probe, (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(()))
}

fn tags(m: &Manifest) -> Vec<(u32, u64)> {
    under(m).unwrap().iter().map(|e| (e.part_no, e.fxid)).collect()
}

#[test]
fn a_maintenance_publish_stacks_on_an_add_only_writer_and_tags_its_parts() {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    let base = bulk(&mut vfs, 100, &committed);
    // A (200) publishes and stays open; X (300) is the maintenance step
    let a = bulk(&mut vfs, 200, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress));
    let a_part = *a.part_nos.first().expect("A's part");
    let open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress);
    assert!(matches!(noop(&mut vfs, 300, &open()), Err(WriteError::PublishBusy { holder_fxid: 200 })), "a foreground publish waits");
    let x = noop(&mut vfs, 300, &open().stacking()).expect("a maintenance publish stacks");
    assert_eq!((x.manifest.header.prev_gen, x.stacked_over.clone()), (a.gen, vec![200]), "chained on A's generation");
    assert_eq!(tags(&x.manifest), vec![(a_part, 200)], "A's part carries its creator");
    assert_eq!(x.manifest.header.flags & MANIFESTF_UNDER, MANIFESTF_UNDER);
    assert!(x.manifest.parts.iter().any(|p| p.part_no == base.part_nos[0]), "the committed part is untagged");
    // X committed, A still open: a foreground publisher (400) waits for A under the committed head; the next
    // maintenance step carries the tag
    let x_done = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress);
    assert!(matches!(noop(&mut vfs, 400, &x_done()), Err(WriteError::PublishBusy { holder_fxid: 200 })), "waits for the tagged creator");
    let y = noop(&mut vfs, 500, &x_done().stacking()).expect("stacks again");
    assert_eq!((tags(&y.manifest), y.stacked_over.clone()), (vec![(a_part, 200)], vec![200]));
}

#[test]
fn a_committed_creator_loses_its_tag_and_a_rolled_back_one_its_parts() {
    for aborted in [false, true] {
        let mut vfs = mem_with_dir();
        bulk(&mut vfs, 100, &Probe::new(TxnVerdict::Committed));
        let a = bulk(&mut vfs, 200, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress));
        noop(&mut vfs, 300, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking()).expect("stacked");
        let ended = Probe::new(TxnVerdict::Committed).set(200, if aborted { TxnVerdict::Aborted } else { TxnVerdict::Committed });
        let z = noop(&mut vfs, 400, &ended).expect("a publish after A ended");
        assert!(tags(&z.manifest).is_empty() && z.manifest.header.flags & MANIFESTF_UNDER == 0, "no tag left (aborted {aborted})");
        let kept = z.manifest.parts.iter().any(|p| p.part_no == a.part_nos[0]);
        assert_eq!((kept, z.dropped.clone()), if aborted { (false, a.part_nos.clone()) } else { (true, vec![]) });
        let disk = effective_manifest(&mut vfs, DIR, &ended, None).unwrap().unwrap();
        assert_eq!(disk.header.gen, z.gen);
        assert!(z.stats.check(&z.manifest).is_ok(), "the stats follow the part set");
    }
}

#[test]
fn a_run_that_changed_more_than_its_parts_is_never_stacked_on() {
    let mut vfs = mem_with_dir();
    bulk(&mut vfs, 100, &Probe::new(TxnVerdict::Committed));
    let open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress);
    let spec = crate::seal::PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: effective_manifest(&mut vfs, DIR, &open(), None).unwrap().unwrap().header.schema_fingerprint };
    // A's publish edits the extension block (a first activation, a tombstone-heap switch): not add-only
    let t = TombsRecord { cur: 1, ret: None };
    publish_ext(&mut vfs, &mut NoWal, DIR, &spec, 200, &open(), (None, None), &mut |b: Option<&Manifest>, p: &mut PlanBuilder<'_>| {
        p.ext(with_tombs(b.map_or(&[][..], |m| &m.ext[..]), &t)?);
        Ok(())
    })
    .expect("A's edit");
    assert!(matches!(noop(&mut vfs, 300, &open().set(300, TxnVerdict::InProgress).stacking()), Err(WriteError::PublishBusy { holder_fxid: 200 })), "the maintenance step defers");
    // and a maintenance edit of a tagged part is a contract error
    let mut vfs = mem_with_dir();
    bulk(&mut vfs, 100, &Probe::new(TxnVerdict::Committed));
    let a = bulk(&mut vfs, 200, &open());
    let stack = open().set(300, TxnVerdict::InProgress).stacking();
    let err = publish_ext(&mut vfs, &mut NoWal, DIR, &spec, 300, &stack, (None, None), &mut |_: Option<&Manifest>, p: &mut PlanBuilder<'_>| {
        p.retire(a.part_nos[0]);
        Ok(())
    });
    assert!(matches!(err, Err(WriteError::Contract { .. })), "{:?}", err.err());
}
