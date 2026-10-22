use std::collections::BTreeMap;
use std::sync::Once;

use pgrc_format::wal::{
    encode_dir, BulkRowsRec, DirLoc, FileWriteRec, RowRec, TupleImage, XLOG_PGRC_BULK_ROWS, XLOG_PGRC_FILE_CREATE,
    XLOG_PGRC_FILE_WRITE, XLOG_PGRC_INSTALL, XLOG_PGRC_PUBLISH, XLOG_PGRC_RECLAIM, XLOG_PGRC_ROW_DELETE,
    XLOG_PGRC_ROW_UPDATE, XLOG_PGRC_TRUNCATE,
};
use pgrc_write::publish::TxnVerdict;
use pgrc_write::testkit::{
    append_int8_rows, finish_and_publish, int8_col, mem_with_dir, open_writer_policy, stamp, Kit, Probe, DB, DIR,
    RELFILENUMBER, SPC,
};
use pgrc_write::walmodel::{RecordingSink, WalLog, WalRecord};
use pgrc_write::writer::{PartCutPolicy, TableWriter};
use pgrc_write::wvfs::{MemVfs, RealVfs, WriteVfs};
use pgsync::{lock, Mutex};
use std::sync::Arc;
use types_error::{ERROR, PANIC};

use crate::redo::{keep_catalog, redo_op};
use crate::sync::{dirty_names, register, sync_loc, DIRTY_NAMES_CAP};

const LOC: DirLoc = DirLoc { spc: SPC, db: DB, relnumber: RELFILENUMBER as u32 };

const D4711_NO_SYNC: u32 = 4711;

fn seams() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        sync_seams::register_sync_request::set(|tag, _, _| match tag.rlocator.relNumber {
            D4711_NO_SYNC => Err(Box::new(types_error::PgError::error("D4711: the sync request was not queued".to_string()))),
            _ => Ok(true),
        })
    });
}

fn writer(fxid: u64, log: &Arc<Mutex<WalLog>>) -> TableWriter {
    let policy = PartCutPolicy { max_rows: 1000, max_bytes: u64::MAX, cut_granule_rows: 500 };
    open_writer_policy(vec![int8_col(1)], stamp(fxid, 1), policy).with_sink(Box::new(RecordingSink::new(DIR, log.clone())))
}

fn encoded(r: &WalRecord) -> Option<(u8, Vec<u8>)> {
    encoded_at(LOC, r)
}

fn encoded_at(loc: DirLoc, r: &WalRecord) -> Option<(u8, Vec<u8>)> {
    let mut b = Vec::new();
    let info = match r {
        WalRecord::File { name, off, bytes, create, .. } => {
            FileWriteRec { dir: loc, name, off: *off, bytes }.encode(&mut b).expect("encode");
            if *create { XLOG_PGRC_FILE_CREATE } else { XLOG_PGRC_FILE_WRITE }
        }
        WalRecord::Publish { rec, .. } => {
            rec.encode(&mut b).expect("encode");
            XLOG_PGRC_PUBLISH
        }
        WalRecord::Install { rec, .. } => {
            rec.encode(&mut b).expect("encode");
            XLOG_PGRC_INSTALL
        }
        WalRecord::Reclaim { rec, .. } => {
            rec.encode(&mut b).expect("encode");
            XLOG_PGRC_RECLAIM
        }
        WalRecord::Commit { .. } => return None,
        WalRecord::FileRef { name, len, crc, masks, .. } => {
            pgrc_format::wal::FileRefRec { dir: loc, name, len: *len, crc: *crc, masks: *masks }.encode(&mut b).expect("encode");
            pgrc_format::wal::XLOG_PGRC_FILE_REF
        }
    };
    Some((info, b))
}

fn image(vfs: &mut MemVfs) -> BTreeMap<String, Vec<u8>> {
    vfs.list_dir(DIR).expect("list").into_iter().map(|n| {
        let b = vfs.read_full(&format!("{DIR}/{n}")).expect("read");
        (n, b)
    }).collect()
}

fn here(loc: DirLoc) -> types_error::PgResult<Option<String>> {
    assert_eq!(loc, LOC);
    Ok(Some(DIR.to_string()))
}

