//! `LoggedVfs` and the sinks: chunking, the create flag, `file_done` in place of fsync (and its redo-pointer
//! branch), refused namespace changes, and a worker's forwarded chunks logging exactly like direct ones.

use super::*;
use crate::walmodel::{RecordingSink, WalLog, WalRecord};
use crate::walsink::{log_forwarded, ForwardSink, LoggedVfs, NoWal};
use crate::{WriteError, WriteResult};
use pgrc_format::wal::FILE_WRITE_MAX_CHUNK;
use pgsync::lock;

fn file_records(log: &WalLog) -> Vec<(String, u64, usize, bool)> {
    log.records
        .iter()
        .filter_map(|r| match r {
            WalRecord::File { name, off, bytes, create, .. } => Some((name.clone(), *off, bytes.len(), *create)),
            _ => None,
        })
        .collect()
}

#[test]
fn writes_split_at_the_chunk_bound_and_only_the_first_creates() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let mut sink = RecordingSink::new(DIR, log.clone());
    let mut lvfs = LoggedVfs::new(&mut vfs, &mut sink, DIR);
    let big = vec![5u8; 2 * FILE_WRITE_MAX_CHUNK + 100];
    let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-0.pgrct")).expect("create");
    lvfs.pwrite_at(&fd, 10, &big).expect("write");
    lvfs.pwrite_at(&fd, 0, b"head").expect("write");
    lvfs.close_file(fd).expect("close");
    let k = FILE_WRITE_MAX_CHUNK;
    let n = "tmp-9-0.pgrct".to_string();
    assert_eq!(
        file_records(&lock(&log)),
        vec![(n.clone(), 10, k, true), (n.clone(), 10 + k as u64, k, false), (n.clone(), 10 + 2 * k as u64, 100, false), (n, 0, 4, false)]
    );
    let bytes = vfs.read_full(&format!("{DIR}/tmp-9-0.pgrct")).expect("read");
    assert_eq!(&bytes[..4], b"head");
    assert_eq!(bytes.len(), 10 + big.len());
}

#[test]
fn an_empty_file_still_logs_its_create() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let mut sink = RecordingSink::new(DIR, log.clone());
    let mut lvfs = LoggedVfs::new(&mut vfs, &mut sink, DIR);
    let fd = lvfs.create_rw(&format!("{DIR}/sections-3.pgrcx")).expect("create");
    lvfs.close_file(fd).expect("close");
    assert_eq!(file_records(&lock(&log)), vec![("sections-3.pgrcx".to_string(), 0, 0, true)]);
}

#[test]
fn file_done_registers_instead_of_fsyncing_unless_a_checkpoint_began() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let mut sink = RecordingSink::new(DIR, log.clone());
    {
        let mut lvfs = LoggedVfs::new(&mut vfs, &mut sink, DIR);
        let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-0.pgrct")).expect("create");
        lvfs.pwrite_at(&fd, 0, b"abc").expect("write");
        lvfs.fsync_file(&fd).expect("done");
        lvfs.close_file(fd).expect("close");
        let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-1.pgrct")).expect("create");
        lvfs.pwrite_at(&fd, 0, b"def").expect("write");
        lock(&log).begin_checkpoint();
        lvfs.close_file(fd).expect("close");
    }
    let ops = vfs.ops().to_vec();
    assert!(!ops.iter().any(|o| o.contains("tmp-9-0") && o.starts_with("fsync")), "{ops:#?}");
    assert!(ops.iter().any(|o| o == &format!("fsync {DIR}/tmp-9-1.pgrct")), "a checkpoint began mid-file: the sink fsyncs");
    assert!(ops.iter().any(|o| o == &format!("fsyncdir {DIR}")), "and its directory");
    let g = lock(&log);
    assert!(g.dirty[DIR].contains("tmp-9-0.pgrct"));
    assert!(!g.dirty[DIR].contains("tmp-9-1.pgrct"));
    assert_eq!(g.self_synced, vec!["tmp-9-1.pgrct".to_string()]);
}

