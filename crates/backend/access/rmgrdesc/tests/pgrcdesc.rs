use pgrc_format::wal::{
    encode_dir, BulkRowsRec, DirLoc, FileRefRec, FileWriteRec, InstallRec, ReclaimRec, Retire, RowRec, RowidExtentsRec, RowidReserveRec,
    TupleImage, NO_MASKS, XLOG_PGRC_ROW_DELETE, XLOG_PGRC_ROW_UPDATE,
};
use rmgrdesc::pgrcdesc::{pgrc_desc, pgrc_identify};
use stringinfo::StringInfo;
use xlogreader_seams::{DecodedXLogRecord, XLogReaderState};

const D: DirLoc = DirLoc { spc: 1663, db: 5, relnumber: 16384 };
const SPECIAL_REL_UPDATE: u8 = 0x01;

fn run(info: u8, data: &[u8]) -> String {
    let mut rec = DecodedXLogRecord::default();
    rec.xl_info = info;
    rec.main_data = data.as_ptr();
    rec.main_data_len = data.len() as u32;
    let record = XLogReaderState { record: Some(rec), ..Default::default() };
    let ctx = Box::leak(Box::new(mcx::MemoryContext::new("test")));
    let mut buf = StringInfo::new_in(ctx.mcx()).unwrap();
    pgrc_desc(&mut buf, &record).unwrap();
    String::from_utf8(buf.as_bytes().to_vec()).unwrap()
}

#[test]
fn directory_file_and_namespace_records() {
    let mut b = Vec::new();
    encode_dir(D, &mut b);
    assert_eq!(run(SPECIAL_REL_UPDATE, &b), "dir 1663/5/16384");

    let mut b = Vec::new();
    FileWriteRec { dir: D, name: "tmp-744-0.pgrct", off: 262144, bytes: &[7; 100] }.encode(&mut b).unwrap();
    assert_eq!(run(0x90 | SPECIAL_REL_UPDATE, &b), "dir 1663/5/16384 tmp-744-0.pgrct off 262144 len 100");

    let mut b = Vec::new();
    let r = FileRefRec { dir: D, name: "tmp-744-0.pgrct", len: 17_234_567, crc: 0x0BAD_F00D, masks: [(0, 64), (17_234_471, 96)] };
    r.encode(&mut b).unwrap();
    assert_eq!(
        run(0xA0 | SPECIAL_REL_UPDATE, &b),
        "dir 1663/5/16384 tmp-744-0.pgrct len 17234567 crc 0BADF00D masks 2 [0+64] [17234471+96]"
    );
    let mut b = Vec::new();
    FileRefRec { dir: D, name: "bankstats-7.pgrcbs", len: 123_320_008, crc: 1, masks: NO_MASKS }.encode(&mut b).unwrap();
    assert_eq!(run(0xA0, &b), "dir 1663/5/16384 bankstats-7.pgrcbs len 123320008 crc 00000001 masks 0");

    let mut b = Vec::new();
    InstallRec { dir: D, renames: vec![("tmp-744-1.pgrct".into(), "sections-3.pgrcx".into())] }.encode(&mut b).unwrap();
    assert_eq!(run(0x30, &b), "dir 1663/5/16384 renames 1; tmp-744-1.pgrct -> sections-3.pgrcx");

    let mut b = Vec::new();
    ReclaimRec { dir: D, xid: 744, retire_below: Some(Retire::Conflict(6)), names: vec!["tmp-744-0.pgrct".into()] }.encode(&mut b).unwrap();
    assert_eq!(run(0x40, &b), "dir 1663/5/16384 xid 744 retire_below 6 names 1");

    let mut b = Vec::new();
    ReclaimRec { dir: D, xid: 745, retire_below: Some(Retire::Deferred(7)), names: vec![] }.encode(&mut b).unwrap();
    assert_eq!(run(0x40, &b), "dir 1663/5/16384 xid 745 retire_below 7 deferred names 0");

    let mut b = Vec::new();
    RowidReserveRec { dir: D, high: 298_275 }.encode(&mut b);
    assert_eq!(run(0xB0, &b), "dir 1663/5/16384 high 298275");
    assert_eq!(run(0xD0, &b), "dir 1663/5/16384 high 298275", "ROWID_SET has ROWID_RESERVE's layout");

    let e = |part_no, idx, base| pgrc_format::rowid::SegExtent { part_no, idx, base };
    let mut b = Vec::new();
    RowidExtentsRec { dir: D, table: vec![e(4, 0, 291), e(4, 1, 291 + 297_984), e(9, 0, 291 + 2 * 297_984)] }.encode(&mut b).unwrap();
    assert_eq!(run(0xC0, &b), "dir 1663/5/16384 extents 3 segments 2");
}

#[test]
fn logical_records() {
    let img = |data| TupleImage { infomask2: 3, infomask: 0, hoff: 24, data };
    let mut b = Vec::new();
    RowRec { parent: D, old: Some(img(&[0; 40])), new: None }.encode(XLOG_PGRC_ROW_DELETE, &mut b).unwrap();
    assert_eq!(run(0x60, &b), "parent 1663/5/16384 old 40");

    let mut b = Vec::new();
    RowRec { parent: D, old: None, new: Some(img(&[0; 12])) }.encode(XLOG_PGRC_ROW_UPDATE, &mut b).unwrap();
    assert_eq!(run(0x70, &b), "parent 1663/5/16384 old none new 12");

    let (mut b, mut tuples) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        img(&[0; 11]).encode_into(&mut tuples).unwrap();
    }
    BulkRowsRec { parent: D, ntuples: 3, tuples: &tuples }.encode(&mut b).unwrap();
    assert_eq!(run(0x80, &b), "parent 1663/5/16384 ntuples 3 bytes 60");
}

#[test]
fn identify_ignores_the_flag_bits() {
    let want = [
        (0x00, "DIR_CREATE"),
        (0x10, "FILE_WRITE"),
        (0x20, "PUBLISH"),
        (0x30, "INSTALL"),
        (0x40, "RECLAIM"),
        (0x50, "TRUNCATE"),
        (0x60, "ROW_DELETE"),
        (0x70, "ROW_UPDATE"),
        (0x80, "BULK_ROWS"),
        (0x90, "FILE_CREATE"),
        (0xA0, "FILE_REF"),
        (0xB0, "ROWID_RESERVE"),
        (0xC0, "ROWID_EXTENTS"),
        (0xD0, "ROWID_SET"),
    ];
    for (info, name) in want {
        assert_eq!(pgrc_identify(info | SPECIAL_REL_UPDATE), Some(name));
    }
    for reserved in [0xE0, 0xF0] {
        assert_eq!(pgrc_identify(reserved), None);
    }
}
