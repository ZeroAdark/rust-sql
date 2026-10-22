#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Mutex, Once};

use ::datum::Datum;
use ::mcx::{Mcx, MemoryContext, PgVec};
use ::types_core::{
    BlockNumber, Buffer, InvalidBlockNumber, InvalidBuffer, Oid, BLCKSZ, INVALID_PROC_NUMBER,
    RELPERSISTENCE_PERMANENT,
};
use ::types_nbtree::{
    IndexUniqueCheck::{UNIQUE_CHECK_NO, UNIQUE_CHECK_YES},
    BTMetaPageData, BTPageOpaqueData, BTP_META, BTREE_MAGIC, BTREE_VERSION, P_NONE,
};
use ::types_nodes::node_tree::Node;
use ::types_nodes::primnodes::OpExpr;
use ::types_nodes::NodeList;
use ::types_rel::{
    FormData_pg_class, FormData_pg_index, LockInfoData, LockRelId, Relation, RelationData,
    RELKIND_INDEX, RELKIND_RELATION, REPLICA_IDENTITY_DEFAULT,
};
use ::types_snapshot::{SnapshotData, SnapshotType};
use ::types_storage::bufpage::SizeOfPageHeaderData;
use ::types_tuple::tupdesc::CompactAttribute;
use ::types_tuple::{ItemPointerData, NameData, TupleDescData};

pub const HEAP_OID: Oid = 61000;
pub const IDX_OID: Oid = 61001;
/// H-e (b): `t2`, a second heap with `t`'s one int4 column and no index (an inheritance child of `t` in the multi-target tests).
pub const HEAP2_OID: Oid = 61002;
pub const INT4OID: Oid = 23;
pub const INT4_BTREE_OPFAMILY: Oid = 1976;
pub const OP_INT4EQ: Oid = 96;
pub const F_INT4EQ: Oid = 65;
pub const OP_INT4GT: Oid = 521;
pub const F_INT4GT: Oid = 147;
const F_BTINT4CMP: Oid = 351;
pub const INDEX_VAR: i32 = -3;

#[repr(align(8))]
struct TestPage([u8; BLCKSZ]);

struct Fake {
    tables: HashMap<Oid, Vec<Buffer>>,
    pages: Vec<usize>,
    pins: Vec<i32>,
    wal: Vec<(u8, u8)>, // (rmid, info)
    lsn: u64,
}

static FAKE: Mutex<Option<Fake>> = Mutex::new(None);
static SERIAL: Mutex<()> = Mutex::new(());

pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_fake<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
    let mut g = FAKE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(|| Fake {
        tables: HashMap::new(),
        pages: Vec::new(),
        pins: Vec::new(),
        wal: Vec::with_capacity(1 << 16),
        lsn: 0x1000,
    }))
}

fn add_page(f: &mut Fake, relid: Oid, page: Box<TestPage>) -> Buffer {
    let addr = Box::leak(page).0.as_mut_ptr() as usize;
    f.pages.push(addr);
    f.pins.push(0);
    let buf = f.pages.len() as Buffer;
    f.tables.entry(relid).or_default().push(buf);
    buf
}

fn empty_bt_meta() -> Box<TestPage> {
    let mut p = Box::new(TestPage([0u8; BLCKSZ]));
    let special = BLCKSZ - core::mem::size_of::<BTPageOpaqueData>();
    p.0[12..14].copy_from_slice(&(SizeOfPageHeaderData as u16).to_ne_bytes());
    p.0[14..16].copy_from_slice(&(special as u16).to_ne_bytes());
    p.0[16..18].copy_from_slice(&(special as u16).to_ne_bytes());
    // pd_pagesize_version as PageInit writes it (layout version 4): the special size is the page size minus pd_special
    p.0[18..20].copy_from_slice(&((BLCKSZ as u16) | 4).to_ne_bytes());
    let metad = BTMetaPageData {
        btm_magic: BTREE_MAGIC,
        btm_version: BTREE_VERSION,
        btm_root: P_NONE,
        btm_level: 0,
        btm_fastroot: P_NONE,
        btm_fastlevel: 0,
        btm_last_cleanup_num_delpages: 0,
        btm_last_cleanup_num_heap_tuples: -1.0,
        btm_allequalimage: false, // dedup lane is loud; splits stay live
    };
    // SAFETY: owned page, in-bounds aligned writes.
    unsafe {
        p.0.as_mut_ptr()
            .add(SizeOfPageHeaderData)
            .cast::<BTMetaPageData>()
            .write(metad);
        p.0.as_mut_ptr()
            .add(special)
            .cast::<BTPageOpaqueData>()
            .write(BTPageOpaqueData {
                btpo_prev: P_NONE,
                btpo_next: P_NONE,
                btpo_level: 0,
                btpo_flags: BTP_META,
                btpo_cycleid: 0,
            });
    }
    p
}