#[test]
fn namespace_changes_outside_a_record_are_refused() {
    let mut vfs = mem_with_dir();
    let fd = vfs.create_rw(&format!("{DIR}/tmp-1-0.pgrct")).expect("create");
    vfs.close_file(fd).expect("close");
    let mut sink = NoWal;
    let mut lvfs = LoggedVfs::new(&mut vfs, &mut sink, DIR);
    let p = format!("{DIR}/tmp-1-0.pgrct");
    let contract = |r: WriteResult<()>| matches!(r, Err(WriteError::Contract { .. }));
    assert!(contract(lvfs.rename_path(&p, &format!("{DIR}/part-0.pgrc"))));
    assert!(contract(lvfs.unlink_path(&p)));
    assert!(contract(lvfs.mkdir_path(&format!("{DIR}/x"))));
    assert!(matches!(lvfs.open_rw(&p), Err(WriteError::Contract { .. })));
    assert!(matches!(lvfs.create_rw("/elsewhere/tmp-1-1.pgrct"), Err(WriteError::Contract { .. })));
    assert!(lvfs.exists_path(&p).expect("reads pass through"));
}

#[test]
fn forwarded_chunks_log_exactly_like_direct_writes() {
    fn write(vfs: &mut MemVfs, sink: &mut dyn crate::walsink::WalSink) {
        let payload: Vec<u8> = (0..(FILE_WRITE_MAX_CHUNK + 77)).map(|i| i as u8).collect();
        let mut lvfs = LoggedVfs::new(vfs, sink, DIR);
        let fd = lvfs.create_rw(&format!("{DIR}/tmp-4-2.pgrct")).expect("create");
        lvfs.pwrite_at(&fd, 0, &payload[..64]).expect("w");
        lvfs.pwrite_at(&fd, 64, &payload[64..]).expect("w");
        lvfs.close_file(fd).expect("close");
    }
    let direct = WalLog::shared();
    let mut v1 = mem_with_dir();
    write(&mut v1, &mut RecordingSink::new(DIR, direct.clone()));

    let mut v2 = mem_with_dir();
    let mut fwd = ForwardSink::default();
    write(&mut v2, &mut fwd);
    assert_eq!(fwd.files.len(), 1);
    let leader = WalLog::shared();
    log_forwarded(&mut RecordingSink::new(DIR, leader.clone()), &mut v2, DIR, &fwd.files[0]).expect("log");
    let (a, b) = (lock(&direct), lock(&leader));
    assert_eq!(a.records, b.records);
    assert_eq!(a.dirty, b.dirty);
}

// ---- parts by reference (WR L1) ----------------------------------------------------------------------------------

use crate::plan::{apply_file_ref, verify_ref, RefSource};
use crate::walsink::WalSink;
use pgrc_format::spool::{masked_crc, spool_name};
use pgrc_format::wal::{FileRefRec, SmallMasks, NO_MASKS};

type RefView = (String, u64, u32, SmallMasks);

