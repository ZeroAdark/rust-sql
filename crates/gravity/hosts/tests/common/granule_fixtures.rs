//! The pgrc fixtures `granule_source.rs` and `granule_census.rs` share (each is its own test binary).
#![allow(dead_code)]

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_driver::{BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot};
use gravity_hosts::ops::scan::pgrc::granule_source::{PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::PartExpect;
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

/// The tests' scratch site lanes (a test bound, not the ABI's: sites size their lane arrays per program).
pub const MAX_LANES: usize = 32;

pub static FLAG: AtomicU8 = AtomicU8::new(0);
pub const ROWS: u64 = 60_000;
pub const GRANULE: u64 = 8192;

pub fn part_list(bf: &BuiltFixture) -> Arc<PartList> {
    part_list_at(bf, 500)
}

/// `part_list` with the in-memory parts' inode numbers from `ino` (a fixture whose part images must not meet another's).
pub fn part_list_at(bf: &BuiltFixture, ino: u64) -> Arc<PartList> {
    let parts = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
            PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: ino + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }
        })
        .collect();
    Arc::new(PartList::new(parts))
}

pub fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

pub const REL_SORTED: u32 = 4244;

/// A sorted int8 table (`a = i`): parts of 24 576, 24 576 and 10 848 rows, granules 0..8 hold [8192·g, 8192·(g+1)).
pub fn sorted_fixture() -> BuiltFixture {
    build_fixture(int8_fixture("gravity_zones", 7303, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some(i as i64)))
}

pub const REL_TEXT: u32 = 4246;

/// The text value of row `i`: 23 words in the first part, 23 words shifted by ten in the second
/// (thirteen shared), so the two parts' dictionaries differ and the value fold crosses them.
pub fn tv(i: u64) -> Vec<u8> {
    let w = if i < 24_576 { i % 23 } else { i % 23 + 10 };
    format!("w{w:02}").into_bytes()
}

/// A dictionary-encoded text column over two parts (pgrc's real election with the dict policy the AM gives text).
pub fn text_fixture() -> BuiltFixture {
    text_fixture_of("gravity_text", 7305, 35_424, 20_000, tv)
}

/// P5-18: one part of 100,000 rows over 50,000 distinct 16-hex-digit values, so its dictionary spans 49 blocks of 1,024
/// entries and the writer wraps it (a wrapped single-extent payload is the one served at block grain; a front-coded run of
/// sequential numbers compresses too far to be wrapped, and an unwrapped payload faults by frame).
pub fn wide_text_fixture(name: &'static str, relfilenumber: u32) -> BuiltFixture {
    text_fixture_of(name, relfilenumber, 100_000, 120_000, hex_value)
}

/// A 16-hex-digit image of `i % 50,000` through a mix (splitmix64's finalizer): distinct per residue, in no byte order.
pub fn hex_value(i: u64) -> Vec<u8> {
    let mut z = (i % 50_000).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    format!("{:016x}", z ^ (z >> 31)).into_bytes()
}

pub fn text_fixture_of(name: &'static str, relfilenumber: u32, rows: u64, max_rows: u64, tv: fn(u64) -> Vec<u8>) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name,
        dir: format!("/qa/t{relfilenumber}"),
        spc: 1663,
        db: 5,
        relfilenumber: relfilenumber as u64,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Bytes(tv(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows, ..PartCutPolicy::default() },
    };
    let posture = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 100_000, exec_ok: true, sem: TextSemantics::BytesOnly }), ..ColumnPosture::default() };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, posture);
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}
