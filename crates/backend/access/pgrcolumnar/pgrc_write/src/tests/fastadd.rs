//! FASTADD (pgrc-plans/FASTADD.done): an ALTER TABLE that only adds or drops columns publishes the new schema's
//! fingerprint in the manifest header and describes the old one (`PlanBuilder::describe`); the parts written before it
//! keep their schema, listed under it in `EXT_SCHEMAS`. Every later publish keeps the record (`PlanBuilder::finish`): a
//! part new to the manifest takes the publisher's schema, a carried one keeps its own, and the record goes when no part
//! is left under an older schema. A publisher whose schema is not the base's, with nothing describing the base's, is
//! refused as before (`manifest base identity mismatch`); stats runs written under the old schema still load.

use super::*;
use crate::plan::{PlanBuilder, PublishPlan};
use crate::publish::{effective_manifest, publish_ext, BaseStats, PublishOutcome};
use crate::seal::PartSpec;
use crate::stats::{StatsState, LOOSE_MAX};
use crate::walsink::NoWal;
use crate::{WriteError, WriteResult};
use pgrc_format::ident::schema_fingerprint;
use pgrc_format::manifest::{part_fingerprint, part_fingerprints, schemas, stats_ref, PartRecord, SchemaEntry, MANIFESTF_SCHEMAS};
use pgrc_format::wal::PublishKind;

fn spec_of(cols: &[ColSchema]) -> PartSpec {
    PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: schema_fingerprint(cols) }
}

/// `n` rows of a two-int8-column table, one part, published by `fxid`.
fn publish_two(vfs: &mut MemVfs, kit: &mut Kit, fxid: u64, n: u64) -> WriteResult<PublishOutcome> {
    let mut w = open_writer(vec![int8_col(1), int8_col(2)], stamp(fxid, 1));
    for i in 0..n {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.append_row(&[RawDatum::Word(i), RawDatum::Word(i * 7)], &mut kit.ext, &mut env).expect("append");
    }
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    w.publish(vfs, &Probe::new(TxnVerdict::Committed))
}

/// `parts` one-column parts of 10 rows, one publish each from fxid 100 on; the last outcome.
fn one_column_table(vfs: &mut MemVfs, kit: &mut Kit, parts: u64) -> PublishOutcome {
    let probe = Probe::new(TxnVerdict::Committed);
    let mut last = None;
    for k in 0..parts {
        let mut w = open_writer(vec![int8_col(1)], stamp(100 + k, 1));
        append_int8_rows(&mut w, vfs, kit, 10, |i| Some((k * 100 + i) as i64));
        last = Some(finish_and_publish(&mut w, vfs, kit, &probe));
    }
    last.expect("a part")
}

/// The ALTER's ext-only publish to `new`, describing `old` when `describe`.
fn alter(vfs: &mut MemVfs, old: &[ColSchema], new: &[ColSchema], fxid: u64, describe: bool) -> WriteResult<PublishOutcome> {
    let old = old.to_vec();
    let mut edit = |_: Option<&Manifest>, b: &mut PlanBuilder<'_>| -> WriteResult<()> {
        if describe {
            b.describe(old.clone());
        }
        Ok(())
    };
    publish_ext(vfs, &mut NoWal, DIR, &spec_of(new), fxid, &Probe::new(TxnVerdict::Committed), BaseStats::None, &mut edit)
}

fn refused(r: WriteResult<PublishOutcome>) -> bool {
    matches!(r, Err(WriteError::Contract { detail: "manifest base identity mismatch" }))
}

