//! One compaction job's rewrite and publish (K1 L3, design §4.9). The job's inputs are parts of `gen`; every row
//! their DVs mark is dropped (K1 choice 2: every mark is below compile's horizon), every other row is written, in
//! the job's order, into one output part whose `RowIdentMap` carries the rows' identities forward. One `PUBLISH` of
//! kind `Compact` retires the inputs and installs the output; a job with no survivor retires only.
//!
//! The output is built and sealed one column at a time (K1-MEM, `SealWriter::seal_columns`): a column pass decodes
//! that column of the input granules the job's order reaches, appends it in the order, and the seal writes its streams
//! through; no pass holds another column, nor the part. A sorted merge decodes its key columns first, orders the
//! survivors by `(sort key, RowIdent)` with the seal's comparator and feeds them presorted, so the seal only verifies
//! the order and attests it (K1 choice 6). An output whose values need a grain finer than the default takes the row
//! feed instead (`SealWriter::append`: the part's columns, and a sorted merge's decoded granules, held).

use std::sync::Arc;

use pgrc_compact::order::{concat_order, sorted_order, Input};
use pgrc_compact::{ident, Job, JobKind, RowMapView};
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_format::class::ColSchema;
use pgrc_format::manifest::PartRecord;
use pgrc_read::{Residency, StreamCursor};
use pgrc_write::ingest::{ColBuffer, NoExternalDetoast, RawDatum};
use pgrc_write::par::RowChunk;
use pgrc_write::seal::cmp::{decl_slots, path_values};
use pgrc_write::seal::ColumnPool;
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use types_error::{PgError, PgResult};
use types_rel::Relation;

use crate::catalog::{Generation, RowMap};
use crate::ingest::SealWriter;
use crate::scan::{codec_binding, col_cell, decode_column_granule, decode_granule, read_residency, ColBuf, PartCursors};

/// What a job did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactOut {
    /// The output's part number; `None` for a pure retire.
    pub part_no: Option<u32>,
    pub rows: u64,
    pub dropped: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
}

pub(crate) struct In {
    pub(crate) idx: usize,
    pub(crate) rec: PartRecord,
    pub(crate) grain: u32,
    pub(crate) map: Arc<RowMap>,
    /// IX0: the input's row-ID ranges; for a part written before row IDs, fresh ones ([`inputs`]).
    pub(crate) ids: pgrc_format::manifest::IdRanges,
    /// Bit `o` = ordinal `o` is DV-marked.
    pub(crate) dead: Vec<u64>,
    /// FASTADD: which of the table's columns the input holds (`None` = every one): an absent one is written as its
    /// fill (the compaction materializes it).
    pub(crate) present: Option<Arc<[bool]>>,
}

fn contract(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrcolumnar: compaction {what}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))
}

