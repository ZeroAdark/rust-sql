use std::path::PathBuf;

use mcx::PgVec;
use types_core::{InvalidXLogRecPtr, Oid, TransactionId, XLogRecPtr};
use types_error::{PgError, PgResult, DEBUG2, ERRCODE_DATA_CORRUPTED, ERRCODE_OUT_OF_MEMORY};
use types_snapshot::{SnapshotData, SnapshotType};
use types_storage::{RelFileLocator, SharedInvalidationMessage};
use types_tuple::{BlockIdData, ItemPointerData, SizeofHeapTupleHeader};

use crate::{
    dl_delete, dl_iter, rb_error, rb_file_error, ChangeId, ListHead, ReorderBuffer,
    ReorderBufferChange, ReorderBufferChangeData, ReorderBufferChangeType,
    ReorderBufferChangeType::*, TxnId, INVALID_ID, RBTXN_IS_ABORTED, RBTXN_IS_SERIALIZED,
    RBTXN_IS_SERIALIZED_CLEAR,
};
use elog::PCT_M;

const MAX_CHANGES_IN_MEMORY: u64 = 4096;

const SHARED_INVAL_MESSAGE_SIZE: usize = 16;

// This port must reproduce that admission check before its own (infallible) buffer allocation.
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;

pub(crate) fn serialized_path(slot_name: &str, xid: TransactionId, segno: u64) -> PathBuf {
    let seg_size = transam_xlog_seams::wal_segment_size::call() as u64;
    let recptr: XLogRecPtr = segno * seg_size;
    let mut path = crate::startup::replslot_dir().expect("DataDir set on spill paths");
    path.push(slot_name);
    path.push(format!(
        "xid-{}-lsn-{:X}-{:X}.spill",
        xid,
        (recptr >> 32) as u32,
        recptr as u32
    ));
    path
}

fn put_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}
fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_ne_bytes());
}
fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_ne_bytes());
}
fn put_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_ne_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_ne_bytes());
}