fn ref_records(log: &WalLog) -> Vec<RefView> {
    log.records
        .iter()
        .filter_map(|r| match r {
            WalRecord::FileRef { name, len, crc, masks, .. } => Some((name.clone(), *len, *crc, *masks)),
            _ => None,
        })
        .collect()
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

/// Write `name` through a logged vfs over `vfs`: create, the writes in order (masks first when given), close.
fn write_file(vfs: &mut MemVfs, sink: &mut dyn WalSink, name: &str, masks: Option<SmallMasks>, writes: &[(u64, &[u8])]) -> WriteResult<()> {
    let mut lvfs = LoggedVfs::new(vfs, sink, DIR);
    let fd = lvfs.create_rw(&format!("{DIR}/{name}"))?;
    if let Some(m) = masks {
        lvfs.note_ref_masks(&fd, m)?;
    }
    for (off, b) in writes {
        lvfs.pwrite_at(&fd, *off, b)?;
    }
    lvfs.close_file(fd)
}

fn spool_names(vfs: &mut MemVfs) -> Vec<String> {
    vfs.list_dir(SPOOL).expect("list spool")
}

/// The same writes under a plain sink (W0) and under a spool sink: `(W0 log, spool log, spool universe)`.
fn both(ref_min: u64, name: &str, writes: &[(u64, &[u8])]) -> (Arc<Mutex<WalLog>>, Arc<Mutex<WalLog>>, MemVfs) {
    let w0 = WalLog::shared();
    let mut v0 = mem_with_spool();
    write_file(&mut v0, &mut RecordingSink::new(DIR, w0.clone()), name, None, writes).expect("W0 write");
    let wr = spool_log(ref_min, None);
    let mut v1 = mem_with_spool();
    write_file(&mut v1, &mut RecordingSink::new(DIR, wr.clone()), name, None, writes).expect("spool write");
    (w0, wr, v1)
}

use pgsync::Mutex;
use std::sync::Arc;

#[test]
fn a_file_below_the_threshold_logs_w0s_records_byte_for_byte() {
    let k = FILE_WRITE_MAX_CHUNK;
    let big = data(2 * k + 100, 1);
    let small = data(999, 2);
    for (ref_min, writes) in [
        (3 * k as u64, vec![(0u64, &big[..])]),
        (1000, vec![(0u64, &small[..400]), (400, &small[400..])]),
        (1, vec![]),
    ] {
        let (w0, wr, mut v1) = both(ref_min, "tmp-9-0.pgrct", &writes);
        let (a, b) = (lock(&w0), lock(&wr));
        assert_eq!(a.records, b.records, "ref_min {ref_min}");
        assert_eq!(a.dirty, b.dirty);
        assert!(ref_records(&b).is_empty());
        assert!(spool_names(&mut v1).is_empty());
    }
}

#[test]
fn at_and_above_the_threshold_a_file_goes_by_reference() {
    let d = data(3000, 3);
    for (name, writes) in [
        ("tmp-9-0.pgrct", vec![(0u64, &d[..600]), (600, &d[600..1000])]),
        ("tmp-9-1.pgrct", vec![(0u64, &d[..10]), (10, &d[10..2000]), (2000, &d[2000..])]),
        ("part-3-stats-1.pgrcs", vec![(0u64, &d[..])]),
    ] {
        let len: usize = writes.iter().map(|(_, b)| b.len()).sum();
        let (_, wr, mut v1) = both(1000, name, &writes);
        let g = lock(&wr);
        assert!(file_records(&g).is_empty(), "{name}: no bytes in WAL");
        assert_eq!(ref_records(&g), vec![(name.to_string(), len as u64, masked_crc(&d[..len], &NO_MASKS), NO_MASKS)]);
        assert_eq!(spool_names(&mut v1), vec![spool_name(0)]);
        assert_eq!(v1.read_full(&format!("{SPOOL}/{}", spool_name(0))).expect("entry"), d[..len].to_vec());
        let all = v1.ops().to_vec();
        let ops = &all[all.iter().position(|o| o == &format!("create {DIR}/{name}")).expect("create op")..];
        let at = |p: &str| ops.iter().position(|o| o.starts_with(p)).unwrap_or_else(|| panic!("{p}: {ops:#?}"));
        assert!(at(&format!("fsync {DIR}/{name}")) < at(&format!("fsyncdir {DIR}")));
        assert!(at(&format!("fsyncdir {DIR}")) < at("link "));
        assert!(at("link ") < at(&format!("fsyncdir {SPOOL}")));
        assert!(at(&format!("fsyncdir {SPOOL}")) < at("rename "), "pending link durable before the insert and rename");
        assert!(g.dirty[SPOOL].contains(&spool_name(0)), "the spool entry is synced by the next checkpoint");
        assert!(g.dirty.contains_key(DIR));
    }
}

#[test]
fn a_write_elsewhere_before_the_switch_logs_what_was_held_and_continues_as_w0() {
    let d = data(2600, 4);
    let writes = [(0u64, &d[..100]), (500, &d[500..600]), (600, &d[600..])];
    let (w0, wr, _) = both(1000, "tmp-9-0.pgrct", &writes);
    assert_eq!(lock(&w0).records, lock(&wr).records);
    assert!(ref_records(&lock(&wr)).is_empty());
}

#[test]
fn a_write_elsewhere_after_the_switch_is_refused() {
    let d = data(300, 5);
    let log = spool_log(100, None);
    let mut v = mem_with_spool();
    let r = write_file(&mut v, &mut RecordingSink::new(DIR, log.clone()), "tmp-9-0.pgrct", None, &[(0, &d[..200]), (0, &d[200..])]);
    assert!(matches!(r, Err(WriteError::Contract { .. })), "{r:?}");
    let r = write_file(&mut v, &mut RecordingSink::new(DIR, log), "tmp-9-1.pgrct", None, &[(0, &d[..200]), (250, &d[250..])]);
    assert!(matches!(r, Err(WriteError::Contract { .. })), "{r:?}");
}

#[test]
fn masks_ride_the_reference_and_come_before_the_first_write() {
    let d = data(4000, 6);
    let masks: SmallMasks = [(0, 64), (3904, 96)];
    let log = spool_log(1000, None);
    let mut v = mem_with_spool();
    write_file(&mut v, &mut RecordingSink::new(DIR, log.clone()), "tmp-9-0.pgrct", Some(masks), &[(0, &d[..64]), (64, &d[64..])]).expect("write");
    assert_eq!(ref_records(&lock(&log)), vec![("tmp-9-0.pgrct".to_string(), 4000, masked_crc(&d, &masks), masks)]);

    let mut sink = RecordingSink::new(DIR, log);
    let mut lvfs = LoggedVfs::new(&mut v, &mut sink, DIR);
    let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-1.pgrct")).expect("create");
    lvfs.pwrite_at(&fd, 0, &d[..10]).expect("write");
    assert!(matches!(lvfs.note_ref_masks(&fd, masks), Err(WriteError::Contract { .. })));
}

/// K1-MEM: a streamed seal masks its header at create and its footer once it knows where the footer goes, before
/// writing it. The reference is the one the masks noted up front give.
#[test]
fn a_mask_added_ahead_of_the_bytes_fed_gives_the_up_front_reference() {
    let d = data(4000, 11);
    let masks: SmallMasks = [(0, 64), (3904, 96)];
    let up_front = spool_log(1000, None);
    write_file(&mut mem_with_spool(), &mut RecordingSink::new(DIR, up_front.clone()), "tmp-9-0.pgrct", Some(masks), &[(0, &d[..64]), (64, &d[64..])]).expect("write");

    let late = spool_log(1000, None);
    let mut v = mem_with_spool();
    let mut sink = RecordingSink::new(DIR, late.clone());
    let mut lvfs = LoggedVfs::new(&mut v, &mut sink, DIR);
    let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-0.pgrct")).expect("create");
    lvfs.note_ref_masks(&fd, [(0, 64), (0, 0)]).expect("the header's mask");
    lvfs.pwrite_at(&fd, 0, &d[..64]).expect("write");
    lvfs.pwrite_at(&fd, 64, &d[64..1500]).expect("write, past ref_min: by reference from here");
    lvfs.pwrite_at(&fd, 1500, &d[1500..3800]).expect("write");
    lvfs.note_ref_masks(&fd, masks).expect("the footer's mask, ahead of the bytes fed");
    lvfs.pwrite_at(&fd, 3800, &d[3800..]).expect("write");
    lvfs.close_file(fd).expect("close");
    assert_eq!(ref_records(&lock(&late)), ref_records(&lock(&up_front)));
    assert_eq!(ref_records(&lock(&late)), vec![("tmp-9-0.pgrct".to_string(), 4000, masked_crc(&d, &masks), masks)]);
}

#[test]
fn a_mask_change_that_drops_or_backdates_a_mask_is_refused() {
    let d = data(4000, 12);
    for (to, why) in [
        ([(3904, 96), (0, 0)], "the header's mask dropped"),
        ([(0, 64), (1000, 96)], "a mask over bytes already fed"),
        ([(0, 64), (32, 3000)], "overlapping masks"),
    ] {
        let log = spool_log(1000, None);
        let mut v = mem_with_spool();
        let mut sink = RecordingSink::new(DIR, log);
        let mut lvfs = LoggedVfs::new(&mut v, &mut sink, DIR);
        let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-0.pgrct")).expect("create");
        lvfs.note_ref_masks(&fd, [(0, 64), (0, 0)]).expect("the header's mask");
        lvfs.pwrite_at(&fd, 0, &d[..2000]).expect("write");
        assert!(matches!(lvfs.note_ref_masks(&fd, to), Err(WriteError::Contract { .. })), "{why}");
    }
}

#[test]
fn without_a_ref_min_masks_are_ignored_and_nothing_is_buffered() {
    let d = data(4000, 7);
    let log = WalLog::shared();
    let mut v = mem_with_dir();
    let mut sink = RecordingSink::new(DIR, log.clone());
    let mut lvfs = LoggedVfs::new(&mut v, &mut sink, DIR);
    let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-0.pgrct")).expect("create");
    lvfs.note_ref_masks(&fd, [(0, 64), (0, 0)]).expect("a no-op");
    lvfs.pwrite_at(&fd, 0, &d).expect("write");
    assert_eq!(file_records(&lock(&log)).len(), 1, "logged at the write, as W0");
    lvfs.close_file(fd).expect("close");
}

#[test]
fn a_forwarded_reference_logs_exactly_like_a_direct_one() {
    let d = data(5000, 8);
    let writes = [(0u64, &d[..64]), (64, &d[64..])];
    let direct = spool_log(1000, None);
    let mut v1 = mem_with_spool();
    write_file(&mut v1, &mut RecordingSink::new(DIR, direct.clone()), "tmp-4-2.pgrct", None, &writes).expect("direct");

    let mut v2 = mem_with_spool();
    let mut fwd = ForwardSink::with_ref_min(Some(1000));
    write_file(&mut v2, &mut fwd, "tmp-4-2.pgrct", None, &writes).expect("worker");
    assert_eq!(fwd.files.len(), 1);
    assert!(fwd.files[0].chunks.is_empty(), "no bytes cross to the leader");
    assert!(fwd.files[0].by_ref.is_some());
    assert!(v2.ops().iter().any(|o| o == &format!("fsync {DIR}/tmp-4-2.pgrct")), "the worker fsynced its file");
    let leader = spool_log(1000, None);
    log_forwarded(&mut RecordingSink::new(DIR, leader.clone()), &mut v2, DIR, &fwd.files[0]).expect("log");
    let (a, b) = (lock(&direct), lock(&leader));
    assert_eq!(a.records, b.records);
    assert_eq!(a.dirty, b.dirty);
    assert_eq!(spool_names(&mut v1), spool_names(&mut v2));
}

#[test]
fn checkpoints_drop_spool_entries_only_below_the_slowest_consumer() {
    let d = data(2000, 9);
    for (slot, early, kept) in [(Some(0), false, true), (None, false, false), (Some(0), true, false), (Some(5), false, false)] {
        let log = spool_log(1000, slot);
        lock(&log).teeth.skip_spool_hold = early;
        let mut v = mem_with_spool();
        write_file(&mut v, &mut RecordingSink::new(DIR, log.clone()), "tmp-9-0.pgrct", None, &[(0, &d)]).expect("write");
        lock(&log).checkpoint(&mut v).expect("checkpoint");
        assert_eq!(spool_names(&mut v).len(), kept as usize, "slot {slot:?} early {early}");
        let r = lock(&log).check_spool(&mut v, 0, 1);
        assert_eq!(r.is_ok(), kept, "{r:?}");
    }
}

#[test]
fn redo_takes_the_bytes_from_the_spool_the_pending_link_or_the_file_itself() {
    let d = data(3000, 10);
    let masks: SmallMasks = [(0, 64), (2904, 96)];
    let rec = FileRefRec { dir: crate::walmodel::model_loc(DIR), name: "tmp-9-0.pgrct", len: 3000, crc: masked_crc(&d, &masks), masks };
    let t = format!("{DIR}/tmp-9-0.pgrct");
    let put = |v: &mut MemVfs, p: &str, b: &[u8]| {
        let fd = v.create_rw(p).expect("create");
        v.pwrite_at(&fd, 0, b).expect("write");
        v.close_file(fd).expect("close");
    };
    let entry = format!("{SPOOL}/{}", spool_name(7));
    // From the spool: the table file appears, as the same inode.
    let mut v = mem_with_spool();
    put(&mut v, &entry, &d);
    apply_file_ref(&mut v, DIR, &rec, RefSource::Spool(entry.clone())).expect("from the spool");
    assert!(verify_ref(&mut v, &t, &rec).expect("verify"));
    assert!(v.ops().iter().any(|o| o.starts_with("link ")));
    // A patch inside the masks, through the other name, still verifies.
    let fd = v.open_rw(&t).expect("open");
    v.pwrite_at(&fd, 0, &[0xEE; 64]).expect("patch");
    v.close_file(fd).expect("close");
    assert!(verify_ref(&mut v, &entry, &rec).expect("verify"));
    // A stale table file of another content is replaced.
    let mut v = mem_with_spool();
    put(&mut v, &entry, &d);
    put(&mut v, &t, b"stale");
    apply_file_ref(&mut v, DIR, &rec, RefSource::Pending(entry.clone())).expect("over a stale name");
    assert_eq!(v.read_full(&t).expect("read"), d);
    // In place: the file must already hold the bytes.
    apply_file_ref(&mut v, DIR, &rec, RefSource::InPlace).expect("in place");
    let mut v = mem_with_spool();
    assert!(apply_file_ref(&mut v, DIR, &rec, RefSource::InPlace).is_err(), "reference without bytes");
    // A source that does not verify is corrupt; so is a byte flipped outside the masks.
    let mut bad = d.clone();
    bad[100] ^= 1;
    put(&mut v, &entry, &bad);
    assert!(apply_file_ref(&mut v, DIR, &rec, RefSource::Spool(entry.clone())).is_err());
    put(&mut v, &entry, &d[..2999]);
    assert!(apply_file_ref(&mut v, DIR, &rec, RefSource::Spool(entry)).is_err(), "a short file");
}

/// A transaction stages two files under one tmp name (a seal's RowIdentMap, then a compile's DV of the same part):
/// the first goes by reference and is renamed before the second is created. Redo after a crash replays the
/// FILE_REF, the rename and the second create over the directory the crash left, where the final name is the spool
/// entry's inode. Linking the entry under the tmp name made the rename a no-op and the create truncate the final
/// name's bytes (the crash e2e's "part 0 RowIdentMap: kind mismatch").
#[test]
fn redo_of_a_reused_tmp_name_keeps_the_spool_entry_and_the_later_name_intact() {
    let put = |v: &mut MemVfs, p: &str, b: &[u8]| {
        let fd = v.create_rw(p).expect("create");
        v.pwrite_at(&fd, 0, b).expect("write");
        v.close_file(fd).expect("close");
    };
    let redo_rename = |v: &mut MemVfs, from: &str, to: &str| match v.rename_path(from, to) {
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => {}
        r => r.expect("rename"),
    };
    let (map, dv) = (data(3000, 11), data(700, 12));
    let tmp_name = "tmp-9-1073741824.pgrct";
    let rec = FileRefRec { dir: crate::walmodel::model_loc(DIR), name: tmp_name, len: 3000, crc: masked_crc(&map, &NO_MASKS), masks: NO_MASKS };
    let (tmp, map_final, dv_final) = (format!("{DIR}/{tmp_name}"), format!("{DIR}/part-0-rowmap-1.pgrcs"), format!("{DIR}/part-0-dv-5.pgrcs"));
    let entry = format!("{SPOOL}/{}", spool_name(7));
    for dv_staged in [true, false] {
        // The primary: the map by reference and renamed; the DV at the same tmp name, renamed too unless the crash
        // came first.
        let mut v = mem_with_spool();
        put(&mut v, &tmp, &map);
        v.link_path(&tmp, &entry).expect("spool link");
        v.rename_path(&tmp, &map_final).expect("publish");
        put(&mut v, &tmp, &dv);
        if dv_staged {
            v.rename_path(&tmp, &dv_final).expect("publish");
        }
        // Redo from before the FILE_REF.
        apply_file_ref(&mut v, DIR, &rec, RefSource::Spool(entry.clone())).expect("FILE_REF redo");
        redo_rename(&mut v, &tmp, &map_final);
        crate::plan::apply_file_write(&mut v, DIR, tmp_name, 0, &dv, true).expect("FILE_CREATE redo");
        redo_rename(&mut v, &tmp, &dv_final);
        assert_eq!(v.read_full(&map_final).expect("read"), map, "the later name keeps the referenced bytes ({dv_staged})");
        assert_eq!(v.read_full(&entry).expect("read"), map, "so does the spool entry ({dv_staged})");
        assert_eq!(v.read_full(&dv_final).expect("read"), dv);
        assert!(!v.exists_path(&tmp).expect("stat"), "no tmp left ({dv_staged})");
    }
}

fn image_of(vfs: &mut MemVfs, dir: &str) -> BTreeMap<String, Vec<u8>> {
    vfs.list_dir(dir).expect("list").into_iter().map(|n| {
        let b = vfs.read_full(&format!("{dir}/{n}")).expect("read");
        (n, b)
    }).collect()
}

/// Sealed parts go by reference with the publish's two patch ranges masked; the publish patches the one inode both
/// names share, and the reference still verifies. A standby holding only the spool rebuilds the directory.
#[test]
fn a_sealed_part_is_referenced_with_its_publish_patches_masked() {
    let mut vfs = mem_with_spool();
    let mut kit = Kit::new();
    let log = spool_log(4096, Some(0));
    let policy = PartCutPolicy { max_rows: 1000, max_bytes: u64::MAX, cut_granule_rows: 500 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(50, 1), policy).with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 3000, |i| Some(i as i64 * 7));
    finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::InProgress));
    log_commit(&log, 50);
    let g = lock(&log);
    let publish = g.records.iter().find_map(|r| match r {
        WalRecord::Publish { rec, .. } => Some(rec.clone()),
        _ => None,
    }).expect("a publish");
    assert_eq!(publish.installs.len(), 3);
    let mut referenced = 0;
    for (lsn, r) in g.records.iter().enumerate() {
        let WalRecord::FileRef { name, len, crc, masks, .. } = r else { continue };
        let Some(i) = publish.installs.iter().find(|i| &i.tmp == name) else { continue };
        referenced += 1;
        assert_eq!(*masks, crate::seal::part_ref_masks(i.footer_off));
        assert_eq!(masks[0], (0, i.header.len() as u32), "the header patch of apply_part_install");
        assert_eq!(masks[1], (i.footer_off, i.footer.len() as u32), "the footer patch of apply_part_install");
        let entry = format!("{SPOOL}/{}", spool_name(lsn as u64));
        let part = vfs.read_full(&format!("{DIR}/{}", pgrc_format::dirlayout::part_file_name(i.part_no))).expect("part");
        assert_eq!(&part[..64], &i.header[..], "published");
        assert_eq!(vfs.read_full(&entry).expect("entry"), part, "one inode under both names");
        let rec = FileRefRec { dir: crate::walmodel::model_loc(DIR), name, len: *len, crc: *crc, masks: *masks };
        assert!(verify_ref(&mut vfs, &entry, &rec).expect("verify"), "the patched file verifies");
    }
    assert_eq!(referenced, 3, "every part went by reference");
    assert!(g.check_spool(&mut vfs, 0, g.records.len()).is_ok());
    let upto = g.records.len();
    drop(g);

    let mut standby = mem_with_spool();
    for (n, b) in image_of(&mut vfs, SPOOL) {
        let fd = standby.create_rw(&format!("{SPOOL}/{n}")).expect("create");
        standby.pwrite_at(&fd, 0, &b).expect("write");
        standby.close_file(fd).expect("close");
    }
    {
        let mut g = lock(&log);
        g.replay_from = 0;
        g.replay(&mut standby, upto).expect("replay from the spool");
    }
    assert_eq!(image_of(&mut standby, DIR), image_of(&mut vfs, DIR));
}

