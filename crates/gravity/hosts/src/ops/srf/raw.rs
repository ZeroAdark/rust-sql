//! `ops::srf`'s raw reads in hosts (PLAN-v5 §3.2 R6 / R7): the `unsafe` the store reader (store.rs `ScanStore`, H-c / H-d /
//! H-e) needs, each a named function with its SAFETY text: the window's words, the node's lifetimes over its executor
//! state, the store's rows, a by-reference value's copy and the head's three entries. Moved unchanged from store.rs and
//! store_row.rs (construct bundle srf-04).
#![allow(unsafe_code)]
use executils::EStateData;
use gravity_abi::Ctx;
use mcx::Mcx;
use nodefunctionscan::exec_init_function_scan;
use nodenamedtuplestorescan::exec_init_named_tuplestore_scan;
use nodetablefuncscan::exec_init_table_func_scan;
use tuplestore::Tuplestore;
use types_error::PgResult;
use types_nodes::bitmapset::Bitmapset;
use types_nodes::plannodes::{FunctionScan, PlannedStmt};
use types_nodes::Node;
use types_portal::{ParamListHandle, QueryEnvHandle};
use types_slot::EXEC_FLAG_REWIND;
use types_tuple::varatt;

use crate::ops::srf::store::{win_valid, ScanStore, Store, StoreHead, WIN_N, WIN_VALUES};
use crate::ops::srf::store_row::Fixed;

/// Row count `n` into the window's `WIN_N` word (store.rs `ScanStore::batch`).
pub(super) fn put_count(win: *mut u8, n: usize) {
    // SAFETY: the window holds `win_bytes(natts, max)` bytes.
    unsafe { win.add(WIN_N).cast::<u64>().write(n as u64) };
}

/// A changed rewind's chgParam `set` at the node's lifetime for its rescan (store.rs `ScanStore::rewind`).
pub(super) fn chg_param<'s, 'm: 's>(set: &'s Bitmapset<'_>) -> &'s Bitmapset<'m> {
    // SAFETY: `set`'s words live in `chgcx`, reset only at the next changed rewind; the rescan only reads it.
    unsafe { &*(set as *const Bitmapset<'_>).cast::<Bitmapset<'_>>() }
}

