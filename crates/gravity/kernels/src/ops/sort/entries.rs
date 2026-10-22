//! M3's host entries (M3-plan.md §6, cards/M3.md L1): the C-ABI functions M3's lowered IR calls beyond host_abi.rs's
//! base utilities and M1's entries. The existing sort / top-k / agg_sort / pair entries are reused, not redeclared.
//! M3-HOSTS declares here, each as its user lands, every entry under `guard!` from the start (V4R-16's rule: a bug's
//! panic never crosses the `extern "C"` frame): `sort_open_res` / `sort_open_res_var` (runs/resident.rs),
//! `code_nonbase_select` (source/code_sparse.rs), `front_light_bind` and `front_light_flush` (keyed/codefront_light.rs, m3_light.rs),
//! `front_pair_flush` (keyed/codefront_pair.rs), `walk_round` (runs/walk.rs), `extent_resident` (runs/walk_width.rs).
//! Each entry is listed in the `hosts:` rows of the lowerv4 file that calls it. `RunShape` is the sealed runs' merge for
//! exec v4/m3_sort.rs (the run layout is this crate's). Created by M3 (owner M3-HOSTS).
use crate::host_abi::{RowCmp, SortSpec, VarShape};

pub use crate::ops::sort::cut::{CUT_NATURAL, CUT_TIES};
pub use crate::ops::sort::raw::{sort_cut_res, sort_cut_res_var, table_bytes, take_table};
pub use crate::dict::strategies::front_light::{front_light_bind, front_light_flush, LightWords, LD_NONE, LD_WAYS, LW_ALLOC, LW_BYTES, LW_CAP, LW_CNT, LW_N, LW_NO_PART, LW_PART, LW_TOUCHED, LW_WAYS};
pub use crate::ops::sort::raw::{BitsOrderWord, BytesOrderWord, NetOrderWord, NumOrderWord, SortFinish, SortHeapStep, SortStreamRearm, SortStreamRecycle, SpanCut, SpanSeen, SpoolLive, SpoolPoint, SPAN_DISTINCT};
pub use crate::ops::sort::raw::cosplit::{sort_cosplit_begin, sort_cosplit_next, sort_cosplit_total, SORT_MODE_SLICES};
pub use crate::ops::sort::raw::place::sort_grow;
pub use crate::ops::sort::runs::{describe, sort_full_res, sort_full_res_var, sort_last_res, sort_last_res_var, sort_open_res, sort_open_res_top, sort_open_res_var, sort_open_res_var_top, RESIDENT_MAX, RUN_CUT, RUN_KEYLANE, RUN_ORDERED, RUN_SEALED};
pub use crate::ops::sort::runs::{RUN_HELD, RUN_HELD_AT};
/// The last word of a var run's table copy (`sort_open_res_var`), at its rows' end: the merge reads its shape there.
pub(crate) const DESCRIBED: u32 = 0x5e5a_7ab1;

/// A sealed run's merge (exec v4/m3_sort.rs): its row comparator (host_abi.rs `RowCmp`) and, for a var row, the var slots
/// whose copies travel with the row.
#[derive(Clone, Copy)]
pub struct RunShape {
    pub(crate) cmp: RowCmp,
    pub(crate) rb: usize,
    pub(crate) var: Option<VarShape>,
}

impl RunShape {
    /// A fixed row's shape: `nkey` key words and `spec` as `sort_open_res` takes them.
    pub fn fixed(row_bytes: u32, nkey: usize, spec: u64) -> RunShape {
        RunShape { cmp: RowCmp::fixed(nkey / 2, SortSpec(spec)), rb: row_bytes as usize, var: None }
    }
}
