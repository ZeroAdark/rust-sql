//! General parquet reader for the COPY ingest path (increment 1: the
//! single-threaded correctness reader with the staged kernel set).
//!
//! Owned per the build-vs-borrow ruling: a hand-rolled thrift-compact footer
//! SKIP-PARSER (schema + per-chunk offsets/codec/counts only; statistics and
//! page indexes never decoded), the RLE/bit-packed hybrid kernels, PLAIN
//! decoders, and per-page kernel dispatch. Borrowed: snap / lz4_flex / zstd
//! codecs, simdutf8 validation.
//!
//! Decodes: data pages v1 and v2, dictionary pages; PLAIN (every physical
//! type), PLAIN_DICTIONARY/RLE_DICTIONARY, RLE booleans, DELTA_BINARY_PACKED,
//! DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY, BYTE_STREAM_SPLIT; levels at
//! any depth (RLE and the deprecated BIT_PACKED); snappy, zstd, lz4 (raw and
//! Hadoop-framed), gzip, brotli, uncompressed; the page crc. Refused by
//! name: LZO, encryption, external chunks, unknown encodings — never a
//! wrong answer.
//!
//! The batch API deliberately separates page decode from batch fill so a
//! later increment can consume dictionary + index streams without
//! materializing values (the columnar-store transcode seam).

mod codec;
mod column;
mod delta;
mod fast;
pub mod held;
mod levels;
mod meta;
mod page;
mod plain;
mod rle;
mod thrift;
#[cfg(test)]
mod tests;

use std::fs::File;

use types_error::{PgError, PgResult, ERRCODE_BAD_COPY_FILE_FORMAT, ERRCODE_OUT_OF_MEMORY};

pub(crate) fn oom_in(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("out of memory {what}")).with_sqlstate(ERRCODE_OUT_OF_MEMORY)) // alloc: Cold
}

pub use column::{batches_held, BatchData, ColumnBatch, ColumnCursor, Levels};
pub use fast::{FastCol, FastRowGroup, FastRun, FastVals};
pub use meta::{
    Ancestor, ColumnSchema, FileMeta, FileSpan, Logical, Phys, Repetition, TimeUnit,
};

/// Parse a bare FileMetaData footer (the bytes between the leading magic and
/// the trailing length+magic) — footer-inspection tooling.
pub fn parse_footer_bytes(buf: &[u8]) -> PgResult<FileMeta> {
    meta::parse_file_meta(buf)
}

const MAGIC: &[u8; 4] = b"PAR1";
const MAGIC_ENCRYPTED: &[u8; 4] = b"PARE";

/// Hard cap on footer size we will materialize (a corrupt length field must
/// not drive a giant allocation). Generous: real footers of 100k-chunk files
/// run single-digit MB.
const MAX_FOOTER: u64 = 256 << 20;

#[track_caller]
#[cold]
#[inline(never)]
fn not_parquet(path: &str, what: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("\"{path}\" is not a parquet file: {what}")) // alloc: Cold
            .with_sqlstate(ERRCODE_BAD_COPY_FILE_FORMAT),
    )
}

#[track_caller]
#[cold]
#[inline(never)]
fn io_error(path: &str, op: &str, e: std::io::Error) -> Box<PgError> {
    Box::new(PgError::error(format!("could not {op} \"{path}\": {e}"))) // alloc: Cold
}

fn read_exact_at(file: &File, path: &str, buf: &mut [u8], off: u64) -> PgResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, off).map_err(|e| io_error(path, "read", e))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, buf, off);
        Err(Box::new(PgError::error(format!( // alloc: Cold
            "parquet read is not supported on this platform (\"{path}\")"
        ))))
    }
}

pub struct FileReader {
    file: File,
    path: String,
    file_len: u64,
    /// Arc so a parallel driver can share the parsed footer across worker
    /// threads without reparsing (FileMeta is plain data: Send + Sync).
    pub meta: std::sync::Arc<FileMeta>,
}

