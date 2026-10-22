//! read_stream.c: buffered relation reads with adaptive look-ahead over
//! StartReadBuffers/WaitReadBuffers.
//! C divergences: the block-number callback is passed to each call instead
//! of being stored at begin (C's callback_private_data is the caller's own
//! state, which Rust cannot alias while the caller also owns the stream);
//! per-buffer data is a typed `D` slot per queue entry.

#![allow(non_snake_case)]

use bufmgr_seams::{
    ReadBuffersOperation, READ_BUFFERS_ISSUE_ADVICE, READ_BUFFERS_SYNCHRONOUSLY,
};
use mcx::Mcx;
use types_core::{
    BlockNumber, Buffer, ForkNumber, InvalidBlockNumber, InvalidBuffer, OidIsValid,
    RELPERSISTENCE_TEMP,
};
use types_error::PgResult;
use core::ops::Deref;
use types_rel::{Relation, RelationData};
use types_storage::buf::BufferAccessStrategy;
use types_storage::RelFileLocatorBackend;

pub const READ_STREAM_DEFAULT: i32 = 0x00;
pub const READ_STREAM_MAINTENANCE: i32 = 0x01;
pub const READ_STREAM_SEQUENTIAL: i32 = 0x02;
pub const READ_STREAM_FULL: i32 = 0x04;
pub const READ_STREAM_USE_BATCHING: i32 = 0x08;

const PG_INT16_MAX: i32 = i16::MAX as i32;

#[derive(Clone, Copy, Debug)]
pub struct BlockRangeReadStreamPrivate {
    pub current_blocknum: BlockNumber,
    pub last_exclusive: BlockNumber,
}

impl BlockRangeReadStreamPrivate {
    pub fn new(current_blocknum: BlockNumber, last_exclusive: BlockNumber) -> Self {
        BlockRangeReadStreamPrivate { current_blocknum, last_exclusive }
    }

    #[inline]
    pub fn next_block(&mut self) -> BlockNumber {
        if self.current_blocknum < self.last_exclusive {
            let b = self.current_blocknum;
            self.current_blocknum += 1;
            return b;
        }
        InvalidBlockNumber
    }
}

struct InProgressIO {
    buffer_index: usize,
    op: ReadBuffersOperation,
}

// R holds the relation for per-relation stats: an owned alias, or a borrow
// for callers that only have the RelationData.
pub struct ReadStream<'mcx, D: Copy + Default = (), R: Deref<Target = RelationData<'mcx>> = Relation<'mcx>> {
    max_ios: i32,
    io_combine_limit: i32,
    ios_in_progress: i32,
    queue_size: usize,
    max_pinned_buffers: i32,
    forwarded_buffers: i32,
    pinned_buffers: i32,
    distance: i32,
    initialized_buffers: usize,
    read_buffers_flags: u32,
    batch_mode: bool,
    advice_enabled: bool,
    temporary: bool,

    buffered_blocknum: BlockNumber,

    seq_blocknum: BlockNumber,
    seq_until_processed: BlockNumber,

    pending_read_blocknum: BlockNumber,
    pending_read_nblocks: i32,

    per_buffer_data: Vec<D>,

    // Read operations that have been started but not waited for yet. Never
    // reallocated after begin: in-flight IOs point at their op's io_return.
    ios: Vec<InProgressIO>,
    oldest_io_index: usize,
    next_io_index: usize,

    fast_path: bool,

    oldest_buffer_index: usize,
    next_buffer_index: usize,
    buffers: Vec<Buffer>,

    rel: Option<R>,
}

fn oom(what: &str, bytes: usize) -> Box<types_error::PgError> {
    Box::new(mcx::oom_named(what, bytes))
}

fn vec_with<T>(n: usize, mut f: impl FnMut() -> T) -> PgResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| oom("ReadStream", n * core::mem::size_of::<T>()))?;
    for _ in 0..n {
        v.push(f());
    }
    Ok(v)
}

pub fn read_stream_begin_relation<'mcx, D: Copy + Default>(
    mcx: Mcx<'_>,
    flags: i32,
    strategy: BufferAccessStrategy,
    rel: &Relation<'mcx>,
    forknum: ForkNumber,
) -> PgResult<ReadStream<'mcx, D>> {
    let smgr = bufmgr_seams::relation_smgr_locator::call(rel);
    let persistence = rel.rd_rel.relpersistence;
    read_stream_begin_impl(mcx, flags, strategy, Some(rel.alias()), smgr, persistence, forknum)
}

