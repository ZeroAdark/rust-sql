//! The four operations tcop calls (docs/gravity/exec.md §4.7). `bind` admits a

use std::rc::Rc;

use std::cell::Cell;
use std::sync::Arc;

use gravity_driver::{Binding, Error, Exec, Grant, Merge, PullAdapter};
use gravity_hosts::ops::scan::pgrc::granule_source;
use gravity_hosts::ops::scan::heap_source;
use gravity_hosts::HostRegistry;
use gravity_parallel::{installed_pool, order_topk, run_section, startup_c0, BlockRange, ClaimSize, FirstError, GranuleRange, HashAggPartial, HashBuildPartial, Job, MorselSource, Plan, PlainAggPartial, Pool, RowsUnordered, Section, TopKPartial};
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg, KeyCell, KeyKind, OwnerFns};
use gravity_program::{limits, End, Program, WordsDesc};
use limits::PARAM_WORDS as WORDS;
use gravity_program::{Cmd, Generation, Handle, PlanKey};
use types_nodes::parsenodes::FETCH_ALL;
use mcx::{Mcx, MemoryContext};
use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_portal::{CachedPlanHandle, ParamListHandle, Portal, QueryCompletion};
use types_snapshot::SnapshotData;

use crate::admit;
use crate::glue;
use crate::legs::LegSink;
use crate::output_cid::output_cid;
use crate::receivers::{CellSink, DestSink, Receiver};
use crate::shapes::{self, Admitted, KeySrc, Walk};
use crate::{Backend, Verdict};

pub(crate) use crate::inputs::knobs::width::{dense_on, forced_c0, forced_width, width_for, BLOCK_C0, GRANULE_C0};
pub use crate::inputs::knobs::width::{tail_from_part, whole_part_claims};

pub(crate) use crate::inputs::knobs::room::{granted_memory, granted_width};
pub(crate) use crate::inputs::knobs::grant::{admit_grant, admit_grant_narrowing, GrantLease};
pub use crate::inputs::knobs::grant::unadmitted;
pub use crate::inputs::knobs::grant::{caller_granted, hand, NestedGrants};
pub(crate) use crate::ops::hashjoin::admit::{build_room, build_would_spill, lowered_build, spilled_build_error};

mod portal;
mod point;
pub use self::point::plan_skip;
mod admission;
mod execute;
mod drive;
mod plan;
pub use plan::producer_ran;
pub use plan::PlanScratch;
mod twopass;
mod producers;
mod keys;
mod raw;
mod recheck;
pub mod held;
pub mod start_gates;

pub use self::portal::*;
pub use self::admission::*;
pub use self::execute::*;
pub use self::drive::*;
use self::plan::*;
use self::twopass::*;
pub use self::producers::*;
pub use self::keys::*;
pub use self::recheck::{ahead_tagged, in_plan_order, note_recheck, note_tag_refused, plan_order, plan_order_for, recheck_line, rechecked, rechecks, reruns, surfaces_over_refusal, tag_refusal_of, tag_refused, tag_stands, take_key_ties, PlState};