/// Resolve `job`'s inputs in `gen`: records, maps and DV marks as ordinal bitsets.
/// IX0: an input written before row IDs (no `EXT_RANGES` entry) gets fresh IDs here, `align_up(rows)` from the
/// table's counter, numbered in ordinal order (an implicit map): its rows never had a valid ctid and nothing indexes
/// them, so the output homes them as any other input's, and an old table gains IDs as it compacts. A job that fails
/// afterwards burns them (the counter is not transactional).
/// IX0 L4: a renumbering job (`renumber`) reserves nothing (the counter may be at its refusal mark): its output's IDs
/// come from its base, so an input without IDs is read through a stand-in map its IDs never leave.
/// SORTRUNS-E0 L1: a member of a sorted run that homes its IDs has no `EXT_RANGES` entry but its rows have IDs (the
/// run's), never fresh ones: the sole live member of its run brings the run's ranges (the job's publish empties and
/// drops the run); a member of a run with others is refused ([`member_ids`]: the run pass merges and rewrites those,
/// L3). L2: a run job (`whole_runs`, `crate::runmerge`) takes every live member of each run it merges, so a member
/// brings no ranges of its own and the job brings its run's once.
pub(crate) fn inputs(rel: &Relation<'_>, gen: &Generation, dir: &str, job: &Job, presences: &mut crate::fastadd::Presences<'_>, renumber: bool, whole_runs: bool) -> PgResult<Vec<In>> {
    job.inputs
        .iter()
        .map(|&no| {
            let idx = gen.manifest.parts.iter().position(|p| p.part_no == no).ok_or_else(|| contract(&format!("input part {no} is not in generation {}", gen.gen)))?;
            let rec = gen.manifest.parts[idx];
            let grain = if rec.granule_rows == 0 { GRANULE_ROWS } else { rec.granule_rows };
            let mut dead = crate::alloc::try_filled((rec.rows as usize).div_ceil(64), 0u64, "compact")?;
            if let Some(d) = &gen.parts[idx].deletes {
                for g in 0..rec.granule_count {
                    let mut mask = [0u64; pgrc_scan::DV_GRANULE_WORDS];
                    d.union_into(g, &mut mask);
                    for r in (0..grain).filter(|&r| mask[r as usize / 64] >> (r % 64) & 1 == 1) {
                        let o = u64::from(g) * u64::from(grain) + u64::from(r);
                        if o < rec.rows {
                            dead[o as usize / 64] |= 1 << (o % 64);
                        }
                    }
                }
            }
            let part_fp = gen.parts[idx].expect.schema_fingerprint.unwrap_or(gen.schema_fp);
            // D2189: a stamp a column dropped with no ALTER publish left (`schema::book_dropped_stamp`)
            if part_fp != presences.reader_fp() {
                crate::schema::book_dropped_stamp(rel, part_fp)?;
            }
            let present = presences.of(part_fp)?;
            let (map, ids) = match job_input_ids(gen, idx, renumber, whole_runs)? {
                InputIds::Own(ids) | InputIds::Run(ids) => (gen.row_map(idx, dir)?, ids),
                InputIds::StandIn => (Arc::new(RowMap::Implicit(pgrc_format::rowid::ROWID_FIRST)), Vec::new()),
                InputIds::Fresh => {
                    let r = crate::rowid_alloc::reserve(rel, rec.rows)?;
                    (Arc::new(RowMap::Implicit(r.lo)), vec![(r.lo, r.hi)])
                }
            };
            Ok(In { idx, rec, grain, map, ids, dead, present })
        })
        .collect()
}

/// Where a job input's row IDs come from ([`input_ids`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputIds {
    /// Its `EXT_RANGES` entry (read through its map).
    Own(pgrc_format::manifest::IdRanges),
    /// SORTRUNS-E0 L1: the ranges of the sorted run it is the sole live member of (read through its map).
    Run(pgrc_format::manifest::IdRanges),
    /// A renumbering job's stand-in map for a part written before row IDs.
    StandIn,
    /// Fresh IDs for a part written before row IDs.
    Fresh,
}

/// Where input `idx` of `gen`'s row IDs come from (IX0: a part written before row IDs gets fresh ones, or a
/// renumbering job's stand-in). SORTRUNS-E0 L1: a member of a sorted run that homes its IDs has no `EXT_RANGES` entry
/// but its rows have IDs, never fresh ones: the sole live member of its run brings the run's ranges (all its rows'; the
/// job's publish empties and drops the run); a member of a run with others is refused (no L0 job takes one,
/// [`Generation::pinned_member`]; one naming it is a planning bug, refused before any ID is reserved).
pub fn input_ids(gen: &Generation, idx: usize, renumber: bool) -> PgResult<InputIds> {
    let rec = &gen.manifest.parts[idx];
    Ok(match (gen.ids_of(idx), gen.run_member(idx)) {
        ([], Some((r, slot))) => {
            let run = &gen.runs()[r].run;
            if run.live().count() != 1 {
                return Err(contract(&format!("input part {} is member {slot} of sorted run {} of generation {}, which has other members: the run pass merges them", rec.part_no, run.run_id, gen.gen)));
            }
            InputIds::Run(run.ranges.clone())
        }
        ([], None) if rec.rows > 0 && renumber => InputIds::StandIn,
        ([], None) if rec.rows > 0 => InputIds::Fresh,
        (ids, _) => InputIds::Own(crate::alloc::try_to_vec(ids, "compact")?),
    })
}

