// copyfrom.c, text/CSV from file or frontend, CIM_MULTI lane
// (heap_multi_insert buffering with a shared BulkInsertState, matching C's
// default insert method and its bulk relation-extension page geometry).

use backend_progress::progress::{
    PROGRESS_COPY_BYTES_TOTAL, PROGRESS_COPY_COMMAND, PROGRESS_COPY_COMMAND_FROM,
    PROGRESS_COPY_TUPLES_EXCLUDED, PROGRESS_COPY_TUPLES_PROCESSED, PROGRESS_COPY_TUPLES_SKIPPED,
    PROGRESS_COPY_TYPE, PROGRESS_COPY_TYPE_CALLBACK, PROGRESS_COPY_TYPE_FILE,
    PROGRESS_COPY_TYPE_PIPE,
};
use backend_progress::{
    pgstat_progress_end_command, pgstat_progress_start_command, pgstat_progress_update_multi_param,
    pgstat_progress_update_param, PROGRESS_COMMAND_COPY,
};
use elog::{ereport, PCT_M};
use mcx::{vec_from_elem_in, Mcx, MemoryContext, PgVec};
use stringinfo::StringInfo;
use types_core::Oid;
use types_dest::CommandDest;
use types_error::{
    ErrorLocation, PgError, PgResult, ERRCODE_UNDEFINED_FUNCTION,
    ERRCODE_WRONG_OBJECT_TYPE, ERROR,
};
use types_fmgr::FmgrInfo;
use types_nodes::nodes_enums::CmdType;
use types_nodes::NodeList;
use types_rel::Relation;
use types_tuple::NameData;

use crate::fromparse::{EolType, INPUT_BUF_SIZE, RAW_BUF_SIZE};
use crate::{
    force_flags, CopyFormatOptions, CopyGetAttnums, ProcessCopyOptions, RELKIND_RELATION,
};

