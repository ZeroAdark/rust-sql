//! The pgrc_write test suite (M3-D exit slice + the M3-I parallel
//! suite). Shared scaffolding lives in [`crate::testkit`] (the pgrc_read
//! `testpart` precedent: a test-support module in the lib so the
//! `pgrc_ingest_par` binding crate's integration tests drive the same kit).

mod band_forms;
mod bankplane_tests;
mod bulksort;
mod facts_publish;
mod fastadd;
mod crash_matrix;
mod compactp;
mod concload;
mod cut_estimate;
mod detoast_tests;
mod determinism;
mod grain_tests;
mod par_determinism;
mod par_seam;
mod par_wal;
mod pooled_seal;
mod streamed_seal;
mod dict_inherit;
mod dict_tests;
mod fold_fusion;
mod freeze_tests;
mod genid;
mod idburn;
mod election;
mod full_election;
mod lifecycle_tests;
mod meta_wire_tests;
mod newer_format;
mod no_storage;
mod overflow_tests;
mod publish_order;
mod stacked;
mod reseal;
mod rowmap2;
mod roundtrip_gate;
mod runs_heap;
mod shred_jsonb_tests;
mod shred_tests;
mod size_pins;
mod sort_cmp;
mod sorted_ingest;
mod sortruns;
mod stats_runs;
mod ext_stats_tombs;
mod stats_sidecar;
mod structural_arrays;
mod two_witness;
mod wal_plan;
mod wal_sink;

/// D4084: door 1 of the memory ledger over this test binary's allocations, as the server's allocator counts them
/// (`memledger::door_heap`: the calling thread's current account), so a test can read which account an allocation lands
/// on. A pass-through to the system allocator.
struct LedgerDoor;

// SAFETY: every method forwards to System under the caller's contract; door 1 only counts.
unsafe impl std::alloc::GlobalAlloc for LedgerDoor {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        // SAFETY: forwarded under the caller's contract.
        let p = unsafe { std::alloc::System.alloc(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        // SAFETY: forwarded under the caller's contract.
        let p = unsafe { std::alloc::System.alloc_zeroed(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        memledger::door_heap(-(l.size() as isize));
        // SAFETY: forwarded under the caller's contract.
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded under the caller's contract.
        let np = unsafe { std::alloc::System.realloc(p, l, new_size) };
        if !np.is_null() {
            memledger::door_heap(new_size as isize - l.size() as isize);
        }
        np
    }
}

#[global_allocator]
static LEDGER_DOOR: LedgerDoor = LedgerDoor;

pub use crate::testkit::*;

// The suite files were written against the pre-split scaffolding where these
// names lived in this module; re-export them so `use super::*` keeps working.
// (Some rows shadow testkit re-exports — allow the overlap.)
#[allow(unused_imports)]
pub use crate::elect::ReferenceCandidates;
#[allow(unused_imports)]
pub use crate::par::{SharedMemVfs, SharedMemVfsProvider};
#[allow(unused_imports)]
pub use crate::ingest::{NoExternalDetoast, RawDatum};
#[allow(unused_imports)]
pub use crate::publish::{TxnProbe, TxnVerdict};
#[allow(unused_imports)]
pub use crate::seal::ReferenceResolver;
#[allow(unused_imports)]
pub use crate::shred::NoShred;
#[allow(unused_imports)]
pub use crate::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter, TxnStamp};
#[allow(unused_imports)]
pub use crate::wvfs::{MemVfs, WriteVfs};
#[allow(unused_imports)]
pub use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
#[allow(unused_imports)]
pub use pgrc_format::manifest::Manifest;
#[allow(unused_imports)]
pub use pgrc_format::part::{
    ExtentRecord, FooterFixed, PartTail, SectionEntry, SectionKind, StreamEntry, StreamRole,
};
#[allow(unused_imports)]
pub use pgrc_format::relopt::ShredOptions;
#[allow(unused_imports)]
pub use pgrc_format::wire::Cur;
#[allow(unused_imports)]
pub use std::collections::BTreeMap;