/// Calls of the `relation_open` shim (the relcache lookups a bind performs).
pub static RELATION_OPENS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// D4213: while set, every heap row is invisible to every snapshot (a dead or uncommitted version's): an index walk's TIDs
/// then lead to no row.
pub static HIDE_ROWS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A relcache rebuild: the entries are replaced, the old `Rc`s dropped.
pub fn rebuild_relcache() {
    RELCACHE.with(|c| *c.borrow_mut() = None);
}

pub fn reset_fixture() {
    with_fake(|f| {
        f.tables.clear();
        f.pages.clear();
        f.pins.clear();
        f.wal.clear();
        f.wal.reserve(1 << 16);
        add_page(f, IDX_OID, empty_bt_meta());
    });
}

pub fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        genam_seams::build_index_value_description::set(|_, _, _| Ok(None));
        syscache_seams::pg_namespace_nspname::set(|_| Ok(None));
        bufmgr_seams::read_buffer::set(|rel, block| {
            with_fake(|f| {
                let buf = f.tables[&rel.rd_id][block as usize];
                f.pins[(buf - 1) as usize] += 1;
                Ok(buf)
            })
        });
        bufmgr_seams::read_buffer_strategy::set(|rel, block, _strategy| bufmgr_seams::read_buffer::call(rel, block));
        bufmgr_seams::get_access_strategy::set(|_| None);
        bufmgr_seams::free_access_strategy::set(|_| {});
        bufmgr_seams::read_buffer_without_relcache::set(|loc, _fork, block, _mode, _strategy, _permanent| {
            if FAIL_BLOCK.load(std::sync::atomic::Ordering::Relaxed) == block as u64 {
                return Err(Box::new(::types_error::PgError::error("could not read block (fixture)".to_string())));
            }
            with_fake(|f| {
                let buf = f.tables[&loc.relNumber][block as usize];
                f.pins[(buf - 1) as usize] += 1;
                Ok(buf)
            })
        });
        // the combined read: the fake pins the first block alone and records the run it was asked for
        bufmgr_seams::read_buffer_batched_without_relcache::set(|loc, block, hint, strategy, permanent| {
            BATCHED_HINT_MAX.fetch_max(hint as u64, std::sync::atomic::Ordering::Relaxed);
            let _ = BATCHED_FIRST.compare_exchange(u64::MAX, block as u64, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
            BATCHED_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            bufmgr_seams::read_buffer_without_relcache::call(loc, ::types_core::ForkNumber::MAIN_FORKNUM, block, ::types_storage::ReadBufferMode::Normal, strategy, permanent).map(|b| (b, true))
        });
        // C `ReadRecentBuffer`: the guessed buffer pinned when it still holds the block, else nothing.
        bufmgr_seams::read_recent_buffer::set(|loc, _fork, block, recent| {
            // the block whose read fails is not resident: no guess or hint pins it
            if FAIL_BLOCK.load(std::sync::atomic::Ordering::Relaxed) == block as u64 {
                return Ok(false);
            }
            with_fake(|f| {
                if f.tables[&loc.relNumber].get(block as usize) != Some(&recent) {
                    return Ok(false);
                }
                f.pins[(recent - 1) as usize] += 1;
                Ok(true)
            })
        });
        bufmgr_seams::release_buffer::set(|buf| {
            with_fake(|f| {
                let p = &mut f.pins[(buf - 1) as usize];
                assert!(*p > 0, "double release of buffer {buf}");
                *p -= 1;
            });
            Ok(())
        });
        bufmgr_seams::release_and_read_buffer::set(|buf, rel, blkno| {
            if buf != InvalidBuffer {
                let same = with_fake(|f| f.tables[&rel.rd_id].get(blkno as usize) == Some(&buf));
                if same {
                    return Ok(buf);
                }
                bufmgr_seams::release_buffer::call(buf)?;
            }
            bufmgr_seams::read_buffer::call(rel, blkno)
        });
        bufmgr_seams::buffer_get_block_number::set(|buf| {
            with_fake(|f| {
                for pages in f.tables.values() {
                    if let Some(i) = pages.iter().position(|b| *b == buf) {
                        return i as BlockNumber;
                    }
                }
                panic!("unknown buffer {buf}")
            })
        });
        bufmgr_seams::buffer_get_page::set(|buf| {
            let addr = with_fake(|f| {
                assert!(f.pins[(buf - 1) as usize] > 0, "page access without pin");
                f.pages[(buf - 1) as usize]
            });
            NonNull::new(addr as *mut u8).unwrap()
        });
        bufmgr_seams::incr_buffer_ref_count::set(|buf| {
            with_fake(|f| f.pins[(buf - 1) as usize] += 1);
        });
        bufmgr_seams::lock_buffer::set(|_buf, _mode| Ok(()));
        bufmgr_seams::conditional_lock_buffer::set(|_buf| Ok(true));
        bufmgr_seams::mark_buffer_dirty::set(|_buf| Ok(()));
        bufmgr_seams::mark_buffer_dirty_hint::set(|_buf, _std| Ok(()));
        bufmgr_seams::buffer_get_lsn_atomic::set(|_buf| 0x1234);
        bufmgr_seams::extend_buffered_rel_by::set(|rel, _fork, _strategy, _flags, n| {
            assert_eq!(n, 1);
            let buf = with_fake(|f| {
                let buf = add_page(f, rel.rd_id, Box::new(TestPage([0u8; BLCKSZ])));
                f.pins[(buf - 1) as usize] += 1;
                buf
            });
            Ok((buf, 1))
        });
        bufmgr_seams::relation_get_number_of_blocks_in_fork::set(|rel, _fork| {
            Ok(with_fake(|f| {
                f.tables.get(&rel.rd_id).map_or(0, |v| v.len()) as BlockNumber
            }))
        });
        bufmgr_seams::relation_smgr_locator::set(|rel| ::types_storage::RelFileLocatorBackend {
            locator: ::types_storage::RelFileLocator::new(1663, 5, rel.rd_id),
            backend: INVALID_PROC_NUMBER,
        });
        smgr_seams::smgr_cached_nblocks::set(|_loc, _fork| 0);
        smgr_seams::smgr_set_cached_nblocks::set(|_loc, _fork, _n| Ok(()));
        smgr_seams::smgr_exists::set(|_loc, _fork| Ok(false));

        xloginsert_seams::xlog_insert_record::set(|rmid, info, _flags, _main, _bufs| {
            with_fake(|f| {
                f.wal.push((rmid, info));
                f.lsn += 8;
                Ok(f.lsn)
            })
        });

        xact_seams::get_current_transaction_id::set(|| Ok(10));
        multixact_seams::multi_xact_id_set_oldest_member::set(|| Ok(()));
        multixact_seams::multi_xact_id_is_running::set(|_m, _lock_only| Ok(false));
        multixact_seams::get_multi_xact_id_members::set(|_m, _pgu, _lock_only, _consume| Ok(0));
        if !combocid_seams::heap_tuple_header_get_cmin::is_installed() {
            combocid_seams::heap_tuple_header_get_cmin::set(|_t| CURRENT_CID.with(Cell::get));
        }
        if !combocid_seams::heap_tuple_header_get_cmax::is_installed() {
            combocid_seams::heap_tuple_header_get_cmax::set(|_t| CURRENT_CID.with(Cell::get));
        }
        if !combocid_seams::heap_tuple_header_adjust_cmax::is_installed() {
            combocid_seams::heap_tuple_header_adjust_cmax::set(|_t, cid| Ok((cid, false)));
        }
        if !procarray_seams::transaction_id_is_in_progress::is_installed() {
            procarray_seams::transaction_id_is_in_progress::set(|_xid| Ok(false));
        }
        // a fetch of an invisible row asks whether its chain is surely dead (HIDE_ROWS)
        if !procarray_seams::global_vis_test_for::is_installed() {
            procarray_seams::global_vis_test_for::set(|_rel| ::types_core::GlobalVisStateHandle::new(0));
        }
        if !lock_seams::do_lock_modes_conflict::is_installed() {
            lock_seams::do_lock_modes_conflict::set(|_a, _b| false);
        }
        if !snapmgr_seams::transaction_xmin::is_installed() {
            snapmgr_seams::transaction_xmin::set(|| 10);
        }
        if !xact_seams::transaction_id_is_current_transaction_id::is_installed() {
            xact_seams::transaction_id_is_current_transaction_id::set(|xid| xid == 10);
        }
        if !xact_seams::get_top_transaction_id_if_any::is_installed() {
            xact_seams::get_top_transaction_id_if_any::set(|| 10);
        }
        if !xact_seams::is_in_parallel_mode::is_installed() {
            xact_seams::is_in_parallel_mode::set(|| false);
        }
        if !transam_seams::transaction_id_did_commit::is_installed() {
            transam_seams::transaction_id_did_commit::set(|_xid| Ok(false));
        }
        if !transam_seams::transaction_id_did_abort::is_installed() {
            transam_seams::transaction_id_did_abort::set(|_xid| Ok(false));
        }
        relcache_seams::relation_get_index_attr_bitmap::set(|_relid| {
            let mcx = relcache_mcx();
            let one = || {
                let mut v = PgVec::new_in(mcx);
                v.push(1i16);
                v
            };
            Ok(Rc::new(relcache_seams::IndexAttrBitmaps { hot_blocking: one(), summarized: PgVec::new_in(mcx), key: one(), pk: one(), identity: one() }))
        });
        xact_seams::get_current_command_id::set(|_used| Ok(CURRENT_CID.with(Cell::get)));
        xact_seams::set_xact_accessed_temp_namespace::set(|| ());
        lmgr_seams::lock_relation_oid::set(|_relid, _mode| Ok(()));
        lmgr_seams::unlock_relation_oid::set(|_relid, _mode| Ok(()));
        pgstat_seams::pgstat_init_relation::set(|_relid, _kind| false);
        miscinit_seams::is_bootstrap_processing_mode::set(|| false);
        miscinit_seams::get_user_id::set(|| 10);
        aclchk_seams::object_aclcheck::set(|_classid, _objid, _roleid, _mode| Ok(0));
        // a constraint violation's row description (execMain.c ExecBuildSlotValueDescription): no RLS, SELECT granted
        rls_seams::check_enable_rls::set(|_relid, _user, _no_error| Ok(rls_seams::CheckEnableRls::RlsNone));
        aclchk_seams::pg_class_aclcheck_ext::set(|_relid, _roleid, _mode| Ok((0, false)));

        predicate_seams::check_for_serializable_conflict_in::set(|_rel, _tid, _blk| Ok(()));
        predicate_seams::check_table_for_serializable_conflict_in::set(|_rel| Ok(()));
        predicate_seams::transfer_predicate_locks_to_heap_relation::set(|_rel| Ok(()));
        predicate_seams::check_for_serializable_conflict_out_needed::set(|_r, _s| Ok(false));
        predicate_seams::predicate_lock_relation::set(|_r, _s| Ok(()));
        predicate_seams::predicate_lock_page::set(|_r, _b, _s| Ok(()));
        predicate_seams::predicate_lock_tid::set(|_r, _t, _s, _x| Ok(()));
        predicate_seams::predicate_lock_page_split::set(|_r, _o, _n| Ok(()));
        pruneheap_seams::heap_page_prune_opt::set(|_r, _b| Ok(()));
        freespace_seams::get_page_with_free_space::set(|_rel, _need| Ok(InvalidBlockNumber));
        freespace_seams::record_and_get_page_with_free_space::set(|_rel, _old, _avail, _need| {
            Ok(InvalidBlockNumber)
        });
        catalog_seams::is_catalog_relation::set(|_rel| false);

        heapam_visibility_seams::heap_tuple_satisfies_visibility::set(|_h, _s, _b| Ok(!HIDE_ROWS.load(std::sync::atomic::Ordering::Relaxed)));
        heapam_visibility_seams::heap_tuple_satisfies_mvcc_page::set(|_h, _s, _b, _m| Ok(!HIDE_ROWS.load(std::sync::atomic::Ordering::Relaxed)));
        heapam_visibility_seams::heap_tuple_is_surely_dead::set(|_h, _v| Ok(false));
        heapam_visibility_seams::heap_tuple_header_is_only_locked::set(|_h| Ok(false));
        heapam_visibility_seams::heap_tuple_satisfies_update::set(|htup, _cid, _buf| {
            use ::tableam_vocab::TM_Result::*;
            use ::types_tuple::{HEAP_XMAX_INVALID, HEAP_XMAX_LOCK_ONLY};
            let h = htup.t_data();
            let xmax = h.xmax_raw();
            if xmax == 0 || h.t_infomask & (HEAP_XMAX_INVALID | HEAP_XMAX_LOCK_ONLY) != 0 {
                return Ok(TM_Ok);
            }
            if xmax == 10 {
                return Ok(TM_SelfModified);
            }
            Ok(if ::types_tuple::ItemPointerEquals(&h.t_ctid, &htup.t_self) { TM_Deleted } else { TM_Updated })
        });

        relation_seams::relation_open::set(|_mcx, relid, _lockmode| {
            RELATION_OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mcx = relcache_mcx();
            RELCACHE.with(|c| {
                let mut c = c.borrow_mut();
                let (heap, index) = c.get_or_insert_with(|| {
                    let unique = UNIQUE_IDX.with(Cell::get);
                    (Rc::new(heap_relation_data(mcx)), Rc::new(index_relation_data(mcx, unique)))
                });
                match relid {
                    HEAP_OID => Ok(Relation::open_rc(heap.clone(), None)),
                    IDX_OID => Ok(Relation::open_rc(index.clone(), noop_closer())),
                    HEAP2_OID => Ok(Relation::open(heap2_relation_data(mcx), None)),
                    other => panic!("unknown relation oid {other}"),
                }
            })
        });
        relcache_seams::relation_get_index_list::set(|mcx, relid| {
            let mut v = PgVec::new_in(mcx);
            if relid == HEAP2_OID {
                return Ok(v);
            }
            assert_eq!(relid, HEAP_OID);
            v.push(IDX_OID);
            Ok(v)
        });

        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok(match typid {
                INT4OID => Some(::types_tuple::PgTypeShape {
                    typlen: 4,
                    typbyval: true,
                    typalign: ::types_tuple::TYPALIGN_INT,
                    typstorage: ::types_tuple::TYPSTORAGE_PLAIN,
                    typcollation: 0,
                }),
                16 => Some(::types_tuple::PgTypeShape {
                    typlen: 1,
                    typbyval: true,
                    typalign: b'c' as i8,
                    typstorage: ::types_tuple::TYPSTORAGE_PLAIN,
                    typcollation: 0,
                }),
                _ => None,
            })
        });
        syscache_seams::lookup_pg_amop_by_operator::set(|opno, purpose, opfamily| {
            assert_eq!(purpose, b's');
            assert_eq!(opfamily, INT4_BTREE_OPFAMILY);
            let strategy = match opno {
                OP_INT4EQ => 3,
                OP_INT4GT => 5,
                _ => return Ok(None),
            };
            Ok(Some(syscache_seams::PgAmopShape {
                amopstrategy: strategy,
                amopsortfamily: 0,
                amoplefttype: INT4OID,
                amoprighttype: INT4OID,
            }))
        });
        syscache_seams::lookup_pg_amop_by_strategy::set(|opfamily, lefttype, righttype, strategy| {
            assert_eq!(
                (opfamily, lefttype, righttype),
                (INT4_BTREE_OPFAMILY, INT4OID, INT4OID)
            );
            Ok(match strategy {
                3 => OP_INT4EQ,
                5 => OP_INT4GT,
                _ => 0,
            })
        });
        syscache_seams::lookup_pg_operator_shape::set(|opno| {
            Ok((opno == OP_INT4EQ).then_some(syscache_seams::PgOperatorShape {
                oprnamespace: 11,
                oprleft: INT4OID,
                oprright: INT4OID,
                oprresult: 16,
                oprcom: OP_INT4EQ,
                oprnegate: 0,
                oprcode: F_INT4EQ,
                oprrest: 0,
                oprjoin: 0,
                oprcanmerge: true,
                oprcanhash: true,
            }))
        });
        syscache_seams::lookup_pg_amproc::set(|opfamily, lefttype, righttype, procnum| {
            assert_eq!(
                (opfamily, lefttype, righttype, procnum),
                (INT4_BTREE_OPFAMILY, INT4OID, INT4OID, 1)
            );
            Ok(F_BTINT4CMP)
        });
    });
    test_boot::boot_wal("execindexing");
    static BTREE_SHMEM: Once = Once::new();
    BTREE_SHMEM.call_once(|| nbtree::BTreeShmemInit().unwrap());
}

