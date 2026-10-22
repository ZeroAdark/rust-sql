// A thread's pending counts taken as a value and folded into another thread's
// pending. Gravity's pool workers are threads of the backend with pgstat
// thread-locals of their own and no pgstat_report_stat of their own: the
// leader takes each claim's counts and reports them as the serial backend
// that PostgreSQL's plan (parallelism assessed off) would have been.

use core::mem::replace;

use types_core::InvalidOid;

use crate::io::{
    with_pending_block, PgStat_PendingIO, IOCONTEXT_NUM_TYPES, IOOBJECT_NUM_TYPES,
    IOOP_NUM_TYPES, PENDING_IO_ZERO,
};
use crate::pending::{self, PendingData, PgStat_HashKey, PGSTAT_KIND_RELATION};
use crate::relation::PgStat_TableCounts;
use crate::slru::{PgStat_SLRUStats, SLRU_NUM_ELEMENTS};
use crate::PgStat_Counter;

pub struct ThreadDeltas {
    pub io: PgStat_PendingIO,
    pub relations: Vec<(PgStat_HashKey, PgStat_TableCounts)>,
    /// D4241: the pending block read and write time (pgStatBlockReadTime / pgStatBlockWriteTime, pg_stat_database's
    /// blk_read_time / blk_write_time).
    pub block_time: (PgStat_Counter, PgStat_Counter),
    /// D4241: the pending SLRU counts (pg_stat_slru), None when the thread had none.
    pub slru: Option<[PgStat_SLRUStats; SLRU_NUM_ELEMENTS]>,
}

impl ThreadDeltas {
    pub const fn empty() -> ThreadDeltas {
        ThreadDeltas { io: PENDING_IO_ZERO, relations: Vec::new(), block_time: (0, 0), slru: None }
    }

    pub fn is_empty(&self) -> bool {
        self.io == PENDING_IO_ZERO && self.relations.is_empty() && self.block_time == (0, 0) && self.slru.is_none()
    }

    pub fn add(&mut self, other: &ThreadDeltas) {
        add_io(&mut self.io, &other.io);
        for &(key, counts) in &other.relations {
            match self.relations.iter_mut().find(|(k, _)| *k == key) {
                Some((_, mine)) => add_counts(mine, &counts),
                None => self.relations.push((key, counts)),
            }
        }
        self.block_time.0 += other.block_time.0;
        self.block_time.1 += other.block_time.1;
        if let Some(theirs) = &other.slru {
            let mine = self.slru.get_or_insert([PgStat_SLRUStats::default(); SLRU_NUM_ELEMENTS]);
            for (m, o) in mine.iter_mut().zip(theirs) {
                add_slru(m, o);
            }
        }
    }
}

fn add_slru(dst: &mut PgStat_SLRUStats, src: &PgStat_SLRUStats) {
    dst.blocks_zeroed += src.blocks_zeroed;
    dst.blocks_hit += src.blocks_hit;
    dst.blocks_read += src.blocks_read;
    dst.blocks_written += src.blocks_written;
    dst.blocks_exists += src.blocks_exists;
    dst.flush += src.flush;
    dst.truncate += src.truncate;
}

fn add_io(dst: &mut PgStat_PendingIO, src: &PgStat_PendingIO) {
    for o in 0..IOOBJECT_NUM_TYPES {
        for c in 0..IOCONTEXT_NUM_TYPES {
            for p in 0..IOOP_NUM_TYPES {
                dst.counts[o][c][p] += src.counts[o][c][p];
                dst.bytes[o][c][p] += src.bytes[o][c][p];
                dst.pending_times_ns[o][c][p] += src.pending_times_ns[o][c][p];
            }
        }
    }
}

