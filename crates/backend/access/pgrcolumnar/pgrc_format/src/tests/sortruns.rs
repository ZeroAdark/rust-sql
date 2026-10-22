//! SORTRUNS-E0 L0 (pgrc-plans/SORTRUNS-E0-STEP1.md D1): the manifest's sorted runs (`EXT_RUNS`, tag 10) behind
//! `MANIFESTF_RUNS` (32), the PUBLISH kind bit `PUBLISH_KIND_RUNS` (0x20), the one-home law over parts and runs, the
//! run reverse's sidecar kind and file name, and the publish's edit of the runs when members retire.

use crate::dirlayout::{classify, parse_runrev_file_name, runrev_file_name, FileClass};
use crate::manifest::{
    encode_ranges, ext_records, put_ext_record, with_next_row_id, IdRanges, Manifest, PartRecord, EXT_RANGES, EXT_ROWID, EXT_RUNS, MANIFESTF_EXT, MANIFESTF_ROWIDS, MANIFESTF_ROWMAP2,
    MANIFESTF_RUNS, MANIFESTF_SCHEMAS, MANIFESTF_UNDER, MANIFEST_HEADER_LEN,
};
use crate::rowid::IDS_PER_BLOCK;
use crate::runrev::PAGE;
use crate::sidecar::SidecarKind;
use crate::sortrun::{decode, encode, retire_members, runs, runs_in, with_runs, RankForm, RunRevForm, SortRun, MAX_MEMBERS};
use crate::wal::{PublishKind, PublishRec, PUBLISH_KIND_FLAGS, PUBLISH_KIND_ROWMAP2, PUBLISH_KIND_RUNS, PUBLISH_KIND_UNDER};

const B: u64 = IDS_PER_BLOCK;

/// The golden sample's header with parts 1..=5 (`next_part_no` 10), the ranges table `r`, the floor `next` and the
/// runs `rs` (none when empty).
fn table(r: &[(u32, IdRanges)], next: u64, rs: &[SortRun]) -> Manifest {
    let g = crate::tests::golden_sample_manifest();
    let parts: Vec<PartRecord> = (1..=5).map(|n| PartRecord { part_no: n, dv_gen: 0, dv_len: 0, dv_crc: 0, ..g.parts[0] }).collect();
    let mut h = g.header;
    h.part_count = parts.len() as u32;
    h.next_part_no = 10;
    let mut ext = Vec::new();
    if !r.is_empty() {
        encode_ranges(r, &mut ext);
    }
    let ext = with_next_row_id(&ext, next).unwrap();
    let ext = with_runs(&ext, rs).unwrap();
    Manifest::new(h, parts).with_ext(ext)
}

fn homes(run_id: u64, members: &[u32], ranges: &[(u64, u64)]) -> SortRun {
    let span: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
    SortRun { run_id, level: 1, rev: RunRevForm::Labels, rank: RankForm::Dense, rows: span, rev_len: 3 * PAGE as u64, rev_crc: 0x1234_5678, members: members.to_vec(), ranges: ranges.to_vec() }
}

fn selfhomed(run_id: u64, members: &[u32]) -> SortRun {
    SortRun { run_id, level: 1, rev: RunRevForm::None, rank: RankForm::Dense, rows: 10, rev_len: 0, rev_crc: 0, members: members.to_vec(), ranges: Vec::new() }
}

fn decodes(m: &Manifest) -> bool {
    Manifest::decode(&m.encode()).is_ok()
}

fn with_flags(m: &Manifest, flags: u32) -> Vec<u8> {
    let mut b = m.encode();
    b[64..68].copy_from_slice(&flags.to_le_bytes());
    let n = b.len();
    let crc = crate::wire::crc32c(&b[..n - 4]);
    b[n - 4..].copy_from_slice(&crc.to_le_bytes());
    b
}