pub(crate) enum CopySrc<'mcx, 's> {
    File { fd: i32, filename: &'s str },
    // COPY FROM PROGRAM (copyfrom.c is_program arm): `fd` is an
    // OpenPipeStream index; the child's stdout feeds the same refill path a
    // file does. Closed via ClosePipeFromProgram semantics in EndCopyFrom.
    Program { fd: i32, filename: &'s str },
    Frontend { msgbuf: StringInfo<'mcx> },
    // copyfrom.c:1850 (`cstate->copy_file = stdin`): the pipe source of a
    // non-remote session — single-user mode's COPY ... FROM STDIN. The read
    // path mirrors C's greedy fread(maxread): bytes past the \. terminator
    // that land in raw_buf are discarded with it, exactly as C's shared
    // stdio buffer loses them to the interactive query loop.
    Stdin,
    // COPY_CALLBACK (copyfrom_internal.h): tablesync pulls bytes from the
    // publisher's COPY OUT stream. cb(buf, minread) fills up to buf.len()
    // bytes, at least minread unless the stream ends; 0 = EOF.
    //
    // The trait object is lifetime-erased to 'static so that dropck does NOT
    // extend the closure's captured borrows across CopyFromState's drop. This
    // keeps parallel.rs's worker teardown (which drops a chunk-sourced
    // CopyFromState<'mcx, 'a> and then moves `rel` into table_close on the same
    // 'a) borrow-checkable. The erasure is a deliberate soundness/ergonomics
    // trade: it means a callback whose captures own Drop types borrowing data
    // shorter-lived than the returned CopyFromState would UAF in the box's drop
    // glue. That obligation is pushed onto the caller — the ONLY constructor of
    // this variant is the `unsafe fn BeginCopyFromCallback`, whose safety
    // contract forbids exactly such captures. Do not add a safe constructor.
    Callback { cb: Box<dyn FnMut(&mut [u8], usize) -> PgResult<usize> + 'static> },
    /// FORMAT 'parquet': typed column batches from a server-side file (the
    /// reader owns the file handle; drop closes it on every exit path).
    Parquet(Box<crate::fromparquet::ParquetSrc>),
    /// Parallel text COPY (text_pgrc.rs): a cut chunk's bytes. `parked`
    /// holds the statement's own source while the serial body reads the
    /// buffered prefix; EndCopyFrom restores it.
    Chunk { cur: crate::textseg::ChunkCursor, parked: Option<Box<CopySrc<'mcx, 's>>> },
}

pub struct CopyFromState<'mcx, 's> {
    pub opts: CopyFormatOptions<'s>,
    pub(crate) src: CopySrc<'mcx, 's>,
    pub(crate) raw_buf: PgVec<'mcx, u8>,
    pub(crate) raw_buf_index: usize,
    pub(crate) raw_buf_len: usize,
    pub(crate) raw_reached_eof: bool,
    pub(crate) input_reached_eof: bool,
    pub(crate) input_reached_error: bool,
    pub(crate) input_buf: Option<PgVec<'mcx, u8>>,
    pub(crate) input_buf_index: usize,
    pub(crate) input_buf_len: usize,
    pub(crate) line_buf: PgVec<'mcx, u8>,
    pub(crate) line_buf_valid: bool,
    pub(crate) attribute_buf: PgVec<'mcx, u8>,
    pub(crate) binary_attr_buf: StringInfo<'mcx>,
    pub(crate) raw_fields: PgVec<'mcx, i32>,
    pub(crate) max_fields: usize,
    pub(crate) eol_type: EolType,
    pub cur_lineno: u64,
    pub(crate) cur_attidx: Option<usize>,
    pub(crate) cur_attval_off: Option<i32>,
    // C's cur_attval keeps pointing into attribute_buf after a soft error;
    // the bytes it addressed before the next line overwrote the buffer.
    pub(crate) cur_attval_stale: PgVec<'mcx, u8>,
    pub(crate) file_encoding: i32,
    pub(crate) need_transcoding: bool,
    pub(crate) conversion_proc: Oid,
    pub(crate) convertcx: MemoryContext,
    pub(crate) attnumlist: PgVec<'mcx, i16>,
    pub(crate) in_functions: PgVec<'mcx, FmgrInfo>,
    pub(crate) typioparams: PgVec<'mcx, Oid>,
    pub(crate) atttypmods: PgVec<'mcx, i32>,
    pub(crate) attnames: PgVec<'mcx, NameData>,
    pub(crate) force_notnull_flags: PgVec<'mcx, bool>,
    pub(crate) force_null_flags: PgVec<'mcx, bool>,
    pub(crate) convert_select_flags: Option<PgVec<'mcx, bool>>,
    // Per physical attribute; defmap lists attrs absent from attnumlist whose
    // default fills the column, defaults[] carries per-row DEFAULT markers.
    pub(crate) defexprs: PgVec<'mcx, Option<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>>>,
    pub(crate) defmap: PgVec<'mcx, usize>,
    pub(crate) defaults: PgVec<'mcx, bool>,
    pub(crate) where_clause: NodeList<'mcx>,
    pub(crate) relname: String,
    pub(crate) relname_only: bool,
    pub(crate) escontext: Option<Box<types_fmgr::ErrorSaveNode>>,
    pub(crate) num_errors: u64,
    pub(crate) bytes_processed: u64,
    pub(crate) volatile_defexprs: bool,
}

impl CopyFromState<'_, '_> {
    pub(crate) fn attname(&self, m: usize) -> String {
        String::from_utf8_lossy(self.attnames[m].name_str()).into_owned()
    }

    pub fn num_errors(&self) -> u64 {
        self.num_errors
    }

    // What C's cur_attval addresses: attribute_buf at `off` up to its NUL, or,
    // once a shorter line has been parsed over it, the bytes it held before.
    pub(crate) fn cur_attval_bytes(&self, off: i32) -> &[u8] {
        let off = off as usize;
        if off >= self.attribute_buf.len() {
            return &self.cur_attval_stale;
        }
        let bytes = &self.attribute_buf[off..];
        let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        &bytes[..nul]
    }

    // resetStringInfo(&attribute_buf) leaves a dangling cur_attval in C;
    // remember what it pointed at so the error context can still show it.
    pub(crate) fn stash_cur_attval(&mut self) -> PgResult<()> {
        if let Some(off) = self.cur_attval_off {
            let off = off as usize;
            if off < self.attribute_buf.len() {
                let bytes = &self.attribute_buf[off..];
                let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                self.cur_attval_stale.clear();
                mcx::vec_append_bytes(&mut self.cur_attval_stale, &bytes[..nul])?;
            }
        }
        Ok(())
    }

    /// `cstate->escontext->error_occurred` (file_fdw's soft-error probe).
    pub fn soft_error_occurred(&self) -> bool {
        self.escontext.as_ref().is_some_and(|n| n.ctx.error_occurred())
    }

    pub fn reset_soft_error(&mut self) {
        if let Some(n) = self.escontext.as_mut() {
            n.ctx.reset_error_occurred();
        }
    }
}

#[track_caller]
fn loc(funcname: &'static str) -> ErrorLocation {
    // pgrust is Rust: report where in OUR source this was raised.
    // #[track_caller] resolves to the call site, not this helper.
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, funcname)
}

/// `BeginCopyFrom` (copyfrom.c), text/CSV from file or frontend.
pub fn BeginCopyFrom<'mcx: 's, 's>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    where_clause: NodeList<'mcx>,
    filename: Option<&'s str>,
    is_program: bool,
    attnamelist: &NodeList<'_>,
    options: &NodeList<'s>,
    source_text: Option<&str>,
) -> PgResult<CopyFromState<'mcx, 's>> {
    begin_copy_from_guts(
        mcx,
        rel,
        where_clause,
        filename,
        is_program,
        None,
        attnamelist,
        options,
        source_text,
    )
}

// BeginCopyFrom's data_source_cb form (COPY_CALLBACK): tablesync feeds bytes
// from the publisher's COPY OUT stream. cb(buf, minread) -> bytes written,
// 0 at end of stream.
///
/// # Safety
///
/// The callback's captured state must outlive the returned `CopyFromState`'s
/// drop. Concretely, the closure's by-value captures must EITHER have no drop
/// glue (e.g. only shared/`Copy` references, integers), OR any `Drop`-carrying
/// capture must reference only data that lives at least as long as the returned
/// `CopyFromState`. This mirrors C's `data_source_cb`, which is stored in
/// `CopyFromStateData` and may only touch state valid for that struct's
/// lifetime (copyfrom.c `BeginCopyFrom`).
///
/// This precondition cannot be expressed in the type system here: the callback
/// is lifetime-erased to `'static` inside `CopySrc::Callback` so that the
/// parallel-COPY worker teardown (which drops a chunk-sourced `CopyFromState`
/// and then reuses its borrow lifetime for `table_close`) stays
/// borrow-checkable. Because that erasure discards dropck's guarantee that the
/// captures outlive the box's drop glue, a caller passing `Drop`-owning
/// captures that borrow shorter-lived data would trigger a use-after-free when
/// `CopyFromState` is dropped. Uses of the callback (`next_copy_from`) remain
/// lifetime-checked via the `'s` still carried by `CopyFromState<'mcx, 's>`;
/// only the drop-time obligation is the caller's responsibility.
pub unsafe fn BeginCopyFromCallback<'mcx: 's, 's>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    attnamelist: &NodeList<'_>,
    options: &NodeList<'s>,
    cb: Box<dyn FnMut(&mut [u8], usize) -> PgResult<usize> + 's>,
) -> PgResult<CopyFromState<'mcx, 's>> {
    begin_copy_from_guts(
        mcx,
        rel,
        NodeList::nil(),
        None,
        false,
        Some(cb),
        attnamelist,
        options,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn begin_copy_from_guts<'mcx: 's, 's>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    where_clause: NodeList<'mcx>,
    filename: Option<&'s str>,
    is_program: bool,
    mut data_source_cb: Option<Box<dyn FnMut(&mut [u8], usize) -> PgResult<usize> + 's>>,
    attnamelist: &NodeList<'_>,
    options: &NodeList<'s>,
    source_text: Option<&str>,
) -> PgResult<CopyFromState<'mcx, 's>> {
    // C has no parquet format (copy.c:574). The pgrust reader serves
    // server-side files only: a PROGRAM or STDIN source under FORMAT
    // 'parquet' gets C's own refusal at the format option.
    let parquet_ok = !is_program && (filename.is_some() || data_source_cb.is_some());
    let opts = crate::process_copy_options(mcx, true, parquet_ok, options, source_text)?;
    let tup_desc = &rel.rd_att;
    let attnumlist = CopyGetAttnums(mcx, tup_desc, Some(rel), attnamelist)?;
    let num_phys_attrs = tup_desc.natts as usize;

    let force_notnull_flags = force_flags(
        mcx,
        tup_desc,
        Some(rel),
        &attnumlist,
        opts.force_notnull,
        opts.force_notnull_all,
        "FORCE_NOT_NULL",
    )?;
    let force_null_flags = force_flags(
        mcx,
        tup_desc,
        Some(rel),
        &attnumlist,
        opts.force_null,
        opts.force_null_all,
        "FORCE_NULL",
    )?;

    // C builds these after the force_notnull/force_null flags (observable as
    // error precedence when several option lists name un-referenced columns).
    let convert_select_flags = if opts.convert_selectively {
        let mut flags = vec_from_elem_in(mcx, false, num_phys_attrs);
        let empty = NodeList::nil();
        let sel = CopyGetAttnums(mcx, tup_desc, Some(rel), opts.convert_select.unwrap_or(&empty))?;
        for &attnum in sel.iter() {
            if !attnumlist.contains(&attnum) {
                let att = tup_desc.attr(attnum as usize - 1);
                return Err(Box::new(
                    PgError::error(format!(
                        "selected column \"{}\" not referenced by COPY",
                        String::from_utf8_lossy(att.attname.name_str())
                    ))
                    .with_sqlstate(types_error::ERRCODE_INVALID_COLUMN_REFERENCE),
                ));
            }
            flags[attnum as usize - 1] = true;
        }
        Some(flags)
    } else {
        None
    };

    let file_encoding = if opts.file_encoding < 0 {
        mbutils::pg_get_client_encoding()
    } else {
        opts.file_encoding
    };
    let db_encoding = mbutils::GetDatabaseEncoding();
    let need_transcoding = !(file_encoding == db_encoding
        || file_encoding == wchar::PG_SQL_ASCII
        || db_encoding == wchar::PG_SQL_ASCII);
    let conversion_proc = if need_transcoding {
        let p = namespace_seams::find_default_conversion_proc::call(file_encoding, db_encoding)?;
        if p == 0 {
            return Err(Box::new(
                PgError::error(format!(
                    "default conversion function for encoding \"{}\" to \"{}\" does not exist",
                    mbutils::pg_encoding_to_char(file_encoding),
                    mbutils::pg_encoding_to_char(db_encoding),
                ))
                .with_sqlstate(ERRCODE_UNDEFINED_FUNCTION),
            ));
        }
        p
    } else {
        0
    };

    let mut in_functions: PgVec<'mcx, FmgrInfo> = PgVec::new_in(mcx);
    let mut typioparams: PgVec<'mcx, Oid> = PgVec::new_in(mcx);
    let mut atttypmods: PgVec<'mcx, i32> = PgVec::new_in(mcx);
    let mut attnames: PgVec<'mcx, NameData> = PgVec::new_in(mcx);
    let mut defexprs: PgVec<'mcx, Option<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>>> =
        PgVec::new_in(mcx);
    let mut defmap: PgVec<'mcx, usize> = PgVec::new_in(mcx);
    let mut volatile_defexprs = false;
    let mut attr_in_funcs: Vec<Option<(FmgrInfo, Oid)>> =
        (0..num_phys_attrs).map(|_| None).collect();
    for i in 0..num_phys_attrs {
        let att = tup_desc.attr(i);
        attnames.push(att.attname);
        defexprs.push(None);
        if att.attisdropped {
            continue;
        }
        let (func_oid, typioparam) = if opts.binary {
            lsyscache::typ::getTypeBinaryInputInfo(att.atttypid)?
        } else {
            lsyscache::typ::getTypeInputInfo(att.atttypid)?
        };
        attr_in_funcs[i] = Some((fmgr_core::fmgr_info(func_oid)?, typioparam));
        let in_list = attnumlist.contains(&(i as i16 + 1));
        if (opts.default_print.is_some() || !in_list) && att.attgenerated == 0 {
            let Some(defexpr) = rewrite_handler::build_column_default(mcx, rel, i + 1)? else {
                continue;
            };
            let defexpr = clauses::eval_const_expressions(mcx, defexpr)?;
            nodes_core::fix_opfuncids(defexpr)?;
            let mut state = execexpr::exec_init_expr(mcx, Some(defexpr), execexpr::ParamBind::NONE)?
                .expect("column default expression");
            state.arm_result_mcx(mcx);
            defexprs[i] = Some(state);
            if !in_list {
                defmap.push(i);
            }
            if !volatile_defexprs {
                volatile_defexprs = clauses::contain_volatile_functions_not_nextval(defexpr)?;
            }
        }
    }
    for &attnum in attnumlist.iter() {
        let (in_fn, typioparam) =
            attr_in_funcs[attnum as usize - 1].take().expect("listed column is not dropped");
        in_functions.push(in_fn);
        typioparams.push(typioparam);
        atttypmods.push(tup_desc.attr(attnum as usize - 1).atttypmod);
    }
    pgstat_progress_start_command(PROGRESS_COMMAND_COPY, rel.rd_id);
    let mut progress_type = PROGRESS_COPY_TYPE_PIPE;
    let mut progress_bytes_total: i64 = 0;
    let src = if opts.parquet {
        // Server-side file only in this increment (PROGRAM and STDIN were
        // refused above with C's format error; the callback source is
        // internal and errors cleanly here).
        if data_source_cb.is_some() {
            return Err(Box::new(
                PgError::error(
                    "COPY FROM with parquet format only supports reading from a file",
                )
                .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
            ));
        }
        let (psrc, file_len) = crate::fromparquet::open_source(
            filename,
            tup_desc,
            &attnumlist,
            opts.parquet_match_by_name,
            opts.parquet_coerce_epoch,
        )?;
        progress_type = PROGRESS_COPY_TYPE_FILE;
        progress_bytes_total = file_len as i64;
        CopySrc::Parquet(psrc)
    } else if let Some(cb) = data_source_cb.take() {
        progress_type = PROGRESS_COPY_TYPE_CALLBACK;
        // SAFETY: erasing the caller-chosen 's to 'static drops dropck's
        // dyn-Drop borrow-extension obligation on CopyFromState's drop. That is
        // only sound because every path that reaches here does so through the
        // `unsafe fn BeginCopyFromCallback` (or the internal serial callers
        // that pass `None`), whose documented contract requires the callback's
        // captures to either be free of Drop glue or to only reference data
        // that outlives the returned CopyFromState. CopyFromState<'mcx, 's>
        // still carries 's through `opts`/filename fields, so every *use* of
        // the callback (next_copy_from) remains lifetime-checked; the erasure
        // relaxes only the drop-time obligation, which the caller now owns.
        let cb: Box<dyn FnMut(&mut [u8], usize) -> PgResult<usize> + 'static> =
            unsafe { core::mem::transmute(cb) };
        CopySrc::Callback { cb }
    } else {
        match filename {
        Some(filename) if is_program => {
            // copyfrom.c is_program arm: popen the command, read its stdout.
            let fd = fd::OpenPipeStream(filename, "r")?;
            if fd < 0 {
                ereport(ERROR)
                    .with_saved_errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
                    .errcode_for_file_access()
                    .errmsg(format!("could not execute command \"{filename}\": {PCT_M}"))
                    .finish(loc("BeginCopyFrom"))?;
            }
            progress_type = backend_progress::progress::PROGRESS_COPY_TYPE_PROGRAM;
            CopySrc::Program { fd, filename }
        }
        Some(filename) => {
            let fd = fd::AllocateFile(filename, "rb")?;
            if fd < 0 {
                // copy errno because ereport subfunctions might change it
                let save_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                let mut e = ereport(ERROR)
                    .with_saved_errno(save_errno)
                    .errcode_for_file_access()
                    .errmsg(format!("could not open file \"{filename}\" for reading: {PCT_M}"));
                if crate::open_failure_hint_applies(save_errno) {
                    e = e.errhint(
                        "COPY FROM instructs the PostgreSQL server process to read a file. You \
                         may want a client-side facility such as psql's \\copy.",
                    );
                }
                e.finish(loc("BeginCopyFrom"))?;
            }
            progress_type = PROGRESS_COPY_TYPE_FILE;
            // fstat (copyfrom.c:1886): a failure is an error, never a silent
            // "not a directory, size 0".
            let (is_dir, size) = match fd::with_allocated_stdio(fd, |f| f.metadata()) {
                Some(Ok(m)) => (m.is_dir(), m.len()),
                Some(Err(e)) => return Err(crate::could_not_stat_file(filename, &e)),
                None => panic!("COPY FROM: AllocateFile index {fd} vanished"),
            };
            if is_dir {
                return Err(Box::new(
                    PgError::error(format!("\"{filename}\" is a directory"))
                        .with_sqlstate(ERRCODE_WRONG_OBJECT_TYPE),
                ));
            }
            progress_bytes_total = size as i64;
            CopySrc::File { fd, filename }
        }
        None => {
            // copyfrom.c:1847: a pipe COPY outside a remote session
            // (single-user mode) reads from the process's stdin instead of
            // the frontend protocol.
            if elog::config::where_to_send_output() != CommandDest::Remote {
                CopySrc::Stdin
            } else {
                receive_copy_begin(mcx, attnumlist.len(), opts.binary)?
            }
        }
        }
    };
    pgstat_progress_update_multi_param(
        &[PROGRESS_COPY_COMMAND, PROGRESS_COPY_TYPE, PROGRESS_COPY_BYTES_TOTAL],
        &[PROGRESS_COPY_COMMAND_FROM, progress_type, progress_bytes_total],
    );

    let max_fields = attnumlist.len();
    let opts_on_error = opts.on_error;
    let is_binary = opts.binary;
    let mut cstate = CopyFromState {
        opts,
        src,
        raw_buf: vec_from_elem_in(mcx, 0u8, RAW_BUF_SIZE + 1),
        raw_buf_index: 0,
        raw_buf_len: 0,
        raw_reached_eof: false,
        input_reached_eof: false,
        input_reached_error: false,
        input_buf: need_transcoding.then(|| vec_from_elem_in(mcx, 0u8, INPUT_BUF_SIZE + 1)),
        input_buf_index: 0,
        input_buf_len: 0,
        line_buf: PgVec::new_in(mcx),
        line_buf_valid: false,
        attribute_buf: PgVec::new_in(mcx),
        binary_attr_buf: StringInfo::new_in(mcx)?,
        raw_fields: PgVec::new_in(mcx),
        max_fields,
        eol_type: EolType::Unknown,
        cur_lineno: 0,
        cur_attidx: None,
        cur_attval_off: None,
        cur_attval_stale: PgVec::new_in(mcx),
        file_encoding,
        need_transcoding,
        conversion_proc,
        convertcx: MemoryContext::new("COPY convert"),
        attnumlist,
        in_functions,
        typioparams,
        atttypmods,
        attnames,
        force_notnull_flags,
        force_null_flags,
        convert_select_flags,
        defexprs,
        defmap,
        defaults: vec_from_elem_in(mcx, false, num_phys_attrs),
        where_clause,
        relname: rel.name().to_string(),
        relname_only: false,
        escontext: (opts_on_error == crate::CopyOnErrorChoice::Ignore)
            .then(|| Box::new(types_fmgr::ErrorSaveNode::new(false))),
        num_errors: 0,
        bytes_processed: 0,
        volatile_defexprs,
    };
    if is_binary {
        cstate.receive_copy_binary_header()?;
    }
    Ok(cstate)
}

// ReceiveCopyBegin (copyfromparse.c): CopyInResponse, then flush so the
// frontend knows it can send.
fn receive_copy_begin<'mcx, 's>(
    mcx: Mcx<'mcx>,
    natts: usize,
    binary: bool,
) -> PgResult<CopySrc<'mcx, 's>> {
    let format: u16 = if binary { 1 } else { 0 };
    let mut buf = pqformat::pq_beginmessage(mcx, b'G')?;
    pqformat::pq_sendbyte(&mut buf, format as u8)?;
    pqformat::pq_sendint16(&mut buf, natts as u16)?;
    for _ in 0..natts {
        pqformat::pq_sendint16(&mut buf, format)?;
    }
    pqformat::pq_endmessage(buf)?;
    let msgbuf = StringInfo::new_in(mcx)?;
    pqcomm::pq_flush()?;
    Ok(CopySrc::Frontend { msgbuf })
}

// copyfrom.c MAX_BUFFERED_TUPLES / MAX_BUFFERED_BYTES.
const MAX_BUFFERED_TUPLES: usize = 1000;
const MAX_BUFFERED_BYTES: usize = 65535;

/// `CopyFrom` (copyfrom.c): read rows, insert into the heap + indexes. Every
/// CIM_SINGLE trigger in C (BEFORE/INSTEAD triggers, FDW, volatile defaults,
/// volatile WHERE) is unported-loud upstream, so this is always CIM_MULTI.
pub fn CopyFrom<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
) -> PgResult<u64> {
    let relkind = rel.rd_rel.relkind;
    let trigdesc = if rel.rd_hastriggers {
        relcache::RelationGetTriggerDesc(rel.rd_id)?
    } else {
        None
    };
    let has_instead = trigdesc.as_ref().is_some_and(|td| td.trig_insert_instead_row);
    if tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam) {
        ::pgrc_front::route::admit_insert(false)?;
    }
    // A non-table target is allowed only with an INSTEAD OF INSERT row
    // trigger (copyfrom.c:809-841).
    if relkind != RELKIND_RELATION
        && relkind != types_rel::RELKIND_FOREIGN_TABLE
        && relkind != types_rel::RELKIND_PARTITIONED_TABLE
        && !has_instead
    {
        return Err(cannot_copy_to_relkind(rel));
    }
    // New-in-transaction storage: probing the FSM is a waste of time
    // (RELKIND_HAS_STORAGE arm; partitioned roots have none).
    let mut ti_options = if relkind == RELKIND_RELATION
        && (rel.rd_createSubid.get() != types_core::xact::InvalidSubTransactionId
            || rel.rd_firstRelfilelocatorSubid.get()
                != types_core::xact::InvalidSubTransactionId)
    {
        tableam_vocab::TABLE_INSERT_SKIP_FSM
    } else {
        0
    };
    if cstate.opts.freeze {
        if relkind == types_rel::RELKIND_PARTITIONED_TABLE {
            return Err(Box::new(
                PgError::error("cannot perform COPY FREEZE on a partitioned table")
                    .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
            ));
        }
        if relkind == types_rel::RELKIND_FOREIGN_TABLE {
            return Err(Box::new(
                PgError::error("cannot perform COPY FREEZE on a foreign table")
                    .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
            ));
        }
        snapmgr::InvalidateCatalogSnapshot();
        if !snapmgr::ThereAreNoPriorRegisteredSnapshots() || !portalmem::ThereAreNoReadyPortals()
        {
            return Err(Box::new(
                PgError::error("cannot perform COPY FREEZE because of prior transaction activity")
                    .with_sqlstate(types_error::ERRCODE_INVALID_TRANSACTION_STATE),
            ));
        }
        let cur = xact::GetCurrentSubTransactionId();
        if rel.rd_createSubid.get() != cur && rel.rd_newRelfilelocatorSubid.get() != cur {
            return Err(Box::new(
                PgError::error(
                    "cannot perform COPY FREEZE because the table was not created or truncated \
                     in the current subtransaction",
                )
                .with_sqlstate(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
            ));
        }
        ti_options |= tableam_vocab::TABLE_INSERT_FROZEN;
    }
    // CheckValidResultRel(CMD_INSERT) + BeginForeignInsert (copyfrom.c:921,
    // 927-941): a foreign target inserts through its FDW's ExecForeignInsert.
    let mut foreign = if relkind == types_rel::RELKIND_FOREIGN_TABLE {
        Some(begin_copy_foreign_insert(mcx, rel)?)
    } else {
        None
    };
    // Parallel COPY FROM parquet into pgrcolumnar: admission refusals
    // (triggers included — fail-closed, traced) fall through to the serial
    // body, byte-identically. Errors return UNWRAPPED (the workers attached
    // the exact contexts already). Engagement implies no triggers, so the
    // early return skips no trigger work; partitioned targets never reach
    // here.
    if relkind == RELKIND_RELATION {
        // M4X parquet-parallel: order-preserving parallel COPY FROM parquet
        // into pgrcolumnar (parquet_pgrc.rs). Fail-closed: every refusal
        // falls through byte-identically
        // (here, to the serial parquet reader), errors return fully
        // contexted, and engagement implies no triggers. Publish happens
        // INSIDE (table_finish_bulk_insert — the serial statement-end path).
        if let Some(processed) = crate::parquet_pgrc::copy_from_parquet_pgrc(
            mcx,
            cstate,
            rel,
            trigdesc.is_some(),
            ti_options,
        )? {
            debug_assert!(trigdesc.is_none(), "parallel COPY engaged on a trigger-bearing rel");
            return Ok(processed);
        }
        // Parallel COPY FROM text / CSV into pgrcolumnar (text_pgrc.rs),
        // the same contract.
        if let Some(processed) = crate::text_pgrc::copy_from_text_pgrc(
            mcx,
            cstate,
            rel,
            trigdesc.is_some(),
            ti_options,
        )? {
            debug_assert!(trigdesc.is_none(), "parallel COPY engaged on a trigger-bearing rel");
            return Ok(processed);
        }
    }
    // C forces CIM_SINGLE on a partitioned target with BEFORE/INSTEAD row
    // triggers (copyfrom.c:995-1005) or statement-level transition tables
    // (copyfrom.c:1016-1027) — the flush path can't fire either correctly.
    let part_force_single = relkind == types_rel::RELKIND_PARTITIONED_TABLE
        && trigdesc.as_ref().is_some_and(|td| {
            td.trig_insert_before_row || td.trig_insert_instead_row || td.trig_insert_new_table
        });
    // AfterTriggerBeginQuery precedes MakeTransitionCaptureState (copyfrom.c
    // 961/972): the capture registry keys off the query depth it opens. A
    // triggerless partitioned target still needs it — routed-into leaves may
    // carry their own row triggers.
    let open_trigger_query =
        trigdesc.is_some() || relkind == types_rel::RELKIND_PARTITIONED_TABLE;
    if open_trigger_query {
        trigger::AfterTriggerBeginQuery();
    }
    let transition_capture = match &trigdesc {
        Some(td) => trigger::MakeTransitionCaptureState(td, rel.rd_id, CmdType::CMD_INSERT)?,
        None => None,
    };
    let mut trig_fmgr = trigger::TriggerFmgrCache::default();
    let mut trig_when = trigger::TriggerWhenCache::default();
    // C CopyFrom runs ExecOpenIndices(resultRelInfo, false) at setup, BEFORE
    // ExecBSInsertTriggers (copyfrom.c:924 vs 1102): the open pins the
    // target's (for a partitioned target: the root's partitioned) index
    // relations for the whole load, so a BS-trigger DROP INDEX fails
    // CheckTableNotInUse exactly as in C, and the index list is read
    // pre-trigger (stale-held-rd_rel audit). A partitioned root's state is
    // only held — routed inserts use the per-leaf opens below.
    let mut root_index_state = execindexing::ExecOpenIndices(mcx, rel, false)?;
    // copyfrom.c:792: the COPY command id is fixed before the BS triggers
    // run (a trigger's SPI insert advances the counter).
    let mycid = xact::GetCurrentCommandId(true)?;
    if let Some(td) = &trigdesc {
        let mut when =
            trigger::TriggerWhenEval { mcx, cache: &mut trig_when, modified_cols: None };
        trigger::ExecBSInsertTriggers(mcx, rel, td, &mut trig_fmgr, &mut when)?;
    }
    // CopyFromErrorCallback scope: C installs error_context_stack here, after
    // the relkind + FREEZE checks; buffered-but-unflushed slots on the Err
    // path are simply dropped, as C's are (the aborted xact kills flushed
    // ones). Non-ERROR reports emitted inside the row loop (trigger or
    // default-expression notices) carry the COPY context through the emit
    // callback; the ERROR path attaches it on propagation below.
    let cb_state = cstate as *const CopyFromState<'_, '_> as usize;
    let emit_cb = elog::push_emit_context_callback(Box::new(move |e| {
        // SAFETY: the callback is popped right after `body` returns, while
        // `cstate` is still alive; it only reads the cursor fields the loop
        // updates between reports.
        let cstate = unsafe { &*(cb_state as *const CopyFromState<'_, '_>) };
        // relname_only (copyfrom.c:258): the verbose skip reporter attaches
        // C's "COPY <rel>" context itself.
        if !cstate.relname_only {
            e.add_context_line(copy_from_error_context_text(cstate));
        }
    }));
    let mut leaf_indexes: Vec<Option<execindexing::ResultRelIndexState<'mcx>>> = Vec::new();
    let body = if relkind == types_rel::RELKIND_PARTITIONED_TABLE {
        copy_from_partitioned_body(
            mcx,
            cstate,
            rel,
            ti_options,
            part_force_single,
            transition_capture.as_ref(),
            mycid,
            &mut leaf_indexes,
        )
    } else {
        let mut trig = trigdesc.as_ref().map(|td| CopyTrig {
            td,
            tc: transition_capture.as_ref(),
            when: &mut trig_when,
            fmgr: &mut trig_fmgr,
        });
        copy_from_body(
            mcx,
            cstate,
            rel,
            ti_options,
            trig.as_mut(),
            &mut root_index_state,
            mycid,
            foreign.as_mut(),
        )
    };
    elog::pop_emit_context_callback(emit_cb);
    match body {
        Ok(n) => {
            skipped_rows_notice(cstate)?;
            if let Some(td) = &trigdesc {
                let mut when =
                    trigger::TriggerWhenEval { mcx, cache: &mut trig_when, modified_cols: None };
                trigger::ExecASInsertTriggers(rel, td, transition_capture.as_ref(), Some(&mut when))?;
            }
            if open_trigger_query {
                trigger::AfterTriggerEndQuery(None, &[(rel.rd_id, trigdesc.clone())])?;
            }
            // EndForeignInsert after AfterTriggerEndQuery (copyfrom.c:1493-
            // 1496); an error above leaves the state to the xact callback's
            // connection cleanup, as C's aborted transaction does.
            if let Some(f) = foreign.take() {
                (f.routine.end)(f.state)?;
            }
            // ExecCleanupTupleRouting + ExecCloseResultRelations follow
            // AfterTriggerEndQuery (copyfrom.c:1488-1507): the queued AFTER
            // triggers run with every index relation still open.
            for idx in leaf_indexes.iter_mut() {
                if let Some(indexes) = idx.take() {
                    execindexing::ExecCloseIndices(indexes)?;
                }
            }
            execindexing::ExecCloseIndices(root_index_state)?;
            Ok(n)
        }
        Err(e) => Err(copy_from_error_context(cstate, e)),
    }
}

// The row-trigger slice of C's resultRelInfo that the insert loop and
// CopyMultiInsertBufferFlush touch.
struct CopyTrig<'a, 'mcx> {
    td: &'a std::rc::Rc<types_trigger::TriggerDesc<'static>>,
    tc: Option<&'a trigger::TransitionCaptureState>,
    when: &'a mut trigger::TriggerWhenCache<'mcx>,
    fmgr: &'a mut trigger::TriggerFmgrCache,
}

fn is_pgrc(rel: &Relation<'_>) -> bool {
    rel.rd_rel.relkind == RELKIND_RELATION && tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam)
}