fn add_counts(dst: &mut PgStat_TableCounts, src: &PgStat_TableCounts) {
    dst.numscans += src.numscans;
    dst.tuples_returned += src.tuples_returned;
    dst.tuples_fetched += src.tuples_fetched;
    dst.tuples_inserted += src.tuples_inserted;
    dst.tuples_updated += src.tuples_updated;
    dst.tuples_deleted += src.tuples_deleted;
    dst.tuples_hot_updated += src.tuples_hot_updated;
    dst.tuples_newpage_updated += src.tuples_newpage_updated;
    dst.delta_live_tuples += src.delta_live_tuples;
    dst.delta_dead_tuples += src.delta_dead_tuples;
    dst.changed_tuples += src.changed_tuples;
    dst.blocks_fetched += src.blocks_fetched;
    dst.blocks_hit += src.blocks_hit;
}

pub fn take_thread_deltas() -> Option<ThreadDeltas> {
    let has_io = with_pending_block(|blk| blk.have_iostats || blk.backend_has_iostats);
    let has_relations = !pending::pending_is_empty();
    // D4241: the block I/O time and the SLRU counts a claim's thread holds, which it never reports
    let block_time = crate::database::take_block_time_pending();
    let slru = crate::slru::take_slru_pending();
    if !has_io && !has_relations && block_time == (0, 0) && slru.is_none() {
        return None;
    }
    let mut d = ThreadDeltas { block_time, slru, ..ThreadDeltas::empty() };
    if has_io {
        with_pending_block(|blk| {
            d.io = replace(&mut blk.io, PENDING_IO_ZERO);
            blk.backend = PENDING_IO_ZERO;
            blk.have_iostats = false;
            blk.backend_has_iostats = false;
        });
    }
    if has_relations {
        pending::with_state(|st| {
            let keys: Vec<PgStat_HashKey> = st
                .pending_order
                .iter()
                .copied()
                .filter(|k| k.kind == PGSTAT_KIND_RELATION)
                .collect();
            for key in keys {
                let taken = match st.pending.get(&key) {
                    Some(PendingData::Relation(rp)) => {
                        let t = unsafe { &*rp.as_ptr() };
                        t.trans.is_empty().then_some(t.counts)
                    }
                    _ => None,
                };
                if let Some(counts) = taken {
                    st.delete_pending_entry(key);
                    if counts != PgStat_TableCounts::default() {
                        d.relations.push((key, counts));
                    }
                }
            }
        });
    }
    (!d.is_empty()).then_some(d)
}

pub fn discard_thread_deltas() {
    drop(take_thread_deltas());
}

/// `d` into this thread's pending as its own counts: the IO under this backend's type (the
/// per-backend half when the type tracks one), each relation's counts under its key.
pub fn fold_thread_deltas(d: &ThreadDeltas) {
    if d.io != PENDING_IO_ZERO {
        let track_backend =
            crate::backend::pgstat_tracks_backend_bktype(miscinit::GetMyBackendType());
        with_pending_block(|blk| {
            add_io(&mut blk.io, &d.io);
            blk.have_iostats = true;
            if track_backend {
                add_io(&mut blk.backend, &d.io);
                blk.backend_has_iostats = true;
            }
        });
        pending::pgstat_report_fixed_set();
    }
    if !d.relations.is_empty() {
        pending::with_state(|st| {
            for &(key, counts) in &d.relations {
                if let PendingData::Relation(rp) = st.prep_pending_entry(key) {
                    let t = unsafe { &mut *rp.as_ptr() };
                    t.id = key.objid as types_core::Oid;
                    t.shared = key.dboid == InvalidOid;
                    add_counts(&mut t.counts, &counts);
                }
            }
        });
    }
    // D4241: the claims' block I/O time and SLRU counts, the leader's to report
    if d.block_time.0 != 0 {
        crate::database::pgstat_count_buffer_read_time(d.block_time.0);
    }
    if d.block_time.1 != 0 {
        crate::database::pgstat_count_buffer_write_time(d.block_time.1);
    }
    if let Some(slru) = &d.slru {
        crate::slru::add_slru_pending(slru);
    }
}
