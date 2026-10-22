use pgrc_compact::runs::{Force, MemSpill, RevBuilder};
use pgrc_format::dirlayout::{runrev_file_name, sidecar_file_name};
use pgrc_format::manifest::{range_entries, IdRanges, PART_FLAG_COMPACTED, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
use pgrc_format::rowmap::{encode_run_member, GranMap};
use pgrc_format::runrev::{BytePages, Hit, RunRev, PAGE};
use pgrc_format::sidecar::SidecarKind;
use pgrc_format::sortrun::{runs, RunRevForm, SortRun};
use pgrc_write::publish::{effective_manifest, TxnVerdict};
use pgrc_write::testkit::{append_int8_rows, committed_probe, crash_and_recover, durable_upto, int8_col, log_commit, logged_writer, mem_with_dir, stamp, Kit, Probe, DIR};
use pgrc_write::walmodel::{SharedLog, WalLog};
use pgrc_write::writer::{SealEnv, TableWriter};
use pgrc_write::wvfs::{MemVfs, WriteVfs};
use pgrc_write::{WriteError, WriteResult};
use std::collections::BTreeMap;

const RUN_ID: u64 = 3;
const CUT: usize = 600;
const JOB: u64 = 200;

struct Inputs {
    vfs: MemVfs,
    log: SharedLog,
    kit: Kit,
    members: Vec<Vec<u64>>,
    ranges: IdRanges,
    probe: Probe,
}

fn inputs() -> Inputs {
    let mut vfs = mem_with_dir();
    vfs.fsync_dir(DIR).expect("dir");
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let mut probe = Probe::new(TxnVerdict::InProgress);
    let mut next = 291u64;
    for (fxid, rows) in [(100, 1000), (110, 500)] {
        probe = probe.set(fxid, TxnVerdict::Committed);
        let mut w = logged_writer(vec![int8_col(1)], stamp(fxid, 1), &log);
        for i in 0..rows {
            if let Some(n) = w.needs_ids() {
                w.grant_ids(next, next + n).expect("grant");
                next += n;
            }
            append_int8_rows(&mut w, &mut vfs, &mut kit, 1, |_| Some(i as i64));
        }
        pgrc_write::testkit::finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
        log_commit(&log, fxid);
    }
    let eff = effective_manifest(&mut vfs, DIR, &probe, None).expect("walk").expect("effective");
    let ranges_of = range_entries(&eff).expect("ranges");
    let mut ids: Vec<u64> = Vec::new();
    for (i, p) in eff.parts.iter().enumerate() {
        let own = &ranges_of.iter().find(|(no, _)| *no == p.part_no).expect("a fresh part's range").1;
        assert_eq!((p.part_no, own.len()), (i as u32, 1));
        assert!(own[0].1 - own[0].0 >= p.rows);
        ids.extend(own[0].0..own[0].0 + p.rows);
    }
    let mut ranges: IdRanges = ranges_of.iter().flat_map(|(_, r)| r.iter().copied()).collect();
    ranges.sort_unstable();
    ids.sort_unstable_by_key(|&id| (id % 7, id));
    let members = ids.chunks(CUT).map(|c| c.to_vec()).collect();
    Inputs { vfs, log, kit, members, ranges, probe: probe.used(RUN_ID, 0) }
}

fn sealed(inp: &mut Inputs) -> TableWriter {
    let mut w = logged_writer(vec![int8_col(1)], stamp(JOB, 1), &inp.log);
    for m in &inp.members {
        for &id in m {
            append_int8_rows(&mut w, &mut inp.vfs, &mut inp.kit, 1, |_| Some(id as i64));
        }
        let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&inp.kit.cands];
        let mut env = SealEnv { vfs: &mut inp.vfs, sources: &sources, resolver: &inp.kit.resolver, shred: &mut inp.kit.shred, shred_opts: &inp.kit.opts };
        w.finish(&mut env).expect("a member seals");
    }
    w
}

