use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const D: DirLoc = DirLoc { spc: 1663, db: 5, relnumber: 16384 };

fn tmp(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!("pgrc-spool-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("table")).unwrap();
    base.to_str().unwrap().to_string()
}

fn put(p: &str, b: &[u8]) {
    std::fs::write(p, b).unwrap();
}

#[test]
fn pending_then_final_shares_the_file_and_outlives_the_table_name() {
    let base = tmp("life");
    let sp = Spool::at(&format!("{base}/pg_pgrc_spool"));
    let t = format!("{base}/table/tmp-7-0.pgrct");
    put(&t, b"part bytes");
    let pending = sp.link_pending(&t, D, "tmp-7-0.pgrct").unwrap();
    assert!(pending.ends_with("/pending-1663-5-16384-tmp-7-0.pgrct"));
    assert_eq!(sp.link_pending(&t, D, "tmp-7-0.pgrct").unwrap(), pending);
    sp.name_final(&pending, 0x1_0000_0028).unwrap();
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![(0x1_0000_0028, 10)]);
    std::fs::remove_file(&t).unwrap();
    assert_eq!(std::fs::read(sp.path(0x1_0000_0028)).unwrap(), b"part bytes", "reclaim leaves the spool link");
    sp.link_final(&sp.path(0x1_0000_0028), 0x1_0000_0028).unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn listing_is_sorted_half_open_and_skips_pending() {
    let base = tmp("list");
    let sp = Spool::at(&format!("{base}/pg_pgrc_spool"));
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![], "a missing spool is empty");
    sp.ensure_dir().unwrap();
    for (lsn, n) in [(300u64, 3usize), (100, 1), (200, 2)] {
        put(&sp.path(lsn), &vec![0u8; n]);
    }
    put(&sp.pending_path(D, "part-1.pgrc"), b"x");
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![(100, 1), (200, 2), (300, 3)]);
    assert_eq!(sp.list(100, 300).unwrap(), vec![(100, 1), (200, 2)]);
    assert_eq!(sp.bytes_held().unwrap(), 6);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn removal_takes_exactly_the_entries_below_the_cutoff() {
    let base = tmp("rm");
    let sp = Spool::at(&format!("{base}/pg_pgrc_spool"));
    sp.ensure_dir().unwrap();
    for lsn in [0x100u64, 0x1000, 0x1001, 0x2000] {
        put(&sp.path(lsn), b"abcd");
    }
    put(&sp.pending_path(D, "part-1.pgrc"), b"x");
    assert_eq!(sp.remove_below(0x1001).unwrap(), (2, 8));
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![(0x1001, 4), (0x2000, 4)]);
    assert_eq!(sp.remove_below(0x1001).unwrap(), (0, 0));
    assert!(std::path::Path::new(&sp.pending_path(D, "part-1.pgrc")).exists(), "segment removal leaves pending entries");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn debris_is_pending_entries_and_entries_past_the_end_of_wal() {
    let base = tmp("debris");
    let sp = Spool::at(&format!("{base}/pg_pgrc_spool"));
    sp.ensure_dir().unwrap();
    for lsn in [0x100u64, 0x200, 0x300] {
        put(&sp.path(lsn), b"ab");
    }
    put(&sp.pending_path(D, "part-1.pgrc"), b"x");
    put(&format!("{}/unrelated", sp.dir()), b"x");
    assert_eq!(sp.remove_debris(0).unwrap(), 1, "unknown end: pending only");
    assert_eq!(sp.remove_debris(0x200).unwrap(), 2);
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![(0x100, 2)]);
    assert!(std::path::Path::new(&format!("{}/unrelated", sp.dir())).exists(), "names the spool never makes are left");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_streamed_file_lands_under_its_entry_name_only_when_complete() {
    let base = tmp("land");
    let sp = Spool::at(&format!("{base}/pg_pgrc_spool"));
    let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut l = sp.land_begin(0x2_0000_0100).unwrap();
    l.write_at(0, &body[..131_072]).unwrap();
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![], "a partial file is no entry");
    drop(l);
    let mut l = sp.land_begin(0x2_0000_0100).unwrap();
    for off in (0..body.len()).step_by(131_072) {
        l.write_at(off as u64, &body[off..(off + 131_072).min(body.len())]).unwrap();
    }
    let before = REGISTRY.latest_ref_lsn();
    l.finish().unwrap();
    assert!(REGISTRY.latest_ref_lsn() >= 0x2_0000_0100.max(before), "landing names the entry");
    assert_eq!(sp.list(0, u64::MAX).unwrap(), vec![(0x2_0000_0100, body.len() as u64)]);
    let mut r = sp.open_entry(0x2_0000_0100).unwrap();
    let mut got = vec![0u8; body.len() + 10];
    assert_eq!(r.read_at(0, &mut got).unwrap(), body.len(), "short only at end of file");
    assert_eq!(&got[..body.len()], &body[..]);
    assert_eq!(sp.open_entry(0x2_0000_0200).unwrap_err().raw_os_error(), Some(libc::ENOENT));
    drop(sp.land_begin(0x2_0000_0300).unwrap());
    assert_eq!(sp.remove_debris(0).unwrap(), 1);
    assert_eq!(sp.list(0, u64::MAX).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn same_device_holds_within_one_file_system() {
    let base = tmp("dev");
    let sp = Spool::at(&format!("{base}/pg_pgrc_spool"));
    assert!(sp.same_device(&format!("{base}/table")), "judged by the parent before the spool exists");
    sp.ensure_dir().unwrap();
    assert!(sp.same_device(&format!("{base}/table")));
    assert!(!sp.same_device(&format!("{base}/missing")));
    assert_eq!(tablespace_of("base/5/pgrc_16384"), "base");
    assert_eq!(tablespace_of("pg_tblspc/16400/PG_18_x/5/pgrc_1"), "pg_tblspc/16400");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn settle_waits_for_every_reference_begun_before_it() {
    let r = Arc::new(Registry::new());
    let a = r.begin_ref();
    let b = r.begin_ref();
    let settled = Arc::new(AtomicBool::new(false));
    let h = {
        let (r, settled) = (r.clone(), settled.clone());
        std::thread::spawn(move || {
            r.settle();
            settled.store(true, Ordering::SeqCst);
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    let late = r.begin_ref();
    r.end_ref(a, Some(0x500));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!settled.load(Ordering::SeqCst), "b is still in flight");
    r.end_ref(b, None);
    h.join().unwrap();
    assert!(settled.load(Ordering::SeqCst));
    assert_eq!(r.latest_ref_lsn(), 0x500, "a failed reference names nothing");
    assert_eq!(r.in_flight(), 1);
    r.end_ref(late, Some(0x400));
    assert_eq!(r.latest_ref_lsn(), 0x500, "latest is a maximum");
    r.settle();
}

#[test]
fn settle_under_racing_references() {
    let r = Arc::new(Registry::new());
    let ended = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for w in 0..4u64 {
        let (r, ended, stop) = (r.clone(), ended.clone(), stop.clone());
        hs.push(std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let t = r.begin_ref();
                let id = t.0;
                std::thread::yield_now();
                lock(&ended).insert(id);
                r.end_ref(t, Some(w * 1_000_000 + i));
                i += 1;
            }
        }));
    }
    for _ in 0..200 {
        let upto = lock(&r.state).next;
        r.settle();
        let e = lock(&ended);
        let open_below = lock(&r.state).open.iter().filter(|&&t| t <= upto).count();
        assert_eq!(open_below, 0);
        assert!((1..=upto).all(|t| e.contains(&t)), "a ticket at or below {upto} had not ended");
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }
}


#[test]
fn a_backups_stream_attaches_by_its_segment_start_and_sees_the_cover() {
    const SEG: u64 = 16 << 20;
    // A start point away from any other test's (the registry is process-wide).
    let start = 0x7_0000_0000 + 5 * SEG + 0x1234;
    let b = backup_begin(start, SEG);
    let s = backup_stream(start - 0x1234).expect("pg_basebackup streams from the segment start");
    assert!(std::sync::Arc::ptr_eq(&b, &s));
    assert!(backup_stream(start).is_none(), "a stream starting elsewhere carries another client's WAL");
    assert_eq!(s.limit(), Some(u64::MAX), "copying: no hold");
    b.begin_listing();
    assert_eq!(s.limit(), None, "listing: hold at the first reference");
    b.set_cover(start + 999);
    assert_eq!(s.limit(), Some(start + 999));
    b.abort();
    assert_eq!(s.limit(), Some(u64::MAX), "a failed backup's stream is not held");
    let b2 = backup_begin(start + 8, SEG);
    assert!(std::sync::Arc::ptr_eq(&backup_stream(start - 0x1234).unwrap(), &b2));
    for i in 0..BACKUPS_KEPT as u64 {
        backup_begin(0x8_0000_0000 + i * SEG, SEG);
    }
    assert!(backup_stream(start - 0x1234).is_none());
}

#[test]
fn the_ledger_counts_spool_bytes_by_lsn() {
    let mut l = Ledger::new();
    for (lsn, n) in [(100, 10), (300, 30), (200, 20), (400, 40), (200, 20)] {
        l.add(lsn, n);
    }
    assert_eq!((l.total(), l.bytes_from(0), l.bytes_from(200), l.bytes_from(201), l.bytes_from(401)), (100, 100, 90, 70, 0));
    assert_eq!(l.cut_to_budget(0, 100), 0);
    assert_eq!(l.cut_to_budget(0, 75), 201, "100 and 200 go: 70 left");
    assert_eq!(l.cut_to_budget(350, 75), 350);
    assert_eq!(l.cut_to_budget(0, 0), 401);
    assert_eq!(l.keep_for(0, 1000, 70), Some(300));
    assert_eq!(l.keep_for(0, 250, 70), Some(200), "the newest one below `hi` that reaches it");
    assert_eq!(l.keep_for(250, 1000, 71), None);
    assert_eq!(l.keep_for(0, 1000, 101), None);
    assert_eq!((l.last_below(300), l.last_below(100)), (Some(200), None));
    l.remove_below(250);
    assert_eq!((l.total(), l.bytes_from(0), l.removed_upto()), (70, 70, 201));
    l.add(150, 5);
    assert_eq!(l.total(), 70);
    l.add(500, 50);
    l.remove_from(400);
    assert_eq!((l.total(), l.bytes_from(300), l.cut_to_budget(0, 0)), (30, 30, 301));
}

#[test]
fn a_lowlevel_backup_waits_for_references_in_flight_and_refuses_new_ones() {
    let r = Arc::new(Registry::new());
    let a = r.begin_ref_unless_backup().expect("no backup yet");
    let started = Arc::new(AtomicBool::new(false));
    let h = {
        let (r, started) = (r.clone(), started.clone());
        std::thread::spawn(move || {
            r.lowlevel_backup_begin();
            started.store(true, Ordering::SeqCst);
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!started.load(Ordering::SeqCst), "the backup's checkpoint waits for a's record");
    assert!(r.begin_ref_unless_backup().is_none(), "counted before it waits: a new reference goes as bytes");
    r.end_ref(a, Some(0x700));
    h.join().unwrap();
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(r.lowlevel_backups(), 1);
    assert!(r.begin_ref_unless_backup().is_none());
    r.lowlevel_backup_end();
    assert_eq!(r.lowlevel_backups(), 0);
    let b = r.begin_ref_unless_backup().expect("references again after the backup");
    r.end_ref(b, None);
    r.lowlevel_backup_end();
    assert_eq!(r.lowlevel_backups(), 0, "an unmatched end never underflows");
}