/// IXBULKSORT: the index entries of `rel`'s bulk rows whose IDs their parts' seals moved (a `sorted_by` table with
/// indexes: the rows come back at the IDs they were placed at once their parts are cut, `table_take_placed`);
/// rows of a part still buffered keep waiting. Called after each flush and after each `table_finish_bulk_insert`.
fn pgrc_index_placed<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    index_state: &mut execindexing::ResultRelIndexState<'mcx>,
) -> PgResult<()> {
    if !is_pgrc(rel) {
        return Ok(());
    }
    let mut eval: Option<MemoryContext> = None;
    tableam::table_take_placed(mcx, rel, &mut |s| {
        let cx = eval.get_or_insert_with(|| MemoryContext::new_bump("CopyIndexEvalPlaced"));
        let out = execindexing::ExecInsertIndexTuples(mcx, cx.mcx(), index_state, rel, s, None, false, None, &[], false).map(|_| ());
        cx.reset();
        out
    })
}

/// CIM_SINGLE into pgrcolumnar: each row goes straight into the segment (no options, no bistate), so BEFORE ROW
/// triggers and volatile expressions see the earlier rows as they do on heap.
fn insert_single<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut types_slot::SlotData<'mcx>,
    mycid: types_core::CommandId,
    ti_options: i32,
    bistate: &mut tableam_vocab::BulkInsertStateData,
) -> PgResult<()> {
    match is_pgrc(rel) {
        true => tableam::table_tuple_insert(mcx, rel, slot, mycid, 0, None),
        false => tableam::table_tuple_insert(mcx, rel, slot, mycid, ti_options, Some(bistate)),
    }
}