fn reverse(members: &[Vec<u64>], ranges: &IdRanges, bucket: u64) -> (Vec<u8>, SortRun, Vec<Vec<u8>>) {
    let mut b = RevBuilder::new(RUN_ID, ranges.clone(), bucket, MemSpill::default(), Force::default()).expect("builder");
    let maps: Vec<Vec<u8>> = members
        .iter()
        .map(|ids| {
            let fr = b.member(ids).expect("member");
            encode_run_member(ids, &fr.vstarts, Some(&fr.vmin[..])).expect("map")
        })
        .collect();
    let mut plan = b.finish().expect("finish");
    let spec = plan.spec();
    let mut file = Vec::new();
    let (fin, stats) = plan
        .write(&mut |at, bytes| {
            let end = at as usize + bytes.len();
            file.resize(file.len().max(end), 0);
            file[at as usize..end].copy_from_slice(bytes);
            Ok(())
        })
        .expect("write");
    assert!(stats.spilled > 0 && stats.members == members.len() as u32, "every row goes through the spill");
    let run = SortRun { run_id: RUN_ID, level: 1, rev: spec.form, rank: spec.rank, rows: spec.rows, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: Vec::new(), ranges: ranges.clone() };
    (file, run, maps)
}

fn stage(w: &mut TableWriter, vfs: &mut MemVfs, file: &[u8]) -> WriteResult<String> {
    w.write_file(vfs, 0, &mut |write| {
        for page in file.chunks(PAGE) {
            write(page)?;
        }
        Ok(())
    })
}

/// `crash_at`: a crash armed that many ops after the members are sealed.
struct Job {
    inp: Inputs,
    file: Vec<u8>,
    maps: Vec<Vec<u8>>,
    completed: bool,
    err: Option<String>,
    ops_before: u64,
    inputs: [Vec<u8>; 2],
}

fn job(crash_at: Option<u64>, commit: bool, carry: Vec<(usize, Vec<u32>)>) -> Job {
    let mut inp = inputs();
    let (file, run, maps) = reverse(&inp.members, &inp.ranges, 256);
    let mut w = sealed(&mut inp);
    let inputs = [0, 1].map(|p| inp.vfs.read_full(&format!("{DIR}/part-{p}.pgrc")).expect("input"));
    let ops_before = inp.vfs.op_count();
    if let Some(k) = crash_at {
        inp.vfs.crash_at_op(k);
    }
    let res = (|| -> WriteResult<()> {
        let tmp = stage(&mut w, &mut inp.vfs, &file)?;
        let staged = vec![(tmp, runrev_file_name(RUN_ID))];
        let mut c = |base: &pgrc_format::manifest::Manifest| -> WriteResult<Vec<(usize, Vec<u32>)>> {
            assert_eq!(base.header.gen, 2, "carry sees the publish's base");
            Ok(carry.clone())
        };
        let (out, nos) = w.publish_runs(&mut inp.vfs, &inp.probe, (None, None), &maps, &[0, 1], run.clone(), 2, &staged, &mut c)?;
        assert_eq!((out.gen, nos), (4, vec![2, 3, 4]));
        Ok(())
    })();
    let completed = res.is_ok();
    if completed && commit {
        log_commit(&inp.log, JOB);
    }
    Job { inp, file, maps, completed, err: res.err().map(|e| format!("{e:?}")), ops_before, inputs }
}

fn image(vfs: &mut MemVfs) -> BTreeMap<String, Vec<u8>> {
    vfs.list_dir(DIR).expect("list").into_iter().map(|n| {
        let b = vfs.read_full(&format!("{DIR}/{n}")).expect("read");
        (n, b)
    }).collect()
}

fn check_run(vfs: &mut MemVfs, probe: &dyn pgrc_write::publish::TxnProbe, j: &Job, what: &str) {
    let eff = effective_manifest(vfs, DIR, probe, Some(JOB)).expect("walk").expect("effective");
    assert_eq!(eff.header.gen, 4, "{what}");
    let flags = PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT | PART_FLAG_COMPACTED;
    let want: Vec<(u32, u32, u64)> = j.inp.members.iter().enumerate().map(|(k, m)| (2 + k as u32, flags, m.len() as u64)).collect();
    assert_eq!(eff.parts.iter().map(|p| (p.part_no, p.flags, p.rows)).collect::<Vec<_>>(), want, "{what}");
    assert!(range_entries(&eff).expect("ranges").is_empty(), "the run homes its members' IDs ({what})");
    let rs = runs(&eff).expect("runs");
    assert_eq!(rs.len(), 1, "{what}");
    let run = &rs[0];
    assert_eq!((run.run_id, run.level, run.members.clone(), run.ranges.clone()), (RUN_ID, 1, vec![2, 3, 4], j.inp.ranges.clone()), "{what}");
    assert!(run.homes() && run.rev != RunRevForm::None, "{what}");
    let names = vfs.list_dir(DIR).expect("list");
    assert!(names.iter().all(|n| !n.starts_with("tmp-")), "a temp survived ({what}): {names:?}");
    for no in [2, 3, 4] {
        assert!(names.contains(&sidecar_file_name(no, SidecarKind::RowIdentMap, 1)), "member {no}'s map ({what}): {names:?}");
    }
    let bytes = vfs.read_full(&format!("{DIR}/{}", runrev_file_name(RUN_ID))).expect("the reverse");
    assert!(bytes == j.file, "the reverse's bytes are the staged ones ({what})");
    let rev = RunRev::open(run, &BytePages(&bytes)).expect("the reverse opens against its EXT_RUNS entry");
    let maps: Vec<GranMap> = j.maps.iter().zip(&j.inp.members).map(|(m, ids)| GranMap::decode(m, ids.len() as u64).expect("map")).collect();
    for (s, ids) in j.inp.members.iter().enumerate() {
        for (o, &id) in ids.iter().enumerate().step_by(7) {
            let got = match rev.lookup(id, &BytePages(&bytes)).expect("lookup") {
                Hit::Absent => None,
                Hit::Group { slot, va, vb } => maps[slot as usize].run_find(va, vb, id).expect("find").map(|o| (slot as usize, o)),
                Hit::Frame { slot, vframe } => maps[slot as usize].run_find_in(vframe, id).expect("find").map(|o| (slot as usize, o)),
            };
            assert_eq!(got, Some((s, o as u32)), "id {id} ({what})");
        }
    }
}

