use std::sync::atomic::Ordering::Relaxed;

use types_core::{
    LocalTransactionId, Oid, ProcNumber, TransactionId, VirtualTransactionId, InvalidOid,
    InvalidLocalTransactionId, InvalidTransactionId, INVALID_PROC_NUMBER,
};
use types_error::PgResult;
use types_storage::lock::{
    ExclusiveLock, LockMethod, ShareLock, ShareUpdateExclusiveLock, DEFAULT_LOCKMETHOD, LOCKMODE,
    LOCKTAG, LOCKTAG_RELATION,
};
use types_storage::storage::{Spinlock, SyncCell, PGPROC, FP_LOCK_SLOTS_PER_GROUP};

use crate::locallock::with_local;
use crate::shared::{find_lock, find_proclock, LockHashPartitionLock, LockTagHashCode, SetupLockInTable};

pub(crate) const FAST_PATH_BITS_PER_SLOT: u32 = 3;
pub(crate) const FAST_PATH_LOCKNUMBER_OFFSET: LOCKMODE = 1;
pub(crate) const FAST_PATH_MASK: u64 = (1 << FAST_PATH_BITS_PER_SLOT) - 1;

pub(crate) const FAST_PATH_STRONG_LOCK_HASH_BITS: u32 = 10;
pub(crate) const FAST_PATH_STRONG_LOCK_HASH_PARTITIONS: usize =
    1 << FAST_PATH_STRONG_LOCK_HASH_BITS;

pub(crate) fn FastPathStrongLockHashPartition(hashcode: u32) -> u32 {
    hashcode % FAST_PATH_STRONG_LOCK_HASH_PARTITIONS as u32
}

#[repr(C)]
pub(crate) struct FastPathStrongRelationLockData {
    mutex: Spinlock,
    // SeqCst count ops + SeqCst hasMessages ops (sinval) restore C's guarantee:
    // observing the decrement implies observing every store the releasing backend made before it.
    count: [std::sync::atomic::AtomicU32; FAST_PATH_STRONG_LOCK_HASH_PARTITIONS],
}

const _: () = assert!(
    std::mem::size_of::<FastPathStrongRelationLockData>() == 4100,
    "sizeof(FastPathStrongRelationLockData) is 4100 in C 18.6"
);

pub(crate) fn shmem_init_strong_locks() -> PgResult<&'static FastPathStrongRelationLockData> {
    let (raw, found) = shmem::ShmemInitStruct(
        "Fast Path Strong Relation Lock Data",
        std::mem::size_of::<FastPathStrongRelationLockData>(),
    )?;
    // SAFETY: a cache-line-aligned, zeroed ShmemIndex block of exactly
    // sizeof(FastPathStrongRelationLockData) bytes (4-aligned atomics over
    // zero bytes are valid values); leaked for the cluster lifetime like C
    // shmem, and every field is an atomic, so sharing it across backend
    // threads is sound.
    let data = unsafe { &*raw.cast::<FastPathStrongRelationLockData>() };
    if !found {
        data.mutex.unlock();
    }
    Ok(data)
}

fn strong_locks() -> &'static FastPathStrongRelationLockData {
    crate::shared::shared().fast_path_strong
}

fn strong_spin_acquire() {
    let lock = &strong_locks().mutex;
    if lock.tas() != 0 {
        let mut delay = s_lock_seams::SpinDelayStatus::new(
            file!(),
            line!() as i32,
            "FastPathStrongRelationLocks",
        );
        while lock.tas_spin() != 0 {
            s_lock_seams::perform_spin_delay::call(&mut delay);
        }
        s_lock_seams::finish_spin_delay::call(&delay);
    }
}

pub(crate) fn strong_lock_count(fasthashcode: u32) -> u32 {
    // SeqCst pairs with the SeqCst decrement + sinval hasMessages ops to keep
    // invalidation delivery ordered before a fast-path grant that observes the release.
    strong_locks().count[fasthashcode as usize]
        .load(std::sync::atomic::Ordering::SeqCst)
}

pub(crate) fn increment_strong_lock_count_partition(fasthashcode: u32) {
    strong_spin_acquire();
    strong_locks().count[fasthashcode as usize]
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    strong_locks().mutex.unlock();
}

pub(crate) fn decrement_strong_lock_count_partition(fasthashcode: u32) {
    strong_spin_acquire();
    let prev = strong_locks().count[fasthashcode as usize]
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    debug_assert!(prev > 0);
    strong_locks().mutex.unlock();
}