/// SORTRUNS-E0 L2: where a job's input `idx` of `gen` gets its row IDs. Without `whole_runs` (K1's jobs: today's call
/// site) exactly [`input_ids`]. A run job (`whole_runs`, `crate::runmerge`) takes every live member of each run it
/// merges, so a member of a run that homes its IDs brings none of its own (`Run` of no ranges): the job brings its
/// run's ranges once.
pub fn job_input_ids(gen: &Generation, idx: usize, renumber: bool, whole_runs: bool) -> PgResult<InputIds> {
    if whole_runs && gen.ids_of(idx).is_empty() && gen.run_member(idx).is_some() {
        return Ok(InputIds::Run(Vec::new()));
    }
    input_ids(gen, idx, renumber)
}

/// The decoded granules a job reads, by `(input, granule)`: every one for a sorted merge, the last one otherwise.
pub(crate) struct Granules<'g> {
    pub(crate) schemas: &'g [pgrc_format::class::ColSchema],
    pub(crate) ins: &'g [In],
    pub(crate) fills: &'g [crate::fastadd::Fill],
    pub(crate) hold_all: bool,
    pub(crate) held: Vec<((u16, u32), Vec<ColBuf>)>,
    /// C1 L5: one cursor set per input, under `scan::read_residency` over the job's input files.
    pub(crate) cursors: Vec<PartCursors>,
}

impl Granules<'_> {
    fn get(&mut self, n: u16, g: u32, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<&[ColBuf]> {
        if let Some(i) = self.held.iter().position(|(k, _)| *k == (n, g)) {
            return Ok(&self.held[i].1);
        }
        interrupt(true)?;
        let mut cols = match self.hold_all || self.held.is_empty() {
            true => (0..self.schemas.len()).map(|_| ColBuf::new()).collect(),
            false => self.held.pop().expect("one held").1,
        };
        let lacks = self.ins[n as usize].present.as_deref().map(|present| crate::scan::Lacks { present, fills: self.fills });
        decode_granule(&mut self.cursors[n as usize], codec_binding(), self.schemas, g, &mut cols, lacks)?;
        self.held.push(((n, g), cols));
        Ok(&self.held.last().expect("pushed").1)
    }
}

/// Row `o` of input `n` as the slot arrays `raw_row` reads (`natts` wide; dropped columns NULL).
pub(crate) fn row_of(g: &mut Granules<'_>, ins: &[In], n: u16, o: u32, values: &mut [datum::Datum], isnull: &mut [bool], interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<()> {
    let grain = ins[n as usize].grain;
    let schemas = g.schemas;
    let cols = g.get(n, o / grain, interrupt)?;
    isnull.fill(true);
    for (c, s) in schemas.iter().enumerate() {
        if let Some(w) = col_cell(cols, c, (o % grain) as usize) {
            values[s.attno as usize - 1] = datum::Datum::from_u64(w);
            isnull[s.attno as usize - 1] = false;
        }
    }
    Ok(())
}

/// K1-MEM: the job's rows one column at a time, in `order`. A column pass decodes that column of the input granules
/// `order` reaches: one granule per input while `order` visits the input's granules in turn (a concatenation, a DV
/// rewrite, a merge of sorted parts), through one cursor per input for the pass under the job's read residency
/// (`scan::read_residency`, C1 L5); the column of an input `order` revisits (a merge of unsorted parts) is held for the
/// pass, each granule holding what its datums alias.
pub(crate) struct ColumnFeed<'j> {
    gen: &'j Generation,
    ins: &'j [In],
    fills: &'j [crate::fastadd::Fill],
    order: &'j [(u16, u32)],
    /// Per input: `order` visits its granules in nondecreasing order.
    monotone: Vec<bool>,
    /// C1 L5: the job's read residency over its input files.
    residency: Residency,
}

/// One input's decoded granules of the column in a pass.
enum Held {
    /// The granule decoded last, and the pass's cursor on the input's column.
    Last(Option<(u32, ColBuf)>, Option<StreamCursor<'static>>),
    /// Every granule decoded so far, and the pass's cursor on the input's column.
    All(Vec<Option<ColBuf>>, PartCursors),
}

impl<'j> ColumnFeed<'j> {
    pub(crate) fn new(gen: &'j Generation, ins: &'j [In], fills: &'j [crate::fastadd::Fill], order: &'j [(u16, u32)]) -> PgResult<ColumnFeed<'j>> {
        let (mut last, mut monotone) = (crate::alloc::try_filled(ins.len(), 0u32, "compact")?, crate::alloc::try_filled(ins.len(), true, "compact")?);
        for &(n, o) in order {
            let (n, g) = (n as usize, o / ins[n as usize].grain);
            match g < last[n] {
                true => monotone[n] = false,
                false => last[n] = g,
            }
        }
        let residency = read_residency(ins.iter().map(|i| i.rec.file_len).sum());
        Ok(ColumnFeed { gen, ins, fills, order, monotone, residency })
    }

