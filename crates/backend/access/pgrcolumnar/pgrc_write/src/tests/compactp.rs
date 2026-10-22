//! COMPACTP (pgrc-plans/COMPACTP.done, item 2 (b)): a compaction publishes over a foreign in-progress head whose run
//! only added parts (the stacking probe, as a bulk load does: CONCLOAD) instead of deferring to it. It retires
//! committed parts below that run and installs its output; the run's parts ride its generation tagged with their
//! creators (`EXT_UNDER`), so every reader resolves them by its own snapshot, and the next publish after a creator
//! ended drops its tag (committed) or its parts (rolled back). A run that changed more than its parts is never stacked
//! on, and an input an open writer added is refused. The model of the e2e's herd and of the isolation spec
//! pgrc-compact-stack: the heap twin's rows are what each reader's generation lists.

use super::*;
use crate::publish::{effective_manifest, publish_ext, PublishOutcome};
use crate::plan::PlanBuilder;
use crate::walsink::NoWal;
use crate::{WriteError, WriteResult};
use pgrc_format::manifest::{under, with_tombs, Manifest, TombsRecord, MANIFESTF_UNDER, PART_FLAG_COMPACTED, PART_ROWMAP_GRANULES};

fn bulk(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> PublishOutcome {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, 10, |i| Some((fxid * 1000 + i) as i64));
    finish_and_publish(&mut w, vfs, &mut kit, probe)
}

/// A compaction by `fxid` retiring `retire` (`(part_no, dv_gen)`) into one output of `rows` rows homing one ID range.
fn compact(vfs: &mut MemVfs, fxid: u64, probe: &dyn crate::publish::TxnProbe, retire: &[(u32, u64)], rows: u64) -> WriteResult<(PublishOutcome, Option<u32>)> {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, rows, |i| Some(i as i64));
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    let map = pgrc_format::rowmap::encode_runs(&[(291, rows as u32)], pgrc_format::rowmap::Opts::default()).expect("the map");
    w.publish_compact(vfs, probe, (None, None), retire, (PART_ROWMAP_GRANULES, &map), &[(291, 291 + 291)])
}

fn tags(m: &Manifest) -> Vec<(u32, u64)> {
    under(m).unwrap().iter().map(|e| (e.part_no, e.fxid)).collect()
}

fn nos(m: &Manifest) -> Vec<u32> {
    m.parts.iter().map(|p| p.part_no).collect()
}

/// Two committed parts (0, 1) and a writer A (fxid 200) whose bulk load (part 2) is published and still open.
fn open_load() -> (MemVfs, PublishOutcome, PublishOutcome) {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    bulk(&mut vfs, 100, &committed);
    let base = bulk(&mut vfs, 110, &committed);
    let a = bulk(&mut vfs, 200, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress));
    assert_eq!((nos(&base.manifest), nos(&a.manifest)), (vec![0, 1], vec![0, 1, 2]));
    (vfs, base, a)
}

#[test]
fn a_compaction_stacks_on_an_add_only_writer_and_tags_its_parts() {
    let (mut vfs, base, a) = open_load();
    let both_open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress);
    // the line's compaction (no stacking probe) defers to A's head
    assert!(matches!(compact(&mut vfs, 300, &both_open(), &[(0, 0), (1, 0)], 20), Err(WriteError::PublishBusy { holder_fxid: 200 })));
    let (c, out) = compact(&mut vfs, 300, &both_open().stacking(), &[(0, 0), (1, 0)], 20).expect("the compaction stacks");
    let out = out.expect("an output part");
    assert_eq!((c.manifest.header.prev_gen, c.stacked_over.clone()), (a.gen, vec![200]), "chained on A's generation");
    assert_eq!(nos(&c.manifest), vec![2, out], "the inputs retired, A's part kept, the output installed");
    assert!(out > 2 && c.gen > a.gen, "numbered above A's");
    assert_eq!(tags(&c.manifest), vec![(2, 200)], "A's part carries its creator");
    assert_eq!(c.manifest.header.flags & MANIFESTF_UNDER, MANIFESTF_UNDER);
    let rec = c.manifest.parts.iter().find(|p| p.part_no == out).unwrap();
    assert_eq!(rec.flags & PART_FLAG_COMPACTED, PART_FLAG_COMPACTED);
    assert!(c.stats.check(&c.manifest).is_ok(), "the stats follow the part set");
    // each reader's generation by its verdicts: the compaction committed (A open: the tag hides part 2 from it), A
    // committed alone (A's generation: the inputs and part 2), neither (the base)
    let mut seen = |p: Probe| effective_manifest(&mut vfs, DIR, &p, None).unwrap().unwrap();
    let r = seen(Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress));
    assert_eq!((r.header.gen, tags(&r)), (c.gen, vec![(2, 200)]));
    let r = seen(Probe::new(TxnVerdict::Committed).set(300, TxnVerdict::InProgress));
    assert_eq!((r.header.gen, nos(&r)), (a.gen, vec![0, 1, 2]));
    let r = seen(both_open());
    assert_eq!((r.header.gen, nos(&r)), (base.gen, vec![0, 1]));
}