#[test]
fn a_run_merge_publishes_its_members_and_renames_its_reverse() {
    let mut j = job(None, false, Vec::new());
    assert!(j.completed, "{:?}", j.err);
    assert_eq!(j.inp.members.iter().map(Vec::len).collect::<Vec<_>>(), vec![600, 600, 300]);
    let probe = Probe::new(TxnVerdict::InProgress).set(100, TxnVerdict::Committed).set(110, TxnVerdict::Committed);
    let mut vfs = std::mem::replace(&mut j.inp.vfs, MemVfs::new());
    check_run(&mut vfs, &probe, &j, "in the job's transaction");
    let kinds: Vec<_> = pgsync_records(&j.inp.log);
    assert_eq!(kinds.last(), Some(&(pgrc_format::wal::PublishKind::Compact, vec![0, 1], 3)), "one PUBLISH: retire 2, install 3");
}

fn pgsync_records(log: &SharedLog) -> Vec<(pgrc_format::wal::PublishKind, Vec<u32>, usize)> {
    let g = log.lock().unwrap_or_else(|e| e.into_inner());
    g.records
        .iter()
        .filter_map(|r| match r {
            pgrc_write::walmodel::WalRecord::Publish { rec, .. } => Some((rec.kind, rec.retire.clone(), rec.installs.len())),
            _ => None,
        })
        .collect()
}

#[test]
fn dv_carry_marks_the_routed_ordinals_in_a_dv_of_the_publish() {
    let ords = vec![5u32, 77, 599];
    let j = job(None, false, vec![(1, ords.clone())]);
    assert!(j.completed, "{:?}", j.err);
    let mut vfs = j.inp.vfs;
    let probe = Probe::new(TxnVerdict::InProgress).set(100, TxnVerdict::Committed).set(110, TxnVerdict::Committed);
    let eff = effective_manifest(&mut vfs, DIR, &probe, Some(JOB)).expect("walk").expect("effective");
    let p = eff.parts.iter().find(|p| p.part_no == 3).expect("member 3");
    let grain = p.granule_rows;
    let rows: Vec<(u32, u16)> = ords.iter().map(|&o| (o / grain, (o % grain) as u16)).collect();
    let (_, t) = pgrc_write::dml::compile_part_dv(None, 3, &rows, 4).expect("compile").expect("a DV");
    assert_eq!((p.dv_gen, p.dv_len, p.dv_crc), (4, t.dv_len, t.dv_crc));
    assert!(eff.parts.iter().filter(|p| p.part_no != 3).all(|p| p.dv_gen == 0), "only the routed member has a DV");
    assert!(vfs.list_dir(DIR).expect("list").contains(&sidecar_file_name(3, SidecarKind::Dv, 4)));
    for bad in [vec![(0, vec![600u32])], vec![(3, vec![1u32])]] {
        let j = job(None, false, bad.clone());
        assert!(!j.completed, "carry {bad:?} is refused");
    }
}

