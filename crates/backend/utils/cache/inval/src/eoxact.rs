use mcx::{Mcx, PgVec};
use types_core::primitive::OidIsValid;
use types_core::Oid;
use types_error::PgResult;
use types_storage::SharedInvalidationMessage;

use crate::local::LocalExecuteInvalidationMessage;
use crate::msgs::{append_invalidation_messages, subgroup_slice, InvalidationMsgsGroup};
use crate::{with_state, InvalState, CAT_CACHE_MSGS, REL_CACHE_MSGS};

// rmgrlist.h / xact.h.
pub(crate) const RM_XACT_ID: u8 = 1;
pub(crate) const XLOG_XACT_INVALIDATIONS: u8 = 0x60;

pub(crate) const REPLAY_CHUNK: usize = 32;

// Borrow released around each execute (it re-enters inval): one short borrow
// + one memcpy per 32-message chunk into stack scratch, dispatch borrow-free.
// The walk bound is fixed at entry (C's ProcessMessageSubGroup `_endmsg`) and
// executes never mutate registered messages (they only append), so the chunk
// image cannot go stale: mid-walk registrations land behind the fixed bound
// on both sides.
#[inline]
pub(crate) fn process_group_with(
    group: &InvalidationMsgsGroup,
    func: impl FnMut(&SharedInvalidationMessage) -> PgResult<()>,
) -> PgResult<()> {
    // Inline guard: the empty group (every no-DDL CommandEnd/abort) must not
    // pay the outlined walk's frame + chunk-scratch setup.
    if group.num_in_group() == 0 {
        return Ok(());
    }
    process_group_slow(group, func)
}

fn process_group_slow(
    group: &InvalidationMsgsGroup,
    mut func: impl FnMut(&SharedInvalidationMessage) -> PgResult<()>,
) -> PgResult<()> {
    use std::mem::MaybeUninit;
    for subgroup in [CAT_CACHE_MSGS, REL_CACHE_MSGS] {
        let end = group.num_in_sub_group(subgroup);
        let mut off = 0usize;
        while off < end {
            // MaybeUninit + one memcpy: a dummy-initialized array is a
            // per-element store loop (the enum stride defeats memset).
            let mut chunk: [MaybeUninit<SharedInvalidationMessage>; REPLAY_CHUNK] =
                [const { MaybeUninit::uninit() }; REPLAY_CHUNK];
            let n = (end - off).min(REPLAY_CHUNK);
            with_state(|state| {
                let msgs = &subgroup_slice(&state.msg_arrays, group, subgroup)[off..off + n];
                // SAFETY: n <= REPLAY_CHUNK; src and dst are distinct objects.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        msgs.as_ptr(),
                        chunk.as_mut_ptr().cast::<SharedInvalidationMessage>(),
                        n,
                    );
                }
            });
            // SAFETY: chunk[..n] fully written under the borrow above.
            let msgs = unsafe {
                std::slice::from_raw_parts(chunk.as_ptr().cast::<SharedInvalidationMessage>(), n)
            };
            for msg in msgs {
                func(msg)?;
            }
            off += n;
        }
    }
    Ok(())
}

pub(crate) fn process_group_locally(
    select: impl Fn(&InvalState<'_>) -> Option<InvalidationMsgsGroup>,
) -> PgResult<()> {
    let Some(group) = with_state(|state| select(state)) else {
        return Ok(());
    };
    process_group_with(&group, LocalExecuteInvalidationMessage)
}

/// D3866: cache `id`'s bit in `InvalState::stamp_touched` (0 outside 0..128: such an id names no catcache).
#[inline]
pub(crate) fn stamp_bit(id: i32) -> u128 {
    u32::try_from(id).ok().and_then(|i| 1u128.checked_shl(i)).unwrap_or(0)
}

pub(crate) fn l2_bump_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PGRUST_L2_DEBUG").is_ok_and(|v| v.trim() == "1"))
}