struct Cur<'a> {
    b: &'a [u8],
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> PgResult<&'a [u8]> {
        if self.b.len() < n {
            return Err(rb_error(format!(
                "could not read from reorderbuffer spill file: read {} instead of {} bytes",
                self.b.len(),
                n
            )));
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Ok(head)
    }
    fn u8(&mut self) -> PgResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> PgResult<u16> {
        Ok(u16::from_ne_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }
    fn u32(&mut self) -> PgResult<u32> {
        Ok(u32::from_ne_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }
    fn i32(&mut self) -> PgResult<i32> {
        Ok(i32::from_ne_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }
    fn u64(&mut self) -> PgResult<u64> {
        Ok(u64::from_ne_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }
}

fn action_from_i32(v: i32) -> PgResult<ReorderBufferChangeType> {
    Ok(match v {
        0 => Insert,
        1 => Update,
        2 => Delete,
        3 => Message,
        4 => Invalidation,
        5 => InternalSnapshot,
        6 => InternalCommandId,
        7 => InternalTupleCid,
        8 => InternalSpecInsert,
        9 => InternalSpecConfirm,
        10 => InternalSpecAbort,
        11 => Truncate,
        _ => {
            return Err(rb_error(format!(
                "unrecognized change action {v} in reorderbuffer spill file"
            )))
        }
    })
}

fn snapshot_type_from_i32(v: i32) -> PgResult<SnapshotType> {
    Ok(match v {
        0 => SnapshotType::SNAPSHOT_MVCC,
        1 => SnapshotType::SNAPSHOT_SELF,
        2 => SnapshotType::SNAPSHOT_ANY,
        3 => SnapshotType::SNAPSHOT_TOAST,
        4 => SnapshotType::SNAPSHOT_DIRTY,
        5 => SnapshotType::SNAPSHOT_HISTORIC_MVCC,
        6 => SnapshotType::SNAPSHOT_NON_VACUUMABLE,
        _ => {
            return Err(rb_error(format!(
                "unrecognized snapshot type {v} in reorderbuffer spill file"
            )))
        }
    })
}

fn put_locator(buf: &mut Vec<u8>, loc: &RelFileLocator) {
    put_u32(buf, loc.spcOid);
    put_u32(buf, loc.dbOid);
    put_u32(buf, loc.relNumber);
}

fn get_locator(cur: &mut Cur) -> PgResult<RelFileLocator> {
    Ok(RelFileLocator {
        spcOid: cur.u32()?,
        dbOid: cur.u32()?,
        relNumber: cur.u32()?,
    })
}

fn put_tid(buf: &mut Vec<u8>, tid: &ItemPointerData) {
    put_u16(buf, tid.ip_blkid.bi_hi);
    put_u16(buf, tid.ip_blkid.bi_lo);
    put_u16(buf, tid.ip_posid);
}

fn get_tid(cur: &mut Cur) -> PgResult<ItemPointerData> {
    Ok(ItemPointerData {
        ip_blkid: BlockIdData { bi_hi: cur.u16()?, bi_lo: cur.u16()? },
        ip_posid: cur.u16()?,
    })
}

fn put_tuple(buf: &mut Vec<u8>, tuple: &heaptuple::HeapTuple<'static>) {
    put_tid(buf, &tuple.t_self);
    put_u32(buf, tuple.t_tableOid);
    put_u32(buf, tuple.t_len);
    buf.extend_from_slice(tuple.image());
}

impl ReorderBuffer {
    fn get_tuple(&self, cur: &mut Cur) -> PgResult<heaptuple::HeapTuple<'static>> {
        let t_self = get_tid(cur)?;
        let t_table_oid = cur.u32()?;
        let t_len = cur.u32()? as usize;
        if t_len < SizeofHeapTupleHeader {
            return Err(rb_error(format!(
                "invalid tuple length {t_len} in reorderbuffer spill file"
            )));
        }
        let image = cur.take(t_len)?;
        // The copy lands in the buffer's own context, never the caller's.
        let mut tuple = self.alloc_tuple_buf(t_len - SizeofHeapTupleHeader)?;
        tuple.image_mut().copy_from_slice(image);
        tuple.t_self = t_self;
        tuple.t_tableOid = t_table_oid;

        // Establish the header invariants heap_deform_tuple relies on before
        // this image is ever handed to unsafe deform/varlena code.
        let header = tuple.t_data();
        let t_hoff = header.t_hoff as usize;
        let null_bitmap_len =
            if tuple.has_nulls() { (header.natts() as usize + 7) / 8 } else { 0 };
        let min_hoff = SizeofHeapTupleHeader + null_bitmap_len;
        if t_hoff < min_hoff || t_hoff > t_len {
            return Err(PgError::error(format!(
                "invalid tuple header offset {t_hoff} (t_len {t_len}) in reorderbuffer spill file"
            ))
            .with_sqlstate(ERRCODE_DATA_CORRUPTED)
            .into());
        }

        Ok(tuple)
    }

    pub(crate) fn largest_txn(&self) -> Option<TxnId> {
        let largest = self.txn_heap.first().map(|&(_, id)| id);
        debug_assert!(largest.map_or(true, |id| self.txn(id).size <= self.size));
        largest
    }

    fn check_and_truncate_aborted_txn(&mut self, txn: TxnId) -> PgResult<bool> {
        if guc_tables::vars::debug_logical_replication_streaming.read()
            == guc_tables::consts::DEBUG_LOGICAL_REP_STREAMING_IMMEDIATE
        {
            return Ok(false);
        }

        if self.txn(txn).is_committed() {
            return Ok(false);
        }
        if self.txn(txn).is_aborted() {
            debug_assert_eq!(self.txn(txn).size, 0);
            return Ok(true);
        }

        let xid = self.txn(txn).xid;
        if procarray_seams::transaction_id_is_in_progress::call(xid)? {
            return Ok(false);
        }
        if transam_seams::transaction_id_did_commit::call(xid)? {
            // Cache the commit so future eviction passes skip the CLOG.
            debug_assert!(!self.txn(txn).is_aborted());
            self.txn_mut(txn).txn_flags |= crate::RBTXN_IS_COMMITTED;
            return Ok(false);
        }

        // Full cleanup happens when the ABORT record of this transaction is decoded.
        let prepared = self.txn(txn).is_prepared();
        self.truncate_txn(txn, prepared)?;
        self.toast_reset(txn);
        debug_assert_eq!(self.txn(txn).size, 0);
        debug_assert!(!self.txn(txn).is_committed());
        self.txn_mut(txn).txn_flags |= RBTXN_IS_ABORTED;
        Ok(true)
    }

    pub(crate) fn check_memory_limit(&mut self) -> PgResult<()> {
        let limit = guc_tables::vars::logical_decoding_work_mem.read() as usize * 1024;
        let immediate = guc_tables::vars::debug_logical_replication_streaming.read()
            == guc_tables::consts::DEBUG_LOGICAL_REP_STREAMING_IMMEDIATE;

        if !immediate && self.size < limit {
            return Ok(());
        }

        // The GUC can shrink between changes, so one eviction is not guaranteed to get back under the limit.
        while self.size >= limit || (immediate && self.size > 0) {
            let streamable = if self.can_start_streaming() {
                self.largest_streamable_top_txn()
            } else {
                None
            };
            if let Some(txn) = streamable {
                debug_assert!(self.txn(txn).is_toptxn());
                debug_assert!(self.txn(txn).total_size > 0);
                debug_assert!(self.size >= self.txn(txn).total_size);

                if self.check_and_truncate_aborted_txn(txn)? {
                    continue;
                }

                self.stream_txn(txn)?;
            } else {
                let txn = self.largest_txn().expect("nonzero rb size implies a sized txn");
                debug_assert!(self.txn(txn).size > 0);
                debug_assert!(self.size >= self.txn(txn).size);

                if self.check_and_truncate_aborted_txn(txn)? {
                    continue;
                }

                self.serialize_txn(txn)?;

                debug_assert_eq!(self.txn(txn).size, 0);
                debug_assert_eq!(self.txn(txn).nentries_mem, 0);
            }
        }

        debug_assert!(self.size < limit);
        Ok(())
    }

    pub(crate) fn serialize_txn(&mut self, txn: TxnId) -> PgResult<()> {
        let size = self.txn(txn).size;
        let nentries_mem = self.txn(txn).nentries_mem;

        let _ = elog::elog(
            DEBUG2,
            format!(
                "spill {} changes in XID {} to disk",
                nentries_mem as u32,
                self.txn(txn).xid
            ),
        );

        let subs: Vec<TxnId> = dl_iter(&self.txns, self.txn(txn).subtxns, |t| t.node).collect();
        for sub in subs {
            self.serialize_txn(sub)?;
        }

        let seg_size = transam_xlog_seams::wal_segment_size::call() as u64;
        let xid = self.txn(txn).xid;
        let mut fd_raw: i32 = -1;
        let mut cur_offset: i64 = 0;
        let mut cur_open_segno: u64 = 0;
        let mut spilled: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();

        loop {
            let head = self.txn(txn).changes.head;
            if head == INVALID_ID {
                break;
            }

            // A change never splits across segment files.
            let segno = self.change(head).lsn / seg_size;
            if fd_raw == -1 || segno != cur_open_segno {
                if fd_raw != -1 {
                    fd::CloseTransientFile(fd_raw);
                }
                cur_open_segno = segno;
                // The files live for a single run, so each LSN maps to one WAL record.
                let path = serialized_path(&self.slot_name, xid, segno);
                fd_raw = fd::OpenTransientFile(
                    &path.to_string_lossy(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_APPEND,
                )?;
                if fd_raw < 0 {
                    return Err(rb_file_error(
                        format!("could not open file \"{}\": {PCT_M}", crate::startup::datadir_relative(&path)),
                        &errno_error(),
                    ));
                }
                cur_offset = fd::pg_file_size_raw(fd_raw);
                if cur_offset < 0 {
                    return Err(rb_file_error(
                        format!("could not open file \"{}\": {PCT_M}", crate::startup::datadir_relative(&path)),
                        &errno_error(),
                    ));
                }
            }

            self.serialize_change(txn, fd_raw, &mut cur_offset, head, &mut buf)?;
            let mut list = self.txn(txn).changes;
            dl_delete(&mut self.changes, &mut list, head, |c| &mut c.node);
            self.txn_mut(txn).changes = list;
            self.free_change(head, false);
            spilled += 1;
        }

        self.change_memory_update(None, Some(txn), false, size);

        if spilled > 0 {
            self.spillCount += 1;
            self.spillBytes += size as i64;
            // Don't count already-serialized transactions again.
            let counted = self.txn(txn).txn_flags & (RBTXN_IS_SERIALIZED | RBTXN_IS_SERIALIZED_CLEAR)
                != 0;
            self.spillTxns += if counted { 0 } else { 1 };
            if let Some(update_stats) = self.update_stats {
                update_stats(self);
            }
        }

        debug_assert_eq!(spilled, nentries_mem);
        debug_assert!(self.txn(txn).changes.is_empty());
        self.txn_mut(txn).nentries_mem = 0;
        self.txn_mut(txn).txn_flags |= RBTXN_IS_SERIALIZED;

        if fd_raw != -1 {
            fd::CloseTransientFile(fd_raw);
        }
        Ok(())
    }

    // ReorderBufferSerializeChange (reorderbuffer.c:4058): one size-prefixed
    // record, one write(2) per change.
    fn serialize_change(
        &mut self,
        txn: TxnId,
        fd_raw: i32,
        cur_offset: &mut i64,
        cid: ChangeId,
        buf: &mut Vec<u8>,
    ) -> PgResult<()> {
        let change = self.change(cid);
        let lsn = change.lsn;

        buf.clear();
        put_u64(buf, 0);
        put_i32(buf, change.action as i32);
        put_u16(buf, change.origin_id);
        put_u64(buf, change.lsn);

        match &change.data {
            // The locator and clear-toast flag survive for the spec-confirm/abort forms too; tuples ride only when present.
            ReorderBufferChangeData::Tp { rlocator, clear_toast_afterwards, oldtuple, newtuple } =>
            {
                put_locator(buf, rlocator);
                put_u8(buf, *clear_toast_afterwards as u8);
                put_u8(buf, oldtuple.is_some() as u8);
                put_u8(buf, newtuple.is_some() as u8);
                if let Some(t) = oldtuple {
                    put_tuple(buf, t);
                }
                if let Some(t) = newtuple {
                    put_tuple(buf, t);
                }
            }
            ReorderBufferChangeData::Msg { prefix, message } => {
                put_u64(buf, prefix.len() as u64);
                buf.extend_from_slice(prefix);
                put_u64(buf, message.len() as u64);
                buf.extend_from_slice(message);
            }
            ReorderBufferChangeData::Inval { invalidations } => {
                put_u64(buf, invalidations.len() as u64);
                for msg in invalidations.iter() {
                    buf.extend_from_slice(&msg.to_wire_bytes());
                }
            }
            ReorderBufferChangeData::Snapshot(snap) => {
                put_i32(buf, snap.snapshot_type as i32);
                put_u32(buf, snap.xmin);
                put_u32(buf, snap.xmax);
                put_u32(buf, snap.xcnt);
                for xid in &snap.xip[..snap.xcnt as usize] {
                    put_u32(buf, *xid);
                }
                let subxcnt = snap.subxcnt.max(0);
                put_i32(buf, subxcnt);
                for xid in &snap.subxip[..subxcnt as usize] {
                    put_u32(buf, *xid);
                }
                put_u8(buf, snap.suboverflowed as u8);
                put_u8(buf, snap.takenDuringRecovery as u8);
                put_u32(buf, snap.speculativeToken);
                put_u32(buf, snap.curcid.get());
                put_u64(buf, snap.vistest.id);
                put_u64(buf, snap.snapXactCompletionCount);
            }
            ReorderBufferChangeData::Truncate { cascade, restart_seqs, relids } => {
                put_u8(buf, *cascade as u8);
                put_u8(buf, *restart_seqs as u8);
                put_u64(buf, relids.len() as u64);
                for relid in relids.iter() {
                    put_u32(buf, *relid);
                }
            }
            ReorderBufferChangeData::CommandId(cmd) => {
                put_u32(buf, *cmd);
            }
            ReorderBufferChangeData::TupleCid { locator, tid, cmin, cmax, combocid } => {
                put_locator(buf, locator);
                put_tid(buf, tid);
                put_u32(buf, *cmin);
                put_u32(buf, *cmax);
                put_u32(buf, *combocid);
            }
        }

        let size = buf.len() as u64;
        buf[..8].copy_from_slice(&size.to_ne_bytes());

        // A write that set no errno is taken as no disk space.
        fd::set_errno(0);
        wait_start(WAIT_EVENT_REORDER_BUFFER_WRITE);
        let written = fd::pg_pwrite(fd_raw, buf, *cur_offset);
        wait_end();
        if written != size as isize {
            let save_errno = fd::get_errno();
            fd::CloseTransientFile(fd_raw);
            let errno = if save_errno != 0 { save_errno } else { libc::ENOSPC };
            let xid = self.txn(txn).xid;
            return Err(rb_file_error(
                format!("could not write to data file for XID {xid}: {PCT_M}"),
                &std::io::Error::from_raw_os_error(errno),
            ));
        }
        *cur_offset += written as i64;

        // Keep final_lsn current with each change sent to disk so that RestoreCleanup works even if a crash leaves the transaction without its abort record. Never move it backwards.
        if self.txn(txn).final_lsn < lsn {
            self.txn_mut(txn).final_lsn = lsn;
        }
        Ok(())
    }

    pub(crate) fn restore_changes(
        &mut self,
        txn: TxnId,
        file: &mut Option<SpillReader>,
        segno: &mut u64,
    ) -> PgResult<u64> {
        debug_assert!(self.txn(txn).first_lsn != InvalidXLogRecPtr);
        debug_assert!(self.txn(txn).final_lsn != InvalidXLogRecPtr);

        let changes: Vec<ChangeId> =
            dl_iter(&self.changes, self.txn(txn).changes, |c| c.node).collect();
        self.txn_mut(txn).changes = ListHead::EMPTY;
        for cid in changes {
            self.free_change(cid, true);
        }
        self.txn_mut(txn).nentries_mem = 0;

        let seg_size = transam_xlog_seams::wal_segment_size::call() as u64;
        let last_segno = self.txn(txn).final_lsn / seg_size;
        let xid = self.txn(txn).xid;

        let mut restored: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        while restored < MAX_CHANGES_IN_MEMORY && *segno <= last_segno {
            postgres_seams::check_for_interrupts::call()?;

            if file.is_none() {
                if *segno == 0 {
                    *segno = self.txn(txn).first_lsn / seg_size;
                }
                debug_assert!(*segno != 0 || self.txn(txn).changes.is_empty());

                let path = serialized_path(&self.slot_name, xid, *segno);
                let vfd = fd::PathNameOpenFile(&path.to_string_lossy(), libc::O_RDONLY)?;
                if vfd.0 < 0 {
                    let errno = fd::get_errno();
                    if errno == libc::ENOENT {
                        *segno += 1;
                        continue;
                    }
                    return Err(rb_file_error(
                        format!("could not open file \"{}\": {PCT_M}", crate::startup::datadir_relative(&path)),
                        &std::io::Error::from_raw_os_error(errno),
                    ));
                }
                *file = Some(SpillReader { vfd, cur_offset: 0 });
            }

            // A clean zero-byte read is end-of-segment.
            let f = file.as_mut().expect("opened above");
            let mut szbuf = [0u8; 8];
            let n = f.read(&mut szbuf)?;
            if n == 0 {
                file.take().expect("opened above").close()?;
                *segno += 1;
                continue;
            }
            if n != szbuf.len() {
                return Err(rb_error(format!(
                    "could not read from reorderbuffer spill file: read {n} instead of {} bytes",
                    szbuf.len()
                )));
            }
            let size = u64::from_ne_bytes(szbuf) as usize;
            if size < szbuf.len() {
                return Err(rb_error(format!(
                    "invalid record size {size} in reorderbuffer spill file"
                )));
            }
            // Without this the resize below is an infallible allocation that aborts the whole process on an oversized value.
            if size > MAX_ALLOC_SIZE {
                return Err(PgError::error(format!(
                    "invalid record size {size} in reorderbuffer spill file"
                ))
                .with_sqlstate(ERRCODE_DATA_CORRUPTED)
                .into());
            }

            let body = size - szbuf.len();
            buf.clear();
            // At or below MaxAllocSize a failure here is genuine memory pressure, not corruption — raise a catchable error instead of aborting via the infallible allocator.
            buf.try_reserve_exact(body).map_err(|_| {
                Box::new(
                    PgError::error(format!(
                        "out of memory restoring {body}-byte change from reorderbuffer spill file"
                    ))
                    .with_sqlstate(ERRCODE_OUT_OF_MEMORY),
                )
            })?;
            ::mcx::std_vec_resize("replication", &mut buf, body, 0)?;
            let n = f.read(&mut buf)?;
            if n != body {
                return Err(rb_error(format!(
                    "could not read from reorderbuffer spill file: read {n} instead of {body} bytes"
                )));
            }

            self.restore_change(txn, &buf)?;
            restored += 1;
        }

        Ok(restored)
    }

    fn restore_change(&mut self, txn: TxnId, body: &[u8]) -> PgResult<()> {
        let mut cur = Cur { b: body };
        let action = action_from_i32(cur.i32()?)?;
        let origin_id = cur.u16()?;
        let lsn = cur.u64()?;

        let data = match action {
            Insert | Update | Delete | InternalSpecInsert | InternalSpecConfirm
            | InternalSpecAbort => {
                let rlocator = get_locator(&mut cur)?;
                let clear_toast_afterwards = cur.u8()? != 0;
                let has_old = cur.u8()? != 0;
                let has_new = cur.u8()? != 0;
                let oldtuple = if has_old { Some(self.get_tuple(&mut cur)?) } else { None };
                let newtuple = if has_new { Some(self.get_tuple(&mut cur)?) } else { None };
                ReorderBufferChangeData::Tp { rlocator, clear_toast_afterwards, oldtuple, newtuple }
            }
            Message => {
                let prefix_len = cur.u64()? as usize;
                let prefix_bytes = cur.take(prefix_len)?;
                let mut prefix = PgVec::new_in(self.mcx);
                mcx::vec_append_bytes(&mut prefix, prefix_bytes)?;
                let msg_len = cur.u64()? as usize;
                let msg_bytes = cur.take(msg_len)?;
                let mut message = PgVec::new_in(self.mcx);
                mcx::vec_append_bytes(&mut message, msg_bytes)?;
                ReorderBufferChangeData::Msg { prefix, message }
            }
            Invalidation => {
                let n = cur.u64()? as usize;
                let mut invalidations = PgVec::new_in(self.mcx);
                for _ in 0..n {
                    let raw: [u8; SHARED_INVAL_MESSAGE_SIZE] = cur
                        .take(SHARED_INVAL_MESSAGE_SIZE)?
                        .try_into()
                        .expect("exact take");
                    let msg = SharedInvalidationMessage::from_wire_bytes(raw).ok_or_else(|| {
                        rb_error("unrecognized invalidation in reorderbuffer spill file".into())
                    })?;
                    invalidations.push(msg);
                }
                ReorderBufferChangeData::Inval { invalidations }
            }
            InternalSnapshot => {
                let ty = snapshot_type_from_i32(cur.i32()?)?;
                let mut snap = SnapshotData::sentinel(self.mcx, ty);
                snap.xmin = cur.u32()?;
                snap.xmax = cur.u32()?;
                let xcnt = cur.u32()?;
                let mut xip = PgVec::new_in(self.mcx);
                for _ in 0..xcnt {
                    xip.push(cur.u32()?);
                }
                snap.xip = xip;
                snap.xcnt = xcnt;
                let subxcnt = cur.i32()?.max(0);
                let mut subxip = PgVec::new_in(self.mcx);
                for _ in 0..subxcnt {
                    subxip.push(cur.u32()?);
                }
                snap.subxip = subxip;
                snap.subxcnt = subxcnt;
                snap.suboverflowed = cur.u8()? != 0;
                snap.takenDuringRecovery = cur.u8()? != 0;
                snap.speculativeToken = cur.u32()?;
                snap.curcid.set(cur.u32()?);
                snap.vistest = types_core::GlobalVisStateHandle::new(cur.u64()?);
                snap.snapXactCompletionCount = cur.u64()?;
                // The restored snapshot is a private copy; refcounts follow copy_snap's fresh-copy convention.
                snap.copied = true;
                snap.active_count.set(1);
                snap.regd_count.set(0);
                ReorderBufferChangeData::Snapshot(std::rc::Rc::new(snap))
            }
            Truncate => {
                let cascade = cur.u8()? != 0;
                let restart_seqs = cur.u8()? != 0;
                let n = cur.u64()? as usize;
                let mut relids: PgVec<'static, Oid> = PgVec::new_in(self.mcx);
                for _ in 0..n {
                    relids.push(cur.u32()?);
                }
                ReorderBufferChangeData::Truncate { cascade, restart_seqs, relids }
            }
            InternalCommandId => ReorderBufferChangeData::CommandId(cur.u32()?),
            InternalTupleCid => {
                let locator = get_locator(&mut cur)?;
                let tid = get_tid(&mut cur)?;
                ReorderBufferChangeData::TupleCid {
                    locator,
                    tid,
                    cmin: cur.u32()?,
                    cmax: cur.u32()?,
                    combocid: cur.u32()?,
                }
            }
        };

        let mut change = ReorderBufferChange::new(action, data);
        change.lsn = lsn;
        change.origin_id = origin_id;
        change.txn = txn;

        let cid = self.alloc_change_slot(change);
        let mut list = self.txn(txn).changes;
        crate::dl_push_tail(&mut self.changes, &mut list, cid, |c| &mut c.node);
        self.txn_mut(txn).changes = list;
        self.txn_mut(txn).nentries_mem += 1;

        // It is released through the same accounting later, and the counters must not underflow then.
        let sz = self.change_size(cid);
        self.change_memory_update(Some(cid), None, true, sz);
        Ok(())
    }

    pub(crate) fn restore_cleanup(&mut self, txn: TxnId) -> PgResult<()> {
        debug_assert!(self.txn(txn).first_lsn != InvalidXLogRecPtr);
        debug_assert!(self.txn(txn).final_lsn != InvalidXLogRecPtr);

        let seg_size = transam_xlog_seams::wal_segment_size::call() as u64;
        let first = self.txn(txn).first_lsn / seg_size;
        let last = self.txn(txn).final_lsn / seg_size;
        let xid = self.txn(txn).xid;

        for segno in first..=last {
            let path = serialized_path(&self.slot_name, xid, segno);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(rb_file_error(
                        format!("could not remove file \"{}\": {PCT_M}", crate::startup::datadir_relative(&path)),
                        &e,
                    ))
                }
            }
        }
        Ok(())
    }
}

const PG_WAIT_IO: u32 = 0x0A00_0000;
const WAIT_EVENT_REORDER_BUFFER_READ: u32 = PG_WAIT_IO + 43;
const WAIT_EVENT_REORDER_BUFFER_WRITE: u32 = PG_WAIT_IO + 44;

fn wait_start(info: u32) {
    if waitevent_seams::pgstat_report_wait_start::is_installed() {
        waitevent_seams::pgstat_report_wait_start::call(info);
    }
}

fn wait_end() {
    if waitevent_seams::pgstat_report_wait_end::is_installed() {
        waitevent_seams::pgstat_report_wait_end::call();
    }
}

fn errno_error() -> std::io::Error {
    std::io::Error::from_raw_os_error(fd::get_errno())
}

// Never closed on drop, as C's File is a plain int.
pub(crate) struct SpillReader {
    vfd: types_storage::File,
    cur_offset: i64,
}

impl SpillReader {
    fn read(&mut self, buf: &mut [u8]) -> PgResult<usize> {
        let n = fd::FileRead(self.vfd, buf, self.cur_offset, WAIT_EVENT_REORDER_BUFFER_READ)?;
        if n < 0 {
            return Err(rb_file_error(
                format!("could not read from reorderbuffer spill file: {PCT_M}"),
                &errno_error(),
            ));
        }
        self.cur_offset += n as i64;
        Ok(n as usize)
    }

    pub(crate) fn close(self) -> PgResult<()> {
        fd::FileClose(self.vfd)
    }
}
