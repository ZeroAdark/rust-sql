//! F1 L6: a pooled seal (streams on other threads, chunks appended column by column) writes the serial seal's bytes.

use super::*;
use crate::dict::TextSemantics;
use crate::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy};
use crate::par::RowChunk;
use crate::seal::{CodecResolver, ColumnPool, SerialPool, VerifyResolver};
use crate::writer::SealPool;
use std::sync::Arc;

const FXID: u64 = 77;
const NCOLS: u32 = 12;

fn unbounded() -> PartCutPolicy {
    PartCutPolicy { max_rows: u64::MAX, max_bytes: u64::MAX, ..PartCutPolicy::default() }
}

fn schema() -> Vec<ColSchema> {
    (1..=NCOLS).map(|a| if a % 3 == 0 { text_col(a) } else { int8_col(a) }).collect()
}

/// Production's storage posture, dict offered to the text columns.
fn codec_cands() -> CodecCandidates {
    let storage = ColumnPosture { cold: true, ..ColumnPosture::default() };
    let dict = ColumnPosture {
        dict: Some(DictPolicy { ndv_cap: 1 << 24, exec_ok: true, sem: TextSemantics::BytesOnly }),
        ..storage
    };
    (1..=NCOLS).filter(|a| a % 3 == 0).fold(CodecCandidates::new(storage), |c, a| c.with_column(a, 0, dict))
}

fn with_row<R>(i: u64, f: impl FnOnce(&[RawDatum<'_>]) -> R) -> R {
    let imgs: Vec<Option<Vec<u8>>> = (1..=NCOLS)
        .map(|a| (a % 3 == 0 && (i + a as u64) % 11 != 0).then(|| img_4b_u(format!("v{}-{}", a, (i * a as u64) % 301).as_bytes())))
        .collect();
    let row: Vec<RawDatum<'_>> = (1..=NCOLS)
        .map(|a| match (a % 3 == 0, &imgs[a as usize - 1]) {
            (true, Some(b)) => RawDatum::Bytes(b),
            (true, None) => RawDatum::Null,
            (false, _) if (i + a as u64) % 29 == 0 => RawDatum::Null,
            (false, _) => RawDatum::Word((i as i64 * a as i64 - 900) as u64),
        })
        .collect();
    f(&row)
}

/// The threads of a scope, each taking every `threads`-th stream.
struct ThreadPool(usize);

impl ColumnPool for ThreadPool {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|s| {
            for t in 0..self.0 {
                s.spawn(move || (t..n).step_by(self.0).for_each(f));
            }
        });
    }
}

/// Streams in reverse: stitching must not depend on completion order.
struct Reversed;

impl ColumnPool for Reversed {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        (0..n).rev().for_each(f)
    }
}

fn tmp_bytes(vfs: &mut MemVfs, w: &TableWriter) -> Vec<Vec<u8>> {
    (0..w.sealed_parts().len() as u32)
        .map(|seq| vfs.read_full(&format!("{DIR}/{}", pgrc_format::dirlayout::temp_file_name(FXID, seq))).expect("tmp bytes"))
        .collect()
}

/// Rows through `append_row` (no pool) or through chunks (with `pool`), finished under the codec candidates.
fn seal(n: u64, pool: Option<Arc<dyn ColumnPool>>, chunk_rows: u32) -> (Vec<Vec<u8>>, Vec<crate::seal::SealReport>) {
    let (mut vfs, mut kit, cands) = (mem_with_dir(), Kit::new(), codec_cands());
    let mut w = open_writer_policy(schema(), stamp(FXID, 1), unbounded());
    let resolver = CodecResolver;
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    match pool {
        None => (0..n).for_each(|i| with_row(i, |r| w.append_row(r, &mut kit.ext, &mut env).expect("append"))),
        Some(pool) => {
            let resolver: Arc<dyn VerifyResolver + Send + Sync> = Arc::new(CodecResolver);
            w.set_pool(SealPool { pool, sources: vec![Arc::new(codec_cands())], resolver });
            let mut chunk = RowChunk::new(NCOLS as usize, chunk_rows);
            for i in 0..n {
                with_row(i, |r| chunk.push_row(r).expect("capture"));
                if chunk.rows() == chunk_rows || i + 1 == n {
                    w.append_chunk(&chunk).expect("chunk");
                    chunk = RowChunk::new(NCOLS as usize, chunk_rows);
                }
            }
        }
    }
    w.finish(&mut env).expect("finish");
    let reports = w.seal_reports().to_vec();
    (tmp_bytes(&mut vfs, &w), reports)
}