impl FileReader {
    /// Open + footer skip-parse. Everything is validated against the file
    /// length before any chunk read. Uncharged: [`FileReader::open_on`] with no owner.
    pub fn open(path: &str) -> PgResult<FileReader> {
        FileReader::open_on(path, None)
    }

    /// memsig E-pub t5 (memlead 2026-10-15T21:30Z): [`FileReader::open`] with the footer read charged to `owner` (the
    /// loading statement's, the one its row groups take) while it lives, and the parsed footer ([`FileMeta`]) holding its
    /// heap bytes on the same owner until its last `Arc` drops (`FileMeta::heap_bytes`); `None`: uncharged.
    pub fn open_on(path: &str, owner: Option<memledger::Owner>) -> PgResult<FileReader> {
        let file = File::open(path).map_err(|e| io_error(path, "open", e))?;
        let md = file.metadata().map_err(|e| io_error(path, "stat", e))?;
        if md.is_dir() {
            return Err(Box::new(
                PgError::error(format!("\"{path}\" is a directory")) // alloc: Cold
                    .with_sqlstate(types_error::ERRCODE_WRONG_OBJECT_TYPE),
            ));
        }
        let file_len = md.len();
        // Leading magic + footer length + trailing magic.
        if file_len < 12 {
            return Err(not_parquet(path, "file too small"));
        }
        let mut head = [0u8; 4];
        read_exact_at(&file, path, &mut head, 0)?;
        let mut tail = [0u8; 8];
        read_exact_at(&file, path, &mut tail, file_len - 8)?;
        let tail_magic: &[u8; 4] = tail[4..8].try_into().expect("4-byte slice");
        if tail_magic == MAGIC_ENCRYPTED {
            return Err(meta::unsupported("encrypted parquet file".to_string())); // alloc: Cold
        }
        if &head != MAGIC || tail_magic != MAGIC {
            return Err(not_parquet(path, "magic bytes missing"));
        }
        let footer_len = u64::from(u32::from_le_bytes(
            tail[0..4].try_into().expect("4-byte slice"),
        ));
        if footer_len > MAX_FOOTER || footer_len + 12 > file_len {
            return Err(not_parquet(path, "footer length out of range"));
        }
        // the footer bytes: charged while they live (until the parse below is done)
        let mut footer: memledger::ChargedVec<u8> = memledger::ChargedVec::new(owner, memledger::Kind::Region);
        footer.resize(footer_len as usize, 0u8).map_err(|_| oom_in("reading parquet footer"))?;
        read_exact_at(&file, path, footer.as_mut_slice(), file_len - 8 - footer_len)?;
        let meta = meta::parse_file_meta_on(footer.as_slice(), owner)?;
        drop(footer);

        // Offset sanity for every chunk, before anything is read. Zero-value
        // chunks (zero-row groups) are never read; some writers emit them
        // with a zero data_page_offset.
        for rg in &meta.row_groups {
            for ch in &rg.chunks {
                if ch.num_values == 0 {
                    continue;
                }
                let start = ch.start_offset();
                let size = ch.total_compressed_size;
                if start < 4
                    || size < 0
                    || (start as u64).saturating_add(size as u64) > file_len
                {
                    return Err(not_parquet(path, "column chunk offsets out of range"));
                }
            }
        }
        Ok(FileReader {
            file,
            path: path.to_string(),
            file_len,
            meta: std::sync::Arc::new(meta),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    pub fn meta_arc(&self) -> std::sync::Arc<FileMeta> {
        std::sync::Arc::clone(&self.meta)
    }

    /// Duplicate the underlying file handle (positioned reads only) for a
    /// parallel driver's worker threads.
    pub fn try_clone_file(&self) -> PgResult<File> {
        self.file.try_clone().map_err(|e| io_error(&self.path, "dup", e))
    }

    /// Open one row group, building cursors for the requested columns.
    /// `validate_utf8[i]` pairs with `columns[i]` (schema ordinals).
    pub fn row_group(
        &mut self,
        rg_idx: usize,
        columns: &[usize],
        validate_utf8: &[bool],
    ) -> PgResult<RowGroupReader> {
        RowGroupReader::open(&self.file, &self.path, &self.meta, rg_idx, columns, validate_utf8)
    }

    /// CONVERT wave 3 (cvwrite job 3): [`FileReader::row_group`] with the row group's chunks, pages, levels and
    /// dictionaries charged on `owner` ([`RowGroupReader::open_on`]; `None`: uncharged, as `row_group`).
    pub fn row_group_on(
        &mut self,
        rg_idx: usize,
        columns: &[usize],
        validate_utf8: &[bool],
        owner: Option<memledger::Owner>,
    ) -> PgResult<RowGroupReader> {
        RowGroupReader::open_on(&self.file, &self.path, &self.meta, rg_idx, columns, validate_utf8, owner)
    }
}

/// memsig E-pub t5: a row group's loaded chunks in `columns` order, each with its chunk meta, in a list charged to the
/// reader's chunk charge's owner (its bytes while it lives).
pub(crate) type LoadedChunks<'m> = memledger::ChargedVec<(&'m meta::ChunkMeta, column::ChunkBytes)>;

/// CONVERT wave 3: the heap bytes a row group's loaded chunks hold: every owned chunk's block plus the one shared
/// whole-row-group block (counted once).
pub(crate) fn chunks_held<'a>(chunks: impl Iterator<Item = &'a column::ChunkBytes>) -> u64 {
    let (mut owned, mut shared) = (0u64, 0u64);
    for c in chunks {
        let (o, s) = c.held();
        owned += o;
        shared = shared.max(s.unwrap_or(0));
    }
    owned + shared
}

/// Compressed bytes this row group's requested chunks occupy (the
/// parallel driver's memory-budget input).
pub fn rg_compressed_bytes(meta: &FileMeta, rg_idx: usize, columns: &[usize]) -> u64 {
    let Some(rg) = meta.row_groups.get(rg_idx) else { return 0 };
    columns
        .iter()
        .filter_map(|&col| rg.chunks.iter().find(|c| c.column == col))
        .map(|c| c.total_compressed_size.max(0) as u64)
        .sum()
}

/// Read the requested column chunks of one row group: when the chunks are
/// DENSE in the file — the layout every columnar writer produces — the
/// whole row-group span is read in ONE sequential request and demuxed in
/// memory (per-chunk preads are wrong at the small median chunk sizes of
/// wide tables); scattered layouts fall back to per-chunk reads. Returns
/// the chunks in `columns` order plus the compressed bytes read.
#[allow(clippy::type_complexity)]
pub(crate) fn load_rg_chunks<'m>(
    file: &File,
    path: &str,
    meta: &'m FileMeta,
    rg_idx: usize,
    columns: &[usize],
    // CONVERT wave 3: the reader's chunk charge, brought to each block as it is reserved (before its read)
    charge: &mut held::HeldCharge,
) -> PgResult<(LoadedChunks<'m>, u64)> {
    let rg = meta
        .row_groups
        .get(rg_idx)
        .ok_or_else(|| not_parquet(path, "row group index out of range"))?;
    // Resolve the chunk set once; compute the span for the dense test.
    // memsig E-pub t5: the chunk set, on the reader's charge owner while it lives
    let mut chunks: memledger::ChargedVec<&meta::ChunkMeta> = memledger::ChargedVec::new(charge.owner(), memledger::Kind::Region);
    chunks.try_reserve(columns.len()).map_err(|_| oom_in("reading row group"))?;
    let (mut lo, mut hi, mut sum) = (u64::MAX, 0u64, 0u64);
    for &col in columns.iter() {
        let ch = rg
            .chunks
            .iter()
            .find(|c| c.column == col)
            .ok_or_else(|| not_parquet(path, "column chunk missing from row group"))?;
        if ch.num_values > 0 {
            let start = ch.start_offset() as u64;
            let end = start + ch.total_compressed_size as u64;
            lo = lo.min(start);
            hi = hi.max(end);
            sum += ch.total_compressed_size as u64;
        }
        chunks.push(ch).map_err(|_| oom_in("reading row group"))?;
    }
    // Dense: the span wastes at most 25% + a page of slack over the sum.
    let span = hi.saturating_sub(if lo == u64::MAX { 0 } else { lo });
    let dense = sum > 0 && span <= sum + sum / 4 + 8192;
    let shared: Option<std::sync::Arc<Vec<u8>>> = if dense {
        let mut buf = Vec::new();
        buf.try_reserve(span as usize + codec::PAD) // alloc: Exec charged charge (the reader's)
            .map_err(|_| Box::new(PgError::error("out of memory reading row group")))?; // alloc: Cold
        // cvwrite job 3: charged as reserved, before the zero fill (the w3 view caught 3 decoders' spans, 96 MiB,
        // between the reserve and a sync placed after the fill)
        charge.sync(buf.capacity() as u64);
        // the span and one pad for the whole shared buffer (initialized capacity), zeroed at once, the span read over it
        buf.resize(span as usize + codec::PAD, 0); // alloc: Exec charged chunk_charge (the reader's chunks_held)
        read_exact_at(file, path, &mut buf[..span as usize], lo)?;
        buf.truncate(span as usize);
        Some(std::sync::Arc::new(buf))
    } else {
        None
    };

    let mut compressed_bytes: u64 = 0;
    let mut held: u64 = shared.as_ref().map_or(0, |b| b.capacity() as u64);
    let mut out: LoadedChunks<'m> = memledger::ChargedVec::new(charge.owner(), memledger::Kind::Region);
    out.try_reserve(chunks.len()).map_err(|_| oom_in("reading row group"))?;
    for &ch in chunks.iter() {
        let bytes = if ch.num_values == 0 {
            column::ChunkBytes::Owned(Vec::new())
        } else if let Some(buf) = &shared {
            let start = (ch.start_offset() as u64 - lo) as usize;
            compressed_bytes += ch.total_compressed_size as u64;
            column::ChunkBytes::Shared {
                buf: std::sync::Arc::clone(buf),
                start,
                len: ch.total_compressed_size as usize,
            }
        } else {
            let start = ch.start_offset() as u64;
            let size = ch.total_compressed_size as usize;
            let mut buf = Vec::new();
            buf.try_reserve(size + codec::PAD).map_err(|_| Box::new(PgError::error("out of memory reading column chunk")))?; // alloc: Exec charged charge (the reader's)
            // cvwrite job 3: charged as reserved, before the zero fill
            held += buf.capacity() as u64;
            charge.sync(held);
            buf.resize(size, 0); // alloc: Exec charged chunk_charge (the reader's chunks_held)
            read_exact_at(file, path, &mut buf, start)?;
            compressed_bytes += size as u64;
            column::ChunkBytes::Owned(buf)
        };
        out.push((ch, bytes)).map_err(|_| oom_in("reading row group"))?;
    }
    Ok((out, compressed_bytes))
}