#[test]
fn an_add_column_lists_the_old_parts_under_their_schema_and_later_publishes_keep_it() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let (one, two) = (vec![int8_col(1)], vec![int8_col(1), int8_col(2)]);
    let (fp1, fp2) = (schema_fingerprint(&one), schema_fingerprint(&two));
    let before = one_column_table(&mut vfs, &mut kit, 3);
    assert!(refused(alter(&mut vfs, &one, &two, 200, false)), "a schema change nothing describes refuses, as before");
    let out = alter(&mut vfs, &one, &two, 201, true).expect("the ALTER's publish");
    let m = &out.manifest;
    assert_eq!((m.header.schema_fingerprint, m.header.prev_gen, m.parts.len()), (fp2, before.gen, 3));
    assert_ne!(m.header.flags & MANIFESTF_SCHEMAS, 0);
    let want = SchemaEntry { fingerprint: fp1, cols: one.iter().map(|c| ColSchema { semantics: TypeSemantics::Opaque, ..*c }).collect(), runs: vec![(0, 2)] };
    assert_eq!(schemas(m).unwrap(), vec![want.clone()]);
    assert_eq!(part_fingerprints(m).unwrap(), vec![fp1; 3]);
    let probe = Probe::new(TxnVerdict::Committed);
    let on_disk = effective_manifest(&mut vfs, DIR, &probe, None).expect("walk").expect("effective");
    assert_eq!(&on_disk, m, "the record is on disk as published");

    // a part of the new schema: new parts take the publisher's schema, the old ones keep theirs
    let out = publish_two(&mut vfs, &mut kit, 202, 10).expect("a publish of the new schema");
    let m = &out.manifest;
    assert_eq!(m.header.schema_fingerprint, fp2);
    assert_eq!(schemas(m).unwrap(), vec![want.clone()]);
    assert_eq!(part_fingerprints(m).unwrap(), vec![fp1, fp1, fp1, fp2]);
    assert_eq!(part_fingerprint(m, 3).unwrap(), fp2);

    // a publisher of the old schema (a stale relcache) never moves the header back
    let mut w = open_writer(one.clone(), stamp(203, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 10, |i| Some(i as i64));
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    assert!(refused(w.publish(&mut vfs, &probe)));

    // a DROP back to one column: the header is the old schema again, the parts of the two-column one are listed
    let out = alter(&mut vfs, &two, &one, 204, true).expect("the DROP's publish");
    let m = &out.manifest;
    assert_eq!(m.header.schema_fingerprint, fp1);
    assert_eq!(part_fingerprints(m).unwrap(), vec![fp1, fp1, fp1, fp2]);
    assert_eq!(schemas(m).unwrap().iter().map(|e| (e.fingerprint, e.runs.clone())).collect::<Vec<_>>(), vec![(fp2, vec![(3, 3)])]);
}

fn rec(part_no: u32) -> PartRecord {
    PartRecord { rows: 10, file_len: 4096, footer_off: 2048, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 }
}

/// A manifest of parts 0, 2, 4, 6 at the old schema under a header at the new one (the ALTER's generation).
fn altered_base() -> (Manifest, u64, u64, Vec<ColSchema>) {
    let (one, two) = (vec![int8_col(1)], vec![int8_col(1), int8_col(2)]);
    let (fp1, fp2) = (schema_fingerprint(&one), schema_fingerprint(&two));
    let parts: Vec<PartRecord> = [0, 2, 4, 6].into_iter().map(rec).collect();
    let header = pgrc_format::manifest::ManifestHeader {
        gen: 3,
        prev_gen: 2,
        publisher_fxid: 50,
        relfilenumber: RELFILENUMBER,
        schema_fingerprint: fp1,
        magic: pgrc_format::manifest::MANIFEST_MAGIC,
        format_version: pgrc_format::FORMAT_VERSION,
        spc: SPC,
        db: DB,
        part_count: 4,
        next_part_no: 8,
        flags: 0,
        reserved: 0,
    };
    let base = Manifest::new(header, parts);
    let mut b = PublishPlan::build(Some(&base), &spec_of(&two), 60, PublishKind::Rotate);
    b.describe(one.clone());
    let m = b.finish().expect("finish").manifest;
    assert_eq!(part_fingerprints(&m).unwrap(), vec![fp1; 4]);
    (m, fp1, fp2, two)
}