// D3.2: bump the L2 generation of every domain a message batch touches,
// STRICTLY BEFORE the batch enters the shared sinval queue. Any backend that
// later processes one of these messages re-syncs its view from the global
// counters, so the bump must already be visible then — otherwise an L1 drop
// could be undone by an L2 hit at the stale generation. The bump also adopts
// the new generation as the sender's own view (its caches reflect the new
// state; atomics only, safe under the live state borrow).
//
// D3866: a row-stamped cache's program stamp (l2cache ROW_STAMPED_CAT_IDS) moves
// after its generation when `touched` (this transaction's own batch: the
// caches a committed row of which it changed, `InvalState::stamp_touched`)
// names it, or for every such message when `touched` is None (a batch whose
// origin this backend did not register: a replayed commit's).
//
// C3 S5 (D3890, D3889): a relcache message whose relation this transaction registered only under a scope (`Scopes`:
// `InvalState::rel_scopes`, the triggers' or the storage's) moves the L2 relcache, and the storage generation for the
// storage's, not the relation's program generation (l2cache `invalidate_rel_part`); a catcache message of a row registered only under the
// storage's scope (`cat_scopes`: a pg_class row) leaves its row's (`invalidate_cat_storage`). With no `touched` (a batch
// of unknown origin) every message is whole.
pub(crate) struct Scopes<'a> {
    pub(crate) touched: u128,
    pub(crate) rels: &'a [(Oid, u8)],
    pub(crate) cats: &'a [(i32, u32, u8)],
}

pub(crate) fn l2_bump_for_messages(msgs: &[SharedInvalidationMessage], scoped: Option<Scopes<'_>>) {
    let (touched, scopes, cat_scopes) = match scoped {
        Some(s) => (Some(s.touched), s.rels, s.cats),
        None => (None, &[][..], &[][..]),
    };
    for msg in msgs {
        match *msg {
            SharedInvalidationMessage::Catcache(m) => {
                let id = m.id as i32;
                if crate::cat_scope(cat_scopes, id, m.hashValue) == crate::REL_SCOPE_STORAGE {
                    l2cache::invalidate_cat_storage(id, m.dbId, m.hashValue);
                    continue;
                }
                l2cache::invalidate_key(
                    l2cache::Domain::Cat(id),
                    l2cache::cat_key(id, m.dbId, m.hashValue),
                );
                if l2cache::row_stamped(id) && touched.is_none_or(|t| stamp_bit(id) & t != 0) {
                    l2cache::bump_stamp(id, m.hashValue, m.dbId);
                }
            }
            SharedInvalidationMessage::Catalog(_) => l2cache::bump_all_cat(),
            SharedInvalidationMessage::Relcache(m) => {
                let s = crate::rel_scope(scopes, m.relId);
                if m.relId == types_core::InvalidOid {
                    l2cache::bump_all_rel();
                } else if s & crate::REL_SCOPE_ALL == 0 {
                    l2cache::invalidate_rel_part(m.relId, m.dbId, s & crate::REL_SCOPE_STORAGE != 0);
                } else {
                    l2cache::invalidate_key(
                        l2cache::Domain::Rel(m.relId),
                        l2cache::rel_key(m.relId, m.dbId),
                    );
                    if l2_bump_debug() {
                        eprintln!(
                            "L2DBG bump rel={} newgen={} thr={:?}",
                            m.relId,
                            l2cache::current_gen(l2cache::Domain::Rel(m.relId)),
                            std::thread::current().id()
                        );
                    }
                }
            }
            // Snapshot/Smgr/Relmap/RelSync invalidate nothing the L2 stores.
            _ => {}
        }
    }
}

// sinval send never re-enters inval: dense subgroup slices go straight to the
// seam under the live borrow — alloc-free, like C.
fn send_group(state: &InvalState<'_>, group: &InvalidationMsgsGroup) -> PgResult<()> {
    for subgroup in [CAT_CACHE_MSGS, REL_CACHE_MSGS] {
        let msgs = subgroup_slice(&state.msg_arrays, group, subgroup);
        if !msgs.is_empty() {
            l2_bump_for_messages(msgs, Some(Scopes { touched: state.stamp_touched, rels: &state.rel_scopes[..], cats: &state.cat_scopes[..] }));
            sinval_seams::send_shared_invalid_messages::call(msgs)?;
        }
    }
    Ok(())
}

pub fn CommandEndInvalidationMessages() -> PgResult<()> {
    let Some(group) =
        with_state(|state| Some(state.trans_stack.last()?.ii.current_cmd_invalid_msgs))
    else {
        return Ok(());
    };
    process_group_with(&group, LocalExecuteInvalidationMessage)?;
    // C3 S4b: what this thread read under its own messages before them no longer holds (gravity_deps)
    if group.num_in_group() > 0 {
        l2cache::note_own_invals();
    }

    if transam_xlog_seams::xlog_logical_info_active::call() {
        LogLogicalInvalidations()?;
    }

    with_state(|state| {
        let info = state.trans_stack.last_mut().expect("checked non-empty");
        append_invalidation_messages(
            &mut info.prior_cmd_invalid_msgs,
            &mut info.ii.current_cmd_invalid_msgs,
        );
    });

    Ok(())
}