#[test]
fn sortruns_tags_flags_and_kind_bit_are_pinned() {
    assert_eq!(EXT_RUNS, 10, "tags 1-7 and 9 are used, 8 is reserved");
    assert_eq!(MANIFESTF_RUNS, 32);
    assert_eq!(MANIFESTF_RUNS & (MANIFESTF_EXT | MANIFESTF_UNDER | MANIFESTF_SCHEMAS | MANIFESTF_ROWIDS | MANIFESTF_ROWMAP2), 0, "a new bit: a pre-E0 binary's mask refuses it");
    assert_eq!(PUBLISH_KIND_RUNS, 0x20);
    assert_eq!(PUBLISH_KIND_FLAGS, PUBLISH_KIND_UNDER | PUBLISH_KIND_ROWMAP2 | PUBLISH_KIND_RUNS);
    assert!(PublishKind::from_u8(PublishKind::Compact as u8 | PUBLISH_KIND_RUNS).is_none(), "a pre-E0 decode (flags 0xC0) reads 0x2N as an unknown kind");
    assert_eq!(SidecarKind::from_u16(8).unwrap(), SidecarKind::RunReverse);
    assert_eq!(SidecarKind::RunReverse.tag(), "runrev");
    assert!(SidecarKind::RunReverse.authoritative());
    assert_eq!(runrev_file_name(77), "runrev-77.pgrcv");
    assert_eq!(classify("runrev-77.pgrcv"), Some(FileClass::RunRev(77)));
    assert_eq!(parse_runrev_file_name("runrev-077.pgrcv"), None);
}

#[test]
fn sortruns_record_round_trips_and_is_40_bytes_a_run() {
    let rs = vec![homes(5, &[1, 2], &[(B, 4 * B), (10 * B, 11 * B)]), selfhomed(9, &[3, 0, 4])];
    let body = encode(&rs);
    assert_eq!(body.len(), 8 + (40 + 2 * 4 + 2 * 16) + (40 + 3 * 4), "header 8, per run 40 + 4 a member + 16 a range");
    assert_eq!(decode(&body).unwrap(), rs);
    let m = table(&[(3, vec![(20 * B, 21 * B)])], 40 * B, &rs);
    assert_eq!(m.header.flags & (MANIFESTF_RUNS | MANIFESTF_EXT), MANIFESTF_RUNS | MANIFESTF_EXT, "with_ext sets the bit");
    let back = Manifest::decode(&m.encode()).unwrap();
    assert_eq!(back, m);
    assert_eq!(runs(&back).unwrap(), rs);
    assert_eq!(runs_in(&back.ext).unwrap(), rs);
    let tags: Vec<u16> = ext_records(&back.ext).unwrap().iter().map(|r| r.0).collect();
    assert_eq!(tags, vec![EXT_RANGES, EXT_ROWID, EXT_RUNS]);
    let none = m.clone().with_ext(with_runs(&m.ext, &[]).unwrap());
    assert_eq!(none.header.flags & MANIFESTF_RUNS, 0, "no runs, no record, no bit");
    assert!(!ext_records(&none.ext).unwrap().iter().any(|r| r.0 == EXT_RUNS));
    assert!(decodes(&none));
}

#[test]
fn sortruns_bit_and_record_must_agree() {
    let m = table(&[], 40 * B, &[homes(5, &[1, 2], &[(B, 4 * B)])]);
    assert!(decodes(&m));
    assert!(Manifest::decode(&with_flags(&m, m.header.flags & !MANIFESTF_RUNS)).is_err(), "a record without the bit");
    let plain = table(&[], 40 * B, &[]);
    assert!(Manifest::decode(&with_flags(&plain, plain.header.flags | MANIFESTF_RUNS)).is_err(), "the bit without a record");
    let bare = crate::tests::golden_sample_manifest();
    assert!(Manifest::decode(&with_flags(&bare, MANIFESTF_RUNS)).is_err(), "the bit without an extension block");
    assert!(Manifest::decode(&with_flags(&bare, 64)).is_err(), "the next unknown bit is still refused");
    // a pre-E0 binary's flag check: the bit is outside its mask, so it refuses the manifest at open
    let old_mask = MANIFESTF_EXT | MANIFESTF_UNDER | MANIFESTF_SCHEMAS | MANIFESTF_ROWIDS | MANIFESTF_ROWMAP2;
    let enc = m.encode();
    let flags = u32::from_le_bytes(enc[64..68].try_into().unwrap());
    assert_ne!(flags & !old_mask, 0, "a manifest with runs is unreadable to a pre-E0 binary");
    assert!(enc.len() > MANIFEST_HEADER_LEN);
}