/// Cursors over one row group's requested columns; all columns advance in
/// lock-step through `read_batch` calls made by the driver.
pub struct RowGroupReader {
    /// CONVERT wave 3 (cvwrite job 2): the loading statement's charge on the row group's column chunks
    /// ([`chunks_held`]). Declared first: it drops before the cursors free the chunks.
    chunk_charge: held::HeldCharge,
    cursors: Vec<ColumnCursor>,
    num_rows: u64,
    rows_read: u64,
    /// Total compressed chunk bytes read for this group (progress metering).
    pub compressed_bytes: u64,
}

impl RowGroupReader {
    /// Shareable open (parallel drivers pass a duplicated handle + the Arc'd
    /// meta). When the requested chunks are DENSE in the file — the layout
    /// every columnar writer produces — the whole row-group span is read in
    /// ONE sequential request and demuxed in memory (per-chunk preads are
    /// wrong at the small median chunk sizes of wide tables); scattered
    /// layouts fall back to per-chunk reads.
    pub fn open(
        file: &File,
        path: &str,
        meta: &FileMeta,
        rg_idx: usize,
        columns: &[usize],
        validate_utf8: &[bool],
    ) -> PgResult<RowGroupReader> {
        RowGroupReader::open_on(file, path, meta, rg_idx, columns, validate_utf8, None)
    }