#[test]
fn redo_of_the_encoded_log_rebuilds_the_primary_directory() {
    seams();
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let log = WalLog::shared();
    let mut w = writer(50, &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 2500, |i| Some(i as i64 * 7));
    finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::InProgress));
    let mut aborted = writer(60, &log);
    append_int8_rows(&mut aborted, &mut vfs, &mut kit, 1200, |i| Some(i as i64));
    aborted.abort(&mut vfs).expect("abort");
    let mut w = writer(70, &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 700, |i| if i % 5 == 0 { None } else { Some(-(i as i64)) });
    let probe = Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::Committed).set(70, TxnVerdict::InProgress);
    finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);

    let records: Vec<(u8, Vec<u8>)> = lock(&log).records.iter().filter_map(encoded).collect();
    let kinds: Vec<u8> = records.iter().map(|(i, _)| *i).collect();
    for k in [XLOG_PGRC_FILE_CREATE, XLOG_PGRC_FILE_WRITE, XLOG_PGRC_PUBLISH, XLOG_PGRC_RECLAIM] {
        assert!(kinds.contains(&k), "the log has no record of kind {k:#x}: {kinds:x?}");
    }
    let mut replica = mem_with_dir();
    for (info, data) in &records {
        redo_op(&mut replica, *info, data, &here).expect("redo");
    }
    let want = image(&mut vfs);
    assert!(want.contains_key("manifest-2.pgrcm") && want.contains_key("CURRENT"), "{:?}", want.keys());
    assert_eq!(image(&mut replica), want);

    for (info, data) in &records {
        redo_op(&mut replica, *info, data, &here).expect("second redo");
    }
    assert_eq!(image(&mut replica), want, "redo is idempotent");
    let (names, overflow) = dirty_names(LOC).expect("redo registers");
    assert!(!overflow && names.iter().any(|n| n == "manifest-2.pgrcm"), "{names:?}");

    let mut b = Vec::new();
    encode_dir(LOC, &mut b);
    redo_op(&mut replica, XLOG_PGRC_TRUNCATE, &b, &here).expect("truncate");
    assert!(image(&mut replica).is_empty());
}

