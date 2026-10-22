//! Conversion wave 2 (cvdict): a verbatim text part's dictionary build (`text_hosts::row_codes`) carves its working memory
//! (the column's copy, its row entries, the order and the sort's scratch) from arenas charged to the owner its source hands
//! it (`TextScratch::set_owner`); the table it keeps is the fingerprint cache's, not the owner's. The account's kind lane
//! peaks at the build's bytes, nothing stays on the owner after, and the codes are the same as with no owner.
use std::sync::Arc;

use gravity_hosts::dict::text_hosts::{row_codes, BuildOwner, TextScratch};
use memledger::charge::usage;
use memledger::{Acct, Kind, Owner};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{open_part_bytes, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::StreamCursor;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

const ROWS: u64 = 40_000;
const PART_ROWS: u64 = 20_000;

/// A distinct 199-byte value a row: no dictionary policy, so the writer stores the column verbatim.
fn value(i: u64) -> Vec<u8> {
    format!("{i:08}-{}", "x".repeat(190)).into_bytes()
}

fn verbatim_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_cvdict_verbatim",
        dir: "/qa/t7331".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7331,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(value(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: PART_ROWS, ..PartCutPolicy::default() },
    };
    let cands = CodecCandidates::new(ColumnPosture::default());
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, ROWS, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// Each part's row codes, every part's dictionary built now under `owner` (`ino`: the parts' inode numbers, a fresh part
/// identity a call, so no cached table serves it).
fn codes(bf: &BuiltFixture, ino: u64, owner: BuildOwner) -> Vec<Vec<u32>> {
    let binding = pgrc_am::scan::codec_binding();
    let mut out = Vec::new();
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let part = open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), ino + i as u64).expect("open part");
        let mut cur = StreamCursor::open(Arc::clone(&part), binding, 1, 0).expect("cursor");
        let mut scratch = TextScratch::new();
        scratch.set_owner(owner);
        let t = row_codes(&part, &mut cur, 1, &mut scratch, None).map_err(|e| format!("{e:?}")).expect("built");
        let r = t.ranks().expect("a verbatim part's ranks");
        out.push((0..cur.granule_count()).flat_map(|lg| r.granule_codes(lg).to_vec()).collect());
    }
    out
}

#[test]
fn a_verbatim_parts_dictionary_build_is_its_owners_while_it_runs() {
    let bf = Arc::new(verbatim_fixture());
    let plain = codes(&bf, 4_431_000, None);
    assert_eq!(plain.iter().map(Vec::len).sum::<usize>() as u64, ROWS);
    let bf2 = Arc::clone(&bf);
    std::thread::spawn(move || {
        memledger::bind_backend(3976, 9976);
        memledger::statement_begin();
        let a = Acct::backend(3976);
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let slot = memledger::charge::op_begin(st, Kind::TopK as u32, 0);
        let before = usage(slot).held;
        let charged = codes(&bf2, 4_431_100, Some((slot, Kind::TopK)));
        assert_eq!(usage(slot).held, before, "nothing left on the owner after the builds");
        let mut peak = 0;
        memledger::for_each_kind_row(|r| {
            if r.acct == a && r.kind == Kind::TopK {
                peak = r.peak_bytes;
            }
        });
        assert!(peak >= PART_ROWS * 199, "a part's copied column on its owner at the build's peak: {peak}");
        assert_eq!(charged, plain, "the same codes as with no owner");
        memledger::charge::op_end(slot);
        let _ = memledger::statement_end();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
