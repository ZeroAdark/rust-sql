use ::datum::Datum;
use ::mcx::{Mcx, PgVec};
use ::types_core::Oid;
use ::types_error::{PgError, PgResult, ERRCODE_DATA_CORRUPTED};
use ::types_rel::Relation;
use ::types_scan::scankey::{
    BTEqualStrategyNumber, BTGreaterEqualStrategyNumber, BTLessEqualStrategyNumber, ScanKeyData,
};
use ::types_scan::sdir::ForwardScanDirection;
use ::types_storage::lock::AccessShareLock;
use ::types_tuple::varatt::{
    set_varsize_4b_c_word, set_varsize_4b_word, VARHDRSZ, VARHDRSZ_SHORT,
};
use ::types_tuple::heap_getattr;
use toastdesc::VarattExternal;

use crate::helper::va_slice;
use crate::internals::{
    open_toast_relation, toast_close_indexes, toast_open_indexes, valueid_scan_key, F_INT4EQ,
    F_INT4GE, F_INT4LE,
};
use crate::TOAST_MAX_CHUNK_SIZE;

#[track_caller]
#[cold]
#[inline(never)]
fn corrupted(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_DATA_CORRUPTED))
}

#[track_caller]
#[cold]
#[inline(never)]
fn toasted_toast_chunk(valueid: Oid, relname: &str) -> Box<PgError> {
    Box::new(PgError::error(format!(
        "found toasted toast chunk for toast value {valueid} in {relname}"
    )))
}

fn int4_scan_key(
    attno: i16,
    strategy: u16,
    (foid, func): (Oid, ::types_fmgr::PGFunction),
    arg: i32,
) -> ScanKeyData {
    let mut k = ScanKeyData::empty();
    k.sk_attno = attno;
    k.sk_strategy = strategy;
    k.sk_collation = ::types_core::C_COLLATION_OID;
    k.sk_func = ::types_fmgr::FmgrInfo::new(func, foid, 2, true, false);
    k.sk_argument = Datum::from_i32(arg);
    k
}

pub(crate) struct ChunkSink<'a> {
    valueid: Oid,
    relname: &'a str,
    totalchunks: i32,
    startchunk: i32,
    endchunk: i32,
    sliceoffset: i32,
    slicelength: i32,
    attrsize: i32,
    expectedchunk: i32,
}

impl<'a> ChunkSink<'a> {
    pub(crate) fn new(valueid: Oid, relname: &'a str, attrsize: i32, sliceoffset: i32, slicelength: i32) -> Self {
        let max_chunk = TOAST_MAX_CHUNK_SIZE as i32;
        let startchunk = sliceoffset / max_chunk;
        ChunkSink {
            valueid,
            relname,
            totalchunks: (attrsize - 1) / max_chunk + 1,
            startchunk,
            endchunk: (sliceoffset + slicelength - 1) / max_chunk,
            sliceoffset,
            slicelength,
            attrsize,
            expectedchunk: startchunk,
        }
    }

    /// One chunk tuple's `chunk_seq` and `chunk_data` varlena image.
    pub(crate) fn put(&mut self, curchunk: i32, chunk: &[u8], result: &mut [u8]) -> PgResult<()> {
        let max_chunk = TOAST_MAX_CHUNK_SIZE as i32;
        let (valueid, relname) = (self.valueid, self.relname);
        let first = *chunk.first().ok_or_else(|| toasted_toast_chunk(valueid, relname))?;
        let chunkdata: &[u8] = if (first & 0x03) == 0 && chunk.len() >= VARHDRSZ {
            &chunk[VARHDRSZ..]
        } else if (first & 0x01) == 0x01 && first != 0x01 {
            // short header: heap_form_tuple can pack even PLAIN columns
            &chunk[VARHDRSZ_SHORT..]
        } else {
            return Err(toasted_toast_chunk(valueid, relname));
        };
        let chunksize = chunkdata.len() as i32;
        let (startchunk, endchunk, totalchunks) = (self.startchunk, self.endchunk, self.totalchunks);

        if curchunk != self.expectedchunk {
            return Err(corrupted(format!(
                "unexpected chunk number {curchunk} (expected {}) for toast value {valueid} in {relname}",
                self.expectedchunk
            )));
        }
        if curchunk > endchunk {
            return Err(corrupted(format!(
                "unexpected chunk number {curchunk} (out of range {startchunk}..{endchunk}) for toast value {valueid} in {relname}"
            )));
        }
        let expected_size = if curchunk < totalchunks - 1 {
            max_chunk
        } else {
            self.attrsize - (totalchunks - 1) * max_chunk
        };
        if chunksize != expected_size {
            return Err(corrupted(format!(
                "unexpected chunk size {chunksize} (expected {expected_size}) in chunk {curchunk} of {totalchunks} for toast value {valueid} in {relname}"
            )));
        }

        let mut chcpystrt = 0i32;
        let mut chcpyend = chunksize - 1;
        if curchunk == startchunk {
            chcpystrt = self.sliceoffset % max_chunk;
        }
        if curchunk == endchunk {
            chcpyend = (self.sliceoffset + self.slicelength - 1) % max_chunk;
        }

        let dst = (VARHDRSZ as i32 + curchunk * max_chunk - self.sliceoffset + chcpystrt) as usize;
        let n = (chcpyend - chcpystrt + 1) as usize;
        result[dst..dst + n]
            .copy_from_slice(&chunkdata[chcpystrt as usize..chcpystrt as usize + n]);

        self.expectedchunk += 1;
        Ok(())
    }