/// D3888 (xact.c's pre-commit / pre-prepare point: catalog access allowed, before the commit or PREPARE record): the
/// relations built from the operator families this transaction's support catalog rows named (`support_families`) get
/// relcache messages of their own: an index's support material and a partition key's are their relcache entries', which
/// no other message covers. A build without the scan's seam registers a whole relcache message instead.
pub fn PreCommit_Inval() -> PgResult<()> {
    // every commit comes here and nearly none changed a support catalog: no context, no copy (the counter bar's q0)
    if with_state(|state| state.support_families.is_empty()) {
        return Ok(());
    }
    let cx = mcx::MemoryContext::new("PreCommit_Inval");
    let families = with_state(|state| mcx::slice_in(cx.mcx(), &state.support_families))?;
    if !relcache_seams::support_family_relids::is_installed() {
        crate::invalidate::CacheInvalidateRelcacheAll()?;
    } else {
        for &relid in relcache_seams::support_family_relids::call(cx.mcx(), &families)?.iter() {
            crate::invalidate::cache_invalidate_relcache_by_relid_if_exists(relid)?;
        }
    }
    with_state(|state| state.support_families.clear());
    Ok(())
}

pub fn AtEOXact_Inval(isCommit: bool) -> PgResult<()> {
    enum Eox {
        Empty,
        Commit(bool),
        Abort(InvalidationMsgsGroup),
    }

    let action = with_state(|state| {
        state.inplace_info = None;
        match state.trans_stack.first() {
            None => {
                // D3866: an in-place update's bits went with its own send (AtInplace_Inval)
                state.stamp_touched = 0;
                state.pending = crate::PendingSummary::default();
                state.rel_scopes.clear();
                state.cat_scopes.clear();
                state.cat_unscoped = 0;
                state.support_families.clear();
                Eox::Empty
            }
            Some(info) => {
                /* Must be at top of stack */
                debug_assert!(state.trans_stack.len() == 1 && info.my_level == 1);
                if isCommit {
                    Eox::Commit(info.ii.relcache_init_file_inval)
                } else {
                    Eox::Abort(info.prior_cmd_invalid_msgs)
                }
            }
        }
    });

    match action {
        Eox::Empty => return Ok(()),
        Eox::Commit(relcache_init_file_inval) => {
            if relcache_init_file_inval {
                relcache_seams::relation_cache_init_file_pre_invalidate::call()?;
            }

            with_state(|state| {
                let info = &mut state.trans_stack[0];
                append_invalidation_messages(
                    &mut info.prior_cmd_invalid_msgs,
                    &mut info.ii.current_cmd_invalid_msgs,
                );
                let group = info.prior_cmd_invalid_msgs;
                send_group(state, &group)
            })?;

            if relcache_init_file_inval {
                relcache_seams::relation_cache_init_file_post_invalidate::call()?;
            }
        }
        Eox::Abort(group) => {
            process_group_with(&group, LocalExecuteInvalidationMessage)?;
        }
    }
    // C3 S4b: what this thread read under its own uncommitted messages holds no longer, committed (their generations
    // moved as they were sent) or rolled back (no generation records them)
    l2cache::note_own_invals();

    // C frees the arrays with TopTransactionContext; capacity retained here
    // (alloc-free steady state per commit).
    with_state(|state| {
        state.trans_stack.clear();
        state.stamp_touched = 0;
        state.pending = crate::PendingSummary::default();
        state.rel_scopes.clear();
        state.cat_scopes.clear();
        state.cat_unscoped = 0;
        state.support_families.clear();
        for arr in &mut state.msg_arrays {
            arr.clear();
        }
    });

    Ok(())
}

