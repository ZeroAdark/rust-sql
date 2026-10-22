//! Gravity ctx ABI: the Ctx header, Site windows, Program block, Out block, the one offset table
//! and the status vocabulary (docs/gravity/abi.md, docs/gravity/abi-v2.md).
#![no_std]

pub mod breaker;
pub mod buildrows;
pub mod ctx;
pub mod hash;
pub mod host;
pub mod key;
pub mod layout;
pub mod limits;
pub mod program;
pub mod site;
pub mod spill;
pub mod status;
pub mod t3;

pub use ctx::lane;
pub use layout::{unword, word, PtrWord, Word};
pub use breaker::{rec_face, rec_open, RecFace, RecOpenFn, NO_VALIDITY, ORDER_KEY, ORDER_NONE, hash_at, keys_eq, table_kind, AccCell, Block, Chain, Owned, OwnerFns, OwnerTop, T1, OWNED_HDR, OWNERTOP_HDR, T1_HDR, TABLE_OWNED, TABLE_T1, T1_BYPASS, LAYOUT_VALIDITY, LAYOUT_IMPLICIT_COUNT, AggSlot, Breaker, BreakerKind, Budget, DenseFront, HashAgg, HashBuild, HashBuildEntry, KeyCell, KeyKind, Rec, RecordLayout, SlotKind, SortRun, Spill, TopK, BUILD_PARTITION, BUILD_TABLE, ACC_CELL, DENSE, FLAG_ACC, FLAG_FRONT, HASHAGG_HDR, HASHBUILD_ENTRY, HASHBUILD_HDR, HASHED, MAX_RECORD_KEYS, MAX_RECORD_SLOTS, RECORD_HDR, SELECT_MAX, SHARE, SORTRUN_HDR, TABLE_AGG, TABLE_BUILD, TABLE_KIND_OFF, TABLE_SORT, TOPK_TIES, USED_NULL};
pub use ctx::{Answer, Ctx, Datum, Identity, Lane, MorselRange, Out, Partials, ScanPos, ANSWER_MINMAX, ANSWER_SIZE, ANSWER_SUM, HEADER_SIZE, IDENTITY_SIZE, LANE_SIZE, OUT_SIZE, PARTIALS_SIZE};
pub use program::{ProgramBlock, PROGRAM_SIZE};
pub use site::{Site, SITE_SIZE};
pub use status::Status;