pub fn read_stream_begin_relation_data<'r, 'mcx, D: Copy + Default>(
    mcx: Mcx<'_>,
    flags: i32,
    strategy: BufferAccessStrategy,
    rel: &'r RelationData<'mcx>,
    forknum: ForkNumber,
) -> PgResult<ReadStream<'mcx, D, &'r RelationData<'mcx>>> {
    let smgr = bufmgr_seams::relation_smgr_locator::call(rel);
    let persistence = rel.rd_rel.relpersistence;
    read_stream_begin_impl(mcx, flags, strategy, Some(rel), smgr, persistence, forknum)
}

pub fn read_stream_begin_smgr_relation<'mcx, D: Copy + Default>(
    mcx: Mcx<'_>,
    flags: i32,
    strategy: BufferAccessStrategy,
    smgr: RelFileLocatorBackend,
    smgr_persistence: u8,
    forknum: ForkNumber,
) -> PgResult<ReadStream<'mcx, D>> {
    read_stream_begin_impl(mcx, flags, strategy, None, smgr, smgr_persistence, forknum)
}

fn read_stream_begin_impl<'mcx, D: Copy + Default, R: Deref<Target = RelationData<'mcx>>>(
    mcx: Mcx<'_>,
    flags: i32,
    strategy: BufferAccessStrategy,
    rel: Option<R>,
    smgr: RelFileLocatorBackend,
    persistence: u8,
    forknum: ForkNumber,
) -> PgResult<ReadStream<'mcx, D, R>> {
    if let Some(rel) = rel.as_deref() {
        if rel.rd_rel.relpersistence == RELPERSISTENCE_TEMP && !rel.rd_islocaltemp {
            return Err(Box::new(
                types_error::PgError::error("cannot access temporary tables of other sessions")
                    .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
            ));
        }
    }

    let tablespace_id = smgr.locator.spcOid;
    let mut max_ios = if !OidIsValid(init_small::globals::MyDatabaseId())
        || rel.as_deref().is_some_and(|r| catalog_seams::is_catalog_relation_oid::call(r.rd_id))
        || catalog_seams::is_catalog_relation_oid::call(smgr.locator.relNumber)
    {
        guc_tables::vars::effective_io_concurrency.read()
    } else if flags & READ_STREAM_MAINTENANCE != 0 {
        spccache::get_tablespace_maintenance_io_concurrency(mcx, tablespace_id)?
    } else {
        spccache::get_tablespace_io_concurrency(mcx, tablespace_id)?
    };
    max_ios = max_ios.min(PG_INT16_MAX);

    let io_combine_limit = bufmgr_seams::io_combine_limit::call();

    let queue_overflow = io_combine_limit - 1;

    let mut max_pinned_buffers: u32 = ((max_ios + 1) * io_combine_limit) as u32;
    max_pinned_buffers = max_pinned_buffers.min((PG_INT16_MAX - queue_overflow - 1) as u32);

    let strategy_pin_limit = bufmgr_seams::get_access_strategy_pin_limit::call(&strategy);
    max_pinned_buffers = max_pinned_buffers.min(strategy_pin_limit.max(0) as u32);

    let temporary = persistence == RELPERSISTENCE_TEMP;
    let max_possible_buffer_limit = if temporary {
        bufmgr_seams::get_local_pin_limit::call()
    } else {
        bufmgr_seams::get_pin_limit::call()
    };
    max_pinned_buffers = max_pinned_buffers.min(max_possible_buffer_limit).max(1);

    let queue_size = max_pinned_buffers as usize + 1;

    let sync_mode = aio_core::io_method() == guc_tables::consts::IOMETHOD_SYNC;
    let mut advice_enabled = false;
    if sync_mode
        && fd::io_direct_flags() & types_storage::IO_DIRECT_DATA == 0
        && flags & READ_STREAM_SEQUENTIAL == 0
        && max_ios > 0
    {
        advice_enabled = true;
    }

    let mut read_buffers_flags = 0;
    if max_ios == 0 {
        max_ios = 1;
        read_buffers_flags = READ_BUFFERS_SYNCHRONOUSLY;
    }

    let buffers = vec_with(queue_size + queue_overflow as usize, || InvalidBuffer)?;
    let ios = vec_with(max_ios as usize, || InProgressIO {
        buffer_index: 0,
        op: ReadBuffersOperation::new(smgr, persistence, forknum, strategy.clone(), 0),
    })?;
    let per_buffer_data = if core::mem::size_of::<D>() == 0 {
        Vec::new()
    } else {
        vec_with(queue_size, D::default)?
    };

    let distance = if flags & READ_STREAM_FULL != 0 {
        (max_pinned_buffers as i32).min(io_combine_limit)
    } else {
        1
    };

    Ok(ReadStream {
        max_ios,
        io_combine_limit,
        ios_in_progress: 0,
        queue_size,
        max_pinned_buffers: max_pinned_buffers as i32,
        forwarded_buffers: 0,
        pinned_buffers: 0,
        distance,
        initialized_buffers: 0,
        read_buffers_flags,
        batch_mode: flags & READ_STREAM_USE_BATCHING != 0
            && aio_seams::pgaio_enter_batchmode::is_installed(),
        advice_enabled,
        temporary,
        buffered_blocknum: InvalidBlockNumber,
        seq_blocknum: InvalidBlockNumber,
        seq_until_processed: InvalidBlockNumber,
        pending_read_blocknum: InvalidBlockNumber,
        pending_read_nblocks: 0,
        per_buffer_data,
        ios,
        oldest_io_index: 0,
        next_io_index: 0,
        fast_path: false,
        oldest_buffer_index: 0,
        next_buffer_index: 0,
        buffers,
        rel,
    })
}