pub(crate) fn decrement_strong_lock_count(hashcode: u32) {
    decrement_strong_lock_count_partition(FastPathStrongLockHashPartition(hashcode));
}

// Exclusive postmaster-thread access per the crash choreography.
pub(crate) fn reset_strong_locks_after_crash() {
    let strong = strong_locks();
    strong.mutex.unlock();
    for cell in strong.count.iter() {
        cell.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) fn fp_groups_per_backend() -> u32 {
    lmgr_proc::ProcGlobal().fpLockGroupsPerBackend
}

pub fn FastPathLockSlotsPerBackend() -> u32 {
    fp_groups_per_backend() * FP_LOCK_SLOTS_PER_GROUP as u32
}

// The constant and power-of-two masking must match C's FAST_PATH_REL_GROUP.
pub(crate) fn fast_path_rel_group(relid: Oid) -> u32 {
    ((relid as u64).wrapping_mul(49157) & (fp_groups_per_backend() as u64 - 1)) as u32
}

// Never lower than the true count: only we add locks on our own behalf.
fn local_use_count(group: u32) -> i32 {
    let proc = lmgr_proc::GetPGProcByNumber(crate::my_procno());
    // SAFETY: own proc; counts are written only by us (under our fpInfoLock).
    unsafe { proc.fp_use_counts(fp_groups_per_backend() as usize)[group as usize].get() }
}

pub(crate) fn eligible_for_relation_fast_path(locktag: &LOCKTAG, mode: LOCKMODE) -> bool {
    let my_db = init_small::globals::MyDatabaseId();
    locktag.locktag_lockmethodid == DEFAULT_LOCKMETHOD
        && locktag.locktag_type == LOCKTAG_RELATION
        && locktag.locktag_field1 == my_db
        && my_db != InvalidOid
        && mode < ShareUpdateExclusiveLock
}

pub(crate) fn ConflictsWithRelationFastPath(locktag: &LOCKTAG, mode: LOCKMODE) -> bool {
    locktag.locktag_lockmethodid == DEFAULT_LOCKMETHOD
        && locktag.locktag_type == LOCKTAG_RELATION
        && locktag.locktag_field1 != InvalidOid
        && mode > ShareUpdateExclusiveLock
}

pub(crate) fn fast_path_group_in_use(relid: Oid) -> bool {
    local_use_count(fast_path_rel_group(relid)) > 0
}

/// SAFETY contract: proc's fpInfoLock held.
pub(crate) unsafe fn fp_group_conflicts(
    proc: &PGPROC,
    group: u32,
    relid: Oid,
    dbid: Oid,
    conflict_mask: types_storage::lock::LOCKMASK,
) -> bool {
    let view = fp_view(proc);
    if proc.databaseId.load(Relaxed) != dbid || view.group_bits(group) == 0 {
        return false;
    }
    for j in 0..FP_LOCK_SLOTS_PER_GROUP as u32 {
        let f = group * FP_LOCK_SLOTS_PER_GROUP as u32 + j;
        if view.relid(f) != relid {
            continue;
        }
        let lockmask = view.get_bits(f);
        if lockmask == 0 {
            continue;
        }
        let lockmask = (lockmask << FAST_PATH_LOCKNUMBER_OFFSET) as types_storage::lock::LOCKMASK;
        return lockmask & conflict_mask != 0;
    }
    false
}

pub(crate) fn fast_path_local_can_try(locktag: &LOCKTAG, mode: LOCKMODE) -> bool {
    eligible_for_relation_fast_path(locktag, mode)
        && local_use_count(fast_path_rel_group(locktag.locktag_field2))
            < FP_LOCK_SLOTS_PER_GROUP
}

pub(crate) fn fp_info_lock(proc: &PGPROC) -> &lwlock::LWLock {
    const _: () = {
        assert!(
            core::mem::size_of::<types_storage::storage::LWLock>()
                == core::mem::size_of::<lwlock::LWLock>()
        );
        assert!(
            core::mem::offset_of!(types_storage::storage::LWLock, state)
                == core::mem::offset_of!(lwlock::LWLock, state)
        );
        assert!(
            core::mem::offset_of!(types_storage::storage::LWLock, waiters)
                == core::mem::offset_of!(lwlock::LWLock, waiters)
        );
    };
    // SAFETY: identical repr(C) layouts, asserted above; both types carry the
    // same atomics/UnsafeCell synchronization contract.
    unsafe { &*(&proc.fpInfoLock as *const types_storage::storage::LWLock as *const lwlock::LWLock) }
}

pub(crate) struct FpView<'a> {
    bits: &'a [SyncCell<u64>],
    relids: &'a [SyncCell<Oid>],
    use_counts: &'a [SyncCell<i32>],
}