#[test]
fn a_carried_part_keeps_its_schema_and_a_new_one_splits_the_run() {
    let (base, fp1, fp2, two) = altered_base();
    // a DV upsert of an old part keeps its schema; a part carried from elsewhere (not the base's, not installed)
    // takes the publisher's, and splits the run it falls in
    let mut b = PublishPlan::build(Some(&base), &spec_of(&two), 61, PublishKind::Compile);
    b.upsert(PartRecord { dv_gen: 4, dv_len: 64, dv_crc: 9, ..rec(2) });
    b.upsert(rec(3));
    let m = b.finish().expect("finish").manifest;
    assert_eq!(m.parts.iter().map(|p| p.part_no).collect::<Vec<_>>(), vec![0, 2, 3, 4, 6]);
    assert_eq!(part_fingerprints(&m).unwrap(), vec![fp1, fp1, fp2, fp1, fp1]);
    assert_eq!(schemas(&m).unwrap()[0].runs, vec![(0, 2), (4, 6)]);
    let back = Manifest::decode(&m.encode()).expect("decodes");
    assert_eq!(part_fingerprints(&back).unwrap(), vec![fp1, fp1, fp2, fp1, fp1]);
}

#[test]
fn retiring_every_old_part_drops_the_record() {
    let (base, fp1, fp2, two) = altered_base();
    let mut b = PublishPlan::build(Some(&base), &spec_of(&two), 62, PublishKind::Compact);
    for n in [0, 2, 4] {
        b.retire(n);
    }
    let m = b.finish().expect("finish").manifest;
    assert_eq!((part_fingerprints(&m).unwrap(), schemas(&m).unwrap()[0].runs.clone()), (vec![fp1], vec![(6, 6)]));
    let mut b = PublishPlan::build(Some(&m), &spec_of(&two), 63, PublishKind::Compact);
    b.retire(6);
    b.upsert(rec(9));
    let m = b.finish().expect("finish").manifest;
    assert_eq!(m.header.flags & MANIFESTF_SCHEMAS, 0, "no part left under the old schema: no record, no flag");
    assert_eq!((part_fingerprints(&m).unwrap(), m.header.schema_fingerprint), (vec![fp2], fp2));
    // and a later publish of the new schema over it is an ordinary one
    let mut b = PublishPlan::build(Some(&m), &spec_of(&two), 64, PublishKind::Bulk);
    b.upsert(rec(10));
    assert_eq!(b.finish().expect("finish").manifest.header.flags & MANIFESTF_SCHEMAS, 0);
}

/// Stats runs bind the schema of the generation that wrote them: after the ALTER, the runs written before it (bound to
/// the old schema, which the generation describes) still load, so a cold load or a publish does not walk every part.
#[test]
fn stats_runs_written_before_the_alter_still_load() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let (one, two) = (vec![int8_col(1)], vec![int8_col(1), int8_col(2)]);
    let mut n = LOOSE_MAX as u64;
    let mut last = one_column_table(&mut vfs, &mut kit, n);
    while stats_ref(&last.manifest).expect("stats ref").is_none() {
        let mut w = open_writer(one.clone(), stamp(100 + n, 1));
        append_int8_rows(&mut w, &mut vfs, &mut kit, 10, |i| Some(i as i64));
        last = finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
        n += 1;
        assert!(n < 3 * LOOSE_MAX as u64, "no stats root");
    }
    let out = alter(&mut vfs, &one, &two, 500, true).expect("alter");
    assert_eq!(out.runs_written, 0, "the ALTER rewrites no run");
    let mut walked = 0;
    let st = StatsState::load(&mut vfs, DIR, &out.manifest, None, &mut |v, r| {
        walked += 1;
        crate::publish::part_stats_on_disk(v, DIR, &out.manifest, r)
    })
    .expect("the old runs load under the new header");
    assert!(st.runs.iter().map(|r| r.parts.len()).sum::<usize>() > 0);
    assert!(walked < out.manifest.parts.len(), "not every part walked ({walked})");
    // and a publish of the new schema over it (its loose parts and the old runs) succeeds
    publish_two(&mut vfs, &mut kit, 501, 10).expect("publish after the alter");
}