impl<'mcx, D: Copy + Default, R: Deref<Target = RelationData<'mcx>>> ReadStream<'mcx, D, R> {
    #[inline]
    fn has_data() -> bool {
        core::mem::size_of::<D>() != 0
    }

    #[inline]
    fn get_block<F>(&mut self, cb: &mut F, data_index: Option<usize>) -> PgResult<BlockNumber>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        let blocknum = self.buffered_blocknum;
        if blocknum != InvalidBlockNumber {
            self.buffered_blocknum = InvalidBlockNumber;
            return Ok(blocknum);
        }
        match data_index {
            Some(i) if Self::has_data() => {
                self.per_buffer_data[i] = D::default();
                cb(&mut self.per_buffer_data[i])
            }
            _ => {
                let mut scratch = D::default();
                cb(&mut scratch)
            }
        }
    }

    #[inline]
    fn unget_block(&mut self, blocknum: BlockNumber) {
        debug_assert!(self.buffered_blocknum == InvalidBlockNumber);
        debug_assert!(blocknum != InvalidBlockNumber);
        self.buffered_blocknum = blocknum;
    }

    fn start_pending_read(&mut self) -> PgResult<bool> {
        debug_assert!(self.pending_read_nblocks > 0);
        debug_assert!(self.pending_read_nblocks <= self.io_combine_limit);
        debug_assert!(self.pinned_buffers + self.pending_read_nblocks <= self.max_pinned_buffers);

        let mut flags = self.read_buffers_flags;
        if self.advice_enabled {
            if self.pending_read_blocknum == self.seq_blocknum {
                if self.seq_until_processed != InvalidBlockNumber {
                    flags |= READ_BUFFERS_ISSUE_ADVICE;
                }
            } else {
                self.seq_until_processed = self.pending_read_blocknum;
                if self.pinned_buffers > 0 {
                    flags |= READ_BUFFERS_ISSUE_ADVICE;
                }
            }
        }

        // Forwarded buffers are already pinned (counted by the buffer
        // manager, not in pinned_buffers), so they add to the grant.
        let mut buffer_limit = if self.temporary {
            (bufmgr_seams::get_additional_local_pin_limit::call() as i64).min(PG_INT16_MAX as i64)
        } else {
            (bufmgr_seams::get_additional_pin_limit::call() as i64).min(PG_INT16_MAX as i64)
        } as i32;
        debug_assert!(self.forwarded_buffers <= self.pending_read_nblocks);
        buffer_limit = (buffer_limit + self.forwarded_buffers).min(PG_INT16_MAX);
        if buffer_limit == 0 && self.pinned_buffers == 0 {
            buffer_limit = 1; // guarantee progress
        }

        let mut nblocks = self.pending_read_nblocks;
        if buffer_limit < nblocks {
            let new_distance = self.pinned_buffers + buffer_limit;
            if self.distance > new_distance {
                self.distance = new_distance;
            }
            if self.pinned_buffers > 0 {
                return Ok(false);
            }
            nblocks = buffer_limit;
        }

        let mut buffer_index = self.next_buffer_index;
        let io_index = self.next_io_index;
        while self.initialized_buffers < buffer_index + nblocks as usize {
            self.buffers[self.initialized_buffers] = InvalidBuffer;
            self.initialized_buffers += 1;
        }
        let requested_nblocks = nblocks;
        let need_wait = bufmgr_seams::start_read_buffers::call(
            &mut self.ios[io_index].op,
            self.rel.as_deref(),
            &mut self.buffers[buffer_index..buffer_index + requested_nblocks as usize],
            self.pending_read_blocknum,
            &mut nblocks,
            flags,
        )?;
        self.pinned_buffers += nblocks;

        if !need_wait {
            if self.distance > 1 {
                self.distance -= 1;
            }
        } else {
            self.ios[io_index].buffer_index = buffer_index;
            self.next_io_index += 1;
            if self.next_io_index == self.max_ios as usize {
                self.next_io_index = 0;
            }
            debug_assert!(self.ios_in_progress < self.max_ios);
            self.ios_in_progress += 1;
            self.seq_blocknum = self.pending_read_blocknum + nblocks as BlockNumber;
        }

        let mut forwarded = 0;
        while nblocks + forwarded < requested_nblocks
            && self.buffers[buffer_index + (nblocks + forwarded) as usize] != InvalidBuffer
        {
            forwarded += 1;
        }
        self.forwarded_buffers = forwarded;

        // The read got a contiguous range that may run into the overflow
        // zone: copy the overflow to the queue front, where it's consumed;
        // both copies are cleared when handed to the consumer.
        let end = buffer_index + (nblocks + forwarded) as usize;
        if end > self.queue_size {
            let overflow = end - self.queue_size;
            debug_assert!(overflow < self.queue_size);
            let qs = self.queue_size;
            self.buffers.copy_within(qs..qs + overflow, 0);
        }

        buffer_index += nblocks as usize;
        if buffer_index >= self.queue_size {
            buffer_index -= self.queue_size;
        }
        self.next_buffer_index = buffer_index;

        self.pending_read_blocknum += nblocks as BlockNumber;
        self.pending_read_nblocks -= nblocks;

        Ok(true)
    }

    fn look_ahead<F>(&mut self, cb: &mut F) -> PgResult<()>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        // Amortize IO submission over several IOs; the callback opts in to
        // not doing anything that could deadlock on staged-unsubmitted IO.
        if !self.batch_mode {
            return self.look_ahead_ios(cb);
        }
        aio_seams::pgaio_enter_batchmode::call()?;
        let r = self.look_ahead_ios(cb);
        // Batch mode ends on the error path too (D4846). C leaves that to
        // pgaio_error_cleanup at abort; a stream ended before abort drains
        // through here again (end -> reset), and a second enter raises.
        let exit = aio_seams::pgaio_exit_batchmode::call();
        r.and(exit)
    }

    /// read_stream_look_ahead's loop, inside the batch `look_ahead` opened.
    fn look_ahead_ios<F>(&mut self, cb: &mut F) -> PgResult<()>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        while self.ios_in_progress < self.max_ios
            && self.pinned_buffers + self.pending_read_nblocks < self.distance
        {
            if self.pending_read_nblocks == self.io_combine_limit {
                self.start_pending_read()?;
                continue;
            }

            let mut buffer_index = self.next_buffer_index + self.pending_read_nblocks as usize;
            if buffer_index >= self.queue_size {
                buffer_index -= self.queue_size;
            }
            let blocknum = self.get_block(cb, Some(buffer_index))?;
            if blocknum == InvalidBlockNumber {
                self.distance = 0;
                break;
            }

            if self.pending_read_nblocks > 0
                && self.pending_read_blocknum + self.pending_read_nblocks as BlockNumber == blocknum
            {
                self.pending_read_nblocks += 1;
                continue;
            }

            while self.pending_read_nblocks > 0 {
                if !self.start_pending_read()? || self.ios_in_progress == self.max_ios {
                    self.unget_block(blocknum);
                    return Ok(());
                }
            }

            self.pending_read_blocknum = blocknum;
            self.pending_read_nblocks = 1;
        }

        if self.pending_read_nblocks > 0
            && (self.pending_read_nblocks == self.io_combine_limit
                || (self.pending_read_nblocks >= self.distance && self.pinned_buffers == 0)
                || self.distance == 0)
            && self.ios_in_progress < self.max_ios
        {
            self.start_pending_read()?;
        }

        debug_assert!(self.pinned_buffers > 0 || self.distance == 0);
        Ok(())
    }

    /// read_stream_next_buffer: the next pinned buffer in callback order
    /// (the pin passes to the caller), or InvalidBuffer at end of stream.
    #[inline]
    pub fn next_buffer<F>(&mut self, cb: &mut F) -> PgResult<Buffer>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        self.next_buffer_with_data(cb).map(|(b, _)| b)
    }

    pub fn next_buffer_with_data<F>(&mut self, cb: &mut F) -> PgResult<(Buffer, D)>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        if self.fast_path {
            return self.next_buffer_fast(cb).map(|b| (b, D::default()));
        }

        if self.pinned_buffers == 0 {
            debug_assert!(self.oldest_buffer_index == self.next_buffer_index);
            if self.distance == 0 {
                return Ok((InvalidBuffer, D::default()));
            }
            self.look_ahead(cb)?;
            if self.pinned_buffers == 0 {
                debug_assert!(self.distance == 0);
                return Ok((InvalidBuffer, D::default()));
            }
        }

        debug_assert!(self.pinned_buffers > 0);
        let oldest_buffer_index = self.oldest_buffer_index;
        let buffer = self.buffers[oldest_buffer_index];
        let data = if Self::has_data() {
            self.per_buffer_data[oldest_buffer_index]
        } else {
            D::default()
        };
        debug_assert!(buffer != InvalidBuffer);

        if self.ios_in_progress > 0
            && self.ios[self.oldest_io_index].buffer_index == oldest_buffer_index
        {
            let io_index = self.oldest_io_index;
            bufmgr_seams::wait_read_buffers::call(&mut self.ios[io_index].op, self.rel.as_deref())?;

            debug_assert!(self.ios_in_progress > 0);
            self.ios_in_progress -= 1;
            self.oldest_io_index += 1;
            if self.oldest_io_index == self.max_ios as usize {
                self.oldest_io_index = 0;
            }

            self.distance = (self.distance * 2).min(self.max_pinned_buffers);

            if self.advice_enabled && self.ios[io_index].op.blocknum == self.seq_until_processed {
                self.seq_until_processed = InvalidBlockNumber;
            }
        }

        self.buffers[oldest_buffer_index] = InvalidBuffer;
        if oldest_buffer_index < (self.io_combine_limit - 1) as usize {
            self.buffers[self.queue_size + oldest_buffer_index] = InvalidBuffer;
        }

        debug_assert!(self.pinned_buffers > 0);
        self.pinned_buffers -= 1;

        self.oldest_buffer_index += 1;
        if self.oldest_buffer_index == self.queue_size {
            self.oldest_buffer_index = 0;
        }

        self.look_ahead(cb)?;

        if self.ios_in_progress == 0
            && self.forwarded_buffers == 0
            && self.pinned_buffers == 1
            && self.distance == 1
            && self.pending_read_nblocks == 0
            && !Self::has_data()
        {
            if self.oldest_buffer_index < (self.io_combine_limit - 1) as usize {
                self.buffers[self.queue_size + self.oldest_buffer_index] = InvalidBuffer;
            }
            self.fast_path = true;
        }

        Ok((buffer, data))
    }

    fn next_buffer_fast<F>(&mut self, cb: &mut F) -> PgResult<Buffer>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        debug_assert!(self.ios_in_progress == 0);
        debug_assert!(self.forwarded_buffers == 0);
        debug_assert!(self.pinned_buffers == 1);
        debug_assert!(self.distance == 1);
        debug_assert!(self.pending_read_nblocks == 0);
        debug_assert!(!Self::has_data());

        let oldest_buffer_index = self.oldest_buffer_index;
        let buffer = self.buffers[oldest_buffer_index];
        debug_assert!(buffer != InvalidBuffer);

        let next_blocknum = self.get_block(cb, None)?;
        if next_blocknum != InvalidBlockNumber {
            let mut flags = self.read_buffers_flags;
            if self.advice_enabled {
                flags |= READ_BUFFERS_ISSUE_ADVICE;
            }
            let started = bufmgr_seams::start_read_buffer::call(
                &mut self.ios[0].op,
                self.rel.as_deref(),
                &mut self.buffers[oldest_buffer_index],
                next_blocknum,
                flags,
            );
            let started = match started {
                Ok(s) => s,
                Err(e) => {
                    // `buffer` never reaches the caller; the slot keeps
                    // whatever StartReadBuffer pinned for the abort path.
                    let _ = bufmgr_seams::release_buffer::call(buffer);
                    self.pinned_buffers =
                        (self.buffers[oldest_buffer_index] != InvalidBuffer) as i32;
                    self.fast_path = false;
                    return Err(e);
                }
            };
            if !started {
                return Ok(buffer);
            }
            self.oldest_io_index = 0;
            self.next_io_index = if self.max_ios > 1 { 1 } else { 0 };
            self.ios_in_progress = 1;
            self.ios[0].buffer_index = oldest_buffer_index;
            self.seq_blocknum = next_blocknum + 1;
        } else {
            self.distance = 0;
            self.oldest_buffer_index = self.next_buffer_index;
            self.pinned_buffers = 0;
            self.buffers[oldest_buffer_index] = InvalidBuffer;
        }
        self.fast_path = false;
        Ok(buffer)
    }

    pub fn next_block<F>(&mut self, cb: &mut F) -> PgResult<(BlockNumber, BufferAccessStrategy)>
    where
        F: FnMut(&mut D) -> PgResult<BlockNumber>,
    {
        let strategy = self.ios[0].op.strategy.clone();
        Ok((self.get_block(cb, None)?, strategy))
    }

    pub fn clear_strategy(&mut self) {
        for io in self.ios.iter_mut() {
            io.op.strategy = None;
        }
    }

    /// read_stream_reset: release queued buffers so the stream can be used
    /// again from a fresh callback position.
    pub fn reset(&mut self) -> PgResult<()> {
        self.distance = 0;
        self.buffered_blocknum = InvalidBlockNumber;
        self.fast_path = false;

        let mut no_more = |_: &mut D| -> PgResult<BlockNumber> { Ok(InvalidBlockNumber) };
        loop {
            let buffer = self.next_buffer(&mut no_more)?;
            if buffer == InvalidBuffer {
                break;
            }
            bufmgr_seams::release_buffer::call(buffer)?;
        }

        self.release_forwarded()?;

        debug_assert!(self.forwarded_buffers == 0);
        debug_assert!(self.pinned_buffers == 0);
        debug_assert!(self.ios_in_progress == 0);

        self.distance = 1;
        Ok(())
    }

    fn release_forwarded(&mut self) -> PgResult<()> {
        let mut index = self.next_buffer_index;
        while index < self.initialized_buffers && self.buffers[index] != InvalidBuffer {
            let buffer = self.buffers[index];
            debug_assert!(self.forwarded_buffers > 0);
            self.forwarded_buffers -= 1;
            self.buffers[index] = InvalidBuffer;
            if index < (self.io_combine_limit - 1) as usize {
                self.buffers[self.queue_size + index] = InvalidBuffer;
            }
            bufmgr_seams::release_buffer::call(buffer)?;
            index += 1;
            if index == self.queue_size {
                index = 0;
            }
        }
        Ok(())
    }

    pub fn end(mut self) -> PgResult<()> {
        let r = self.reset();
        // Reset released everything (or errored mid-way: the abort-path Drop
        // below mops up what is left).
        drop(self);
        r
    }

    #[cfg(test)]
    fn state(&self) -> (i32, i32, i32, i32, bool) {
        (self.distance, self.pinned_buffers, self.ios_in_progress, self.forwarded_buffers, self.fast_path)
    }

    #[cfg(test)]
    fn max_pinned(&self) -> i32 {
        self.max_pinned_buffers
    }
}