    /// CONVERT wave 3 (cvwrite job 2): [`RowGroupReader::open`] with the decoder's buffers (the row group's chunks,
    /// every cursor's page, levels and dictionary) charged to `owner` (the loading statement's, read once on the COPY
    /// leader), kind `Region`; `None`: uncharged.
    pub fn open_on(
        file: &File,
        path: &str,
        meta: &FileMeta,
        rg_idx: usize,
        columns: &[usize],
        validate_utf8: &[bool],
        owner: Option<memledger::Owner>,
    ) -> PgResult<RowGroupReader> {
        let mut chunk_charge = held::HeldCharge::new(owner);
        let (mut loaded, compressed_bytes) = load_rg_chunks(file, path, meta, rg_idx, columns, &mut chunk_charge)?;
        let mut cursors = Vec::new();
        cursors
            .try_reserve(columns.len())
            .map_err(|_| Box::new(PgError::error("out of memory opening row group")))?; // alloc: Cold
        // memsig E-pub t5: the chunks move out of their charged list into the cursors (its block stays its until it drops)
        loaded.with_vec(|loaded| -> PgResult<()> {
            for ((ch, bytes), &vutf8) in loaded.drain(..).zip(validate_utf8.iter()) {
                let schema = &meta.columns[ch.column];
                cursors.push(ColumnCursor::new(
                    bytes,
                    ch.codec,
                    schema.phys,
                    schema.max_def,
                    schema.max_rep,
                    vutf8,
                    schema.name.clone(),
                    ch.num_values,
                    owner,
                )?);
            }
            Ok(())
        })?;
        drop(loaded);
        // the cursors padded their owned chunks
        chunk_charge.sync(chunks_held(cursors.iter().map(ColumnCursor::chunk)));
        let rg = &meta.row_groups[rg_idx];
        Ok(RowGroupReader {
            chunk_charge,
            cursors,
            num_rows: rg.num_rows as u64,
            rows_read: 0,
            compressed_bytes,
        })
    }

    pub fn num_rows(&self) -> u64 {
        self.num_rows
    }

    pub fn rows_remaining(&self) -> u64 {
        self.num_rows - self.rows_read
    }

    /// Per-slot levels of requested column `i` after `read_batches` (empty
    /// for flat leaves under one OPTIONAL at most; see [`Levels`]).
    pub fn levels(&self, i: usize) -> &Levels {
        self.cursors[i].levels()
    }

    /// Fill one batch per requested column, `n` rows each (the caller keeps
    /// `n <= rows_remaining()`); for repeated leaves `n` counts records and
    /// the batch holds one slot per level entry. Batches are positionally
    /// parallel only across flat leaves.
    pub fn read_batches(&mut self, out: &mut [ColumnBatch], n: usize) -> PgResult<()> {
        debug_assert_eq!(out.len(), self.cursors.len());
        for (cur, batch) in self.cursors.iter_mut().zip(out.iter_mut()) {
            cur.read_batch(batch, n)?;
        }
        self.rows_read += n as u64;
        Ok(())
    }
}