/// The execution's statement, parameters and query environment into the store's fresh executor state (store.rs
/// `Run::start`).
pub(super) fn bind(es: &mut EStateData<'_>, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, query_env: QueryEnvHandle) {
    // SAFETY: the `'static` plan outlives this statement-lived state (`querydesc::shorten_pstmt`'s narrowing, as
    // prune.rs `over_state`); the parameters and the environment outlive the execution (their registrants').
    es.es_plannedstmt = Some(unsafe { &*(pstmt as *const PlannedStmt<'static>).cast::<PlannedStmt<'_>>() });
    es.es_param_list_info = (!params.is_null()).then(|| unsafe { types_portal::params::resolve(params) });
    // D2834: a PL list's slot left to evaluation time is its owner's fetch's, as execMain's start takes it
    es.es_param_fetch = types_portal::params::fetch_hook(params);
    es.es_queryEnv = (!query_env.is_null()).then(|| unsafe { queryenvironment::hold::resolve(query_env) });
}

/// The node's `ExecInit*` over executor state `es` (store.rs `Run::start`): `bare`'s copy when it made one, else the
/// plan's FunctionScan, TableFuncScan or NamedTuplestoreScan.
pub(super) fn init<'m>(mcx: Mcx<'m>, es: &mut EStateData<'m>, plan: Node<'static>, bare: Option<&'m FunctionScan<'m>>) -> PgResult<Box<dyn Store<'static>>> {
    let node: Box<dyn Store<'_> + '_> = unsafe {
        if let Some(b) = bare {
            Box::new(exec_init_function_scan(mcx, b, es, EXEC_FLAG_REWIND)?) // alloc: Exec
        } else if let Some(f) = plan.as_function_scan() {
            Box::new(exec_init_function_scan(mcx, &*(f as *const FunctionScan<'static>).cast(), es, EXEC_FLAG_REWIND)?) // alloc: Exec
        } else if let Some(t) = plan.as_table_func_scan() {
            Box::new(exec_init_table_func_scan(mcx, &*(t as *const types_nodes::plannodes::TableFuncScan<'static>).cast(), es)?) // alloc: Exec
        } else {
            let n = plan.as_named_tuplestore_scan().expect("ScanStore::new admits three kinds");
            Box::new(exec_init_named_tuplestore_scan(mcx, &*(n as *const types_nodes::plannodes::NamedTuplestoreScan<'static>).cast(), es, EXEC_FLAG_REWIND)?) // alloc: Exec
        }
    };
    // SAFETY: the node's data lives in the state's query context, which outlives it (`Drop for Run`).
    Ok(unsafe { core::mem::transmute::<Box<dyn Store<'_> + '_>, Box<dyn Store<'static>>>(node) })
}

/// The node at the executor state's lifetime for one call (store.rs `Run::with`).
pub(super) fn narrow<'s, 'a>(node: &'s mut (dyn Store<'static> + 's)) -> &'s mut (dyn Store<'a> + 's) {
    // SAFETY: the node lives in this state (`start`); the lifetime narrows to the call.
    unsafe { core::mem::transmute::<&mut dyn Store<'static>, &mut (dyn Store<'_> + '_)>(node) }
}

/// The window's validity bitmaps (`natts * max / 64` words after the values) cleared; their address (store.rs `Run::pull`).
pub(super) fn clear_valid(win: *mut u8, natts: usize, max: usize) -> *mut u64 {
    // SAFETY: the bitmaps are `natts * max / 64` words inside the window.
    let valid = unsafe { win.add(win_valid(natts, max)) } as *mut u64;
    unsafe { core::ptr::write_bytes(valid, 0, natts * max / 64) };
    valid
}

/// The node's store behind `s` (store.rs `Run::pull`, `Store::rows`).
pub(super) fn store_of<'a>(s: *mut Tuplestore) -> &'a mut Tuplestore {
    // SAFETY: `rows` is the node's store, which only its rescan and end (never inside a pull) end or move.
    unsafe { &mut *s }
}

/// Column `c` of row `n` into the window: datum `v` and its NOT NULL bit (store.rs `Run::pull`).
pub(super) fn put(win: *mut u8, valid: *mut u64, max: usize, c: usize, n: usize, v: u64) {
    // SAFETY: column c row n is inside the window (c < natts, n < max).
    unsafe {
        win.add(WIN_VALUES + (c * max + n) * 8).cast::<u64>().write(v);
        *valid.add(c * max / 64 + n / 64) |= 1 << (n % 64);
    }
}

impl Fixed {
    /// The slot's minimal tuple deformed straight into row `n` of the window when it holds no NULL (store_row.rs `Fixed`;
    /// store.rs `Run::pull`).
    pub(crate) fn deform(&self, slot: &types_slot::SlotData<'_>, win: *mut u8, valid: *mut u64, max: usize, n: usize, need: u64) -> bool {
        // SAFETY: `win` / `valid` are the window of `max` rows and `natts = cols.len()` columns; `n < max` (the reader's
        // window, store.rs `Run::pull`); the slot holds the store's tuple until the next get.
        unsafe {
            let types_slot::SlotData::Minimal(m) = slot else { return false };
            let Some(p) = m.mintuple else { return false };
            let t = p.as_ref();
            let Some(body) = (t.t_hoff as usize).checked_sub(types_tuple::MINIMAL_TUPLE_OFFSET) else { return false };
            if t.t_infomask & types_tuple::HEAP_HASNULL != 0 || (t.natts() as usize) < self.cols.len() || (t.t_len as usize) < body + self.end {
                return false;
            }
            let tp = p.as_ptr().cast::<u8>().add(body);
            // bounded("key-columns: one cell per column of the scan's row, at most MaxTupleAttributeNumber (1664) columns")
            for (c, &(off, len)) in self.cols.iter().enumerate().filter(|&(c, _)| c >= 64 || need >> c & 1 != 0) {
                let v = types_tuple::tupmacs::fetch_att(tp.add(off), true, len as i32).as_usize() as u64;
                win.add(WIN_VALUES + (c * max + n) * 8).cast::<u64>().write(v);
                *valid.add(c * max / 64 + n / 64) |= 1 << (n % 64);
            }
            true
        }
    }
}

/// A by-reference datum's bytes in the reader's region (a varlena whole, toast pointer included; a cstring with its NUL).
pub(super) fn copy(mcx: Mcx<'_>, p: *const u8, attlen: i16) -> PgResult<u64> {
    // SAFETY: `p` is a live datum of the column's type in the slot; `d` holds `n` bytes (8-aligned: `alloc_bytes`).
    unsafe {
        let n = match attlen {
            -1 => varatt::varsize_any(p),
            -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes_with_nul().len(),
            l => l as usize,
        };
        let d = mcx.alloc_bytes(n)?.as_ptr();
        core::ptr::copy_nonoverlapping(p, d, n);
        Ok(d as u64)
    }
}

pub(super) unsafe fn batch_of(h: *mut StoreHead, _ctx: *mut Ctx, ptr: u32, max: u32, natts: u32, need: u64) -> Result<u64, ()> {
    (*(h as *mut ScanStore)).batch(ptr, max, natts, need).map_err(gravity_fn::stash_error)
}

pub(super) unsafe fn rewind_of(h: *mut StoreHead, _ctx: *mut Ctx, ptr: u32) -> Result<(), ()> {
    (*(h as *mut ScanStore)).rewind(ptr).map_err(gravity_fn::stash_error)
}

pub(super) unsafe fn param_of(h: *mut StoreHead, _ctx: *mut Ctx, id: u32, value: u64, flags: u64, changed: bool) -> Result<(), ()> {
    (*(h as *mut ScanStore)).param(id, value, flags, changed).map_err(gravity_fn::stash_error)
}