pub fn set_command_id(cid: u32) {
    CURRENT_CID.with(|c| c.set(cid));
}

thread_local! {
    pub static CURRENT_CID: Cell<u32> = const { Cell::new(0) };
    static UNIQUE_IDX: Cell<bool> = const { Cell::new(false) };
    static UNREADY_IDX: Cell<bool> = const { Cell::new(false) };
    static DEFERRABLE_IDX: Cell<bool> = const { Cell::new(false) };
    static NOT_NULL: Cell<bool> = const { Cell::new(false) };
    static CHECK_NN: Cell<bool> = const { Cell::new(false) };
    static GENERATED: Cell<bool> = const { Cell::new(false) };
    static HAS_TRIGGERS: Cell<bool> = const { Cell::new(false) };
    /// m4_park's heap trigger descriptor (the relcache seam's answer for HEAP_OID; D375: test TLS lives in this block)
    pub static TRIG_DESC: std::cell::RefCell<Option<Rc<types_trigger::TriggerDesc<'static>>>> = const { std::cell::RefCell::new(None) };
    static RELCACHE: std::cell::RefCell<Option<(Rc<RelationData<'static>>, Rc<RelationData<'static>>)>> = const { std::cell::RefCell::new(None) };
    /// `RELCACHE`'s memory, this thread's own (D860: `relcache_mcx`)
    static RELCACHE_CTX: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("relcache-test")));
}

