use types_core::{
    ForkNumber, BLCKSZ, INVALID_PROC_NUMBER, MAX_FORKNUM, RELPERSISTENCE_PERMANENT,
    RELPERSISTENCE_UNLOGGED,
};
use types_error::PgResult;
use types_storage::buf::BufferAccessStrategyType;
use types_storage::{ReadBufferMode, RelFileLocator, RelFileLocatorBackend};

use crate::ops::{
    BufferGetBlockNumber, BufferGetPagePtr, LockBuffer, MarkBufferDirty, UnlockReleaseBuffer,
    BUFFER_LOCK_SHARE,
};
use crate::read::ReadBufferWithoutRelcache;
use crate::{FreeAccessStrategy, GetAccessStrategy};

fn RelationCopyStorageUsingBuffer(
    srclocator: RelFileLocator,
    dstlocator: RelFileLocator,
    fork_num: ForkNumber,
    permanent: bool,
) -> PgResult<()> {
    let use_wal = transam_xlog_seams::xlog_standby_info_active::call()
        && (permanent || fork_num == ForkNumber::INIT_FORKNUM);

    let src_key = RelFileLocatorBackend { locator: srclocator, backend: INVALID_PROC_NUMBER };
    let dst_key = RelFileLocatorBackend { locator: dstlocator, backend: INVALID_PROC_NUMBER };
    let nblocks = smgr_seams::smgr_nblocks::call(src_key, fork_num)?;
    if nblocks == 0 {
        return Ok(());
    }

    smgr_seams::smgr_zeroextend::call(dst_key, fork_num, nblocks - 1, 1, true)?;

    let bstrategy_src = GetAccessStrategy(BufferAccessStrategyType::BasBulkread);
    let bstrategy_dst = GetAccessStrategy(BufferAccessStrategyType::BasBulkwrite);

    // bufmgr.c:5203: block_range_read_stream_cb takes no locks, so batch
    // mode is safe.
    let mut p = read_stream::BlockRangeReadStreamPrivate::new(0, nblocks);
    let stream_cxt = mcx::MemoryContext::new("RelationCopyStorageUsingBuffer");
    let mut src_stream = read_stream::read_stream_begin_smgr_relation::<()>(
        stream_cxt.mcx(),
        read_stream::READ_STREAM_FULL | read_stream::READ_STREAM_USE_BATCHING,
        bstrategy_src.clone(),
        src_key,
        if permanent { RELPERSISTENCE_PERMANENT } else { RELPERSISTENCE_UNLOGGED },
        fork_num,
    )?;

    for _ in 0..nblocks {
        postgres_seams::check_for_interrupts::call()?;

        let src_buf = src_stream.next_buffer(&mut |_: &mut ()| Ok(p.next_block()))?;
        LockBuffer(src_buf, BUFFER_LOCK_SHARE)?;

        let dst_buf = ReadBufferWithoutRelcache(
            dstlocator,
            fork_num,
            BufferGetBlockNumber(src_buf),
            ReadBufferMode::ZeroAndLock,
            bstrategy_dst.clone(),
            permanent,
        )?;

        init_small::globals::StartCriticalSection();

        // SAFETY: both pages are pinned buffer-pool slots, BLCKSZ each; dst is
        // exclusively locked (RBM_ZERO_AND_LOCK) so nothing else writes it.
        // src is only SHARE-locked, which does NOT make it stable: hint-bit
        // setters mutate t_infomask under a share lock, and MarkBufferDirtyHint
        // can set pd_lsn. That is C's contract too — RelationCopyStorageUsingBuffer
        // memcpys a share-locked page, and a hint bit landing mid-copy is
        // dropped rather than torn, because the copy is WAL-logged (or the
        // relation is not WAL-logged) and hint bits are recoverable by
        // definition. Raw pointers, not slices: an &[u8] here would additionally
        // promise the optimizer that the source cannot change, which is false
        // (see types_storage::writechunk for the same argument on the write path).
        unsafe {
            core::ptr::copy_nonoverlapping(
                BufferGetPagePtr(src_buf).as_ptr(),
                BufferGetPagePtr(dst_buf).as_ptr(),
                BLCKSZ,
            );
        }
        MarkBufferDirty(dst_buf)?;

        if use_wal {
            xloginsert_seams::log_newpage_buffer::call(dst_buf, true)?;
        }

        init_small::globals::EndCriticalSection();

        UnlockReleaseBuffer(dst_buf)?;
        UnlockReleaseBuffer(src_buf)?;
    }
    debug_assert_eq!(
        src_stream.next_buffer(&mut |_: &mut ()| Ok(p.next_block()))?,
        types_core::InvalidBuffer
    );
    src_stream.end()?;

    FreeAccessStrategy(bstrategy_src);
    FreeAccessStrategy(bstrategy_dst);
    Ok(())
}

pub fn CreateAndCopyRelationData(
    src_rlocator: RelFileLocator,
    dst_rlocator: RelFileLocator,
    permanent: bool,
) -> PgResult<()> {
    let relpersistence = if permanent {
        RELPERSISTENCE_PERMANENT
    } else {
        RELPERSISTENCE_UNLOGGED
    };

    let src_key = RelFileLocatorBackend { locator: src_rlocator, backend: INVALID_PROC_NUMBER };
    let dst_key = RelFileLocatorBackend { locator: dst_rlocator, backend: INVALID_PROC_NUMBER };

    catalog_storage_seams::relation_create_storage::call(dst_rlocator, relpersistence, false)?;

    RelationCopyStorageUsingBuffer(src_rlocator, dst_rlocator, ForkNumber::MAIN_FORKNUM, permanent)?;

    for fork_i in (ForkNumber::MAIN_FORKNUM as i32 + 1)..=(MAX_FORKNUM as i32) {
        let fork_num = ForkNumber::from_i32(fork_i).expect("fork range");
        if smgr_seams::smgr_exists::call(src_key, fork_num)? {
            smgr_seams::smgr_create::call(dst_key, fork_num, false)?;

            if permanent || fork_num == ForkNumber::INIT_FORKNUM {
                catalog_storage_seams::log_smgrcreate::call(dst_rlocator, fork_num)?;
            }

            RelationCopyStorageUsingBuffer(src_rlocator, dst_rlocator, fork_num, permanent)?;
        }
    }
    Ok(())
}
