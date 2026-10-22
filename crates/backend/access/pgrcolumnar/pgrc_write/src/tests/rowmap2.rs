//! ROWMAP2 M1 (pgrc-plans/ROWMAP2.done, main 10-06): a PUBLISH whose manifest names a granules map carries the kind
//! byte's ROWMAP2 bit, so a binary before M1 refuses the record before its redo touches a file (SEAL-OFFHEAD's UNDER
//! bit, for the same reason). A compaction stacked on writer A's open load (COMPACTP) whose output has a granules map
//! sets both bits (UNDER | ROWMAP2); the record's bytes decode to itself, and redo of the decoded record (the
//! publisher's manifest and CURRENT removed first) writes them again byte for byte.

use super::*;
use crate::publish::PublishOutcome;
use crate::walmodel::{WalLog, WalRecord};
use pgrc_format::dirlayout::{manifest_file_name, CURRENT_FILE_NAME};
use pgrc_format::manifest::{MANIFESTF_ROWMAP2, MANIFESTF_UNDER, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
use pgrc_format::wal::{PublishKind, PublishRec, PUBLISH_KIND_ROWMAP2, PUBLISH_KIND_UNDER};
use pgsync::lock;

fn bulk(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> PublishOutcome {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, 10, |i| Some((fxid * 1000 + i) as i64));
    finish_and_publish(&mut w, vfs, &mut kit, probe)
}

#[test]
fn a_stacked_compaction_over_a_granules_map_sets_both_kind_bits_and_redoes_byte_equal() {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    bulk(&mut vfs, 100, &committed);
    bulk(&mut vfs, 110, &committed);
    let a = bulk(&mut vfs, 200, &Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress));
    assert_eq!(a.manifest.header.flags & MANIFESTF_ROWMAP2, 0, "bulk parts with one range: implicit maps, no bit");
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let mut w = logged_writer(vec![int8_col(1)], stamp(300, 1), &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 20, |i| Some(i as i64));
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
    }
    let map = pgrc_format::rowmap::encode_runs(&[(291, 20)], pgrc_format::rowmap::Opts::default()).expect("the map");
    let probe = Probe::new(TxnVerdict::Committed).set(200, TxnVerdict::InProgress).set(300, TxnVerdict::InProgress).stacking();
    let (c, out) = w.publish_compact(&mut vfs, &probe, (None, None), &[(0, 0), (1, 0)], (PART_ROWMAP_GRANULES, &map), &[(291, 291 + 291)]).expect("the compaction stacks");
    let out = out.expect("an output part");
    assert_eq!(c.stacked_over, vec![200], "stacked on A's open load");
    assert_eq!(c.manifest.header.flags & (MANIFESTF_UNDER | MANIFESTF_ROWMAP2), MANIFESTF_UNDER | MANIFESTF_ROWMAP2);
    let rec = c.manifest.parts.iter().find(|p| p.part_no == out).expect("the output's record");
    assert_eq!(rec.flags & (PART_FLAG_ROWMAP | 0b11 << PART_ROWMAP_FORM_SHIFT), PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT);
    let logged = lock(&log).records.iter().rev().find_map(|r| match r {
        WalRecord::Publish { rec, .. } => Some(rec.clone()),
        _ => None,
    });
    let logged = logged.expect("the compaction's PUBLISH");
    let mut bytes = Vec::new();
    logged.encode(&mut bytes).expect("encode");
    assert_eq!(bytes[12 + 8], PublishKind::Compact as u8 | PUBLISH_KIND_UNDER | PUBLISH_KIND_ROWMAP2, "0xC0 over the kind");
    let back = PublishRec::decode(&bytes).expect("decode");
    assert_eq!(back, logged);
    let (mpath, cpath) = (format!("{DIR}/{}", manifest_file_name(c.gen)), format!("{DIR}/{CURRENT_FILE_NAME}"));
    let (written, current) = (vfs.read_full(&mpath).expect("the publisher's manifest"), vfs.read_full(&cpath).expect("CURRENT"));
    vfs.unlink_path(&mpath).expect("unlink the manifest");
    vfs.unlink_path(&cpath).expect("unlink CURRENT");
    let redone = crate::plan::redo_publish(&mut vfs, DIR, &back).expect("redo").expect("applied over A's generation");
    assert_eq!(vfs.read_full(&mpath).expect("redo's manifest"), written, "redo's manifest is the publisher's, byte for byte");
    assert_eq!(vfs.read_full(&cpath).expect("redo's CURRENT"), current);
    assert_eq!(redone, c.manifest);
}

/// ROWMAP2 M2 (main 10-07): a compaction's output map is the granules form; a retired form (here a runs body, form 1,
/// as a pre-M1 concatenation wrote it) is refused before anything is published, and the head stays where it was.
#[test]
fn a_compaction_output_map_in_a_retired_form_is_refused() {
    let mut vfs = mem_with_dir();
    let committed = Probe::new(TxnVerdict::Committed);
    bulk(&mut vfs, 100, &committed);
    bulk(&mut vfs, 110, &committed);
    let cpath = format!("{DIR}/{CURRENT_FILE_NAME}");
    let head = vfs.read_full(&cpath).expect("CURRENT");
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(300, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 20, |i| Some(i as i64));
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
    }
    // a runs body: u32 count, then u64 id_lo, u32 ord_start, u32 len, u8 form (0 = a range) for the one run
    let mut runs = vec![1, 0, 0, 0];
    runs.extend(291u64.to_le_bytes());
    runs.extend(0u32.to_le_bytes());
    runs.extend(20u32.to_le_bytes());
    runs.push(0);
    let r = w.publish_compact(&mut vfs, &committed, (None, None), &[(0, 0), (1, 0)], (1, &runs), &[(291, 291 + 291)]);
    assert!(matches!(r, Err(crate::WriteError::Contract { .. })), "{:?}", r.err());
    assert_eq!(vfs.read_full(&cpath).expect("CURRENT"), head, "nothing published");
}