/// Crash recovery with every record flushed: redo re-links each referenced temp from its spool entry (or pending
/// link), publish redo renames it, and nothing pending is left.
#[test]
fn crash_recovery_over_references_rebuilds_the_generation() {
    let mut vfs = mem_with_spool();
    let mut kit = Kit::new();
    let log = spool_log(4096, Some(0));
    let policy = PartCutPolicy { max_rows: 1000, max_bytes: u64::MAX, cut_granule_rows: 500 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(50, 1), policy).with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 3000, |i| Some(i as i64 * 7));
    finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::InProgress));
    log_commit(&log, 50);
    let before = image_of(&mut vfs, DIR);
    let upto = durable_upto(&log, false);
    crash_and_recover(&mut vfs, &log, upto);
    assert_eq!(image_of(&mut vfs, DIR), before);
    let spool = spool_names(&mut vfs);
    assert_eq!(spool.len(), 3);
    assert!(spool.iter().all(|n| pgrc_format::spool::parse_spool_name(n).is_some()), "{spool:?}");
}

/// Gate A's golden: a whole generation (parts, sections, sidecars, publish) whose files all end below `ref_min` logs
/// exactly the records W0 logs, in the same order.
#[test]
fn a_generation_below_the_threshold_logs_exactly_w0s_records() {
    let run = |log: &Arc<Mutex<WalLog>>| {
        let mut vfs = mem_with_spool();
        let mut kit = Kit::new();
        let policy = PartCutPolicy { max_rows: 1000, max_bytes: u64::MAX, cut_granule_rows: 500 };
        let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(50, 1), policy)
            .with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
        for i in 0..2500u64 {
            let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
            with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
        }
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
        w.publish(&mut vfs, &Probe::new(TxnVerdict::Aborted).set(50, TxnVerdict::InProgress)).expect("publish");
        image_of(&mut vfs, DIR)
    };
    let w0 = WalLog::shared();
    let a = run(&w0);
    let wr = spool_log(u64::MAX, Some(0));
    let b = run(&wr);
    assert_eq!(a, b);
    let (g0, g1) = (lock(&w0), lock(&wr));
    assert!(g0.records.len() > 5);
    assert_eq!(g0.records, g1.records);
    assert_eq!(g0.dirty, g1.dirty);
}