#[test]
fn a_pooled_seal_writes_the_serial_bytes_under_any_stream_order() {
    let n = 40_000;
    let (serial, serial_rep) = seal(n, None, 0);
    assert_eq!(serial.len(), 1, "an unbounded policy seals one part");
    for (name, pool) in [
        ("serial pool", Arc::new(SerialPool) as Arc<dyn ColumnPool>),
        ("reversed", Arc::new(Reversed)),
        ("4 threads", Arc::new(ThreadPool(4))),
    ] {
        for chunk_rows in [1_000, 8_192, 65_536] {
            let (pooled, rep) = seal(n, Some(Arc::clone(&pool)), chunk_rows);
            assert!(pooled == serial, "{name}, chunks of {chunk_rows}: the part's bytes moved");
            assert_eq!(rep[0].col_hashes, serial_rep[0].col_hashes, "{name}");
            assert_eq!(rep[0].granules_verified, serial_rep[0].granules_verified, "{name}");
            assert_eq!(rep[0].nonnull_crosschecks, serial_rep[0].nonnull_crosschecks, "{name}");
            assert_eq!(rep[0].elections.len(), serial_rep[0].elections.len(), "{name}");
        }
    }
}

#[test]
fn a_reserved_column_takes_a_chunk_without_growing() {
    // C1-REG: `append_chunk` reserves every column from the chunk's counts before the pool appends it, so a worker never
    // grows (reallocates) a buffer its caller owns and frees.
    use crate::ingest::ColBuffer;
    let schema = [int8_col(1), text_col(2), fixed16_col(3), bool_col(4), text_col(5)];
    let texts: Vec<Vec<u8>> = (0..3000u64)
        .map(|i| match i % 4 {
            0 => img_4b_u(format!("four-{i}").as_bytes()),
            1 => img_short(format!("s{i}").as_bytes()),
            2 => img_short(b""),
            _ => img_4b_u(&vec![b'x'; (i % 300) as usize]),
        })
        .collect();
    let fixed: Vec<[u8; 16]> = (0..3000u128).map(|i| (i * 0x9E37_79B9).to_le_bytes()).collect();
    let mut cols: Vec<ColBuffer> = schema.iter().map(|s| ColBuffer::new(*s)).collect();
    for round in 0..3usize {
        let mut chunk = RowChunk::new(schema.len(), 1000);
        for i in round * 1000..(round + 1) * 1000 {
            let row = [
                if i % 7 == 0 { RawDatum::Null } else { RawDatum::Word(i as u64) },
                if i % 5 == 0 { RawDatum::Null } else { RawDatum::Bytes(&texts[i]) },
                RawDatum::Bytes(&fixed[i]),
                RawDatum::Word((i % 2) as u64),
                RawDatum::Bytes(&texts[2]),
            ];
            chunk.push_row(&row).expect("capture");
        }
        for (c, col) in cols.iter_mut().enumerate() {
            col.reserve(u64::from(chunk.rows()), chunk.column_heap_bound(c));
            let before = col.capacities();
            chunk.append_column(c, col, &mut NoExternalDetoast, &mut Vec::new()).expect("append");
            assert_eq!(col.capacities(), before, "round {round}, column {c}: the append grew a reserved buffer");
        }
    }
}

#[test]
fn a_chunk_append_refuses_a_bounded_policy() {
    let mut w = open_writer_policy(schema(), stamp(FXID, 1), PartCutPolicy::default());
    let mut chunk = RowChunk::new(NCOLS as usize, 16);
    with_row(0, |r| chunk.push_row(r).expect("capture"));
    assert!(matches!(w.append_chunk(&chunk), Err(crate::WriteError::Contract { .. })));
}

#[test]
fn a_chunk_narrower_than_the_schema_is_refused() {
    let mut w = open_writer_policy(schema(), stamp(FXID, 1), unbounded());
    let chunk = RowChunk::new(NCOLS as usize - 1, 16);
    assert!(matches!(w.append_chunk(&chunk), Err(crate::WriteError::Contract { detail: "row width mismatch" })));
}
