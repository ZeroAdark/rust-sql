use types_core::{ForkNumber, TransactionId, BLCKSZ};
use types_error::PgResult;
use types_rel::Relation;

pub trait PageSource {
    fn nblocks(&self) -> PgResult<u32>;
    /// Call `f` with the page of `block`, share-locked for the call.
    fn read(&self, block: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()>;
}

pub trait TxnOracle {
    fn is_current(&self, xid: TransactionId) -> bool;
    fn in_progress(&self, xid: TransactionId) -> PgResult<bool>;
    /// Asked after `!in_progress`, as heap does.
    fn committed(&self, xid: TransactionId) -> PgResult<bool>;
    /// `GetOldestNonRemovableTransactionId(companion)`: asked at most once per refresh.
    fn horizon(&self) -> PgResult<TransactionId>;
    fn oldest_clog(&self) -> TransactionId;
}

pub struct BufPages<'a, 'mcx> {
    pub rel: &'a Relation<'mcx>,
}

impl PageSource for BufPages<'_, '_> {
    fn nblocks(&self) -> PgResult<u32> {
        bufmgr::RelationGetNumberOfBlocksInFork(self.rel, ForkNumber::MAIN_FORKNUM)
    }

    fn read(&self, block: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        let buf = bufmgr::ReadBufferExtended(self.rel, ForkNumber::MAIN_FORKNUM, block, types_storage::ReadBufferMode::Normal, None)?;
        if let Err(e) = bufmgr::LockBuffer(buf, types_storage::buf::BUFFER_LOCK_SHARE) {
            let _ = bufmgr::ReleaseBuffer(buf);
            return Err(e);
        }
        let p = bufmgr::BufferGetPagePtr(buf);
        // SAFETY: a pinned, share-locked buffer page of BLCKSZ bytes, alive until the unlock below.
        let page = unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, BLCKSZ) };
        let out = f(page);
        let rel = bufmgr::UnlockReleaseBuffer(buf);
        out.and(rel)
    }
}

pub struct Procs<'a, 'mcx> {
    pub rel: &'a Relation<'mcx>,
}

impl TxnOracle for Procs<'_, '_> {
    fn is_current(&self, xid: TransactionId) -> bool {
        xact::TransactionIdIsCurrentTransactionId(xid)
    }

    fn in_progress(&self, xid: TransactionId) -> PgResult<bool> {
        procarray::TransactionIdIsInProgress(xid)
    }

    fn committed(&self, xid: TransactionId) -> PgResult<bool> {
        transam_seams::transaction_id_did_commit::call(xid)
    }

    fn horizon(&self) -> PgResult<TransactionId> {
        procarray::GetOldestNonRemovableTransactionId(self.rel)
    }

    fn oldest_clog(&self) -> TransactionId {
        varsup::TransamVariables().oldestClogXid.load(std::sync::atomic::Ordering::Relaxed)
    }
}