#[test]
fn sortruns_record_refuses_malformed_bodies() {
    let good = homes(5, &[1, 2], &[(B, 4 * B)]);
    let mut cases: Vec<(&str, Vec<SortRun>)> = Vec::new();
    cases.push(("run_id 0", vec![SortRun { run_id: 0, ..good.clone() }]));
    cases.push(("run_ids not ascending", vec![good.clone(), SortRun { run_id: 5, members: vec![3], ..good.clone() }]));
    cases.push(("level 0", vec![SortRun { level: 0, ..good.clone() }]));
    cases.push(("no members", vec![SortRun { members: vec![], ..good.clone() }]));
    cases.push(("every slot emptied", vec![SortRun { members: vec![0, 0], ..good.clone() }]));
    cases.push(("too many members", vec![SortRun { members: (1..=MAX_MEMBERS as u32 + 1).collect(), ..good.clone() }]));
    cases.push(("homes without ranges", vec![SortRun { ranges: vec![], ..good.clone() }]));
    cases.push(("reverse not whole pages", vec![SortRun { rev_len: PAGE as u64 + 1, ..good.clone() }]));
    cases.push(("reverse under a page", vec![SortRun { rev_len: 0, ..good.clone() }]));
    cases.push(("dense rows past the span", vec![SortRun { rows: 3 * B + 1, ..good.clone() }]));
    cases.push(("ranges overlap", vec![SortRun { ranges: vec![(B, 3 * B), (2 * B, 4 * B)], ..good.clone() }]));
    cases.push(("empty range", vec![SortRun { ranges: vec![(B, B)], ..good.clone() }]));
    cases.push(("NONE with ranges", vec![SortRun { rev: RunRevForm::None, rev_len: 0, rev_crc: 0, ..good.clone() }]));
    cases.push(("NONE with a reverse", vec![SortRun { rev_len: PAGE as u64, rev_crc: 1, ..selfhomed(5, &[1]) }]));
    cases.push(("NONE rank SET", vec![SortRun { rank: RankForm::Set, ..selfhomed(5, &[1]) }]));
    cases.push(("zero rows", vec![SortRun { rows: 0, ..good.clone() }]));
    for (why, rs) in &cases {
        assert!(decode(&encode(rs)).is_err(), "{why}");
    }
    let body = encode(&[good.clone()]);
    for (why, at, v) in [("version", 0usize, 2u8), ("law", 1, 2), ("reserved", 2, 1), ("rev form", 8 + 9, 3), ("rank form", 8 + 10, 2), ("flags", 8 + 11, 1)] {
        let mut b = body.clone();
        b[at] = v;
        assert!(decode(&b).is_err(), "{why}");
    }
    let mut trailing = body.clone();
    trailing.push(0);
    assert!(decode(&trailing).is_err(), "trailing bytes");
    assert!(decode(&body[..body.len() - 1]).is_err(), "truncated");
    let mut two = Vec::new();
    put_ext_record(&mut two, EXT_RUNS, &body);
    put_ext_record(&mut two, EXT_RUNS, &body);
    assert!(runs_in(&two).is_err(), "two records");
}

#[test]
fn sortruns_members_must_be_parts_once_and_homing_runs_own_their_ids() {
    let floor = 40 * B;
    assert!(decodes(&table(&[(3, vec![(20 * B, 21 * B)])], floor, &[homes(5, &[1, 2], &[(B, 4 * B)])])));
    assert!(!decodes(&table(&[], floor, &[homes(5, &[1, 7], &[(B, 4 * B)])])), "a slot naming no part");
    assert!(!decodes(&table(&[], floor, &[homes(5, &[1, 1], &[(B, 4 * B)])])), "a part in two slots");
    assert!(!decodes(&table(&[], floor, &[homes(5, &[1, 2], &[(B, 4 * B)]), homes(6, &[2, 3], &[(5 * B, 6 * B)])])), "a part in two runs");
    assert!(!decodes(&table(&[(1, vec![(20 * B, 21 * B)])], floor, &[homes(5, &[1, 2], &[(B, 4 * B)])])), "a homing run's member with its own ranges");
    assert!(decodes(&table(&[(1, vec![(20 * B, 21 * B)])], floor, &[selfhomed(5, &[1, 2])])), "a self-homed run's members keep theirs");
    assert!(decodes(&table(&[], floor, &[homes(5, &[0, 2], &[(B, 4 * B)])])), "an emptied slot");
    // one home: a part or a run
    assert!(!decodes(&table(&[(3, vec![(2 * B, 3 * B)])], floor, &[homes(5, &[1, 2], &[(B, 4 * B)])])), "a part's range inside a run's");
    assert!(!decodes(&table(&[], floor, &[homes(5, &[1], &[(B, 4 * B)]), homes(6, &[2], &[(3 * B, 5 * B)])])), "two runs homing one ID");
    assert!(decodes(&table(&[], floor, &[homes(5, &[1], &[(B, 4 * B)]), homes(6, &[2], &[(4 * B, 5 * B)])])), "adjacent homes");
    assert!(!decodes(&table(&[], 3 * B, &[homes(5, &[1, 2], &[(B, 4 * B)])])), "a run's range past the floor");
    assert!(!decodes(&table(&[], floor, &[homes(5, &[1, 2], &[(B + 1, 4 * B)])])), "an unaligned range");
    assert!(!decodes(&table(&[], floor, &[homes(5, &[1, 2], &[(0, 4 * B)])])), "a range below the first ID");
}

