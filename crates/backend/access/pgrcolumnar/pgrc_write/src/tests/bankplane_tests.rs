//! [fmt-land] Seal-time bank-grain stats plane: birth, first publish only (P-1 OC-2 a),
//! kill switch, and refusal semantics (the crash story rides
//! `crash_matrix.rs`'s op-boundary sweep, extended over the plane window).

use super::*;
use crate::bankplane::PlaneOutcome;
use crate::publish::TxnVerdict;
use pgrc_format::bankstats as fb;

fn plane_path(gen: u64) -> String {
    format!("{DIR}/{}", fb::bankstats_file_name(gen))
}

fn read_plane(
    vfs: &mut MemVfs,
    gen: u64,
) -> (fb::BankStatsHeader, Vec<fb::PartIdent>, Vec<fb::ColDirEntry>, Vec<u8>) {
    let bytes = vfs.read_full(&plane_path(gen)).expect("plane bytes");
    let (h, p, c) = fb::decode_meta(&bytes).expect("plane decodes");
    (h, p, c, bytes)
}

/// Fresh table: the FIRST publish writes a valid plane bound to gen 1 —
/// no offline builder involved (the freshly-written-banks law).
#[test]
fn fresh_publish_writes_valid_plane() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::InProgress).set(7, TxnVerdict::Committed);
    let mut w = open_writer(vec![int8_col(1)], stamp(7, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 1000, |i| Some(i as i64));
    let out = finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    assert!(matches!(out.bankstats_plane, PlaneOutcome::Written { .. }));

    let m = read_manifest(&mut vfs, 1);
    let (h, pidents, cols, bytes) = read_plane(&mut vfs, 1);
    assert_eq!(h.gen, 1);
    assert_eq!(h.schema_fingerprint, m.header.schema_fingerprint);
    assert_eq!(h.part_count, 1);
    assert_eq!(cols.len(), 1);
    assert_eq!(pidents[0].part_no, m.parts[0].part_no);
    assert_eq!(pidents[0].rows, m.parts[0].rows);
    assert_eq!(pidents[0].footer_off, m.parts[0].footer_off);
    assert_eq!(pidents[0].granule_count, m.parts[0].granule_count);
    assert_eq!(pidents[0].band_count, m.parts[0].band_count);
    // Every column payload validates + serves a non-empty §8.1 body.
    for e in &cols {
        let payload = &bytes[e.off as usize..(e.off + e.len) as usize];
        let v = fb::ColPayloadRef::new(e, h.part_count, payload).expect("col validates");
        assert!(v.stats_body(0).expect("stats body present").len() >= 80);
    }
}

/// OC-2 (a), P-1: a publish over a base with parts writes no plane (the carry-forward was O(parts) per publish);
/// the first generation's plane stays bound to its own part set, and the table is fine.
#[test]
fn a_publish_over_a_base_with_parts_writes_no_plane() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::InProgress)
        .set(7, TxnVerdict::Committed)
        .set(9, TxnVerdict::Committed);
    let mut w1 = open_writer(vec![int8_col(1)], stamp(7, 1));
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 1000, |i| Some(i as i64));
    finish_and_publish(&mut w1, &mut vfs, &mut kit, &probe);
    let (h1, _, _, _) = read_plane(&mut vfs, 1);
    assert_eq!(h1.part_count, 1);

    let mut w2 = open_writer(vec![int8_col(1)], stamp(9, 1));
    append_int8_rows(&mut w2, &mut vfs, &mut kit, 500, |i| Some(i as i64 * 3));
    let out2 = finish_and_publish(&mut w2, &mut vfs, &mut kit, &probe);
    assert_eq!(out2.bankstats_plane, PlaneOutcome::Skipped { reason: "base has parts" });
    assert!(!vfs.exists_path(&plane_path(2)).expect("exists"));
    let m = read_manifest(&mut vfs, 2);
    assert_eq!(m.parts.len(), 2);
    assert_eq!(read_plane(&mut vfs, 1).0.part_count, 1, "gen 1's plane untouched");
}