/// SAFETY contract: proc's fpInfoLock held.
pub(crate) unsafe fn fp_view(proc: &PGPROC) -> FpView<'_> {
    let groups = fp_groups_per_backend() as usize;
    FpView {
        bits: proc.fp_lock_bits(groups),
        relids: proc.fp_rel_id(groups),
        use_counts: proc.fp_use_counts(groups),
    }
}

impl FpView<'_> {
    pub(crate) fn get_bits(&self, f: u32) -> u64 {
        let group = f / FP_LOCK_SLOTS_PER_GROUP as u32;
        let index = f % FP_LOCK_SLOTS_PER_GROUP as u32;
        // SAFETY: [FPL] serialized by fpInfoLock
        (unsafe { self.bits[group as usize].get() } >> (FAST_PATH_BITS_PER_SLOT * index)) & FAST_PATH_MASK
    }

    fn bit_position(f: u32, l: LOCKMODE) -> u64 {
        debug_assert!(l >= FAST_PATH_LOCKNUMBER_OFFSET);
        debug_assert!(l < FAST_PATH_BITS_PER_SLOT as i32 + FAST_PATH_LOCKNUMBER_OFFSET);
        let index = f % FP_LOCK_SLOTS_PER_GROUP as u32;
        (l - FAST_PATH_LOCKNUMBER_OFFSET) as u64 + (FAST_PATH_BITS_PER_SLOT * index) as u64
    }

    fn set_lockmode(&self, f: u32, l: LOCKMODE) {
        let group = (f / FP_LOCK_SLOTS_PER_GROUP as u32) as usize;
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { self.bits[group].set(self.bits[group].get() | (1u64 << Self::bit_position(f, l))); }
    }

    fn clear_lockmode(&self, f: u32, l: LOCKMODE) {
        let group = (f / FP_LOCK_SLOTS_PER_GROUP as u32) as usize;
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { self.bits[group].set(self.bits[group].get() & !(1u64 << Self::bit_position(f, l))); }
    }

    fn check_lockmode(&self, f: u32, l: LOCKMODE) -> bool {
        let group = (f / FP_LOCK_SLOTS_PER_GROUP as u32) as usize;
        // SAFETY: [FPL] serialized by fpInfoLock
        (unsafe { self.bits[group].get() }) & (1u64 << Self::bit_position(f, l)) != 0
    }

    pub(crate) fn group_bits(&self, group: u32) -> u64 {
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { self.bits[group as usize].get() }
    }

    pub(crate) fn relid(&self, f: u32) -> Oid {
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { self.relids[f as usize].get() }
    }

    fn set_relid(&self, f: u32, relid: Oid) {
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { self.relids[f as usize].set(relid); }
    }

    // Owner-only (writes require the owner holding fpInfoLock exclusive).
    fn bump_use_count(&self, group: u32) {
        let cell = &self.use_counts[group as usize];
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { cell.set(cell.get() + 1); }
    }

    fn set_use_count(&self, group: u32, v: i32) {
        // SAFETY: [FPL] serialized by fpInfoLock
        unsafe { self.use_counts[group as usize].set(v); }
    }
}

/// SAFETY contract: MyProc's fpInfoLock held exclusive.
pub(crate) unsafe fn FastPathGrantRelationLock(relid: Oid, lockmode: LOCKMODE) -> bool {
    let proc = lmgr_proc::GetPGProcByNumber(crate::my_procno());
    let view = fp_view(proc);
    let group = fast_path_rel_group(relid);

    let mut unused_slot = FastPathLockSlotsPerBackend();
    for i in 0..FP_LOCK_SLOTS_PER_GROUP as u32 {
        let f = group * FP_LOCK_SLOTS_PER_GROUP as u32 + i;
        if view.get_bits(f) == 0 {
            unused_slot = f;
        } else if view.relid(f) == relid {
            debug_assert!(!view.check_lockmode(f, lockmode));
            view.set_lockmode(f, lockmode);
            return true;
        }
    }

    if unused_slot < FastPathLockSlotsPerBackend() {
        view.set_relid(unused_slot, relid);
        view.set_lockmode(unused_slot, lockmode);
        view.bump_use_count(group);
        return true;
    }
    false
}