    pub(crate) fn finish(&self) -> PgResult<()> {
        if self.expectedchunk != self.endchunk + 1 {
            return Err(corrupted(format!(
                "missing chunk number {} for toast value {} in {}",
                self.expectedchunk, self.valueid, self.relname
            )));
        }
        Ok(())
    }
}

/// C `heap_fetch_toast_slice`: writes chunk bytes into `result`'s data area
/// (`result` is the full varlena image, header already set by the caller).
pub fn heap_fetch_toast_slice<'mcx>(
    mcx: Mcx<'mcx>,
    toastrel: &Relation<'mcx>,
    valueid: Oid,
    attrsize: i32,
    sliceoffset: i32,
    slicelength: i32,
    result: &mut [u8],
) -> PgResult<()> {
    let max_chunk = TOAST_MAX_CHUNK_SIZE as i32;
    let toasttup_desc = toastrel.rd_att.clone();
    let totalchunks = (attrsize - 1) / max_chunk + 1;

    let (toastidxs, valid_index) = toast_open_indexes(mcx, toastrel, AccessShareLock)?;

    let startchunk = sliceoffset / max_chunk;
    let endchunk = (sliceoffset + slicelength - 1) / max_chunk;
    debug_assert!(endchunk <= totalchunks);

    let mut toastkey = [valueid_scan_key(valueid), ScanKeyData::empty(), ScanKeyData::empty()];
    let nscankeys: usize = if startchunk == 0 && endchunk == totalchunks - 1 {
        1
    } else if startchunk == endchunk {
        toastkey[1] = int4_scan_key(
            2,
            BTEqualStrategyNumber,
            (F_INT4EQ, adt_int::builtins::fc_int4eq),
            startchunk,
        );
        2
    } else {
        toastkey[1] = int4_scan_key(
            2,
            BTGreaterEqualStrategyNumber,
            (F_INT4GE, adt_int::builtins::fc_int4ge),
            startchunk,
        );
        toastkey[2] = int4_scan_key(
            2,
            BTLessEqualStrategyNumber,
            (F_INT4LE, adt_int::builtins::fc_int4le),
            endchunk,
        );
        3
    };

    let snapshot = std::rc::Rc::new(toastdesc::get_toast_snapshot(
        mcx,
        snapmgr::HaveRegisteredOrActiveSnapshot(),
    )?);
    let mut toastscan = genam::systable_beginscan_ordered(
        mcx,
        toastrel,
        &toastidxs[valid_index],
        Some(snapshot),
        &toastkey[..nscankeys],
    )?;

    // Chunks arrive in (valueid, chunkidx) index order.
    let mut sink = ChunkSink::new(valueid, toastrel.name(), attrsize, sliceoffset, slicelength);
    loop {
        let Some(ttup) =
            genam::systable_getnext_ordered(mcx, &mut toastscan, ForwardScanDirection)?
        else {
            break;
        };

        let mut isnull = false;
        // SAFETY: toast tuples match the 3-column toast descriptor.
        let curchunk =
            unsafe { heap_getattr(ttup, 2, &toasttup_desc, &mut isnull) }.as_usize() as i32;
        debug_assert!(!isnull);
        let chunk_datum = unsafe { heap_getattr(ttup, 3, &toasttup_desc, &mut isnull) };
        debug_assert!(!isnull);
        // SAFETY: non-null deformed varlena datum into the scan slot.
        let chunk = unsafe { va_slice(chunk_datum) };
        sink.put(curchunk, chunk, result)?;
    }
    sink.finish()?;

    genam::systable_endscan_ordered(mcx, toastscan)?;
    toast_close_indexes(toastidxs, AccessShareLock)?;
    Ok(())
}