// Abort path (docs/no-drop.md): an in-flight IO holds a raw pointer to its
// op's io_return, so wait it out before the ops are freed, then release the
// pins the stream still owns (queued and forwarded).
impl<'mcx, D: Copy + Default, R: Deref<Target = RelationData<'mcx>>> Drop for ReadStream<'mcx, D, R> {
    fn drop(&mut self) {
        let mut index = self.oldest_io_index;
        for _ in 0..self.ios_in_progress {
            let wref = self.ios[index].op.io_wref;
            if aio_core::pgaio_wref_valid(&wref) {
                let _ = aio_core::pgaio_wref_wait(&wref);
            }
            index += 1;
            if index == self.max_ios as usize {
                index = 0;
            }
        }
        self.ios_in_progress = 0;
        let mut index = self.oldest_buffer_index;
        for _ in 0..self.pinned_buffers {
            let buffer = self.buffers[index];
            if buffer != InvalidBuffer {
                let _ = bufmgr_seams::release_buffer::call(buffer);
            }
            index += 1;
            if index == self.queue_size {
                index = 0;
            }
        }
        self.pinned_buffers = 0;
        self.oldest_buffer_index = self.next_buffer_index;
        let _ = self.release_forwarded();
    }
}

/// Sizing and batching seams for harnesses that fake the buffer manager;
/// each is installed only when absent.
pub mod testing {
    pub fn install_stream_env() {
        use bufmgr_seams as b;
        if !b::io_combine_limit::is_installed() {
            b::io_combine_limit::set(|| 16);
        }
        if !b::get_pin_limit::is_installed() {
            b::get_pin_limit::set(|| 1000);
            b::get_additional_pin_limit::set(|| 1000);
            b::get_local_pin_limit::set(|| 1000);
            b::get_additional_local_pin_limit::set(|| 1000);
            b::get_access_strategy_pin_limit::set(|_| 1000);
        }
        if !aio_seams::pgaio_enter_batchmode::is_installed() {
            aio_seams::pgaio_enter_batchmode::set(|| Ok(()));
            aio_seams::pgaio_exit_batchmode::set(|| Ok(()));
        }
        if !catalog_seams::is_catalog_relation_oid::is_installed() {
            catalog_seams::is_catalog_relation_oid::set(|_| true);
        }
        let get = guc_tables::GucVarAccessors { get: || 16, set: |_| {} };
        guc_tables::vars::effective_io_concurrency.install_if_absent(get);
        let get = guc_tables::GucVarAccessors { get: || 16, set: |_| {} };
        guc_tables::vars::maintenance_io_concurrency.install_if_absent(get);
        if !b::relation_smgr_locator::is_installed() {
            b::relation_smgr_locator::set(|rel| types_storage::RelFileLocatorBackend {
                locator: rel.rd_locator.get(),
                backend: types_core::INVALID_PROC_NUMBER,
            });
        }
    }