/// SAFETY contract: MyProc's fpInfoLock held exclusive.
pub(crate) unsafe fn FastPathUnGrantRelationLock(relid: Oid, lockmode: LOCKMODE) -> bool {
    let proc = lmgr_proc::GetPGProcByNumber(crate::my_procno());
    let view = fp_view(proc);
    let group = fast_path_rel_group(relid);

    let mut result = false;
    let mut use_count = 0;
    for i in 0..FP_LOCK_SLOTS_PER_GROUP as u32 {
        let f = group * FP_LOCK_SLOTS_PER_GROUP as u32 + i;
        if view.relid(f) == relid && view.check_lockmode(f, lockmode) {
            debug_assert!(!result);
            view.clear_lockmode(f, lockmode);
            result = true;
        }
        if view.get_bits(f) != 0 {
            use_count += 1;
        }
    }
    view.set_use_count(group, use_count);
    result
}

pub(crate) fn FastPathTransferRelationLocks(
    lockMethodTable: LockMethod,
    locktag: &LOCKTAG,
    hashcode: u32,
) -> PgResult<bool> {
    let partition_lock = LockHashPartitionLock(hashcode);
    let relid = locktag.locktag_field2;
    let group = fast_path_rel_group(relid);
    let hdr = lmgr_proc::ProcGlobal();
    let my_procno = crate::my_procno();

    for i in 0..hdr.allProcCount as usize {
        let proc = &hdr.allProcs[i];
        lwlock::LWLockAcquire(fp_info_lock(proc), lwlock::LW_EXCLUSIVE, my_procno)?;

        // SAFETY: fpInfoLock held.
        let view = unsafe { fp_view(proc) };
        if proc.databaseId.load(Relaxed) != locktag.locktag_field1 || view.group_bits(group) == 0 {
            lwlock::LWLockRelease(fp_info_lock(proc))?;
            continue;
        }

        for j in 0..FP_LOCK_SLOTS_PER_GROUP as u32 {
            let f = group * FP_LOCK_SLOTS_PER_GROUP as u32 + j;
            if view.relid(f) != relid || view.get_bits(f) == 0 {
                continue;
            }
            lwlock::LWLockAcquire(partition_lock, lwlock::LW_EXCLUSIVE, my_procno)?;
            for lockmode in FAST_PATH_LOCKNUMBER_OFFSET
                ..FAST_PATH_LOCKNUMBER_OFFSET + FAST_PATH_BITS_PER_SLOT as i32
            {
                if !view.check_lockmode(f, lockmode) {
                    continue;
                }
                // SAFETY: partition lock held exclusive.
                let proclock = unsafe {
                    SetupLockInTable(
                        lockMethodTable,
                        i as ProcNumber,
                        locktag,
                        hashcode,
                        lockmode,
                    )?
                };
                if proclock.is_null() {
                    lwlock::LWLockRelease(partition_lock)?;
                    lwlock::LWLockRelease(fp_info_lock(proc))?;
                    return Ok(false);
                }
                // SAFETY: partition lock held exclusive.
                unsafe {
                    crate::shared::grant_lock_raw((*proclock).tag.myLock, proclock, lockmode);
                }
                view.clear_lockmode(f, lockmode);
            }
            lwlock::LWLockRelease(partition_lock)?;
            break;
        }
        lwlock::LWLockRelease(fp_info_lock(proc))?;
    }
    Ok(true)
}