/// The front seal at minimal (user 2026-10-04): a sink that could go by reference, wrapped in `BytesOnly`, logs a
/// large file exactly as W0 does and links nothing into the spool.
#[test]
fn a_bytes_only_sink_logs_a_large_file_by_its_bytes() {
    let d = data(3000, 5);
    let writes = [(0u64, &d[..])];
    let w0 = WalLog::shared();
    let mut v0 = mem_with_spool();
    write_file(&mut v0, &mut RecordingSink::new(DIR, w0.clone()), "tmp-9-0.pgrct", None, &writes).expect("W0 write");
    let wr = spool_log(1000, None);
    let mut v1 = mem_with_spool();
    let mut sink = crate::walsink::BytesOnly(Box::new(RecordingSink::new(DIR, wr.clone())));
    assert_eq!(sink.ref_min(), None);
    write_file(&mut v1, &mut sink, "tmp-9-0.pgrct", None, &writes).expect("bytes-only write");
    assert_eq!(lock(&w0).records, lock(&wr).records);
    assert!(ref_records(&lock(&wr)).is_empty());
    assert!(spool_names(&mut v1).is_empty());
}

#[test]
fn the_pace_bucket_lets_a_burst_through_then_holds_the_rate() {
    use crate::walsink::pace::Bucket;
    let mb = 1u64 << 20;
    let mut b = Bucket::new(48 * mb, mb, 1_000);
    assert_eq!(b.after(mb, 1_000), 0, "the burst goes through");
    let wait = b.after(48 * mb, 1_000);
    assert_eq!(wait, 1_000_000_000, "48 MiB more at 48 MiB/s: one second");
    assert_eq!(b.after(0, 1_000 + 400_000_000), 600_000_000, "time spent counts");
    assert_eq!(b.after(0, 1_000 + 2_000_000_000), 0, "behind the rate: no wait");
    let mut slow = Bucket::new(0, 0, 0);
    assert!(slow.after(10, 0) > 0, "a zero rate is clamped, never divides by zero");
}