/// detoast.c `toast_fetch_datum` (the toast_internals_seams impl).
pub fn toast_fetch_datum<'mcx>(mcx: Mcx<'mcx>, attr: &[u8]) -> PgResult<PgVec<'mcx, u8>> {
    // detoast.c:351: a runtime check (elog ERROR), not an assertion.
    if !toastdesc::varatt_is_external_ondisk(attr) {
        return Err(Box::new(PgError::error(
            "toast_fetch_datum shouldn't be called for non-ondisk datums",
        )));
    }
    let toast_pointer = VarattExternal::from_image(attr)?;
    let attrsize = toast_pointer.extsize() as i32;

    let mut result = ::mcx::vec_with_capacity_in(mcx, attrsize as usize + VARHDRSZ)?;
    let header = if toast_pointer.is_compressed() {
        set_varsize_4b_c_word(attrsize as u32 + VARHDRSZ as u32)
    } else {
        set_varsize_4b_word(attrsize as u32 + VARHDRSZ as u32)
    };
    ::mcx::vec_append_bytes(&mut result, &header.to_ne_bytes())?;
    // SAFETY: capacity reserved; heap_fetch_toast_slice fills every data byte
    // or errors (missing-chunk check).
    unsafe { result.set_len(attrsize as usize + VARHDRSZ) };

    if attrsize == 0 {
        return Ok(result);
    }

    if let Some(map) = crate::worker::entered() {
        crate::worker::fetch_slice(mcx, &map, &toast_pointer, attrsize, 0, attrsize, &mut result)?;
        return Ok(result);
    }
    let toastrel = open_toast_relation(mcx, toast_pointer.va_toastrelid, AccessShareLock)?;
    heap_fetch_toast_slice(
        mcx,
        &toastrel,
        toast_pointer.va_valueid,
        attrsize,
        0,
        attrsize,
        &mut result,
    )?;
    table::table_close(toastrel, AccessShareLock)?;

    Ok(result)
}

/// detoast.c `toast_fetch_datum_slice` (the toast_internals_seams impl).
pub fn toast_fetch_datum_slice<'mcx>(
    mcx: Mcx<'mcx>,
    attr: &[u8],
    sliceoffset: i32,
    slicelength: i32,
) -> PgResult<PgVec<'mcx, u8>> {
    // detoast.c:405: a runtime check (elog ERROR), not an assertion.
    if !toastdesc::varatt_is_external_ondisk(attr) {
        return Err(Box::new(PgError::error(
            "toast_fetch_datum_slice shouldn't be called for non-ondisk datums",
        )));
    }
    let toast_pointer = VarattExternal::from_image(attr)?;
    debug_assert!(!toast_pointer.is_compressed() || sliceoffset == 0);

    let attrsize = toast_pointer.extsize() as i32;
    let mut sliceoffset = sliceoffset;
    let mut slicelength = slicelength;

    if sliceoffset >= attrsize {
        sliceoffset = 0;
        slicelength = 0;
    }

    // A compressed prefix must cover the leading va_tcinfo int32.
    if toast_pointer.is_compressed() && slicelength > 0 {
        slicelength += 4;
    }

    if sliceoffset + slicelength > attrsize || slicelength < 0 {
        slicelength = attrsize - sliceoffset;
    }

    let total = slicelength as usize + VARHDRSZ;
    let mut result = ::mcx::vec_with_capacity_in(mcx, total)?;
    let header = if toast_pointer.is_compressed() {
        set_varsize_4b_c_word(total as u32)
    } else {
        set_varsize_4b_word(total as u32)
    };
    ::mcx::vec_append_bytes(&mut result, &header.to_ne_bytes())?;
    // SAFETY: capacity reserved; the slice fetch fills every data byte or errors.
    unsafe { result.set_len(total) };

    if slicelength == 0 {
        return Ok(result);
    }

    if let Some(map) = crate::worker::entered() {
        crate::worker::fetch_slice(mcx, &map, &toast_pointer, attrsize, sliceoffset, slicelength, &mut result)?;
        return Ok(result);
    }
    let toastrel = open_toast_relation(mcx, toast_pointer.va_toastrelid, AccessShareLock)?;
    heap_fetch_toast_slice(
        mcx,
        &toastrel,
        toast_pointer.va_valueid,
        attrsize,
        sliceoffset,
        slicelength,
        &mut result,
    )?;
    table::table_close(toastrel, AccessShareLock)?;

    Ok(result)
}