pub(crate) fn FastPathGetRelationLockEntry(
    tag: &types_storage::lock::LOCALLOCKTAG,
) -> PgResult<*mut types_storage::lock::PROCLOCK> {
    let lockMethodTable = crate::lock_method_by_id(1)?;
    let locktag = tag.lock;
    let relid = locktag.locktag_field2;
    let my_procno = crate::my_procno();
    let proc = lmgr_proc::GetPGProcByNumber(my_procno);
    let (hashcode, lockmode) = with_local(|state| {
        let ll = state.table.get(tag).expect("missing LOCALLOCK");
        (ll.hashcode, tag.mode)
    });
    let partition_lock = LockHashPartitionLock(hashcode);
    let group = fast_path_rel_group(relid);

    let mut proclock: *mut types_storage::lock::PROCLOCK = std::ptr::null_mut();
    lwlock::LWLockAcquire(fp_info_lock(proc), lwlock::LW_EXCLUSIVE, my_procno)?;
    // SAFETY: fpInfoLock held.
    let view = unsafe { fp_view(proc) };
    for i in 0..FP_LOCK_SLOTS_PER_GROUP as u32 {
        let f = group * FP_LOCK_SLOTS_PER_GROUP as u32 + i;
        if view.relid(f) != relid || view.get_bits(f) == 0 {
            continue;
        }
        if !view.check_lockmode(f, lockmode) {
            break;
        }
        lwlock::LWLockAcquire(partition_lock, lwlock::LW_EXCLUSIVE, my_procno)?;
        // SAFETY: partition lock held exclusive.
        proclock = unsafe {
            SetupLockInTable(lockMethodTable, my_procno, &locktag, hashcode, lockmode)?
        };
        if proclock.is_null() {
            lwlock::LWLockRelease(partition_lock)?;
            lwlock::LWLockRelease(fp_info_lock(proc))?;
            return Err(crate::acquire::out_of_shmem_error());
        }
        // SAFETY: partition lock held exclusive.
        unsafe {
            crate::shared::grant_lock_raw((*proclock).tag.myLock, proclock, lockmode);
        }
        view.clear_lockmode(f, lockmode);
        lwlock::LWLockRelease(partition_lock)?;
        break;
    }
    lwlock::LWLockRelease(fp_info_lock(proc))?;

    if proclock.is_null() {
        lwlock::LWLockAcquire(partition_lock, lwlock::LW_SHARED, my_procno)?;
        // SAFETY: partition lock held.
        unsafe {
            let lock = find_lock(&locktag, hashcode)?;
            assert!(!lock.is_null(), "failed to re-find shared lock object");
            proclock = find_proclock(lock, my_procno, hashcode)?;
            assert!(!proclock.is_null(), "failed to re-find shared proclock object");
        }
        lwlock::LWLockRelease(partition_lock)?;
    }
    Ok(proclock)
}

pub fn VirtualXactLockTableInsert(vxid: VirtualTransactionId) -> PgResult<()> {
    debug_assert!(vxid.localTransactionId != InvalidLocalTransactionId);
    let my_procno = crate::my_procno();
    let proc = lmgr_proc::GetPGProcByNumber(my_procno);

    lwlock::LWLockAcquire(fp_info_lock(proc), lwlock::LW_EXCLUSIVE, my_procno)?;
    debug_assert_eq!(proc.vxid.procNumber.load(Relaxed), vxid.procNumber);
    debug_assert_eq!(
        proc.fpLocalTransactionId.load(Relaxed),
        InvalidLocalTransactionId
    );
    debug_assert!(!proc.fpVXIDLock.load(Relaxed));
    proc.fpVXIDLock.store(true, Relaxed);
    proc.fpLocalTransactionId
        .store(vxid.localTransactionId, Relaxed);
    lwlock::LWLockRelease(fp_info_lock(proc))
}

pub fn VirtualXactLockTableCleanup() -> PgResult<()> {
    let my_procno = crate::my_procno();
    let proc = lmgr_proc::GetPGProcByNumber(my_procno);
    debug_assert_ne!(proc.vxid.procNumber.load(Relaxed), INVALID_PROC_NUMBER);

    lwlock::LWLockAcquire(fp_info_lock(proc), lwlock::LW_EXCLUSIVE, my_procno)?;
    let fastpath = proc.fpVXIDLock.load(Relaxed);
    let lxid: LocalTransactionId = proc.fpLocalTransactionId.load(Relaxed);
    proc.fpVXIDLock.store(false, Relaxed);
    proc.fpLocalTransactionId
        .store(InvalidLocalTransactionId, Relaxed);
    lwlock::LWLockRelease(fp_info_lock(proc))?;

    // Cleared without touching fpLocalTransactionId: someone moved the lock
    // to the main table.
    if !fastpath && lxid != InvalidLocalTransactionId {
        let locktag = LOCKTAG::virtualtransaction(my_procno as u32, lxid);
        crate::shared::LockRefindAndRelease(
            crate::lock_method_by_id(1)?,
            my_procno,
            &locktag,
            ExclusiveLock,
            false,
        )?;
    }
    Ok(())
}