#[test]
fn the_staged_reverse_is_reclaimed_on_abort_and_checked_at_the_publish() {
    let mut inp = inputs();
    let (file, run, maps) = reverse(&inp.members, &inp.ranges, 1 << 22);
    let mut w = sealed(&mut inp);
    let tmp = stage(&mut w, &mut inp.vfs, &file).expect("staged");
    assert!(tmp.starts_with("tmp-200-"), "{tmp}");
    assert_eq!(inp.vfs.read_full(&format!("{DIR}/{tmp}")).expect("the temp"), file);
    assert!(stage(&mut w, &mut inp.vfs, &file).is_err(), "one file a sequence");
    let mut none = |_: &pgrc_format::manifest::Manifest| -> WriteResult<Vec<(usize, Vec<u32>)>> { Ok(Vec::new()) };
    let forged = vec![("tmp-200-9.pgrct".to_string(), runrev_file_name(RUN_ID))];
    let r = w.publish_runs(&mut inp.vfs, &inp.probe, (None, None), &maps, &[0, 1], run.clone(), 2, &forged, &mut none);
    assert!(matches!(r, Err(WriteError::Contract { .. })), "{r:?}");
    w.abort(&mut inp.vfs).expect("abort");
    let names = inp.vfs.list_dir(DIR).expect("list");
    assert!(names.iter().all(|n| !n.starts_with("tmp-")), "abort reclaims the members and the staged reverse: {names:?}");
    assert!(!names.contains(&runrev_file_name(RUN_ID)));
}

#[test]
fn every_crash_point_of_a_run_publish_recovers_to_the_inputs_or_the_run() {
    let rehearsal = job(None, true, Vec::new());
    assert!(rehearsal.completed);
    let window = rehearsal.inp.vfs.op_count() - rehearsal.ops_before;
    assert!(window > 10, "window too small to be a real sweep: {window}");
    for k in 1..=window + 1 {
        for all in [false, true] {
            let what = format!("crash at op {k}, all WAL {all}");
            let mut j = job(Some(k), true, Vec::new());
            assert_eq!(j.completed, k > window, "{what}");
            let upto = durable_upto(&j.inp.log, all);
            crash_and_recover(&mut j.inp.vfs, &j.inp.log, upto);
            let probe = committed_probe(&j.inp.log);
            let eff = effective_manifest(&mut j.inp.vfs, DIR, &probe, None).expect("walk").expect("effective");
            let names = j.inp.vfs.list_dir(DIR).expect("list");
            assert!(names.iter().all(|n| !n.starts_with("tmp-")), "a temp survived ({what}): {names:?}");
            if eff.header.gen == 4 {
                let mut vfs = std::mem::replace(&mut j.inp.vfs, MemVfs::new());
                check_run(&mut vfs, &probe, &j, &what);
                assert!(!names.iter().any(|n| n == "part-0.pgrc" || n == "part-1.pgrc"), "inputs cleaned ({what}): {names:?}");
            } else {
                assert_eq!(eff.header.gen, 2, "{what}");
                assert!(runs(&eff).expect("runs").is_empty(), "{what}");
                assert!(!names.contains(&runrev_file_name(RUN_ID)), "no reverse without its run ({what}): {names:?}");
                for p in 0..2 {
                    assert!(j.inp.vfs.read_full(&format!("{DIR}/part-{p}.pgrc")).expect("input") == j.inputs[p], "input {p} intact ({what})");
                }
            }
        }
    }
}

#[test]
fn the_wal_rebuilds_the_run_and_its_reverse_on_a_fresh_directory() {
    let mut j = job(None, true, Vec::new());
    assert!(j.completed, "{:?}", j.err);
    let upto = durable_upto(&j.inp.log, true);
    crash_and_recover(&mut j.inp.vfs, &j.inp.log, upto);
    let primary = image(&mut j.inp.vfs);
    assert!(primary.contains_key(&runrev_file_name(RUN_ID)));
    let log = WalLog::shared();
    {
        let src = j.inp.log.lock().unwrap_or_else(|e| e.into_inner());
        let mut dst = log.lock().unwrap_or_else(|e| e.into_inner());
        dst.records = src.records.clone();
    }
    let mut fresh = mem_with_dir();
    fresh.fsync_dir(DIR).expect("dir");
    let all = durable_upto(&log, true);
    crash_and_recover(&mut fresh, &log, all);
    assert_eq!(image(&mut fresh).keys().collect::<Vec<_>>(), primary.keys().collect::<Vec<_>>(), "the same files");
    assert!(image(&mut fresh) == primary, "byte for byte");
    let probe = committed_probe(&log);
    check_run(&mut fresh, &probe, &j, "replayed");
}