    /// Granule `g` of input `n`, column `one`, decoded into `held` unless it is there; `first`: this is the job's
    /// first column pass (`interrupt`'s argument).
    fn granule<'h>(&self, held: &'h mut Held, n: usize, g: u32, one: &[ColSchema], absent: Option<&crate::fastadd::Fill>, first: bool, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<&'h ColBuf> {
        let part = &self.gen.parts[self.ins[n].idx].part;
        match held {
            Held::Last(slot, cursor) => {
                if slot.as_ref().is_none_or(|(h, _)| *h != g) {
                    interrupt(first)?;
                    let mut buf = slot.take().map_or_else(ColBuf::new, |(_, b)| b);
                    if let Some(fill) = absent {
                        buf.fill_const(pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g), fill);
                        *slot = Some((g, buf));
                        return Ok(&slot.as_ref().expect("filled").1);
                    }
                    if cursor.is_none() {
                        let c = StreamCursor::open(Arc::clone(part), codec_binding(), one[0].attno, 0).map_err(crate::read_error)?;
                        *cursor = Some(c.with_residency(self.residency));
                    }
                    decode_column_granule(cursor.as_mut().expect("opened"), &one[0], g, &mut buf)?;
                    *slot = Some((g, buf));
                }
                Ok(&slot.as_ref().expect("decoded").1)
            }
            Held::All(all, pc) => {
                let at = all.get_mut(g as usize).ok_or_else(|| contract("granule is past its input's granule count"))?;
                if at.is_none() {
                    interrupt(first)?;
                    let mut buf = ColBuf::new();
                    match absent {
                        Some(fill) => buf.fill_const(pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g), fill),
                        None => {
                            decode_granule(pc, codec_binding(), one, g, std::slice::from_mut(&mut buf), None)?;
                        }
                    }
                    *at = Some(buf);
                }
                Ok(at.as_ref().expect("decoded"))
            }
        }
    }

    /// Column `c` (of `schemas`) of the rows of `order`, appended to `out` through the row feed's conversion
    /// (`raw_row`'s per-attribute law) and its chunks, so the column's bytes are the row feed's. `first`: the job's
    /// first column pass, the one that decodes each granule it reaches for the first time in the job.
    pub(crate) fn fill(&self, rel: &Relation<'_>, schemas: &[ColSchema], c: usize, out: &mut ColBuffer, first: bool, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<()> {
        let att = &rel.rd_att.attrs[schemas[c].attno as usize - 1];
        let one = &schemas[c..c + 1];
        let mut held: Vec<Held> = self
            .ins
            .iter()
            .zip(&self.monotone)
            .map(|(i, &m)| match m {
                true => Held::Last(None, None),
                false => Held::All((0..i.rec.granule_count).map(|_| None).collect(), PartCursors::new(&self.gen.parts[i.idx].part, self.residency)),
            })
            .collect();
        let absent: Vec<Option<&crate::fastadd::Fill>> = self.ins.iter().map(|i| i.present.as_ref().filter(|p| !p[c]).map(|_| &self.fills[c])).collect();
        let rows = crate::ingest::SEAL_CHUNK_ROWS;
        let (mut chunk, mut scratch) = (RowChunk::new(1, rows), Vec::new());
        for &(n, o) in self.order {
            let grain = self.ins[n as usize].grain;
            let buf = self.granule(&mut held[n as usize], n as usize, o / grain, one, absent[n as usize], first, interrupt)?;
            let cell = col_cell(std::slice::from_ref(buf), 0, (o % grain) as usize);
            let d = datum::Datum::from_u64(cell.unwrap_or(0));
            let raw = match cell {
                None => RawDatum::Null,
                Some(_) => crate::ingest::raw_att(att, &d, None)?,
            };
            chunk.push_row(&[raw]).map_err(crate::write_error)?;
            if chunk.rows() >= rows {
                chunk.append_column(0, out, &mut NoExternalDetoast, &mut scratch).map_err(crate::write_error)?;
                chunk = RowChunk::new(1, rows);
            }
        }
        chunk.append_column(0, out, &mut NoExternalDetoast, &mut scratch).map_err(crate::write_error)
    }
}