#[test]
fn after_a_stacked_compaction_a_committed_creator_loses_its_tag_and_a_rolled_back_one_its_parts() {
    for aborted in [false, true] {
        let (mut vfs, _, a) = open_load();
        let stack = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking();
        let (c, out) = compact(&mut vfs, 300, &stack, &[(0, 0), (1, 0)], 20).expect("stacked");
        let out = out.unwrap();
        // the compaction committed, then A ends; the next bulk load (400) publishes on the compaction's generation
        let ended = Probe::new(TxnVerdict::Committed).set(200, if aborted { TxnVerdict::Aborted } else { TxnVerdict::Committed });
        let z = bulk(&mut vfs, 400, &ended);
        assert_eq!(z.manifest.header.prev_gen, c.gen);
        assert!(tags(&z.manifest).is_empty() && z.manifest.header.flags & MANIFESTF_UNDER == 0, "no tag left (aborted {aborted})");
        let want = if aborted { vec![out, z.part_nos[0]] } else { vec![2, out, z.part_nos[0]] };
        assert_eq!((nos(&z.manifest), z.dropped.clone()), (want, if aborted { a.part_nos.clone() } else { vec![] }));
        assert!(z.stats.check(&z.manifest).is_ok());
        let disk = effective_manifest(&mut vfs, DIR, &ended, None).unwrap().unwrap();
        assert_eq!(disk.header.gen, z.gen);
    }
}

#[test]
fn a_rolled_back_stacked_compaction_leaves_the_load_under_it() {
    let (mut vfs, _, a) = open_load();
    let stack = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking();
    let (c, out) = compact(&mut vfs, 300, &stack, &[(0, 0), (1, 0)], 20).expect("stacked");
    // the compaction rolls back, A commits: readers and the next publish walk past it to A's generation (the probe
    // knows the numbers the process used, as the catalog's does: GENID)
    let after = Probe::new(TxnVerdict::Committed).set(300, TxnVerdict::Aborted).used(c.gen, c.manifest.header.next_part_no);
    let r = effective_manifest(&mut vfs, DIR, &after, None).unwrap().unwrap();
    assert_eq!((r.header.gen, nos(&r)), (a.gen, vec![0, 1, 2]), "the inputs are back, the output never was");
    let z = bulk(&mut vfs, 400, &after);
    assert_eq!(z.manifest.header.prev_gen, a.gen);
    assert!(z.gen > c.gen && z.part_nos[0] > out.unwrap(), "numbered above the rolled-back compaction's (GENID)");
    assert_eq!(nos(&z.manifest), vec![0, 1, 2, z.part_nos[0]]);
}