pub fn AtEOSubXact_Inval(isCommit: bool) -> PgResult<()> {
    let info_level = match with_state(|state| {
        if isCommit {
            debug_assert!(state.inplace_info.is_none());
        } else {
            state.inplace_info = None;
        }
        state.trans_stack.last().map(|top| top.my_level)
    }) {
        Some(level) => level,
        None => return Ok(()),
    };

    let my_level = xact_seams::get_current_transaction_nest_level::call();

    if info_level != my_level {
        debug_assert!(info_level < my_level);
        return Ok(());
    }

    if isCommit {
        /* If CurrentCmdInvalidMsgs still has anything, fix it */
        CommandEndInvalidationMessages()?;

        // Entries are lazy: no parent at the adjacent level, just re-level.
        let parent_is_adjacent = with_state(|state| {
            let len = state.trans_stack.len();
            len >= 2 && state.trans_stack[len - 2].my_level >= my_level - 1
        });

        if !parent_is_adjacent {
            with_state(|state| {
                state.trans_stack.last_mut().expect("checked non-empty").my_level -= 1;
            });
            return Ok(());
        }

        with_state(|state| {
            let len = state.trans_stack.len();
            let (head, tail) = state.trans_stack.split_at_mut(len - 1);
            let parent = &mut head[len - 2];
            let myinfo = &mut tail[0];

            append_invalidation_messages(
                &mut parent.prior_cmd_invalid_msgs,
                &mut myinfo.prior_cmd_invalid_msgs,
            );

            parent
                .ii
                .current_cmd_invalid_msgs
                .set_group_to_follow(&parent.prior_cmd_invalid_msgs);

            if myinfo.ii.relcache_init_file_inval {
                parent.ii.relcache_init_file_inval = true;
            }

            state.trans_stack.pop();
        });
    } else {
        process_group_locally(|state| Some(state.trans_stack.last()?.prior_cmd_invalid_msgs))?;
        // C3 S4b: a rolled-back subtransaction's catalog changes: what was read under them holds no longer
        l2cache::note_own_invals();

        with_state(|state| {
            state.trans_stack.pop();
        });
    }

    Ok(())
}

pub fn PreInplace_Inval() -> PgResult<()> {
    let pre = with_state(|state| {
        state
            .inplace_info
            .as_ref()
            .is_some_and(|info| info.relcache_init_file_inval)
    });
    if pre {
        relcache_seams::relation_cache_init_file_pre_invalidate::call()?;
    }
    Ok(())
}

pub fn AtInplace_Inval() -> PgResult<()> {
    let relcache_init_file_inval = match with_state(|state| {
        state.inplace_info.as_ref().map(|info| info.relcache_init_file_inval)
    }) {
        Some(flag) => flag,
        None => return Ok(()),
    };

    with_state(|state| {
        let group = state
            .inplace_info
            .as_ref()
            .expect("inplace_info checked above")
            .current_cmd_invalid_msgs;
        send_group(state, &group)
    })?;

    if relcache_init_file_inval {
        relcache_seams::relation_cache_init_file_post_invalidate::call()?;
    }

    with_state(forget_inplace_invalidation_state);
    Ok(())
}

pub fn ForgetInplace_Inval() {
    with_state(forget_inplace_invalidation_state);
}

// C only nulls inplaceInvalInfo; the owned arrays physically hold the stash,
// so roll them back to the stash start. An aborted subtransaction leaves dead
// slots past the live cursor (cursor-write overwrites them, C shape), so len
// may exceed nextmsg; the stash still ends the live region, making the
// firstmsg rollback sound.
pub(crate) fn forget_inplace_invalidation_state(state: &mut InvalState<'_>) {
    if let Some(info) = state.inplace_info.take() {
        let group = info.current_cmd_invalid_msgs;
        for subgroup in [CAT_CACHE_MSGS, REL_CACHE_MSGS] {
            debug_assert!(state.msg_arrays[subgroup].len() >= group.nextmsg[subgroup]);
            state.msg_arrays[subgroup].truncate(group.firstmsg[subgroup]);
        }
    }
}

pub fn PostPrepare_Inval() -> PgResult<()> {
    AtEOXact_Inval(false)
}

// AtEOXact_Inval processing order (Prior:Cat, Current:Cat, Prior:Rel,
// Current:Rel); C allocates in CurTransactionContext — the caller's mcx.
pub fn xactGetCommittedInvalidationMessages<'mcx>(
    mcx: Mcx<'mcx>,
) -> PgResult<(PgVec<'mcx, SharedInvalidationMessage>, bool)> {
    with_state(|state| {
        let info = match state.trans_stack.first() {
            Some(info) => info,
            None => return Ok((PgVec::new_in(mcx), false)),
        };
        debug_assert!(state.trans_stack.len() == 1 && info.my_level == 1);

        let relcache_init_file_inval = info.ii.relcache_init_file_inval;
        let prior = info.prior_cmd_invalid_msgs;
        let current = info.ii.current_cmd_invalid_msgs;
        let nummsgs = prior.num_in_group() + current.num_in_group();

        let mut msgarray: PgVec<'mcx, SharedInvalidationMessage> = PgVec::new_in(mcx);
        msgarray
            .try_reserve_exact(nummsgs)
            .map_err(|_| mcx.oom(nummsgs * size_of::<SharedInvalidationMessage>()))?;
        for (group, subgroup) in [
            (&prior, CAT_CACHE_MSGS),
            (&current, CAT_CACHE_MSGS),
            (&prior, REL_CACHE_MSGS),
            (&current, REL_CACHE_MSGS),
        ] {
            msgarray.extend_from_slice(subgroup_slice(&state.msg_arrays, group, subgroup));
        }
        debug_assert_eq!(msgarray.len(), nummsgs);
        Ok((msgarray, relcache_init_file_inval))
    })
}