fn copy_from_body<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    ti_options: i32,
    mut trig: Option<&mut CopyTrig<'_, 'mcx>>,
    // Opened by the caller BEFORE the BS triggers fired (C copyfrom.c:924).
    index_state: &mut execindexing::ResultRelIndexState<'mcx>,
    mycid: types_core::CommandId,
    // Some = the target is a foreign table: rows go to ExecForeignInsert
    // one at a time (CIM_SINGLE, copyfrom.c:1007-1016 with ri_BatchSize 1).
    mut foreign: Option<&mut CopyForeign<'mcx>>,
) -> PgResult<u64> {

    // The DoCopy perminfo's insertedCols (copy.c): constraint-error DETAILs
    // always include the columns the user provided data for (execMain.c
    // ExecBuildSlotValueDescription).
    let inserted_cols = {
        const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
        let mut b = types_nodes::Bitmapset::empty();
        for &a in cstate.attnumlist.iter() {
            b.add_member(mcx, a as i32 - FLIHAN)?;
        }
        b
    };
    let mut qualexpr = init_where_qual(mcx, cstate)?;
    let has_br = trig.as_ref().is_some_and(|t| t.td.trig_insert_before_row);
    let has_ir = trig.as_ref().is_some_and(|t| t.td.trig_insert_instead_row);
    // CIM_SINGLE arms of copyfrom.c:996-1052: BEFORE/INSTEAD row triggers and
    // volatile expressions may query the table mid-load, so every row must be
    // visible as soon as stored.
    // TRIGGERS: rows of a pgrcolumnar target that an AFTER ROW INSERT trigger or a NEW TABLE capture fetches back
    // need positions, which the bulk lane's parts do not have until publish: those go row by row into the segment.
    let pgrc_positions = is_pgrc(rel)
        && ::pgrc_front::route::needs_position(trig.as_ref().map(|t| &**t.td), trig.as_ref().and_then(|t| t.tc).is_some_and(|tc| tc.tcs_insert_new_table));
    let volatile_single = cstate.volatile_defexprs || where_clause_volatile(cstate)?;
    if volatile_single {
        if let Some(f) = foreign.as_deref_mut() {
            copy_foreign_single(f);
        }
    }
    let single_insert = volatile_single || has_br || has_ir || foreign.is_some() || pgrc_positions;
    let mut single_eval_cx = MemoryContext::new_wholesale("CopySingleInsertEval");
    let mut input_row_cx = MemoryContext::new_wholesale("CopyPerRowInput");
    let mut check_exprs = None;
    // C ri_PartitionCheckExpr: compiled once per COPY, on first use.
    let mut partition_check: Option<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>> = None;

    let has_generated_stored =
        rel.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored);
    let mut generated_exprs = None;
    let mut virtual_nn_exprs = None;

    // std Vec: SlotData owns droppy state via the arena-erased views; the
    // slot pool itself is per-statement (CopyMultiInsertBuffer.slots).
    let mut slots: Vec<types_slot::SlotData<'mcx>> = Vec::new();
    let mut linenos: Vec<u64> = Vec::new();
    let mut bistate = heapam::GetBulkInsertState();
    let mut nused = 0usize;
    let mut buffered_bytes = 0usize;

    let mut processed: u64 = 0;
    let mut excluded: i64 = 0;
    let mut flushed: i64 = 0;
    loop {
        postgres_seams::check_for_interrupts::call()?;

        if nused == slots.len() {
            slots.push(tableam::table_slot_create(mcx, rel)?);
            linenos.push(0);
        }
        let slot = &mut slots[nused];
        exectuples::exec_clear_tuple(slot, mcx);

        // Buffered slots own materialized copies before the input context resets.
        input_row_cx.reset();
        {
            let base = slot.base_mut();
            if !next_copy_row(cstate, mcx, input_row_cx.mcx(), &mut base.tts_values, &mut base.tts_isnull)? {
                break;
            }
        }
        if cstate.escontext.as_ref().is_some_and(|n| n.ctx.error_occurred()) {
            cstate.escontext.as_mut().unwrap().ctx.reset_error_occurred();
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_SKIPPED, cstate.num_errors as i64);
            if cstate.opts.reject_limit > 0 && cstate.num_errors > cstate.opts.reject_limit as u64
            {
                return Err(reject_limit_exceeded(cstate.opts.reject_limit));
            }
            continue;
        }

        exectuples::exec_store_virtual_tuple(slot);
        slot.base_mut().tts_tableOid = rel.rd_id;

        if let Some(qual) = qualexpr.as_deref_mut() {
            let mut eval = execexpr::EvalSlots { scan: Some(slot), inner: None, outer: None };
            // SAFETY: input_row_cx is stable and live until the result is consumed;
            // restore the statement context before either success or error leaves this scope.
            unsafe { qual.arm_result_mcx_raw(input_row_cx.mcx()) };
            let accepted = execexpr::exec_qual(Some(qual), &mut eval);
            qual.arm_result_mcx(mcx);
            if !accepted? {
                excluded += 1;
                pgstat_progress_update_param(PROGRESS_COPY_TUPLES_EXCLUDED, excluded);
                continue;
            }
        }

        // BEFORE ROW INSERT triggers (copyfrom.c:1327-1331); a NULL return
        // suppresses the row and it is not counted.
        if has_br {
            let t = trig.as_deref_mut().expect("has_br implies trig");
            let mut when =
                trigger::TriggerWhenEval { mcx, cache: &mut *t.when, modified_cols: None };
            // The trigger call runs in the per-row context (copyfrom.c:1327
            // under GetPerTupleMemoryContext); the slot keeps its own copy.
            if !trigger::ExecBRInsertTriggersIn(
                mcx,
                input_row_cx.mcx(),
                rel,
                &**t.td,
                t.fmgr,
                &mut when,
                slot,
            )? {
                continue;
            }
        }

        // An INSTEAD OF INSERT row trigger owns the row; nothing is stored
        // (copyfrom.c:1340-1343).
        if has_ir {
            let t = trig.as_deref_mut().expect("has_ir implies trig");
            let mut when =
                trigger::TriggerWhenEval { mcx, cache: &mut *t.when, modified_cols: None };
            trigger::ExecIRInsertTriggersIn(
                mcx,
                input_row_cx.mcx(),
                rel,
                &**t.td,
                t.fmgr,
                &mut when,
                slot,
            )?;
            processed += 1;
            flushed += 1;
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
            continue;
        }

        if has_generated_stored {
            nodemodifytable::exec_compute_stored_generated_in_row(
                mcx, input_row_cx.mcx(), &mut generated_exprs, rel, slot,
            )?;
        }

        // ExecConstraints (copyfrom.c:1352-1358): NOT NULL + CHECK, on a
        // plain table only (ri_FdwRoutine == NULL arm; the remote checks a
        // foreign table's constraints).
        if foreign.is_none() {
            nodemodifytable::exec_constraints_in_row(
                mcx,
                input_row_cx.mcx(),
                &mut check_exprs,
                &mut virtual_nn_exprs,
                rel,
                slot,
                None,
                Some(&inserted_cols),
            )?;
        }

        // ExecPartitionCheck (copyfrom.c:1361-1368): a COPY aimed directly at
        // a partition has no tuple routing (proute == NULL), so every row is
        // checked against the partition constraint, BR trigger or not.
        if rel.rd_rel.relispartition
            && !execpartition::exec_partition_check_in(mcx, input_row_cx.mcx(), &mut partition_check, rel, slot)?
        {
            return Err(execpartition::partition_constraint_violation(
                mcx,
                rel,
                slot,
                Some(&inserted_cols),
                None,
            ));
        }

        // ExecForeignInsert (copyfrom.c:1404-1420): the FDW reads the row
        // from its estate's tuple table, so the slot rides there for the
        // call; a NULL return is "do nothing" and the row is not counted.
        if let Some(f) = foreign.as_deref_mut() {
            single_eval_cx.reset();
            if !copy_foreign_insert(cstate, f, &mut slots[nused])? {
                continue;
            }
            // AFTER ROW triggers may reference tableoid (copyfrom.c:1418).
            slots[nused].base_mut().tts_tableOid = rel.rd_id;
            if let Some(t) = trig.as_deref_mut() {
                if t.td.trig_insert_after_row {
                    // AfterTriggerSaveEvent's foreign-table arm: the row
                    // image is spooled from the slot, there is no ctid.
                    let fetched = exectuples::exec_fetch_slot_heap_tuple(
                        &mut slots[nused],
                        true,
                        mcx,
                        single_eval_cx.mcx(),
                    )?;
                    let new_t: &types_tuple::HeapTupleData<'_> = match &fetched {
                        exectuples::FetchedHeapTuple::Slot(t) => t,
                        exectuples::FetchedHeapTuple::Copied(t) => t,
                    };
                    let mut when = trigger::TriggerWhenEval {
                        mcx,
                        cache: &mut *t.when,
                        modified_cols: None,
                    };
                    trigger::ExecARRowTriggersFdw(
                        mcx,
                        rel,
                        t.td,
                        types_trigger::TRIGGER_EVENT_INSERT,
                        None,
                        Some(new_t),
                        Some(&mut when),
                        None,
                    )?;
                }
            }
            processed += 1;
            flushed += 1;
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
            continue;
        }

        if single_insert {
            single_eval_cx.reset();
            insert_single(mcx, rel, slot, mycid, ti_options, &mut bistate)?;
            let recheck_indexes = if index_state.num_indices() > 0 {
                execindexing::ExecInsertIndexTuples(
                    mcx,
                    single_eval_cx.mcx(),
                    index_state,
                    rel,
                    slot,
                    None,
                    false,
                    None,
                    &[],
                    false,
                )?
            } else {
                PgVec::new_in(mcx)
            };
            if let Some(t) = trig.as_deref_mut() {
                let mut when =
                    trigger::TriggerWhenEval { mcx, cache: &mut *t.when, modified_cols: None };
                trigger::ExecARInsertTriggers(
                    mcx,
                    rel,
                    Some(t.td),
                    slot.base().tts_tid,
                    &recheck_indexes,
                    t.tc,
                    Some(&mut when),
                    // COPY's target rel is not a child result rel here, so
                    // there is no child->root capture map (C ri_ChildToRootMap
                    // is NULL for a non-child result relation).
                    None,
                )?;
            }
            processed += 1;
            flushed += 1;
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
            continue;
        }

        exectuples::exec_materialize_slot(slot, mcx)?;
        linenos[nused] = cstate.cur_lineno;
        nused += 1;
        buffered_bytes += cstate.line_buf.len();
        processed += 1;

        if nused >= MAX_BUFFERED_TUPLES || buffered_bytes >= MAX_BUFFERED_BYTES {
            flush_multi_insert(
                mcx,
                cstate,
                rel,
                &mut slots[..nused],
                &linenos[..nused],
                mycid,
                ti_options,
                &mut bistate,
                index_state,
                trig.as_deref_mut(),
            )?;
            flushed += nused as i64;
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
            nused = 0;
            buffered_bytes = 0;
        }
    }

    if nused > 0 {
        flush_multi_insert(
            mcx,
            cstate,
            rel,
            &mut slots[..nused],
            &linenos[..nused],
            mycid,
            ti_options,
            &mut bistate,
            index_state,
            trig.as_deref_mut(),
        )?;
        flushed += nused as i64;
        pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
    }
    if let Some(f) = foreign.as_deref_mut() {
        processed += flush_copy_foreign(cstate, f)?;
    }

    // A view target (INSTEAD OF INSERT trigger) has no table AM; C never
    // reaches table AM calls on that path (copyfrom.c:1340-1343).
    if rel.rd_rel.relkind == RELKIND_RELATION {
        tableam::table_finish_bulk_insert(rel, ti_options)?;
        pgrc_index_placed(mcx, rel, &mut *index_state)?;
    }

    Ok(processed)
}