    /// Plus StartReadBuffer(s) over the harness's own ReadBuffer fake: every
    /// block reads as a hit, one block per call.
    pub fn install_resident_reads() {
        use bufmgr_seams as b;
        install_stream_env();
        if b::start_read_buffers::is_installed() {
            return;
        }
        fn pin(
            op: &b::ReadBuffersOperation,
            rel: Option<&types_rel::RelationData<'_>>,
            block: types_core::BlockNumber,
        ) -> types_error::PgResult<types_core::Buffer> {
            let rel = rel.expect("resident reads need the relation");
            if b::read_buffer_extended::is_installed() {
                let mode = types_storage::ReadBufferMode::Normal;
                b::read_buffer_extended::call(rel, op.forknum, block, mode, op.strategy.clone())
            } else {
                b::read_buffer_strategy::call(rel, block, op.strategy.clone())
            }
        }
        b::start_read_buffers::set(|op, rel, buffers, blocknum, nblocks, _flags| {
            if buffers[0] == types_core::InvalidBuffer {
                buffers[0] = pin(op, rel, blocknum)?;
            }
            *nblocks = 1;
            Ok(false)
        });
        b::start_read_buffer::set(|op, rel, buffer, blocknum, _flags| {
            *buffer = pin(op, rel, blocknum)?;
            Ok(false)
        });
        b::wait_read_buffers::set(|_op, _rel| panic!("resident reads need no wait"));
    }
}

#[cfg(test)]
mod tests;