pub fn inplaceGetInvalidationMessages<'mcx>(
    mcx: Mcx<'mcx>,
) -> PgResult<(PgVec<'mcx, SharedInvalidationMessage>, bool)> {
    with_state(|state| {
        let info = match state.inplace_info.as_ref() {
            Some(info) => info,
            None => return Ok((PgVec::new_in(mcx), false)),
        };
        let relcache_init_file_inval = info.relcache_init_file_inval;
        let group = info.current_cmd_invalid_msgs;
        let nummsgs = group.num_in_group();

        let mut msgarray: PgVec<'mcx, SharedInvalidationMessage> = PgVec::new_in(mcx);
        msgarray
            .try_reserve_exact(nummsgs)
            .map_err(|_| mcx.oom(nummsgs * size_of::<SharedInvalidationMessage>()))?;
        for subgroup in [CAT_CACHE_MSGS, REL_CACHE_MSGS] {
            msgarray.extend_from_slice(subgroup_slice(&state.msg_arrays, &group, subgroup));
        }
        debug_assert_eq!(msgarray.len(), nummsgs);
        Ok((msgarray, relcache_init_file_inval))
    })
}

pub fn ProcessCommittedInvalidationMessages(
    msgs: &[SharedInvalidationMessage],
    relcache_init_file_inval: bool,
    dbid: Oid,
    tsid: Oid,
) -> PgResult<()> {
    if msgs.is_empty() {
        return Ok(());
    }

    // inval.c:1142
    elog::elog(
        types_error::DEBUG4,
        format!(
            "replaying commit with {} messages{}",
            msgs.len(),
            if relcache_init_file_inval { " and relcache file invalidation" } else { "" }
        ),
    )?;

    if relcache_init_file_inval {
        // inval.c:1147
        elog::elog(
            types_error::DEBUG4,
            format!("removing relcache init files for database {dbid}"),
        )?;

        // C pokes DatabasePath directly (SetDatabasePath is once-per-backend);
        // recovery-cold, so a per-call context for the path is fine.
        if OidIsValid(dbid) {
            let ctx = mcx::MemoryContext::new("ProcessCommittedInvalidationMessages");
            let path = relpath_seams::get_database_path::call(ctx.mcx(), dbid, tsid)?;
            miscinit_seams::set_database_path::call(&path);
        }

        relcache_seams::relation_cache_init_file_pre_invalidate::call()?;

        if OidIsValid(dbid) {
            miscinit_seams::clear_database_path::call();
        }
    }

    l2_bump_for_messages(msgs, None);
    sinval_seams::send_shared_invalid_messages::call(msgs)?;

    if relcache_init_file_inval {
        relcache_seams::relation_cache_init_file_post_invalidate::call()?;
    }

    Ok(())
}

pub fn LogLogicalInvalidations() -> PgResult<()> {
    with_state(|state| {
        let group = match state.trans_stack.last() {
            Some(top) => top.ii.current_cmd_invalid_msgs,
            None => return Ok(()),
        };
        let nmsgs = group.num_in_group();
        if nmsgs == 0 {
            return Ok(());
        }

        // Reused scratch wire images; xlog_insert never re-enters inval, so
        // the borrow stays held.
        let InvalState { msg_arrays, wal_scratch, .. } = state;
        for subgroup in [CAT_CACHE_MSGS, REL_CACHE_MSGS] {
            let buf = &mut wal_scratch[subgroup];
            buf.clear();
            for msg in subgroup_slice(msg_arrays, &group, subgroup) {
                mcx::vec_append_bytes(buf, &msg.to_wire_bytes())?;
            }
        }

        let header = (nmsgs as i32).to_ne_bytes();
        let mut fragments: [&[u8]; 3] = [&header, &[], &[]];
        let mut n = 1;
        for subgroup in [CAT_CACHE_MSGS, REL_CACHE_MSGS] {
            if !wal_scratch[subgroup].is_empty() {
                fragments[n] = &wal_scratch[subgroup];
                n += 1;
            }
        }
        xloginsert_seams::xlog_insert::call(RM_XACT_ID, XLOG_XACT_INVALIDATIONS, &fragments[..n])?;
        Ok(())
    })
}