// The FDW half of C's target resultRelInfo for a foreign COPY target
// (copyfrom.c:915-941): the estate BeginForeignInsert/ExecForeignInsert read
// the range table and the row slot from, and the ri_FdwState the provider
// returned. C's mtstate is plan-less (ps.plan = NULL) and only carries the
// estate; the empty ModifyTable stands in for it.
pub(crate) struct CopyForeign<'mcx> {
    estate: executils::EStateData<'mcx>,
    slot: executils::ExecSlotId,
    routine: &'static nodemodifytable::FdwModifyRoutine,
    state: Box<dyn core::any::Any>,
    // ri_BatchSize > 1: rows reach the remote through ExecForeignBatchInsert,
    // whose failures carry only "COPY <rel>" (copyfrom.c:466-470).
    batched: bool,
}

// ExecForeignBatchInsert's flush of the rows the provider buffered
// (copyfrom.c:483-522): the rows the remote reports inserted are counted.
fn flush_copy_foreign(cstate: &mut CopyFromState<'_, '_>, f: &mut CopyForeign<'_>) -> PgResult<u64> {
    if let Some(flush) = f.routine.flush {
        cstate.relname_only = f.batched;
        flush(f.state.as_mut(), &mut f.estate)?;
        cstate.relname_only = false;
    }
    Ok(core::mem::take(&mut f.estate.es_processed))
}

// ExecForeignInsert, or the batch buffering that stands in for C's
// CopyMultiInsertInfoStore + ExecForeignBatchInsert; an error leaves
// relname_only set for the context the caller attaches.
fn copy_foreign_insert<'mcx>(
    cstate: &mut CopyFromState<'_, '_>,
    f: &mut CopyForeign<'mcx>,
    slot: &mut types_slot::SlotData<'mcx>,
) -> PgResult<bool> {
    let sid = f.slot.0 as usize;
    core::mem::swap(slot, &mut f.estate.es_tupleTable[sid]);
    cstate.relname_only = f.batched;
    let inserted = (f.routine.exec_insert)(f.state.as_mut(), &mut f.estate, f.slot, f.slot);
    core::mem::swap(slot, &mut f.estate.es_tupleTable[sid]);
    let inserted = inserted?;
    cstate.relname_only = false;
    Ok(inserted)
}

// CIM_SINGLE (copyfrom.c:995-1052): rows reach the remote one at a time
// through ExecForeignInsert even when the FDW could batch.
fn copy_foreign_single(f: &mut CopyForeign<'_>) {
    if let Some(disable) = f.routine.disable_batching {
        disable(f.state.as_mut());
    }
    f.batched = false;
}

fn begin_copy_foreign_insert<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
) -> PgResult<CopyForeign<'mcx>> {
    let kind = foreigncmds_seams::get_fdw_routine_by_rel_id::call(mcx, rel.rd_id)?;
    // CheckValidResultRel(resultRelInfo, CMD_INSERT, ...) (copyfrom.c:921).
    nodemodifytable::check_valid_foreign_result_rel(mcx, rel, CmdType::CMD_INSERT, kind)?;
    let routine =
        nodemodifytable::fdw_modify_routine(kind).expect("CheckValidResultRel admitted the FDW");
    // BeginCopyFrom's range table (copyfrom.c:1497-1504): the target's own
    // RTE and perminfo, ExecInitRangeTable'd into CopyFrom's estate
    // (copyfrom.c:915-918); ExecInitResultRelation opens it there.
    let mut pstate = parser_small1::make_parsestate(mcx, None);
    parse_relation::addRangeTableEntryForRelation(
        mcx,
        &mut pstate,
        rel,
        types_rel::lock::RowExclusiveLock,
        None,
        false,
        false,
    )?;
    let range_table = mcx::alloc_leak_in(mcx, core::mem::replace(&mut pstate.p_rtable, NodeList::nil()))?;
    let perminfos =
        mcx::alloc_leak_in(mcx, core::mem::replace(&mut pstate.p_rteperminfos, NodeList::nil()))?;
    parser_small1::free_parsestate(pstate)?;
    let mut estate = executils::EStateData::new_in(mcx);
    let mut unpruned = types_nodes::Bitmapset::empty();
    unpruned.add_member(mcx, 1)?;
    estate.exec_init_range_table(range_table, perminfos, unpruned)?;
    estate.exec_get_range_table_relation(1, true)?;
    let slot = estate.exec_init_extra_tuple_slot(
        Some(rel.rd_att.clone()),
        types_slot::TupleSlotKind::Virtual,
    );
    let node = mcx::alloc_leak_in(
        mcx,
        types_nodes::plannodes::ModifyTable {
            operation: CmdType::CMD_INSERT,
            canSetTag: true,
            ..Default::default()
        },
    )?;
    // A provider whose BeginForeignInsert is NULL, or that returns no
    // ri_FdwState, has nothing ExecForeignInsert can run against; both
    // in-tree providers are on the other side of this (postgres_fdw begins,
    // file_fdw failed CheckValidResultRel above).
    let no_state = || {
        Box::new(
            PgError::error(format!("cannot insert into foreign table \"{}\"", rel.name()))
                .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
        )
    };
    let Some(begin) = routine.begin_insert else {
        return Err(no_state());
    };
    let empty = NodeList::nil();
    let Some(state) = begin(node, &mut estate, rel, 1, 1, &empty, &empty, false)? else {
        return Err(no_state());
    };
    let batched = routine.flush.is_some() && routine.batch_size.is_some_and(|b| b(state.as_ref()) > 1);
    Ok(CopyForeign { estate, slot, routine, state, batched })
}

pub fn next_copy_row<'mcx>(
    cstate: &mut CopyFromState<'mcx, '_>,
    statement_mcx: Mcx<'mcx>,
    row_mcx: Mcx<'_>,
    values: &mut [datum::Datum],
    nulls: &mut [bool],
) -> PgResult<bool> {
    for state in cstate.defexprs.iter_mut().flatten() {
        // SAFETY: the caller keeps row_mcx stable and live through evaluation
        // and materializes accepted values before resetting it. Re-arm on errors too.
        unsafe { state.arm_result_mcx_raw(row_mcx) };
    }
    let result = cstate.next_copy_from(row_mcx, values, nulls);
    for state in cstate.defexprs.iter_mut().flatten() {
        state.arm_result_mcx(statement_mcx);
    }
    result
}

// The whereClause volatility arm of C's insert-method selection
// (copyfrom.c:1041).
fn where_clause_volatile(cstate: &CopyFromState<'_, '_>) -> PgResult<bool> {
    for wc in cstate.where_clause.iter() {
        if clauses::contain_volatile_functions(wc)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn init_where_qual<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &CopyFromState<'mcx, '_>,
) -> PgResult<Option<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>>> {
    let mut qualexpr =
        execexpr::exec_init_qual(mcx, &cstate.where_clause, execexpr::ParamBind::NONE)?;
    if let Some(q) = qualexpr.as_mut() {
        q.arm_result_mcx(mcx);
    }
    Ok(qualexpr)
}

fn skipped_rows_notice(cstate: &CopyFromState<'_, '_>) -> PgResult<()> {
    if cstate.num_errors > 0
        && cstate.opts.log_verbosity >= crate::CopyLogVerbosityChoice::Default
    {
        let n = cstate.num_errors;
        let msg = if n == 1 {
            format!("{n} row was skipped due to data type incompatibility")
        } else {
            format!("{n} rows were skipped due to data type incompatibility")
        };
        ereport(types_error::NOTICE).errmsg(msg).finish(loc("CopyFrom"))?;
    }
    Ok(())
}

// CopyMultiInsertBuffer, plain-table arm (foreign partitions are loud).
struct PartBuffer<'mcx> {
    leaf: usize,
    // std Vec: SlotData owns droppy arena-erased views; the pool lives for
    // the statement (CopyMultiInsertBuffer.slots).
    slots: Vec<types_slot::SlotData<'mcx>>,
    linenos: Vec<u64>,
    nused: usize,
    bistate: tableam_vocab::BulkInsertStateData,
}

// copyfrom.c MAX_PARTITION_BUFFERS: trim to this many after each flush.
const MAX_PARTITION_BUFFERS: usize = 32;

// The trigger slice of a routed leaf's resultRelInfo (ExecFindPartition's
// ExecInitPartitionInfo leg): trigdesc, per-rel caches, the leaf->root
// attmap for transition capture (C ri_ChildToRootMap), and C's
// leafpart_use_multi_insert (copyfrom.c:1232-1236).
struct LeafTrig<'mcx> {
    td: Option<std::rc::Rc<types_trigger::TriggerDesc<'static>>>,
    fmgr: trigger::TriggerFmgrCache,
    when: trigger::TriggerWhenCache<'mcx>,
    c2r: Option<mcx::PgVec<'mcx, i16>>,
    pcheck: Option<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>>,
    // Per-leaf compile caches: stored generation expressions
    // (ExecComputeStoredGenerated) and virtual-generated NOT NULL probes.
    gen: Option<mcx::PgVec<'mcx, nodemodifytable::GeneratedExpr<'mcx>>>,
    vnn: Option<mcx::PgVec<'mcx, nodemodifytable::VirtualNnExpr<'mcx>>>,
    use_multi: bool,
    // ExecInitRoutingInfo's BeginForeignInsert on a foreign leaf
    // (execPartition.c:1032-1034); rows go through ExecForeignInsert.
    foreign: Option<CopyForeign<'mcx>>,
}