/// The relcache fixture's memory: one context per thread (D860). A thread's `RELCACHE` entries drop in its thread-local
/// destructor, after its test returned and released `serial()`, so a context shared by every test thread took those frees
/// while the next test's thread allocated in it (a memory context is not thread-safe: d263_scratch aborted in `hashbrown`
/// under parallel load). Leaked per thread, so the entries can drop in any thread-local order.
fn relcache_mcx() -> Mcx<'static> {
    RELCACHE_CTX.with(|c| c.mcx())
}

fn noop_closer() -> Option<::types_rel::RelationCloser> {
    fn close(_oid: Oid, _mode: ::types_rel::LOCKMODE) -> ::types_error::PgResult<()> {
        Ok(())
    }
    Some(close)
}

/// The heap's descriptor: `int4_tupdesc`, its column NOT NULL under `set_not_null` (H-i, D585).
/// `CHECK (a IS NOT NULL)` as the catalog stores it (ccbin: a NullTest, no function, so the fixture needs no pg_proc), under
/// `set_check_not_null` (D608: a state built once per bind).
const CHECK_A_NN: &str = "{NULLTEST :arg {VAR :varno 1 :varattno 1 :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 1 :location -1} :nulltesttype 1 :argisrow false :location -1}";

/// `GENERATED ALWAYS AS (7) STORED` as the catalog stores it (adbin: a Const, no function), under `set_generated`.
const GENERATED_7: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 7 0 0 0 0 0 0 0 ]}";