#[test]
fn a_compaction_does_not_stack_on_a_run_that_changed_more_than_its_parts() {
    let mut vfs = mem_with_dir();
    bulk(&mut vfs, 100, &Probe::new(TxnVerdict::Committed));
    bulk(&mut vfs, 110, &Probe::new(TxnVerdict::Committed));
    let open = || Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress);
    let spec = crate::seal::PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: effective_manifest(&mut vfs, DIR, &open(), None).unwrap().unwrap().header.schema_fingerprint };
    // A's open publish edits the extension block (a tombstone-heap switch): not add-only
    let t = TombsRecord { cur: 1, ret: None };
    publish_ext(&mut vfs, &mut NoWal, DIR, &spec, 200, &open(), (None, None), &mut |b: Option<&Manifest>, p: &mut PlanBuilder<'_>| {
        p.ext(with_tombs(b.map_or(&[][..], |m| &m.ext[..]), &t)?);
        Ok(())
    })
    .expect("A's edit");
    assert!(matches!(compact(&mut vfs, 300, &open().stacking(), &[(0, 0), (1, 0)], 20), Err(WriteError::PublishBusy { holder_fxid: 200 })), "the compaction defers");
}

#[test]
fn a_compaction_input_an_open_writer_added_is_refused() {
    let (mut vfs, _, _) = open_load();
    let stack = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking();
    let err = compact(&mut vfs, 300, &stack, &[(1, 0), (2, 0)], 20);
    assert!(matches!(err, Err(WriteError::Contract { .. })), "{:?}", err.err());
}

/// A stacking probe whose verdict on A (200) is "in progress" for its first `k` asks and "rolled back" after: A ends at
/// each point of the compaction's reads (VERDICTORDER's pauses, in the model). The compaction (300) is in progress; the
/// probe knows the numbers the process used, as the catalog's does (GENID).
struct EndsAt {
    k: usize,
    asked: std::cell::Cell<usize>,
    used: (u64, u32),
}

impl crate::publish::TxnProbe for EndsAt {
    fn verdict(&self, fxid: u64) -> TxnVerdict {
        match fxid {
            200 => {
                let n = self.asked.get();
                self.asked.set(n + 1);
                if n < self.k {
                    TxnVerdict::InProgress
                } else {
                    TxnVerdict::Aborted
                }
            }
            300 => TxnVerdict::InProgress,
            _ => TxnVerdict::Committed,
        }
    }
    fn used_numbers(&self, _dir: &str) -> (u64, u32) {
        self.used
    }
    fn stacks(&self) -> bool {
        true
    }
}

/// e2e force/a-k2's red: A rolls back between the compaction's read of its base (A's stacked head) and its publish,
/// whose own walk then passes A's generation. Wherever A ends, the compaction publishes: stacked on A's generation, on
/// it dropping A's part, or on the generation below with its inputs still that one's, numbered above A's.
#[test]
fn a_creator_rolling_back_during_a_stacked_compaction_moves_its_base_below_the_run() {
    let (mut moved, mut stacked) = (false, false);
    for k in 0..12 {
        let (mut vfs, base, a) = open_load();
        let p = EndsAt { k, asked: Default::default(), used: (a.gen, a.manifest.header.next_part_no) };
        let (c, out) = compact(&mut vfs, 300, &p, &[(0, 0), (1, 0)], 20).unwrap_or_else(|e| panic!("A ends at ask {k}: {e:?}"));
        let out = out.expect("an output part");
        assert!(c.gen > a.gen && out > 2, "k {k}: numbered above A's generation and part (GENID)");
        if !c.stacked_over.is_empty() {
            stacked = true;
            assert_eq!((c.manifest.header.prev_gen, nos(&c.manifest), tags(&c.manifest)), (a.gen, vec![2, out], vec![(2, 200)]), "k {k}");
        } else {
            let below = c.manifest.header.prev_gen == base.gen;
            assert_eq!((c.manifest.header.prev_gen, c.dropped.is_empty()), (if below { base.gen } else { a.gen }, below), "k {k}");
            assert_eq!(nos(&c.manifest), vec![out], "k {k}: A's part is gone");
            // the read saw A in progress (its first ask), the publish's walk saw it rolled back
            moved |= below && k > 0;
        }
        assert!(c.stats.check(&c.manifest).is_ok(), "k {k}");
    }
    assert!(moved && stacked, "both ends were reached: moved {moved}, stacked {stacked}");
}