#[test]
fn sortruns_retiring_members_empties_slots_repoints_and_drops_empty_runs() {
    let rs = vec![homes(5, &[1, 2], &[(B, 4 * B)]), selfhomed(9, &[3, 4])];
    let ext = with_runs(&[], &rs).unwrap();
    assert_eq!(retire_members(&ext, &[5], &[]).unwrap(), ext, "no member retired: unchanged");
    assert_eq!(retire_members(&[], &[1], &[]).unwrap(), Vec::<u8>::new(), "no record: unchanged");
    let one = runs_in(&retire_members(&ext, &[1], &[]).unwrap()).unwrap();
    assert_eq!(one[0].members, vec![0, 2], "the slot empties, the run and its homes stay");
    assert_eq!(one[0].ranges, rs[0].ranges);
    let moved = runs_in(&retire_members(&ext, &[2, 3], &[(2, 8)]).unwrap()).unwrap();
    assert_eq!((moved[0].members.clone(), moved[1].members.clone()), (vec![1, 8], vec![0, 4]), "a rewrite keeps the slot");
    let gone = runs_in(&retire_members(&ext, &[1, 2], &[]).unwrap()).unwrap();
    assert_eq!(gone, vec![rs[1].clone()], "a run with no member left goes");
    let all = retire_members(&ext, &[1, 2, 3, 4], &[]).unwrap();
    assert!(runs_in(&all).unwrap().is_empty() && !ext_records(&all).unwrap().iter().any(|r| r.0 == EXT_RUNS), "the last run takes the record");
}

#[test]
fn sortruns_publish_kind_bit_follows_the_flag() {
    let m = table(&[], 40 * B, &[homes(5, &[1, 2], &[(B, 4 * B)])]);
    let rec = PublishRec {
        dir: crate::wal::DirLoc { spc: 1663, db: 5, relnumber: 90210 },
        base_gen: 6,
        kind: PublishKind::Compact,
        header: m.header,
        upsert: Vec::new(),
        retire: vec![9],
        ext: m.ext.clone(),
        installs: Vec::new(),
        renames: vec![("tmp-5-1.pgrct".into(), runrev_file_name(7))],
        manifest_crc: 7,
    };
    let mut b = Vec::new();
    rec.encode(&mut b).unwrap();
    assert_eq!(b[12 + 8], PublishKind::Compact as u8 | PUBLISH_KIND_RUNS, "0x20 over the kind");
    assert_eq!(PublishRec::decode(&b).unwrap(), rec, "the reverse's rename target classifies");
    let mut off = b.clone();
    off[12 + 8] &= !PUBLISH_KIND_RUNS;
    assert!(PublishRec::decode(&off).is_err(), "the flag without the bit");
    let plain = PublishRec { header: table(&[], 40 * B, &[]).header, ..rec.clone() };
    let mut p = Vec::new();
    plain.encode(&mut p).unwrap();
    assert_eq!(p[12 + 8] & PUBLISH_KIND_RUNS, 0);
    p[12 + 8] |= PUBLISH_KIND_RUNS;
    assert!(PublishRec::decode(&p).is_err(), "the bit without the flag");
}