/// The heap's descriptor: `int4_tupdesc`, its column NOT NULL under `set_not_null` (H-i, D585) and checked `a IS NOT NULL` under
/// `set_check_not_null` (D608).
fn heap_tupdesc(mcx: Mcx<'_>) -> TupleDescData<'_> {
    let mut d = int4_tupdesc(mcx);
    let (nn, ck, gen) = (NOT_NULL.with(|n| n.get()), CHECK_NN.with(|c| c.get()), GENERATED.with(|g| g.get()));
    if nn {
        d.attrs[0].attnotnull = true;
    }
    if gen {
        d.attrs[0].attgenerated = ::types_core::catalog::ATTRIBUTE_GENERATED_STORED as i8;
    }
    d.compact_attrs[0] = CompactAttribute::populate_from(&d.attrs[0]);
    if nn || ck || gen {
        let mut check = PgVec::new_in(mcx);
        if ck {
            check.push(::types_tuple::tupdesc::ConstrCheck { ccname: Some(::mcx::PgString::from_str_in("t_a_check", mcx).unwrap()), ccbin: Some(::mcx::PgString::from_str_in(CHECK_A_NN, mcx).unwrap()), ccenforced: true, ccvalid: true, ccnoinherit: false });
        }
        let mut defval = PgVec::new_in(mcx);
        if gen {
            defval.push(::types_tuple::tupdesc::AttrDefault { adnum: 1, adbin: Some(::mcx::PgString::from_str_in(GENERATED_7, mcx).unwrap()) });
        }
        let constr = ::types_tuple::tupdesc::TupleConstr { defval, check, missing: PgVec::new_in(mcx), missing_store: PgVec::new_in(mcx), num_defval: gen as u16, num_check: ck as u16, relchecks: ck as i16, has_not_null: nn, has_generated_stored: gen, has_generated_virtual: false };
        d.constr = Some(::mcx::box_new_in(mcx, constr));
    }
    d
}

fn int4_tupdesc(mcx: Mcx<'_>) -> TupleDescData<'_> {
    let att = ::types_tuple::FormData_pg_attribute {
        attnum: 1,
        atttypid: INT4OID,
        atttypmod: -1,
        attlen: 4,
        attbyval: true,
        attalign: ::types_tuple::TYPALIGN_INT,
        attstorage: ::types_tuple::TYPSTORAGE_PLAIN,
        ..Default::default()
    };
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    compact.push(CompactAttribute::populate_from(&att));
    attrs.push(att);
    TupleDescData {
        natts: 1,
        tdtypeid: 0,
        tdtypmod: -1,
        tdrefcount: -1,
        constr: None,
        compact_attrs: compact,
        attrs,
    }
}

fn pg_class(relname: &str, oid: Oid, relam: Oid, relkind: u8, hasindex: bool) -> FormData_pg_class {
    let mut name = NameData::default();
    name.namestrcpy(relname);
    FormData_pg_class {
        relname: name,
        relnamespace: 2200,
        reltype: 0,
        relowner: 10,
        relam,
        relfilenode: oid,
        reltablespace: 0,
        relpages: 0,
        reltuples: -1.0,
        relallvisible: 0,
        reltoastrelid: 0,
        relhasindex: hasindex,
        relisshared: false,
        relpersistence: RELPERSISTENCE_PERMANENT,
        relkind,
        relhassubclass: false,
        relrowsecurity: false,
        relispopulated: true,
        relreplident: REPLICA_IDENTITY_DEFAULT,
        relispartition: false,
        relfrozenxid: 3,
        relminmxid: 1,
    }
}

pub fn heap_relation_data(mcx: Mcx<'_>) -> RelationData<'_> {
    RelationData {
        rd_locator: Cell::new(::types_storage::RelFileLocator::new(1663, 5, HEAP_OID)),
        rd_smgr: Default::default(),
        rd_id: HEAP_OID,
        rd_backend: INVALID_PROC_NUMBER,
        rd_islocaltemp: false,
        rd_isvalid: Cell::new(true),
        rd_createSubid: Cell::new(0),
        rd_newRelfilelocatorSubid: Cell::new(0),
        rd_firstRelfilelocatorSubid: Cell::new(0),
        rd_droppedSubid: Cell::new(0),
        rd_lockInfo: LockInfoData {
            lockRelId: LockRelId { relId: HEAP_OID, dbId: 5 },
        },
        rd_rel: pg_class("t", HEAP_OID, ::tableam::HEAP_TABLE_AM_OID, RELKIND_RELATION, true),
        rd_att: Rc::new(heap_tupdesc(mcx)),
        rd_index: None,
        rd_opcintype: PgVec::new_in(mcx),
        rd_opfamily: PgVec::new_in(mcx),
        rd_indoption: PgVec::new_in(mcx),
        rd_indcollation: PgVec::new_in(mcx),
        rd_options: None,
        pgstat_enabled: Cell::new(false),
        pgstat_link: core::cell::Cell::new((0, core::ptr::null_mut())),
        rd_amcache: Default::default(),
        rd_amcache_hash: Default::default(), rd_amcache_gin: Default::default(), rd_amcache_spgist: Default::default(), rd_amcache_bloom: Default::default(),
        rd_support: PgVec::new_in(mcx),
        rd_supportinfo: Default::default(),
        rd_opcoptions: Default::default(),
        rd_indexlist: Default::default(),
            rd_trigdesc: Default::default(),
            rd_hastriggers: HAS_TRIGGERS.with(|t| t.get()), rd_hasrules: false,
    }
}

/// H-e (b): `t2`'s relation (the same layout as `t`, no index).
pub fn heap2_relation_data(mcx: Mcx<'_>) -> RelationData<'_> {
    let mut d = heap_relation_data(mcx);
    d.rd_locator = Cell::new(::types_storage::RelFileLocator::new(1663, 5, HEAP2_OID));
    d.rd_id = HEAP2_OID;
    d.rd_lockInfo = LockInfoData { lockRelId: LockRelId { relId: HEAP2_OID, dbId: 5 } };
    d.rd_rel = pg_class("t2", HEAP2_OID, ::tableam::HEAP_TABLE_AM_OID, RELKIND_RELATION, false);
    d
}

/// H-e (b): rows into `t2` (no index to maintain).
pub fn load_heap2_rows(mcx: Mcx<'_>, rows: &[i32]) -> Vec<ItemPointerData> {
    let heap = Relation::open(heap2_relation_data(mcx), None);
    rows.iter()
        .map(|&v| {
            let mut tuple = ::heaptuple::heap_form_tuple(mcx, &heap.rd_att, &[Datum::from_i32(v)], &[false]).unwrap();
            ::heapam::heap_insert(&heap, tuple.as_tuple_mut(), 0, 0, None).unwrap();
            tuple.as_tuple().t_self
        })
        .collect()
}

pub fn index_relation_data(mcx: Mcx<'_>, unique: bool) -> RelationData<'_> {
    let one = |v: Oid| {
        let mut vec = PgVec::new_in(mcx);
        vec.push(v);
        vec
    };
    let mut indkey = PgVec::new_in(mcx);
    indkey.push(1i16);
    let mut indoption = PgVec::new_in(mcx);
    indoption.push(0i16);
    RelationData {
        rd_locator: Default::default(),
        rd_smgr: Default::default(),
        rd_id: IDX_OID,
        rd_backend: INVALID_PROC_NUMBER,
        rd_islocaltemp: false,
        rd_isvalid: Cell::new(true),
        rd_createSubid: Cell::new(0),
        rd_newRelfilelocatorSubid: Cell::new(0),
        rd_firstRelfilelocatorSubid: Cell::new(0),
        rd_droppedSubid: Cell::new(0),
        rd_lockInfo: LockInfoData {
            lockRelId: LockRelId { relId: IDX_OID, dbId: 5 },
        },
        rd_rel: pg_class("t_idx", IDX_OID, ::types_core::BTREE_AM_OID, RELKIND_INDEX, false),
        rd_att: Rc::new(int4_tupdesc(mcx)),
        rd_index: Some(FormData_pg_index {
            indexrelid: IDX_OID,
            indrelid: HEAP_OID,
            indnatts: 1,
            indnkeyatts: 1,
            indisunique: unique,
            indnullsnotdistinct: false,
            indisprimary: false,
            indisexclusion: false,
            indimmediate: !DEFERRABLE_IDX.with(Cell::get),
            indisvalid: true,
            indisready: !UNREADY_IDX.with(Cell::get),
            indcheckxmin: false,
            indxmin: 0,
            indkey,
            has_indpred: false,
        indexprs_src: None,
        indpred_src: None,
        }),
        rd_opcintype: one(INT4OID),
        rd_opfamily: one(INT4_BTREE_OPFAMILY),
        rd_indoption: indoption,
        rd_indcollation: one(0),
        rd_options: None,
        pgstat_enabled: Cell::new(false),
        pgstat_link: core::cell::Cell::new((0, core::ptr::null_mut())),
        rd_amcache: Default::default(),
        rd_amcache_hash: Default::default(), rd_amcache_gin: Default::default(), rd_amcache_spgist: Default::default(), rd_amcache_bloom: Default::default(),
        rd_support: PgVec::new_in(mcx),
        rd_supportinfo: Default::default(),
        rd_opcoptions: Default::default(),
        rd_indexlist: Default::default(),
            rd_trigdesc: Default::default(),
            rd_hastriggers: false, rd_hasrules: false,
    }
}

pub fn insert_row<'mcx>(mcx: Mcx<'mcx>, heap: &Relation<'mcx>, index: &Relation<'mcx>, val: i32) -> ::types_error::PgResult<ItemPointerData> {
    let mut tuple = ::heaptuple::heap_form_tuple(mcx, &heap.rd_att, &[Datum::from_i32(val)], &[false])?;
    ::heapam::heap_insert(heap, tuple.as_tuple_mut(), 0, 0, None)?;
    let tid = tuple.as_tuple().t_self;
    let mut cache = None;
    ::indexam::index_insert(mcx, index, &[Datum::from_i32(val)], &[false], &tid, heap, if index.rd_index.as_ref().unwrap().indisunique { UNIQUE_CHECK_YES } else { UNIQUE_CHECK_NO }, false, &mut cache)?;
    Ok(tid)
}

pub fn load_rows(mcx: Mcx<'_>, rows: &[i32]) -> Vec<ItemPointerData> {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, UNIQUE_IDX.with(Cell::get)), None);
    rows.iter().map(|&v| insert_row(mcx, &heap, &index, v).unwrap()).collect()
}

pub fn set_unique_index(unique: bool) {
    UNIQUE_IDX.with(|u| u.set(unique));
    rebuild_relcache();
}

/// D624: the index not ready for inserts (`indisready` false: CREATE INDEX CONCURRENTLY's first phase) from the next
/// relcache build on (the relcache rebuilt now).
pub fn set_index_unready(on: bool) {
    UNREADY_IDX.with(|d| d.set(on));
    rebuild_relcache();
}

/// H-a2: the index DEFERRABLE (`indimmediate` false) from the next relcache build on (the relcache rebuilt now).
pub fn set_deferrable_index(on: bool) {
    DEFERRABLE_IDX.with(|d| d.set(on));
    rebuild_relcache();
}

/// The heap's column NOT NULL from the next relcache build on (the relcache rebuilt now).
/// The heap's `CHECK (a IS NOT NULL)` from the next relcache build on (the relcache rebuilt now).
pub fn set_check_not_null(on: bool) {
    CHECK_NN.with(|c| c.set(on));
    rebuild_relcache();
}

/// The heap's column a STORED generated column (always 7) from the next relcache build on (the relcache rebuilt now).
pub fn set_generated(on: bool) {
    GENERATED.with(|g| g.set(on));
    rebuild_relcache();
}

/// The heap's relhastriggers from the next relcache build on (the relcache rebuilt now): its descriptor is the test's
/// `relation_get_trigger_desc` seam.
pub fn set_has_triggers(on: bool) {
    HAS_TRIGGERS.with(|t| t.set(on));
    rebuild_relcache();
}

pub fn set_not_null(on: bool) {
    NOT_NULL.with(|n| n.set(on));
    rebuild_relcache();
}

pub fn static_mvcc_snapshot() -> Rc<SnapshotData<'static>> {
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("snap-test")));
    Rc::new(SnapshotData::sentinel(ctx.mcx(), SnapshotType::SNAPSHOT_MVCC))
}





/// The locator read path fails on this block (u64::MAX = never).
pub static FAIL_BLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// The combined reads asked of the fake and the longest run (the blocks-ahead hint) among them.
pub static BATCHED_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BATCHED_HINT_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BATCHED_FIRST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

pub fn nblocks(rel: Oid) -> u64 {
    with_fake(|f| f.tables.get(&rel).map_or(0, |v| v.len()) as u64)
}

/// The relation's pages moved to other buffers (the block → buffer order reversed, each block's contents kept): a pool not in block order.
pub fn reverse_pool(rel: Oid) {
    with_fake(|f| {
        assert!(f.pins.iter().all(|p| *p == 0));
        let bufs = f.tables.get_mut(&rel).unwrap();
        let addrs: Vec<usize> = bufs.iter().map(|b| f.pages[(*b - 1) as usize]).collect();
        bufs.reverse();
        for (b, a) in bufs.iter().zip(addrs) {
            f.pages[(*b - 1) as usize] = a;
        }
    });
}

pub fn pins_held() -> i32 {
    with_fake(|f| f.pins.iter().sum())
}

pub fn quiesced() {
    with_fake(|f| {
        assert!(f.pins.iter().all(|p| *p == 0), "leaked pins: {:?}", f.pins);
    });
}

pub fn indexqual_param<'mcx>(mcx: Mcx<'mcx>, varno: i32, opno: Oid, opfuncid: Oid) -> NodeList<'mcx> {
    let var = Node::mk_var(mcx, varno, 1, INT4OID, -1, 0, 0).unwrap();
    let p = Node::mk(mcx, ::types_nodes::primnodes::Param { paramkind: ::types_nodes::primnodes::ParamKind::PARAM_EXTERN, paramid: 1, paramtype: INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
    let args = NodeList::make2(mcx, var, p).unwrap();
    let op = Node::mk(mcx, OpExpr { opno, opfuncid, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 }).unwrap();
    NodeList::make1(mcx, op).unwrap()
}
