//! The index-relation surface the btree read path consumes: descent, page
//! reads, scan-key preprocessing and start-key construction take any
//! `BtReadRel` instead of a relcache entry. The relcache implementation
//! (`Relation`) delegates to the relcache fields, bufmgr, the predicate-lock
//! manager and the syscache exactly as the read path did when it held the
//! relation directly.

use ::types_core::{BlockNumber, Buffer, Oid};
use ::types_error::PgResult;
use ::types_fmgr::FmgrInfo;
use ::types_nbtree::{BTMetaPageData, SkipSupportData};
use ::types_rel::Relation;
use ::types_snapshot::SnapshotData;
use ::types_tuple::TupleDescData;

/// What a btree scan reads from its index relation.
///
/// Column accessors take 0-based key column positions (`i`) unless the
/// parameter is named `attno` (1-based, as `sk_attno`).
pub trait BtReadRel {
    /// RelationGetRelationName, for error messages.
    fn name(&self) -> &str;
    /// IndexRelationGetNumberOfAttributes.
    fn indnatts(&self) -> i32;
    /// IndexRelationGetNumberOfKeyAttributes.
    fn indnkeyatts(&self) -> i32;
    /// RelationGetDescr.
    fn tupdesc(&self) -> &TupleDescData<'_>;
    /// rd_opcintype[i].
    fn opcintype(&self, i: usize) -> Oid;
    /// rd_opfamily[i].
    fn opfamily(&self, i: usize) -> Oid;
    /// rd_indoption, one entry per key column.
    fn indoption(&self) -> &[i16];
    /// rd_indcollation[i].
    fn indcollation(&self, i: usize) -> Oid;
    /// The cached metapage copy (rd_amcache).
    fn amcache_get(&self) -> Option<BTMetaPageData>;
    /// Replaces (or with `None`, discards) the cached metapage copy.
    fn amcache_set(&self, metad: Option<BTMetaPageData>);
    fn order_procinfo(&self, attno: usize) -> PgResult<FmgrInfo>;
    /// pgstat_should_count_relation.
    fn pgstat_enabled(&self) -> bool;
    /// ReadBuffer: pin `blkno`.
    fn read_buffer(&self, blkno: BlockNumber) -> PgResult<Buffer>;
    /// ReleaseAndReadBuffer: drop `old` (if valid) and pin `blkno`.
    fn release_and_read_buffer(&self, old: Buffer, blkno: BlockNumber) -> PgResult<Buffer>;
    /// PredicateLockPage.
    fn predicate_lock_page(&self, blkno: BlockNumber, snapshot: &SnapshotData<'_>) -> PgResult<()>;
    /// PredicateLockRelation.
    fn predicate_lock_relation(&self, snapshot: &SnapshotData<'_>) -> PgResult<()>;
    /// get_opfamily_proc.
    fn opfamily_proc(
        &self,
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        procnum: i16,
    ) -> PgResult<Oid>;
    /// get_opfamily_member.
    fn opfamily_member(
        &self,
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        strategy: i16,
    ) -> PgResult<Oid>;
    /// get_opcode.
    fn opcode(&self, opno: Oid) -> PgResult<Oid>;
    /// fmgr_info.
    fn fmgr_info(&self, proc: Oid) -> PgResult<FmgrInfo>;
    /// get_typlenbyvalalign.
    fn typlenbyvalalign(&self, typid: Oid) -> PgResult<(i16, bool, i8)>;
    /// PrepareSkipSupportFromOpclass.
    fn skip_support(
        &self,
        opfamily: Oid,
        opcintype: Oid,
        reverse: bool,
    ) -> PgResult<Option<SkipSupportData>>;
}

impl<'mcx> BtReadRel for Relation<'mcx> {
    #[inline]
    fn name(&self) -> &str {
        ::types_rel::RelationData::name(self)
    }

    #[inline]
    fn indnatts(&self) -> i32 {
        ::types_rel::RelationData::indnatts(self)
    }

    #[inline]
    fn indnkeyatts(&self) -> i32 {
        ::types_rel::RelationData::indnkeyatts(self)
    }

    #[inline]
    fn tupdesc(&self) -> &TupleDescData<'_> {
        &self.rd_att
    }

    #[inline]
    fn opcintype(&self, i: usize) -> Oid {
        self.rd_opcintype[i]
    }

    #[inline]
    fn opfamily(&self, i: usize) -> Oid {
        self.rd_opfamily[i]
    }

    #[inline]
    fn indoption(&self) -> &[i16] {
        &self.rd_indoption
    }

    #[inline]
    fn indcollation(&self, i: usize) -> Oid {
        self.rd_indcollation[i]
    }

    #[inline]
    fn amcache_get(&self) -> Option<BTMetaPageData> {
        self.rd_amcache.get()
    }

    #[inline]
    fn amcache_set(&self, metad: Option<BTMetaPageData>) {
        self.rd_amcache.set(metad)
    }

    #[inline]
    fn order_procinfo(&self, attno: usize) -> PgResult<FmgrInfo> {
        crate::search::relation_order_procinfo(self, attno)
    }

    #[inline]
    fn pgstat_enabled(&self) -> bool {
        self.pgstat_enabled.get()
    }

    #[inline]
    fn read_buffer(&self, blkno: BlockNumber) -> PgResult<Buffer> {
        ::bufmgr_seams::read_buffer::call(self, blkno)
    }

    #[inline]
    fn release_and_read_buffer(&self, old: Buffer, blkno: BlockNumber) -> PgResult<Buffer> {
        ::bufmgr_seams::release_and_read_buffer::call(old, self, blkno)
    }

    #[inline]
    fn predicate_lock_page(&self, blkno: BlockNumber, snapshot: &SnapshotData<'_>) -> PgResult<()> {
        ::predicate_seams::predicate_lock_page::call(self, blkno, snapshot)
    }

    #[inline]
    fn predicate_lock_relation(&self, snapshot: &SnapshotData<'_>) -> PgResult<()> {
        ::predicate_seams::predicate_lock_relation::call(self, snapshot)
    }

    #[inline]
    fn opfamily_proc(
        &self,
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        procnum: i16,
    ) -> PgResult<Oid> {
        ::lsyscache::get_opfamily_proc(opfamily, lefttype, righttype, procnum)
    }

    #[inline]
    fn opfamily_member(
        &self,
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        strategy: i16,
    ) -> PgResult<Oid> {
        ::lsyscache::get_opfamily_member(opfamily, lefttype, righttype, strategy)
    }

    #[inline]
    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        ::lsyscache::get_opcode(opno)
    }

    #[inline]
    fn fmgr_info(&self, proc: Oid) -> PgResult<FmgrInfo> {
        ::fmgr_core::fmgr_info(proc)
    }

    #[inline]
    fn typlenbyvalalign(&self, typid: Oid) -> PgResult<(i16, bool, i8)> {
        ::lsyscache::get_typlenbyvalalign(typid)
    }

    #[inline]
    fn skip_support(
        &self,
        opfamily: Oid,
        opcintype: Oid,
        reverse: bool,
    ) -> PgResult<Option<SkipSupportData>> {
        ::skipsupport::prepare_skip_support_from_opclass(opfamily, opcintype, reverse)
    }
}