/// Run `job` over `gen` (the caller holds the table's maintenance lock, so no compile moves it; a rotation or seal
/// under the front lock may add to the head meanwhile, and the publish lands on the head of its time once its inputs,
/// with their DVs, are still there) and publish it in the caller's transaction. `interrupt` runs before every granule
/// decode, told whether the decode is in the job's first pass over its input granules (the row feed, or the first
/// column pass; a test pause runs there only, so a paused job pauses once per input granule, not once per column and
/// granule). The allocator's freed memory goes back once the job is done (K1-MEM).
pub fn run_job(rel: &Relation<'_>, gen: &Arc<Generation>, job: &Job, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<CompactOut> {
    let out = job_body(rel, gen, job, None, pool, interrupt);
    mcx::release_retained();
    out
}

/// IX0 L4 (OC-4 C): `job` renumbering its survivors: the output's `n` rows take IDs `base..base + n` in output order
/// (one range run), and it homes `[base, base + align_up(n))` alone; its inputs' ranges go with them. The caller
/// (VACUUM FULL's renumber, `pgrc_front::compact::renumber_step`) chose `base` clear of every other part's ranges.
pub fn run_job_renumbered(rel: &Relation<'_>, gen: &Arc<Generation>, job: &Job, base: u64, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<CompactOut> {
    let out = job_body(rel, gen, job, Some(base), pool, interrupt);
    mcx::release_retained();
    out
}

/// The renumbered output's map and ranges: `n` rows from `base` (one dense run, the granules form),
/// `[base, base + align_up(n))`.
fn renumbered_map(base: u64, n: u64) -> PgResult<(ident::OutMap, pgrc_format::manifest::IdRanges)> {
    let map = ident::renumbered(base, n).map_err(|e| contract_owned(format!("renumbered map: {e:?}")))?;
    let ranges = if n == 0 { Vec::new() } else { vec![(base, base + pgrc_format::rowid::align_up(n))] };
    Ok((map, ranges))
}

fn contract_owned(why: String) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrcolumnar compaction: {why}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))
}