// CopyFrom, partitioned arm: CIM_MULTI_CONDITIONAL (each routed leaf batches
// through its own CopyMultiInsertBuffer) with the CIM_SINGLE fallbacks of
// copyfrom.c:995-1052 — force_single covers the target's BR/IR triggers and
// transition tables, volatile defaults/WHERE are computed here, and a leaf
// with BR/IR triggers takes the single-insert arm per row.
fn copy_from_partitioned_body<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    ti_options: i32,
    force_single: bool,
    transition_capture: Option<&trigger::TransitionCaptureState>,
    mycid: types_core::CommandId,
    leaf_indexes: &mut Vec<Option<execindexing::ResultRelIndexState<'mcx>>>,
) -> PgResult<u64> {

    // The DoCopy perminfo's insertedCols (copy.c): constraint-error DETAILs
    // always include the columns the user provided data for (execMain.c
    // ExecBuildSlotValueDescription), root-numbered.
    let inserted_cols = {
        const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
        let mut b = types_nodes::Bitmapset::empty();
        for &a in cstate.attnumlist.iter() {
            b.add_member(mcx, a as i32 - FLIHAN)?;
        }
        b
    };
    let mut qualexpr = init_where_qual(mcx, cstate)?;
    // C CopyFrom's estate (CreateExecutorState) carries the routing's
    // es_partition_directory; freed with it at the end (FreeExecutorState).
    let mut partition_directory: Option<execpartition::PartitionDirectory<'mcx>> = None;
    let mut router =
        execpartition::PartitionTupleRouting::new(mcx, rel, &mut partition_directory)?;
    // C GetPerTupleExprContext: expression partition keys evaluate here,
    // reset per row.
    let mut route_eval_cx = MemoryContext::new_wholesale("CopyRouteEvalPerTuple");
    let mut input_row_cx = MemoryContext::new_wholesale("CopyPerRowInput");
    let mut rootslot = tableam::table_slot_create(mcx, rel)?;
    let mut buffers: Vec<PartBuffer<'mcx>> = Vec::new();
    let mut leaf_checks: Vec<Option<mcx::PgVec<'mcx, nodemodifytable::CheckExpr<'mcx>>>> =
        Vec::new();
    let mut buffered_tuples = 0usize;
    let mut buffered_bytes = 0usize;
    let mut processed: u64 = 0;
    let mut excluded: i64 = 0;
    let mut flushed: i64 = 0;
    // CIM_SINGLE arms of copyfrom.c:1028-1052; the non-batch bistate exists
    // for CIM_MULTI_CONDITIONAL too (copyfrom.c:1083), pin released on
    // partition switch (copyfrom.c:1258-1260).
    let single_insert =
        force_single || cstate.volatile_defexprs || where_clause_volatile(cstate)?;
    let mut single_bistate = heapam::GetBulkInsertState();
    let mut prev_leaf: Option<usize> = None;
    // es_insert_pending_result_relations: foreign leaves holding a buffered
    // batch, in first-buffered order.
    let mut pending_foreign: mcx::PgVec<'mcx, usize> = mcx::PgVec::new_in(mcx);
    let mut leaf_slots: Vec<Option<types_slot::SlotData<'mcx>>> = Vec::new();
    // Slots of evicted partition buffers, per leaf: C frees them
    // (CopyMultiInsertBufferCleanup); the query arena can't, so a re-created
    // buffer reuses them instead of allocating anew.
    let mut spare_slots: Vec<Vec<types_slot::SlotData<'mcx>>> = Vec::new();
    let mut leaf_trig: Vec<Option<LeafTrig<'mcx>>> = Vec::new();
    // D3313: the leaves the single-insert arm wrote, finished at statement end like the buffered leaves (below).
    let mut single_leaves: Vec<usize> = Vec::new();

    loop {
        postgres_seams::check_for_interrupts::call()?;

        exectuples::exec_clear_tuple(&mut rootslot, mcx);
        input_row_cx.reset();
        {
            let base = rootslot.base_mut();
            if !next_copy_row(cstate, mcx, input_row_cx.mcx(), &mut base.tts_values, &mut base.tts_isnull)? {
                break;
            }
        }
        if cstate.escontext.as_ref().is_some_and(|n| n.ctx.error_occurred()) {
            cstate.escontext.as_mut().unwrap().ctx.reset_error_occurred();
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_SKIPPED, cstate.num_errors as i64);
            if cstate.opts.reject_limit > 0 && cstate.num_errors > cstate.opts.reject_limit as u64
            {
                return Err(reject_limit_exceeded(cstate.opts.reject_limit));
            }
            continue;
        }

        exectuples::exec_store_virtual_tuple(&mut rootslot);
        rootslot.base_mut().tts_tableOid = rel.rd_id;

        if let Some(qual) = qualexpr.as_deref_mut() {
            let mut eval =
                execexpr::EvalSlots { scan: Some(&mut rootslot), inner: None, outer: None };
            // SAFETY: input_row_cx is stable and live until the result is consumed;
            // restore the statement context before either success or error leaves this scope.
            unsafe { qual.arm_result_mcx_raw(input_row_cx.mcx()) };
            let accepted = execexpr::exec_qual(Some(qual), &mut eval);
            qual.arm_result_mcx(mcx);
            if !accepted? {
                excluded += 1;
                pgstat_progress_update_param(PROGRESS_COPY_TUPLES_EXCLUDED, excluded);
                continue;
            }
        }

        route_eval_cx.reset();
        let leaf = router.find_partition(
            &mut rootslot,
            route_eval_cx.mcx(),
            &mut partition_directory,
            Some(&inserted_cols),
        )?;
        if leaf_checks.len() <= leaf {
            let needed = leaf + 1 - leaf_checks.len();
            leaf_checks
                .try_reserve(needed)
                .map_err(|_| mcx.oom(needed * core::mem::size_of::<Option<mcx::PgVec<'mcx, nodemodifytable::CheckExpr<'mcx>>>>()))?;
            leaf_checks.resize_with(leaf + 1, || None);
            let needed_idx = leaf + 1 - leaf_indexes.len();
            leaf_indexes
                .try_reserve(needed_idx)
                .map_err(|_| mcx.oom(needed_idx * core::mem::size_of::<Option<execindexing::ResultRelIndexState<'mcx>>>()))?;
            leaf_indexes.resize_with(leaf + 1, || None);
        }
        if leaf_trig.len() <= leaf {
            let needed = leaf + 1 - leaf_trig.len();
            leaf_trig
                .try_reserve(needed)
                .map_err(|_| mcx.oom(needed * core::mem::size_of::<Option<LeafTrig<'mcx>>>()))?;
            leaf_trig.resize_with(leaf + 1, || None);
        }
        if leaf_trig[leaf].is_none() {
            let lrel = router.leaf_rel(leaf);
            let foreign = if lrel.rd_rel.relkind == types_rel::RELKIND_FOREIGN_TABLE {
                let mut f = begin_copy_foreign_insert(mcx, lrel)?;
                if single_insert {
                    copy_foreign_single(&mut f);
                }
                Some(f)
            } else {
                None
            };
            let td = if lrel.rd_hastriggers {
                relcache::RelationGetTriggerDesc(lrel.rd_id)?
            } else {
                None
            };
            // C leafpart_use_multi_insert (copyfrom.c:1232-1236): a leaf with
            // BEFORE/INSTEAD row triggers, or a foreign leaf (ri_BatchSize 1),
            // takes the single-insert arm.
            // TRIGGERS: a pgrcolumnar leaf's AFTER ROW INSERT triggers fetch its rows back by position, which the
            // bulk lane's parts do not have until publish: the single-insert arm (into the segment).
            let use_multi = foreign.is_none()
                && !td.as_ref().is_some_and(|t| {
                    t.trig_insert_before_row || t.trig_insert_instead_row
                })
                && !(is_pgrc(lrel) && ::pgrc_front::route::needs_position(td.as_deref(), false));
            let c2r =
                tupdesc::build_attrmap_by_name_if_req(mcx, &lrel.rd_att, &rel.rd_att, false)?;
            leaf_trig[leaf] = Some(LeafTrig {
                td,
                fmgr: trigger::TriggerFmgrCache::default(),
                when: trigger::TriggerWhenCache::default(),
                c2r,
                pcheck: None,
                gen: None,
                vnn: None,
                use_multi,
                foreign,
            });
            // C ExecInitPartitionInfo opens the leaf's indexes when the leaf
            // ResultRelInfo is built — BEFORE the leaf's BR triggers fire
            // (execPartition.c): the open pins the index relations, so a
            // trigger-run DROP INDEX fails CheckTableNotInUse as in C, and
            // the list is read pre-trigger (stale-held-rd_rel audit).
            if leaf_indexes[leaf].is_none() {
                leaf_indexes[leaf] = Some(execindexing::ExecOpenIndices(mcx, lrel, false)?);
            }
        }
        let leaf_use_multi = !single_insert
            && leaf_trig[leaf].as_ref().expect("leaf initialized").use_multi;

        if prev_leaf != Some(leaf) {
            // copyfrom.c:1245-1260: flush pending inserts before a
            // non-batchable partition's row so its triggers see them, and
            // release the bistate pin on partition switch. A foreign leaf
            // batches through its provider (ri_BatchSize > 1) unless its
            // BR triggers force single rows.
            let lt = leaf_trig[leaf].as_ref().expect("leaf initialized");
            if !leaf_use_multi
                && (lt.foreign.is_none()
                    || lt.td.as_ref().is_some_and(|t| t.trig_insert_before_row))
                && !pending_foreign.is_empty()
            {
                for idx in
                    core::mem::replace(&mut pending_foreign, mcx::PgVec::new_in(mcx)).iter()
                {
                    let lt = leaf_trig[*idx].as_mut().expect("pending leaf initialized");
                    processed += flush_copy_foreign(cstate, lt.foreign.as_mut().expect("foreign leaf"))?;
                }
            }
            if !leaf_use_multi && buffered_tuples > 0 {
                flushed += flush_part_buffers(
                    mcx,
                    cstate,
                    &router,
                    &mut buffers,
                    leaf_indexes,
                    &mut leaf_trig,
                    transition_capture,
                    mycid,
                    ti_options,
                )?;
                pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
                buffered_tuples = 0;
                buffered_bytes = 0;
            }
            heapam::ReleaseBulkInsertStatePin(&mut single_bistate);
            prev_leaf = Some(leaf);
        }

        if !leaf_use_multi {
            if !single_leaves.contains(&leaf) {
                single_leaves.push(leaf); // alloc: one entry per leaf of the statement
            }
            let lrel = router.leaf_rel(leaf);
            if leaf_slots.len() <= leaf {
                let needed = leaf + 1 - leaf_slots.len();
                leaf_slots
                    .try_reserve(needed)
                    .map_err(|_| mcx.oom(needed * core::mem::size_of::<Option<types_slot::SlotData<'mcx>>>()))?;
                leaf_slots.resize_with(leaf + 1, || None);
            }
            let use_slot: &mut types_slot::SlotData<'mcx> = match router.leaf_attrmap(leaf) {
                Some(map) => {
                    if leaf_slots[leaf].is_none() {
                        leaf_slots[leaf] = Some(tableam::table_slot_create(mcx, lrel)?);
                    }
                    let ls = leaf_slots[leaf].as_mut().expect("just created");
                    exectuples::exec_clear_tuple(ls, mcx);
                    exectuples::execute_attr_map_slot(map, &mut rootslot, ls, mcx);
                    ls
                }
                None => &mut rootslot,
            };
            use_slot.base_mut().tts_tableOid = lrel.rd_id;
            let LeafTrig { td, fmgr, when: when_cache, c2r, pcheck, gen, vnn, foreign, .. } =
                leaf_trig[leaf].as_mut().expect("leaf initialized");
            let has_br = td.as_ref().is_some_and(|t| t.trig_insert_before_row);
            // BEFORE ROW INSERT triggers on the leaf (copyfrom.c:1327-1331);
            // a NULL return suppresses the row and it is not counted.
            if has_br {
                let mut when =
                    trigger::TriggerWhenEval { mcx, cache: &mut *when_cache, modified_cols: None };
                if !trigger::ExecBRInsertTriggersIn(
                    mcx,
                    input_row_cx.mcx(),
                    lrel,
                    td.as_deref().expect("has_br implies trigdesc"),
                    fmgr,
                    &mut when,
                    use_slot,
                )? {
                    continue;
                }
            }
            // Compute stored generated columns on the leaf (copyfrom.c:1345-1350).
            if lrel.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored) {
                nodemodifytable::exec_compute_stored_generated_in_row(
                    mcx, input_row_cx.mcx(), gen, lrel, use_slot,
                )?;
            }
            // copyfrom.c:1357: a foreign leaf's constraints are the remote's.
            if foreign.is_none() {
                nodemodifytable::exec_constraints_in_row(
                    mcx,
                    input_row_cx.mcx(),
                    &mut leaf_checks[leaf],
                    vnn,
                    lrel,
                    use_slot,
                    Some(rel),
                    Some(&inserted_cols),
                )?;
            }
            // ExecPartitionCheck (copyfrom.c:1361-1368): a routed row is
            // re-checked only when a BR trigger could have moved it.
            if has_br && !execpartition::exec_partition_check_in(mcx, input_row_cx.mcx(), pcheck, lrel, use_slot)? {
                return Err(execpartition::partition_constraint_violation(
                    mcx,
                    lrel,
                    use_slot,
                    Some(&inserted_cols),
                    Some(rel),
                ));
            }
            // ExecForeignInsert on a routed foreign leaf (copyfrom.c:1408-
            // 1420): the row rides in the FDW's estate for the call.
            if let Some(f) = foreign.as_mut() {
                if !copy_foreign_insert(cstate, f, use_slot)? {
                    if f.routine.flush.is_some() && !pending_foreign.contains(&leaf) {
                        pending_foreign.push(leaf);
                    }
                    continue;
                }
                use_slot.base_mut().tts_tableOid = lrel.rd_id;
                trigger::check_foreign_transition_capture(
                    lrel,
                    transition_capture.is_some_and(|tc| tc.tcs_insert_new_table),
                )?;
                if let Some(t) = td.as_ref().filter(|t| t.trig_insert_after_row) {
                    let fetched = exectuples::exec_fetch_slot_heap_tuple(
                        use_slot,
                        true,
                        mcx,
                        route_eval_cx.mcx(),
                    )?;
                    let new_t: &types_tuple::HeapTupleData<'_> = match &fetched {
                        exectuples::FetchedHeapTuple::Slot(t) => t,
                        exectuples::FetchedHeapTuple::Copied(t) => t,
                    };
                    let mut when = trigger::TriggerWhenEval {
                        mcx,
                        cache: &mut *when_cache,
                        modified_cols: None,
                    };
                    trigger::ExecARRowTriggersFdw(
                        mcx,
                        lrel,
                        t,
                        types_trigger::TRIGGER_EVENT_INSERT,
                        None,
                        Some(new_t),
                        Some(&mut when),
                        None,
                    )?;
                }
                processed += 1;
                flushed += 1;
                pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
                continue;
            }
            insert_single(mcx, lrel, use_slot, mycid, ti_options, &mut single_bistate)?;
            if leaf_indexes[leaf].is_none() {
                leaf_indexes[leaf] = Some(execindexing::ExecOpenIndices(mcx, lrel, false)?);
            }
            let idx = leaf_indexes[leaf].as_mut().expect("just opened");
            let recheck_indexes = if idx.num_indices() > 0 {
                execindexing::ExecInsertIndexTuples(
                    mcx,
                    route_eval_cx.mcx(),
                    idx,
                    lrel,
                    use_slot,
                    None,
                    false,
                    None,
                    &[],
                    false,
                )?
            } else {
                PgVec::new_in(mcx)
            };
            // AFTER ROW INSERT triggers (copyfrom.c:1441); capture converts
            // leaf-format tuples through the leaf->root map.
            if td.is_some() || transition_capture.is_some() {
                let conv = c2r.as_ref().map(|map| trigger::ChildToRoot {
                    map,
                    child_desc: lrel.rd_att.as_ref(),
                    root_desc: rel.rd_att.as_ref(),
                });
                let mut when =
                    trigger::TriggerWhenEval { mcx, cache: &mut *when_cache, modified_cols: None };
                trigger::ExecARInsertTriggers(
                    mcx,
                    lrel,
                    td.as_ref(),
                    use_slot.base().tts_tid,
                    &recheck_indexes,
                    transition_capture,
                    Some(&mut when),
                    conv.as_ref(),
                )?;
            }
            processed += 1;
            flushed += 1;
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
            continue;
        }

        let bidx = match buffers.iter().position(|b| b.leaf == leaf) {
            Some(i) => i,
            None => {
                let slots = spare_slots.get_mut(leaf).map(core::mem::take).unwrap_or_default();
                let mut linenos = Vec::new();
                linenos
                    .try_reserve_exact(slots.len())
                    .map_err(|_| mcx.oom(slots.len() * core::mem::size_of::<u64>()))?;
                linenos.resize(slots.len(), 0);
                buffers.push(PartBuffer {
                    leaf,
                    slots,
                    linenos,
                    nused: 0,
                    bistate: heapam::GetBulkInsertState(),
                });
                buffers.len() - 1
            }
        };
        {
            let lrel = router.leaf_rel(leaf);
            let buf = &mut buffers[bidx];
            if buf.nused == buf.slots.len() {
                buf.slots.push(tableam::table_slot_create(mcx, lrel)?);
                buf.linenos.push(0);
            }
            let slot = &mut buf.slots[buf.nused];
            // ExecPrepareTupleRouting: attno-remapped leaves take the tuple
            // converted (then materialized off rootslot's memory).
            match router.leaf_attrmap(leaf) {
                Some(map) => {
                    exectuples::execute_attr_map_slot(map, &mut rootslot, slot, mcx);
                    exectuples::exec_materialize_slot(slot, mcx)?;
                }
                None => exectuples::exec_copy_slot(slot, &mut rootslot, mcx, mcx)?,
            }
            slot.base_mut().tts_tableOid = lrel.rd_id;
            let lt = leaf_trig[leaf].as_mut().expect("leaf initialized");
            // Compute stored generated columns on the leaf (copyfrom.c:1345-1350).
            if lrel.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored) {
                nodemodifytable::exec_compute_stored_generated_in_row(
                    mcx, input_row_cx.mcx(), &mut lt.gen, lrel, slot,
                )?;
            }
            nodemodifytable::exec_constraints_in_row(
                mcx,
                input_row_cx.mcx(),
                &mut leaf_checks[leaf],
                &mut lt.vnn,
                lrel,
                slot,
                Some(rel),
                Some(&inserted_cols),
            )?;
            // Routed rows skip ExecPartitionCheck (bound proven on descent;
            // the DEFAULT-partition re-check runs inside find_partition).
            buf.linenos[buf.nused] = cstate.cur_lineno;
            buf.nused += 1;
        }
        buffered_tuples += 1;
        buffered_bytes += cstate.line_buf.len();
        processed += 1;

        if buffered_tuples >= MAX_BUFFERED_TUPLES || buffered_bytes >= MAX_BUFFERED_BYTES {
            flushed += flush_part_buffers(
                mcx,
                cstate,
                &router,
                &mut buffers,
                leaf_indexes,
                &mut leaf_trig,
                transition_capture,
                mycid,
                ti_options,
            )?;
            pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
            buffered_tuples = 0;
            buffered_bytes = 0;
            while buffers.len() > MAX_PARTITION_BUFFERS {
                if buffers[0].leaf == leaf {
                    let cur = buffers.remove(0);
                    buffers.push(cur);
                }
                let mut evicted = buffers.remove(0);
                tableam::table_finish_bulk_insert(router.leaf_rel(evicted.leaf), ti_options)?;
                if let Some(idx) = leaf_indexes[evicted.leaf].as_mut() {
                    pgrc_index_placed(mcx, router.leaf_rel(evicted.leaf), idx)?;
                }
                for slot in evicted.slots.iter_mut() {
                    exectuples::exec_clear_tuple(slot, mcx);
                }
                if spare_slots.len() <= evicted.leaf {
                    let needed = evicted.leaf + 1 - spare_slots.len();
                    spare_slots
                        .try_reserve(needed)
                        .map_err(|_| mcx.oom(needed * core::mem::size_of::<Vec<types_slot::SlotData<'mcx>>>()))?;
                    spare_slots.resize_with(evicted.leaf + 1, Vec::new);
                }
                spare_slots[evicted.leaf] = core::mem::take(&mut evicted.slots);
            }
        }
    }

    flushed += flush_part_buffers(
        mcx,
        cstate,
        &router,
        &mut buffers,
        leaf_indexes,
        &mut leaf_trig,
        transition_capture,
        mycid,
        ti_options,
    )?;
    pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, flushed);
    for buf in buffers.iter() {
        tableam::table_finish_bulk_insert(router.leaf_rel(buf.leaf), ti_options)?;
        if let Some(idx) = leaf_indexes[buf.leaf].as_mut() {
            pgrc_index_placed(mcx, router.leaf_rel(buf.leaf), idx)?;
        }
    }
    // D3313: table_tuple_insert into a leaf may buffer too (pgrcolumnar's writer), as copy_from_body's single arm does
    // into its target, which it finishes at its end: without this the leaf's rows are never sealed and its commit fails.
    // A leaf that buffered nothing (heap, or every row skipped by its trigger) finishes as a no-op.
    for &leaf in single_leaves.iter() {
        tableam::table_finish_bulk_insert(router.leaf_rel(leaf), ti_options)?;
    }

    for idx in pending_foreign.iter() {
        let lt = leaf_trig[*idx].as_mut().expect("pending leaf initialized");
        processed += flush_copy_foreign(cstate, lt.foreign.as_mut().expect("foreign leaf"))?;
    }
    // ExecCleanupTupleRouting (copyfrom.c:1504): the routed leaves' FDW
    // inserts (execPartition.c:1267-1272); their indexes close after
    // AfterTriggerEndQuery in the caller.
    for lt in leaf_trig.iter_mut().flatten() {
        if let Some(f) = lt.foreign.take() {
            (f.routine.end)(f.state)?;
        }
    }
    Ok(processed)
}

// CopyMultiInsertInfoFlush over every tracked buffer, in creation order.
#[allow(clippy::too_many_arguments)]
fn flush_part_buffers<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    router: &execpartition::PartitionTupleRouting<'mcx>,
    buffers: &mut [PartBuffer<'mcx>],
    leaf_indexes: &mut [Option<execindexing::ResultRelIndexState<'mcx>>],
    leaf_trig: &mut [Option<LeafTrig<'mcx>>],
    transition_capture: Option<&trigger::TransitionCaptureState>,
    mycid: types_core::CommandId,
    ti_options: i32,
) -> PgResult<i64> {
    let mut flushed: i64 = 0;
    for buf in buffers.iter_mut() {
        if buf.nused == 0 {
            continue;
        }
        let lrel = router.leaf_rel(buf.leaf);
        if leaf_indexes[buf.leaf].is_none() {
            leaf_indexes[buf.leaf] = Some(execindexing::ExecOpenIndices(mcx, lrel, false)?);
        }
        let index_state = leaf_indexes[buf.leaf].as_mut().expect("just opened");
        // AFTER ROW triggers on a batchable leaf fire at flush time
        // (CopyMultiInsertBufferFlush, copyfrom.c:562-598); transition
        // capture never reaches here — it forces the single-insert arm.
        let LeafTrig { td, fmgr, when, .. } =
            leaf_trig[buf.leaf].as_mut().expect("leaf initialized");
        let mut trig =
            td.as_ref().map(|td| CopyTrig { td, tc: transition_capture, when, fmgr });
        flush_multi_insert(
            mcx,
            cstate,
            lrel,
            &mut buf.slots[..buf.nused],
            &buf.linenos[..buf.nused],
            mycid,
            ti_options,
            &mut buf.bistate,
            index_state,
            trig.as_mut(),
        )?;
        flushed += buf.nused as i64;
        buf.nused = 0;
    }
    Ok(flushed)
}

// CopyMultiInsertBufferFlush (copyfrom.c), single non-partitioned table.
// Errors here report the buffered tuple's line number, not the read cursor's
// (C saves/restores cur_lineno and clears line_buf_valid around the flush).
#[allow(clippy::too_many_arguments)]
fn flush_multi_insert<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    slots: &mut [types_slot::SlotData<'mcx>],
    linenos: &[u64],
    mycid: types_core::CommandId,
    ti_options: i32,
    bistate: &mut tableam_vocab::BulkInsertStateData,
    index_state: &mut execindexing::ResultRelIndexState<'mcx>,
    mut trig: Option<&mut CopyTrig<'_, 'mcx>>,
) -> PgResult<()> {
    let save_cur_lineno = cstate.cur_lineno;
    let save_line_buf_valid = cstate.line_buf_valid;
    cstate.line_buf_valid = false;

    let mut refs: Vec<&mut types_slot::SlotData<'mcx>> = slots.iter_mut().collect();
    tableam::table_multi_insert(mcx, rel, &mut refs, mycid, ti_options, Some(bistate))?;

    if index_state.num_indices() > 0 || trig.is_some() {
        // C resets the per-tuple econtext per buffered row (CopyMultiInsertBufferFlush).
        let mut eval_cx = MemoryContext::new_wholesale("CopyIndexEvalPerTuple");
        for (i, slot) in refs.into_iter().enumerate() {
            eval_cx.reset();
            cstate.cur_lineno = linenos[i];
            // IXBULKSORT: a pgrcolumnar bulk row with no position yet waits for its placed ID (pgrc_index_placed)
            let placed_later = is_pgrc(rel) && !types_tuple::itemptr::ItemPointerIsValid(&slot.base().tts_tid);
            let recheck_indexes = if index_state.num_indices() > 0 && !placed_later {
                execindexing::ExecInsertIndexTuples(
                    mcx,
                    eval_cx.mcx(),
                    index_state,
                    rel,
                    slot,
                    None,
                    false,
                    None,
                    &[],
                    false,
                )?
            } else {
                PgVec::new_in(mcx)
            };
            if let Some(t) = trig.as_deref_mut() {
                let mut when =
                    trigger::TriggerWhenEval { mcx, cache: &mut *t.when, modified_cols: None };
                trigger::ExecARInsertTriggers(
                    mcx,
                    rel,
                    Some(t.td),
                    slot.base().tts_tid,
                    &recheck_indexes,
                    t.tc,
                    Some(&mut when),
                    // COPY's target rel is not a child result rel here, so
                    // there is no child->root capture map (C ri_ChildToRootMap
                    // is NULL for a non-child result relation).
                    None,
                )?;
            }
        }
    }

    pgrc_index_placed(mcx, rel, index_state)?;

    cstate.line_buf_valid = save_line_buf_valid;
    cstate.cur_lineno = save_cur_lineno;
    Ok(())
}

/// CopyFromErrorCallback installed around a caller's own `next_copy_row` loop
/// (file_fdw): NOTICE/WARNING reports raised while it lives carry the COPY
/// context line, as `copy_from_error_context` adds it to a propagated ERROR.
pub struct CopyFromEmitContext(u64);

impl CopyFromEmitContext {
    /// # Safety
    /// `cstate` must stay alive and unmoved until the guard is dropped.
    pub unsafe fn push(cstate: &CopyFromState<'_, '_>) -> Self {
        let cb_state = cstate as *const CopyFromState<'_, '_> as usize;
        CopyFromEmitContext(elog::push_emit_context_callback(Box::new(move |e| {
            // SAFETY: push's contract keeps `cstate` alive and in place.
            let cstate = unsafe { &*(cb_state as *const CopyFromState<'_, '_>) };
            if !cstate.relname_only {
                e.add_context_line(copy_from_error_context_text(cstate));
            }
        })))
    }
}

impl Drop for CopyFromEmitContext {
    fn drop(&mut self) {
        elog::pop_emit_context_callback(self.0);
    }
}

// CopyFromErrorCallback + CopyLimitPrintoutLength (copyfrom.c), text arm,
// attached on Err propagation instead of via error_context_stack.
#[cold]
#[inline(never)]
pub fn copy_from_error_context(
    cstate: &CopyFromState<'_, '_>,
    e: Box<PgError>,
) -> Box<PgError> {
    Box::new(e.add_context(copy_from_error_context_text(cstate)))
}

// file_fdw.c:789: CHECK_FOR_INTERRUPTS with CopyFromErrorCallback installed.
pub fn check_for_interrupts_in_copy(cstate: &CopyFromState<'_, '_>) -> PgResult<()> {
    postgres_seams::check_for_interrupts::call().map_err(|e| copy_from_error_context(cstate, e))
}

fn copy_from_error_context_text(cstate: &CopyFromState<'_, '_>) -> String {
    let relname = &cstate.relname;
    if cstate.relname_only {
        return format!("COPY {relname}");
    }
    let lineno = cstate.cur_lineno;
    if cstate.opts.parquet {
        // Parquet rows are not lines and carry no displayable raw text.
        return match cstate.cur_attidx {
            Some(m) => {
                let attname = cstate.attname(m);
                format!("COPY {relname}, row {lineno}, column {attname}")
            }
            None => format!("COPY {relname}, row {lineno}"),
        };
    }
    if cstate.opts.binary {
        // C's binary arm: the raw data is not usefully displayable.
        return match cstate.cur_attidx {
            Some(m) => {
                let attname = cstate.attname(m);
                format!("COPY {relname}, line {lineno}, column {attname}")
            }
            None => format!("COPY {relname}, line {lineno}"),
        };
    }
    match cstate.cur_attidx {
        Some(m) => {
            let attname = cstate.attname(m);
            match cstate.cur_attval_off {
                Some(off) => {
                    let attval = limit_printout_length(cstate.cur_attval_bytes(off));
                    format!("COPY {relname}, line {lineno}, column {attname}: \"{attval}\"")
                }
                None => {
                    format!("COPY {relname}, line {lineno}, column {attname}: null input")
                }
            }
        }
        None => {
            if cstate.line_buf_valid {
                let lineval = limit_printout_length(&cstate.line_buf);
                format!("COPY {relname}, line {lineno}: \"{lineval}\"")
            } else {
                format!("COPY {relname}, line {lineno}")
            }
        }
    }
}

const MAX_COPY_DATA_DISPLAY: i32 = 100;

pub(crate) fn limit_printout_length(bytes: &[u8]) -> String {
    let slen = bytes.len() as i32;
    if slen <= MAX_COPY_DATA_DISPLAY {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let len = mbutils::pg_mbcliplen(bytes, slen, MAX_COPY_DATA_DISPLAY) as usize;
    let mut s = String::from_utf8_lossy(&bytes[..len]).into_owned();
    s.push_str("...");
    s
}

/// `EndCopyFrom` (copyfrom.c).
pub fn EndCopyFrom(mut cstate: CopyFromState<'_, '_>) -> PgResult<()> {
    let parked = match &mut cstate.src {
        CopySrc::Chunk { parked, .. } => parked.take(),
        _ => None,
    };
    if let Some(src) = parked {
        cstate.src = *src;
    }
    if let CopySrc::File { fd, filename } = &cstate.src {
        if fd::FreeFile(*fd)? != 0 {
            ereport(ERROR)
                .with_saved_errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
                .errcode_for_file_access()
                .errmsg(format!("could not close file \"{filename}\": {PCT_M}"))
                .finish(loc("EndCopyFrom"))?;
        }
    }
    if let CopySrc::Program { fd, filename } = &cstate.src {
        close_pipe_from_program(*fd, filename, cstate.raw_reached_eof)?;
    }
    pgstat_progress_end_command();
    Ok(())
}

// ClosePipeFromProgram (copyfrom.c): pclose and check the wait status. A
// SIGPIPE death is expected — and not an error — when COPY FROM PROGRAM
// stopped reading before the child reached EOF.
// wasm32-wasip1's libc has no SIGPIPE (and no popen); carry the POSIX value.
#[cfg(not(target_family = "wasm"))]
const SIGPIPE: i32 = libc::SIGPIPE;
#[cfg(target_family = "wasm")]
const SIGPIPE: i32 = 13;

fn close_pipe_from_program(fd: i32, filename: &str, raw_reached_eof: bool) -> PgResult<()> {
    let pclose_rc = fd::ClosePipeStream(fd)?;
    if pclose_rc == -1 {
        ereport(ERROR)
            .with_saved_errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            .errcode_for_file_access()
            .errmsg(format!("could not close pipe to external command: {PCT_M}"))
            .finish(loc("EndCopyFrom"))?;
    } else if pclose_rc != 0 {
        if !raw_reached_eof && wait_error::wait_result_is_signal(pclose_rc, SIGPIPE) {
            return Ok(());
        }
        return Err(Box::new(
            PgError::error(format!("program \"{filename}\" failed"))
                .with_sqlstate(types_error::ERRCODE_EXTERNAL_ROUTINE_EXCEPTION)
                .with_detail(wait_error::wait_result_to_str(pclose_rc)),
        ));
    }
    Ok(())
}

#[track_caller]
#[cold]
#[inline(never)]
fn reject_limit_exceeded(limit: i64) -> Box<PgError> {
    Box::new(
        PgError::error(format!(
            "skipped more than REJECT_LIMIT ({limit}) rows due to data type incompatibility"
        ))
        .with_sqlstate(types_error::ERRCODE_INVALID_TEXT_REPRESENTATION),
    )
}

#[track_caller]
#[cold]
#[inline(never)]
fn cannot_copy_to_relkind(rel: &Relation<'_>) -> Box<PgError> {
    let name = rel.name();
    let (msg, hint): (String, Option<&str>) = match rel.rd_rel.relkind {
        b'v' => (
            format!("cannot copy to view \"{name}\""),
            Some("To enable copying to a view, provide an INSTEAD OF INSERT trigger."),
        ),
        b'm' => (format!("cannot copy to materialized view \"{name}\""), None),
        b'S' => (format!("cannot copy to sequence \"{name}\""), None),
        _ => (format!("cannot copy to non-table relation \"{name}\""), None),
    };
    let mut e = PgError::error(msg).with_sqlstate(ERRCODE_WRONG_OBJECT_TYPE);
    if let Some(h) = hint {
        e = e.with_hint(h);
    }
    Box::new(e)
}