#[test]
fn redo_keeps_a_live_catalog_current() {
    seams();
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let log = WalLog::shared();
    let mut w = writer(50, &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 1500, |i| Some(i as i64));
    finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::InProgress));
    let mut w = writer(70, &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 300, |i| Some(-(i as i64)));
    finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::Committed).set(70, TxnVerdict::InProgress));
    let records: Vec<(u8, Vec<u8>)> = lock(&log).records.iter().filter_map(encoded).collect();

    let dir = scratch("live");
    let at = |loc: DirLoc| -> types_error::PgResult<Option<String>> {
        assert_eq!(loc, LOC);
        Ok(Some(dir.clone()))
    };
    let slot = (SPC, DB, RELFILENUMBER);
    pgrc_am::catalog::remove_dir(&dir);
    for (info, data) in &records {
        if let Some(a) = redo_op(&mut RealVfs, *info, data, &at).expect("redo") {
            keep_catalog(a);
        }
    }
    let (gens, head) = pgrc_am::catalog::generations(slot).expect("the publishes made the entry");
    let chain: Vec<(u64, u64)> = gens.iter().map(|g| (g.gen, g.prev_gen)).collect();
    assert_eq!((chain, head), (vec![(1, 0), (2, 1)], 2));
    assert_eq!(gens[1].manifest.parts.iter().map(|p| p.rows).sum::<u64>(), 1800);
    assert!(gens.iter().all(|g| g.image().is_some() && g.answers().is_some()));
    for (info, data) in &records {
        if let Some(a) = redo_op(&mut RealVfs, *info, data, &at).expect("second redo") {
            keep_catalog(a);
        }
    }
    let (again, _) = pgrc_am::catalog::generations(slot).expect("entry");
    assert!(again.len() == 2 && Arc::ptr_eq(&again[1], &gens[1]), "a held generation stays");

    let mut b = Vec::new();
    encode_dir(LOC, &mut b);
    keep_catalog(redo_op(&mut RealVfs, XLOG_PGRC_TRUNCATE, &b, &at).expect("truncate").expect("applied"));
    let (gens, head) = pgrc_am::catalog::generations(slot).expect("entry kept");
    assert!(gens.is_empty() && head == 0);
    pgrc_am::catalog::remove_dir(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn genid2_a_standby_unlinks_a_rolled_back_part_and_forgets_it() {
    use pgrc_format::wal::{ReclaimRec, Retire};
    use pgrc_write::writer::SubxactEvidence;
    seams();
    const REL: u32 = 7_906;
    let loc = DirLoc { spc: SPC, db: DB, relnumber: REL };
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let log = WalLog::shared();
    let open = |fxid: u64| {
        TableWriter::open(DIR.to_string(), vec![int8_col(1)], SPC, DB, REL as u64, stamp(fxid, 1), &SubxactEvidence::default(), PartCutPolicy::default())
            .expect("open")
            .with_sink(Box::new(RecordingSink::new(DIR, log.clone())))
    };
    let mut w = open(50);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 100, |i| Some(i as i64));
    finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
    let mut w = open(60);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 100, |i| Some(i as i64 + 1000));
    let dead = finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed).set(60, TxnVerdict::InProgress));
    let mut w = open(70);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 100, |i| Some(i as i64 + 2000));
    let probe = Probe::new(TxnVerdict::Committed).set(60, TxnVerdict::Aborted).set(70, TxnVerdict::InProgress).used(dead.gen, dead.manifest.header.next_part_no);
    let o3 = finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    assert_eq!((dead.gen, o3.gen, o3.manifest.header.prev_gen), (2, 3, 1));
    assert!(o3.part_nos.iter().all(|n| !dead.part_nos.contains(n)), "{:?} {:?}", o3.part_nos, dead.part_nos);

    let records: Vec<(u8, Vec<u8>)> = lock(&log).records.iter().filter_map(|r| encoded_at(loc, r)).collect();
    let dir = scratch("genid2");
    let at = |l: DirLoc| -> types_error::PgResult<Option<String>> {
        assert_eq!(l, loc);
        Ok(Some(dir.clone()))
    };
    let slot = (SPC, DB, REL as u64);
    pgrc_am::catalog::remove_dir(&dir);
    for (info, data) in &records {
        if let Some(a) = redo_op(&mut RealVfs, *info, data, &at).expect("redo") {
            keep_catalog(a);
        }
    }
    let gens = |s| pgrc_am::catalog::generations(s).map_or_else(Vec::new, |(g, _)| g.iter().map(|g| g.gen).collect::<Vec<_>>());
    assert_eq!(gens(slot), vec![1, 3], "the redo of gen 3 dropped the dead gen 2");
    assert_eq!(pgrc_am::catalog::dead_parts(slot), dead.part_nos, "its parts are kept for the reclaim");
    let names: Vec<String> = dead.part_nos.iter().map(|&n| pgrc_format::dirlayout::part_file_name(n)).collect();
    assert!(names.iter().all(|n| std::path::Path::new(&format!("{dir}/{n}")).exists()), "the redo installed them");
    let mut b = Vec::new();
    ReclaimRec { dir: loc, xid: 70, retire_below: Some(Retire::Conflict(3)), names: names.clone() }.encode(&mut b).expect("encode");
    keep_catalog(redo_op(&mut RealVfs, XLOG_PGRC_RECLAIM, &b, &at).expect("reclaim").expect("applied"));
    assert!(names.iter().all(|n| !std::path::Path::new(&format!("{dir}/{n}")).exists()), "the RECLAIM unlinked them");
    assert!(pgrc_am::catalog::dead_parts(slot).is_empty(), "and the numbers left the list");
    assert_eq!(gens(slot), vec![3]);
    pgrc_am::catalog::remove_dir(&dir);
    pgrc_am::catalog::forget_dir(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_record_whose_directory_is_gone_is_skipped() {
    seams();
    let mut vfs = MemVfs::new();
    let mut b = Vec::new();
    FileWriteRec { dir: DirLoc { spc: 1663, db: 5, relnumber: 91 }, name: "tmp-3-0.pgrct", off: 0, bytes: b"x" }
        .encode(&mut b)
        .expect("encode");
    redo_op(&mut vfs, XLOG_PGRC_FILE_CREATE, &b, &|_| Ok(None)).expect("skipped");
    assert!(dirty_names(DirLoc { spc: 1663, db: 5, relnumber: 91 }).is_none());
}

#[test]
fn a_corrupt_record_is_an_error_and_an_unknown_op_panics() {
    let mut vfs = mem_with_dir();
    let e = redo_op(&mut vfs, XLOG_PGRC_FILE_WRITE, &[1, 2, 3], &here).expect_err("corrupt");
    assert_eq!(e.level, ERROR);
    let e = redo_op(&mut vfs, 0xA0, &[], &here).expect_err("unknown");
    assert_eq!(e.level, PANIC);
}

#[test]
fn a_retirement_applied_at_once_resolves_its_conflict_first() {
    use crate::redo::resolves_first;
    use pgrc_format::wal::Retire;
    assert!(resolves_first(Some(Retire::Conflict(7)), true) && resolves_first(Some(Retire::Conflict(7)), false));
    assert!(!resolves_first(Some(Retire::Deferred(7)), true), "queued until this standby's horizon passes the xid");
    assert!(resolves_first(Some(Retire::Deferred(7)), false));
    assert!(!resolves_first(None, true) && !resolves_first(None, false));
}

#[test]
fn logical_records_redo_as_no_ops() {
    seams();
    let mut vfs = mem_with_dir();
    let before = image(&mut vfs);
    let img = TupleImage { infomask2: 1, infomask: 0, hoff: 24, data: b"row" };
    let parent = DirLoc { spc: 1663, db: 5, relnumber: 97 };
    let mut del = Vec::new();
    RowRec { parent, old: Some(img), new: None }.encode(XLOG_PGRC_ROW_DELETE, &mut del).expect("encode");
    let mut upd = Vec::new();
    RowRec { parent, old: None, new: Some(img) }.encode(XLOG_PGRC_ROW_UPDATE, &mut upd).expect("encode");
    let (mut bulk, mut tuple) = (Vec::new(), Vec::new());
    img.encode_into(&mut tuple).expect("encode");
    BulkRowsRec { parent, ntuples: 1, tuples: &tuple }.encode(&mut bulk).expect("encode");
    for (info, b) in [(XLOG_PGRC_ROW_DELETE, del), (XLOG_PGRC_ROW_UPDATE, upd), (XLOG_PGRC_BULK_ROWS, bulk)] {
        let applied = redo_op(&mut vfs, info, &b, &|_| panic!("a logical record looked its directory up"));
        assert!(matches!(applied, Ok(None)), "{info:#x}");
    }
    assert_eq!(image(&mut vfs), before);
    assert!(dirty_names(parent).is_none());
}

fn scratch(tag: &str) -> String {
    let d = std::env::temp_dir().join(format!("pgrc_wal_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d.to_str().expect("utf8").to_string()
}

fn touch(dir: &str, name: &str) {
    let mut v = RealVfs;
    let fd = v.create_rw(&format!("{dir}/{name}")).expect("create");
    v.pwrite_at(&fd, 0, b"pgrc").expect("write");
    v.close_file(fd).expect("close");
}

#[test]
fn a_gone_name_or_directory_counts_as_synced() {
    seams();
    let loc = DirLoc { spc: 1663, db: 5, relnumber: 92 };
    let dir = scratch("gone");
    touch(&dir, "part-0.pgrc");
    register(loc, ["part-0.pgrc".to_string(), "tmp-9-0.pgrct".to_string()]).expect("register");
    assert_eq!(sync_loc(loc, &dir).result, 0);
    assert!(dirty_names(loc).is_none(), "a sync takes the set");
    register(loc, ["part-1.pgrc".to_string()]).expect("register");
    assert_eq!(sync_loc(loc, &format!("{dir}/dropped")).result, 0);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn a_failed_sync_puts_its_names_back() {
    seams();
    let loc = DirLoc { spc: 1663, db: 5, relnumber: 93 };
    let dir = scratch("fail");
    touch(&dir, "part-0.pgrc");
    std::fs::create_dir(format!("{dir}/part-1.pgrc")).expect("a name that cannot be fsynced as a file");
    register(loc, ["part-0.pgrc".to_string(), "part-1.pgrc".to_string()]).expect("register");
    let r = sync_loc(loc, &dir);
    assert_eq!(r.result, -1);
    assert!(r.path.ends_with("part-1.pgrc"), "{}", r.path);
    assert_eq!(dirty_names(loc), Some((vec!["part-0.pgrc".to_string(), "part-1.pgrc".to_string()], false)));
    std::fs::remove_dir(format!("{dir}/part-1.pgrc")).expect("rmdir");
    assert_eq!(sync_loc(loc, &dir).result, 0);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn past_the_cap_a_directory_syncs_every_file() {
    seams();
    let loc = DirLoc { spc: 1663, db: 5, relnumber: 94 };
    let dir = scratch("overflow");
    touch(&dir, "part-0.pgrc");
    register(loc, (0..=DIRTY_NAMES_CAP).map(|i| format!("tmp-1-{i}.pgrct"))).expect("register");
    assert_eq!(dirty_names(loc), Some((Vec::new(), true)));
    register(loc, ["part-0.pgrc".to_string()]).expect("register");
    assert_eq!(dirty_names(loc), Some((Vec::new(), true)), "an overflowed set stays overflowed");
    assert_eq!(sync_loc(loc, &dir).result, 0);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn a_restartpoint_sweep_removes_only_the_named_temps() {
    let dir = scratch("sweep");
    let temps = ["tmp-100-0.pgrct", "tmp-100-7.pgrct", "tmp-200-0.pgrct", "tmp-300-0.pgrct", "tmp-x-0.pgrct"];
    for n in temps.into_iter().chain(["part-1.pgrc", "CURRENT"]) {
        touch(&dir, n);
    }
    let verdict = |fxid: u64| match fxid {
        100 => Ok(true),
        300 => Err(Box::new(types_error::PgError::error("clog".to_string()))),
        _ => Ok(false),
    };
    let (removed, kept) = crate::cleanup::sweep_dir(&dir, &dir, &verdict).expect("sweep");
    assert_eq!(removed, [("tmp-100-0.pgrct".to_string(), 100), ("tmp-100-7.pgrct".to_string(), 100)]);
    assert_eq!(kept.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), [format!("{dir}/tmp-300-0.pgrct")]);
    let mut left: Vec<String> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    assert_eq!(left, ["CURRENT", "part-1.pgrc", "tmp-200-0.pgrct", "tmp-300-0.pgrct", "tmp-x-0.pgrct"]);
    let skip = pgrc_write::plan::apply_file_write(&mut RealVfs, &dir, "tmp-100-0.pgrct", 8, b"late", false);
    assert!(matches!(skip, Err(pgrc_write::WriteError::SweptAlive { fxid: 100, .. })), "noted as swept: {skip:?}");
    let gone = format!("{dir}/gone");
    assert!(crate::cleanup::sweep_dir(&gone, &gone, &verdict).expect("a gone directory").0.is_empty());
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn filters_match_by_database() {
    let a = crate::sync::tag(DirLoc { spc: 1663, db: 5, relnumber: 1 });
    let b = crate::sync::tag(DirLoc { spc: 9000, db: 5, relnumber: 2 });
    let c = crate::sync::tag(DirLoc { spc: 1663, db: 6, relnumber: 1 });
    assert!(crate::sync::filetagmatches(&a, &b));
    assert!(!crate::sync::filetagmatches(&a, &c));
}


mod by_reference {
    use crate::fetch::materialize;
    use crate::insert::{ref_policy, PartsInWal, RefPolicy, DEFAULT_REF_MIN};
    use pgrc_format::spool::{masked_crc, spool_name};
    use pgrc_format::wal::{DirLoc, FileRefRec};
    use pgrc_spool::Spool;
    use pgrc_write::wvfs::RealVfs;
    use types_error::FATAL;

    const D: DirLoc = DirLoc { spc: 1663, db: 5, relnumber: 16384 };

    #[test]
    fn the_policy_logs_bytes_unless_asked_and_able() {
        let p = |mode, archive_parts| RefPolicy { mode, ref_min: 4096, archive_parts };
        assert_eq!(PartsInWal::parse(" Reference "), Some(PartsInWal::Reference));
        assert_eq!(PartsInWal::parse("bytes"), Some(PartsInWal::Bytes));
        assert_eq!(PartsInWal::parse("auto"), Some(PartsInWal::Auto));
        assert_eq!(PartsInWal::parse("on"), None);
        assert_eq!(p(PartsInWal::Bytes, true).ref_min(false, true, || true), None);
        assert_eq!(p(PartsInWal::Reference, false).ref_min(false, true, || true), Some(4096));
        assert_eq!(p(PartsInWal::Auto, false).ref_min(false, true, || true), Some(4096));
        assert_eq!(p(PartsInWal::Auto, false).ref_min(true, true, || true), None, "archiving without parts archiving");
        assert_eq!(p(PartsInWal::Auto, true).ref_min(true, true, || true), Some(4096));
        assert_eq!(p(PartsInWal::Reference, true).ref_min(false, true, || false), None, "another file system");
        assert_eq!(RefPolicy { mode: PartsInWal::Auto, ref_min: 0, archive_parts: false }.ref_min(false, true, || true), Some(1));
        assert_eq!(p(PartsInWal::Auto, false).ref_min(true, false, || false), Some(4096));
        assert_eq!(p(PartsInWal::Reference, false).ref_min(false, false, || false), Some(4096));
        assert_eq!(p(PartsInWal::Bytes, false).ref_min(false, false, || true), Some(4096));
        assert_eq!(p(PartsInWal::Bytes, false).ref_min(true, false, || false), Some(4096));
    }

    #[test]
    fn the_default_mode_is_bytes() {
        assert_eq!(ref_policy().mode, PartsInWal::Bytes);
        assert_eq!(ref_policy().ref_min, DEFAULT_REF_MIN);
    }



    fn tmp(tag: &str) -> (String, String, Spool) {
        let base = std::env::temp_dir().join(format!("pgrc-wal-fetch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("tbl")).unwrap();
        let base = base.to_str().unwrap().to_string();
        let spool = Spool::at(&format!("{base}/pg_pgrc_spool"));
        spool.ensure_dir().unwrap();
        (base.clone(), format!("{base}/tbl"), spool)
    }

    fn bytes() -> Vec<u8> {
        (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn redo_renames_a_pending_entry_and_rebuilds_the_table_file() {
        let (base, dir, spool) = tmp("pending");
        let b = bytes();
        let masks = [(0, 64), (9_904, 96)];
        let rec = FileRefRec { dir: D, name: "tmp-9-0.pgrct", len: b.len() as u64, crc: masked_crc(&b, &masks), masks };
        std::fs::write(spool.pending_path(D, rec.name), &b).unwrap();
        let entry = materialize(&mut RealVfs, &spool, 0x3_0000_0100, &rec, &dir).unwrap();
        assert_eq!(entry, spool_name(0x3_0000_0100));
        assert_eq!(std::fs::read(format!("{dir}/tmp-9-0.pgrct")).unwrap(), b);
        assert!(!std::path::Path::new(&spool.pending_path(D, rec.name)).exists());
        assert_eq!(spool.list(0, u64::MAX).unwrap(), vec![(0x3_0000_0100, b.len() as u64)]);
        materialize(&mut RealVfs, &spool, 0x3_0000_0100, &rec, &dir).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn redo_keeps_a_file_in_place_and_gives_it_a_spool_entry() {
        let (base, dir, spool) = tmp("inplace");
        let b = bytes();
        let rec = FileRefRec { dir: D, name: "bankstats-3.pgrcbs", len: b.len() as u64, crc: masked_crc(&b, &[(0, 0); 2]), masks: [(0, 0); 2] };
        std::fs::write(format!("{dir}/{}", rec.name), &b).unwrap();
        materialize(&mut RealVfs, &spool, 0x400, &rec, &dir).unwrap();
        assert_eq!(std::fs::read(spool.path(0x400)).unwrap(), b);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_reference_with_no_source_is_fatal_and_names_the_file() {
        let (base, dir, spool) = tmp("missing");
        let b = bytes();
        let rec = FileRefRec { dir: D, name: "part-4.pgrc", len: b.len() as u64, crc: 7, masks: [(0, 0); 2] };
        let e = materialize(&mut RealVfs, &spool, 0x1_0000_0028, &rec, &dir).unwrap_err();
        assert_eq!(e.level, FATAL);
        assert!(e.message().contains("the file referenced at 1/28"), "{}", e.message());
        assert!(e.message().contains("part-4.pgrc, 10000 bytes"), "{}", e.message());
        std::fs::write(format!("{dir}/part-4.pgrc"), b"not it").unwrap();
        assert_eq!(materialize(&mut RealVfs, &spool, 0x1_0000_0028, &rec, &dir).unwrap_err().level, FATAL);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[test]
fn rowid_reserve_redo_raises_the_file_and_forgets_the_counter() {
    use pgrc_am::rowid_alloc::{self, read_file};
    use pgrc_format::wal::{RowidReserveRec, XLOG_PGRC_ROWID_RESERVE};
    seams();
    let mut replica = mem_with_dir();
    let rec = |high: u64| {
        let mut b = Vec::new();
        RowidReserveRec { dir: LOC, high }.encode(&mut b);
        b
    };
    struct Fixed(u64);
    impl rowid_alloc::Io for Fixed {
        fn load(&mut self, _: &str) -> types_error::PgResult<u64> {
            Ok(self.0)
        }
        fn persist(&mut self, _: &str, _: u64) -> types_error::PgResult<()> {
            Ok(())
        }
        fn warn(&mut self, _: u64) -> types_error::PgResult<()> {
            Ok(())
        }
        fn load_extents(&mut self, _: &str) -> types_error::PgResult<Vec<pgrc_format::rowid::SegExtent>> {
            Ok(Vec::new())
        }
        fn persist_extents(&mut self, _: &str, _: &[u8]) -> types_error::PgResult<()> {
            Ok(())
        }
        fn warn_wide(&mut self, _: u32, _: u32) -> types_error::PgResult<()> {
            Ok(())
        }
    }
    rowid_alloc::reserve_with(DIR, "t", 1, &mut Fixed(0)).unwrap();
    assert!(rowid_alloc::held(DIR));
    let high = 291 + rowid_alloc::PREFETCH;
    assert!(redo_op(&mut replica, XLOG_PGRC_ROWID_RESERVE, &rec(high), &here).expect("redo").is_none());
    assert_eq!(read_file(&mut replica, DIR).unwrap(), Some(high));
    assert!(!rowid_alloc::held(DIR), "redo drops the in-memory counter");
    redo_op(&mut replica, XLOG_PGRC_ROWID_RESERVE, &rec(high), &here).expect("second redo");
    redo_op(&mut replica, XLOG_PGRC_ROWID_RESERVE, &rec(582), &here).expect("an older record");
    assert_eq!(read_file(&mut replica, DIR).unwrap(), Some(high), "redo is idempotent and never lowers the mark");
    let (names, _) = dirty_names(LOC).expect("redo registers");
    assert!(names.iter().any(|n| n == "rowid"), "{names:?}");
    assert!(redo_op(&mut replica, XLOG_PGRC_ROWID_RESERVE, &rec(high), &|_| Ok(None)).expect("gone").is_none());
    assert!(redo_op(&mut replica, XLOG_PGRC_ROWID_RESERVE, &[0u8; 3], &here).is_err(), "a short record is corrupt");
    rowid_alloc::reserve_with(DIR, "t", 1, &mut Fixed(high)).unwrap();
    assert!(redo_op(&mut replica, pgrc_format::wal::XLOG_PGRC_ROWID_SET, &rec(582), &here).expect("set").is_none());
    assert_eq!(read_file(&mut replica, DIR).unwrap(), Some(582), "ROWID_SET lowers the mark");
    assert!(!rowid_alloc::held(DIR), "and drops the in-memory counter");
    redo_op(&mut replica, XLOG_PGRC_ROWID_RESERVE, &rec(high), &here).expect("a later reservation");
    assert_eq!(read_file(&mut replica, DIR).unwrap(), Some(high), "a reservation after it raises it again");
    assert!(redo_op(&mut replica, pgrc_format::wal::XLOG_PGRC_ROWID_SET, &[0u8; 3], &here).is_err(), "a short ROWID_SET is corrupt");
    let mut b = Vec::new();
    encode_dir(LOC, &mut b);
    redo_op(&mut replica, XLOG_PGRC_TRUNCATE, &b, &here).expect("truncate");
    assert_eq!(read_file(&mut replica, DIR).unwrap(), None, "TRUNCATE takes the counter's file");
}

#[test]
fn rowid_extents_redo_rewrites_the_file_and_the_readers_table() {
    use pgrc_am::rowid_alloc::{self, read_segext};
    use pgrc_format::rowid::{SegExtent, ROWID_FIRST, SEG_EXTENT_IDS};
    use pgrc_format::wal::{RowidExtentsRec, XLOG_PGRC_ROWID_EXTENTS};
    seams();
    // its own directory and locator: the process-wide counters, readers' tables and dirty-name sets are keyed by them
    const XDIR: &str = "/tbl/pgrc_778";
    const XLOC: DirLoc = DirLoc { spc: SPC, db: DB, relnumber: RELFILENUMBER as u32 + 1 };
    let here = |loc: DirLoc| -> types_error::PgResult<Option<String>> {
        assert_eq!(loc, XLOC);
        Ok(Some(XDIR.to_string()))
    };
    let mut replica = MemVfs::new();
    replica.mkdir_path(XDIR).expect("mkdir");
    let e = |part_no: u32, idx: u32, k: u64| SegExtent { part_no, idx, base: ROWID_FIRST + k * SEG_EXTENT_IDS };
    let rec = |table: Vec<SegExtent>| {
        let mut b = Vec::new();
        RowidExtentsRec { dir: XLOC, table }.encode(&mut b).unwrap();
        b
    };
    let long = vec![e(4, 0, 0), e(4, 1, 2), e(7, 0, 1)];
    assert!(redo_op(&mut replica, XLOG_PGRC_ROWID_EXTENTS, &rec(long.clone()), &here).expect("redo").is_none());
    assert_eq!(read_segext(&mut replica, XDIR).unwrap(), long);
    let (names, _) = dirty_names(XLOC).expect("redo registers");
    assert!(names.iter().any(|n| n == "segext"), "{names:?}");
    let short = vec![e(7, 0, 1)];
    redo_op(&mut replica, XLOG_PGRC_ROWID_EXTENTS, &rec(short.clone()), &here).expect("prune");
    redo_op(&mut replica, XLOG_PGRC_ROWID_EXTENTS, &rec(short.clone()), &here).expect("again");
    assert_eq!(read_segext(&mut replica, XDIR).unwrap(), short, "a shorter table over the longer file");
    assert_eq!(rowid_alloc::extents(XDIR).unwrap().bases(7), vec![e(7, 0, 1).base], "the readers' table");
    assert_eq!(rowid_alloc::extents(XDIR).unwrap().bases(4), Vec::<u64>::new());
    assert!(redo_op(&mut replica, XLOG_PGRC_ROWID_EXTENTS, &rec(short.clone()), &|_| Ok(None)).expect("gone").is_none());
    let mut trailing = rec(short);
    trailing.push(0);
    assert!(redo_op(&mut replica, XLOG_PGRC_ROWID_EXTENTS, &trailing, &here).is_err(), "trailing bytes are corrupt");
    rowid_alloc::forget(XDIR);
}

/// D4711: a sync request the end-of-recovery cleanup cannot queue is the cleanup's error. XlogSink::register kept it in
/// INSERT_ERROR and returned nothing, so cleanup_dir unlinked the dead temp and returned Ok.
#[test]
fn d4711_a_sync_request_the_cleanup_cannot_queue_fails_the_cleanup() {
    seams();
    let mut vfs = mem_with_dir();
    let fd = vfs.create_rw(&format!("{DIR}/{}", pgrc_format::dirlayout::temp_file_name(60, 1))).expect("create");
    vfs.close_file(fd).expect("close");
    let mut sink = crate::insert::XlogSink::new(DirLoc { spc: SPC, db: DB, relnumber: D4711_NO_SYNC });
    let r = pgrc_write::cleanup::cleanup_dir(&mut vfs, &mut sink, DIR, &Probe::new(TxnVerdict::Aborted));
    let stashed = pgrc_write::WriteError::Contract { detail: pgrc_write::walsink::WAL_INSERT_FAILED };
    assert_eq!(r.err(), Some(stashed), "D4711: the cleanup went on past a sync request it could not queue");
    let e = crate::insert::take_insert_error().expect("the request's own error");
    assert!(e.message().contains("D4711"), "{}", e.message());
}