fn job_body(rel: &Relation<'_>, gen: &Arc<Generation>, job: &Job, renumber: Option<u64>, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<CompactOut> {
    let dir = crate::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let schemas = crate::schema::col_schemas(rel)?;
    let mut presences = crate::fastadd::Presences::new(&schemas, pgrc_format::ident::schema_fingerprint(&schemas));
    let ins = inputs(rel, gen, &dir, job, &mut presences, renumber.is_some(), false)?;
    let fills = match presences.any_absent() {
        true => crate::fastadd::fills(&rel.rd_att, &schemas)?,
        false => Vec::new(),
    };
    for i in &ins {
        RealVfs.willneed(&format!("{dir}/{}", pgrc_format::dirlayout::part_file_name(i.rec.part_no)), 0, i.rec.file_len);
    }
    let decls = crate::reloptions::cluster_key(rel);
    // A sorted merge, and a DV rewrite of a `sorted_by` table, keep the parts sorted; a forced concatenation does not.
    let sorted = job.kind == JobKind::SortedMerge || (job.kind == JobKind::DvRewrite && !decls.is_empty());
    if sorted && decls.is_empty() {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: a sorted merge of \"{}\", which has no sorted_by key", rel.name())).with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE)));
    }
    let views: Vec<RowMapView<'_>> = ins.iter().map(|i| i.map.view()).collect();
    let dels: Vec<Box<dyn Fn(u32) -> bool + '_>> = ins.iter().map(|i| Box::new(move |o: u32| i.dead[o as usize / 64] >> (o % 64) & 1 == 1) as Box<dyn Fn(u32) -> bool + '_>).collect();
    let job_inputs: Vec<Input<'_>> = ins.iter().zip(&dels).zip(&views).map(|((i, d), v)| Input { part_no: i.rec.part_no, rows: i.rec.rows as u32, deleted: &**d, map: v }).collect();
    let natts = rel.rd_att.natts as usize;
    let (mut values, mut isnull) = (crate::alloc::try_filled(natts, datum::Datum::null(), "compact")?, crate::alloc::try_filled(natts, true, "compact")?);
    let residency = read_residency(ins.iter().map(|i| i.rec.file_len).sum());
    let cursors = ins.iter().map(|i| PartCursors::new(&gen.parts[i.idx].part, residency)).collect();
    let mut g = Granules { schemas: &schemas, ins: &ins, fills: &fills, hold_all: sorted, held: Vec::new(), cursors };
    let first = std::cell::Cell::new(true);
    let order = match sorted {
        false => concat_order(&job_inputs).map_err(crate::format_error)?,
        true => {
            // The key columns of the survivors in concatenation order, through the writer's own capture and appends.
            let base = concat_order(&job_inputs).map_err(crate::format_error)?;
            let feed = ColumnFeed::new(gen, &ins, &fills, &base)?;
            let mut bufs: Vec<ColBuffer> = crate::alloc::try_vec(decls.len(), "compact")?;
            for d in &decls {
                let k = schemas.iter().position(|s| s.attno == d.attno).ok_or_else(|| contract("key column is not a table column"))?;
                let mut b = ColBuffer::new(schemas[k]);
                feed.fill(rel, &schemas, k, &mut b, first.replace(false), interrupt)?;
                bufs.push(b);
            }
            // sortpath (trunk 11a92c3cccf, V5SYNC): a jsonb path key orders by its `->>` value, as the seal's cluster sort
            // does (`seal::cmp::decl_slots`), never by the column's stored bytes: the output map is built from this order
            // and the seal's verify re-sorts by the same key
            let holds = bufs.iter().zip(&decls).map(|(b, d)| (!d.path.is_empty()).then(|| path_values(b, &d.path)).transpose()).collect::<Result<Vec<_>, _>>().map_err(crate::write_error)?;
            let keys = bufs.iter().zip(&decls).zip(&holds).map(|((b, d), h)| decl_slots(b, d, h)).collect::<Result<Vec<_>, _>>().map_err(crate::write_error)?;
            sorted_order(&job_inputs, &keys, &decls).map_err(crate::format_error)?
        }
    };
    // IX0: the output homes every input's ranges (a dropped row's ID stays homed, never handed out again); a renumbered
    // one homes its own dense range
    let (map, ids) = match renumber {
        None => (ident::build_map(&job.kind, &job_inputs, &order).map_err(|e| match e { pgrc_format::FormatError::OutOfMemory { .. } => crate::format_error(e), e => contract_owned(format!("output map: {e:?}")) })?, ident::union_ranges(ins.iter().map(|i| &i.ids[..]))),
        Some(base) => renumbered_map(base, order.len() as u64)?,
    };
    let mut w = SealWriter::open_compact(rel, pool, sorted)?;
    let fed = (|| {
        if order.is_empty() {
            return Ok(());
        }
        let feed = ColumnFeed::new(gen, &ins, &fills, &order)?;
        if w.seal_columns(rel, order.len() as u64, &mut |c, out| feed.fill(rel, &schemas, c, out, first.replace(false), interrupt))? {
            return Ok(());
        }
        // SB-10: a column's values average past the default grain's granule bytes, so the whole-part seal elects
        // the grain from every column: the row feed, which holds the part.
        let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: compaction of \"{}\" ({} rows) needs a finer grain; the job holds its part", rel.name(), order.len())); // alloc: Cold
        for &(n, o) in &order {
            row_of(&mut g, &ins, n, o, &mut values, &mut isnull, interrupt)?;
            w.append(rel, &values, &isnull)?;
        }
        Ok(())
    })();
    drop(g);
    if let Err(e) = fed {
        w.abort()?;
        return Err(e);
    }
    let retire: Vec<(u32, u64)> = ins.iter().map(|i| (i.rec.part_no, i.rec.dv_gen)).collect();
    let (out_gen, part_no) = w.publish_compact(rel, &retire, (map.form(), &map.encode()), &ids)?;
    let output_bytes = part_no.and_then(|no| out_gen.manifest.parts.iter().find(|p| p.part_no == no)).map_or(0, |p| p.file_len);
    let rows: u64 = ins.iter().map(|i| i.rec.rows).sum();
    Ok(CompactOut { part_no, rows: order.len() as u64, dropped: rows - order.len() as u64, input_bytes: ins.iter().map(|i| i.rec.file_len).sum(), output_bytes })
}