fn max_prepared_xacts() -> i32 {
    let hdr = lmgr_proc::ProcGlobal();
    hdr.allProcs.len() as i32 - hdr.allProcCount as i32
}

fn XactLockForVirtualXact(
    vxid: VirtualTransactionId,
    xid: TransactionId,
    wait: bool,
) -> PgResult<bool> {
    if max_prepared_xacts() == 0 {
        return Ok(true);
    }

    let mut xid = xid;
    let mut more = false;
    loop {
        if more {
            xid = InvalidTransactionId;
            more = false;
        }

        if xid == InvalidTransactionId {
            if twophase_seams::two_phase_get_xid_by_virtual_xid::is_installed() {
                let (found, have_more) = twophase_seams::two_phase_get_xid_by_virtual_xid::call(
                    vxid.procNumber,
                    vxid.localTransactionId,
                )?;
                xid = found;
                more = have_more;
            }
        }
        if xid == InvalidTransactionId {
            debug_assert!(!more);
            return Ok(true);
        }

        let tag = LOCKTAG::transaction(xid);
        if crate::acquire::LockAcquire(&tag, ShareLock, false, !wait)?
            == ::types_storage::lock::LOCKACQUIRE_NOT_AVAIL
        {
            return Ok(false);
        }
        crate::acquire::LockRelease(&tag, ShareLock, false)?;

        if !more {
            return Ok(true);
        }
    }
}

pub fn VirtualXactLock(vxid: VirtualTransactionId, wait: bool) -> PgResult<bool> {
    debug_assert!(vxid.localTransactionId != InvalidLocalTransactionId);

    if vxid.procNumber == INVALID_PROC_NUMBER {
        return XactLockForVirtualXact(vxid, vxid.localTransactionId, wait);
    }

    let tag = LOCKTAG::virtualtransaction(vxid.procNumber as u32, vxid.localTransactionId);
    let my_procno = crate::my_procno();

    let hdr = lmgr_proc::ProcGlobal();
    if vxid.procNumber < 0 || vxid.procNumber >= hdr.allProcCount as ProcNumber {
        return XactLockForVirtualXact(vxid, InvalidTransactionId, wait);
    }
    let proc = lmgr_proc::GetPGProcByNumber(vxid.procNumber);
    if proc.pid.load(Relaxed) == 0 {
        return XactLockForVirtualXact(vxid, InvalidTransactionId, wait);
    }

    lwlock::LWLockAcquire(fp_info_lock(proc), lwlock::LW_EXCLUSIVE, my_procno)?;

    if proc.vxid.procNumber.load(Relaxed) != vxid.procNumber
        || proc.fpLocalTransactionId.load(Relaxed) != vxid.localTransactionId
    {
        lwlock::LWLockRelease(fp_info_lock(proc))?;
        return XactLockForVirtualXact(vxid, InvalidTransactionId, wait);
    }

    if !wait {
        lwlock::LWLockRelease(fp_info_lock(proc))?;
        return Ok(false);
    }

    if proc.fpVXIDLock.load(Relaxed) {
        let hashcode = LockTagHashCode(&tag);
        let partition_lock = LockHashPartitionLock(hashcode);
        lwlock::LWLockAcquire(partition_lock, lwlock::LW_EXCLUSIVE, my_procno)?;
        // SAFETY: partition lock held exclusive.
        let proclock = unsafe {
            SetupLockInTable(
                crate::lock_method_by_id(1)?,
                vxid.procNumber,
                &tag,
                hashcode,
                ExclusiveLock,
            )?
        };
        if proclock.is_null() {
            lwlock::LWLockRelease(partition_lock)?;
            lwlock::LWLockRelease(fp_info_lock(proc))?;
            return Err(crate::acquire::out_of_shmem_error());
        }
        // SAFETY: partition lock held exclusive.
        unsafe {
            crate::shared::grant_lock_raw((*proclock).tag.myLock, proclock, ExclusiveLock);
        }
        lwlock::LWLockRelease(partition_lock)?;
        proc.fpVXIDLock.store(false, Relaxed);
    }

    let xid = proc.xid.read();
    lwlock::LWLockRelease(fp_info_lock(proc))?;

    crate::acquire::LockAcquire(&tag, ShareLock, false, false)?;
    crate::acquire::LockRelease(&tag, ShareLock, false)?;
    XactLockForVirtualXact(vxid, xid, wait)
}