#[test]
fn a_pace_scope_sees_every_chunk_and_ends_with_its_guard() {
    use crate::walsink::pace;
    use std::cell::RefCell;
    use std::rc::Rc;
    let seen = Rc::new(RefCell::new(Vec::new()));
    assert!(!pace::active());
    pace::paced(7);
    {
        let s = seen.clone();
        let _scope = pace::scope(Box::new(move |n| s.borrow_mut().push(n)));
        assert!(pace::active());
        pace::paced(3);
        pace::paced(4);
    }
    assert!(!pace::active());
    pace::paced(9);
    assert_eq!(*seen.borrow(), vec![3, 4], "only the scope's chunks");
}

#[test]
fn a_paced_scope_writes_the_same_file_and_records_in_chunk_pieces() {
    use crate::walsink::pace;
    let run = |paced: bool| {
        let mut vfs = mem_with_dir();
        let log = WalLog::shared();
        let mut sink = RecordingSink::new(DIR, log.clone());
        let _scope = paced.then(|| pace::scope(Box::new(|_| {})));
        let mut lvfs = LoggedVfs::new(&mut vfs, &mut sink, DIR);
        let fd = lvfs.create_rw(&format!("{DIR}/tmp-9-1.pgrct")).expect("create");
        lvfs.pwrite_at(&fd, 0, &data(3 * FILE_WRITE_MAX_CHUNK + 17, 3)).expect("write");
        assert_eq!(lvfs.written(), 3 * FILE_WRITE_MAX_CHUNK as u64 + 17);
        lvfs.close_file(fd).expect("close");
        let recs = file_records(&lock(&log));
        (recs, vfs.read_full(&format!("{DIR}/tmp-9-1.pgrct")).expect("read"))
    };
    assert_eq!(run(true), run(false));
}
