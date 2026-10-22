//! The aggregate descriptors other families read, and the classification table's fold of a class (`fold_of`); the
//! descriptors' methods stay with ops/agg.
use gravity_abi::breaker::{KeyKind, SlotKind};
use gravity_program::{ArenaVec, LifeId};
use types_nodes::Node;

use crate::facts::hints::Hint;
use crate::graph::Layout;
use crate::ops::shared::agg_class::{Class, Ext, Mods, Trans};
use crate::ops::shared::sort::KeyImage;
use crate::catalog::typefacts;

pub const AGG_PLAIN: u32 = 0;
pub const AGG_SORTED: u32 = 1;
pub const AGG_HASHED: u32 = 2;
pub const AGG_MIXED: u32 = 3;
pub const NONE: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupForm {
    Plain,
    Hashed,
    Sorted,
    Group,
    Unique,
    Distinct,
    Pass1,
}

/// The transition register form (expr-compiler.md §4.6's `Transition` as the record folds it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Register {
    CountStar,
    CountAny,
    SumInt,
    SumInt128,
    SumF64,
    AvgInt,
    AvgInt128,
    /// `avg(float4 / float8)`: `float8_accum`'s Youngs–Cramer step into an `F64Accum` slot {N, Sx, Sxx} (D244).
    AvgF64,
    MinInt,
    MaxInt,
    MinCode,
    MaxCode,
    DistinctCount,
    SumNum,
    AvgNum,
    MinNum,
    MaxNum,
    /// NEW-numeric-1: `min` / `max` of an unconstrained numeric column over its values' order keys (gravity_fn typesem
    /// numeric `order_key`) in a `MinI64` / `MaxI64` slot; N13: a value past the key folds into the side companion right
    /// after it (abi `NumSideMin` / `NumSideMax`). Only a merge that cannot order a tie raises, under the statement's
    /// recheck licence (ops/agg `held_whole`), whose plan-order run folds `MinVar` / `MaxVar`.
    MinNumDyn,
    MaxNumDyn,
    MinF64,
    MaxF64,
    BoolAnd,
    BoolOr,
    /// `regr_count`: the pairs with both arguments non-null into a `Count`.
    CountPair,
    /// `sum(float4)`: the fold rounds to f32 at every step (the catalog's `float4pl`), kept in a `SumF32` slot.
    SumF32,
    SumInterval,
    AvgInterval,
    /// `min / max(interval)`: PG's Interval in an `IntervalMin` / `IntervalMax` slot, a tie keeping the later member.
    MinInterval,
    MaxInterval,
    /// `sum` / `avg(numeric)` without a decidable scale: the numeric accumulator in the arena (`Trans::Var`).
    SumNumVar,
    AvgNumVar,
    /// V4R-38: `sum` / `avg(numeric)` over a `+` / `-` / `*` tree past the packed register's 37 digits whose products are
    /// each a packed factor times a one-word one (`wide_tree`): the i256 `SumNumWide` / `AvgNumWide` slot at the tree's
    /// fixed scale, folded by lowerv4's class.num_wide (M3) instead of the `Var` register.
    SumNumWide,
    AvgNumWide,
    /// An extremum over a `Var` slot under the row's `sortop` order (heap text, bytea, opaque types, unscaled numeric).
    MinVar,
    MaxVar,
    BitAnd,
    BitOr,
    BitXor,
    /// The Youngs–Cramer `(n, sx, sxx)` fold over f64 (`float8_accum`).
    YoungsCramer,
    /// Exact `(n, sx, sxx)` over i64 / i128.
    ExactStat,
    /// The numeric accumulator triple in the arena.
    NumStat,
    /// The six-sum regression accumulator over f64.
    Regr,
    /// The collecting fold: the arena list per `Collect` form.
    Collect,
    /// The ordered-set fold: the arena list sorted by the WITHIN GROUP keys at the final.
    OrderedSet,
    /// The hypothetical-set fold: the direct arguments ranked among the collected keys at the final.
    Hypothetical,
    /// An extremum over the argument's order-preserving i64 image by `Ext` (`U64`: the sign bit flipped; `Tid`: block << 16 | offset).
    MinImage,
    MaxImage,
    /// `sum(money)`: `cash_pl`'s overflow-checked add into a `SumI64` slot.
    SumMoney,
    /// `AGGSPLITOP_COMBINE`: the input is a partial aggregate's state, folded by the row's `combinefn` into the class's slot.
    Combine,
    /// A user-defined aggregate (decision 79): `pg_aggregate`'s `transfn` / `finalfn` for `AggDecl.fnoid`, called
    /// batched per group over a `Var` state (the tier-C protocol, `aggtab` has no row).
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalForm {
    Identity,
    Int128Sum,
    Int128Avg,
    /// An `F64Accum` slot's Sx / N (`float8_avg`: NULL while N is 0).
    F64Avg,
    Rendered,
    PairCount,
    /// A mantissa (an i128 sum, an i64 min / max) rendered as a numeric at `AggDecl.scale`.
    Numeric,
    NumericAvg,
    /// The pair set's fold on the emit side: the DISTINCT argument's key word folded by `reg` per group (`PairCount`'s twin).
    PairFold,
    /// An f64 kept as its order-preserving i64 image in a min / max slot, rendered back.
    F64Ordered,
    /// A bool kept as 0 / 1 in a min (`bool_and` / `every`) or max (`bool_or`) slot.
    Bool,
    /// A `SumF32` slot's f32 sum rendered as float4.
    F32,
    /// An `IntervalSum` slot's sum (or an `IntervalMin` / `IntervalMax` slot's member) rendered as an interval;
    /// `IntervalAvg` divides the sum by the slot's N (PG's `interval_avg`).
    Interval,
    IntervalAvg,
    /// A statistical accumulator's final by the class's `Stat` / `Regr` (the catalog's `finalfn` on the row).
    Stat,
    /// A user-defined aggregate's `finalfn` (or its state as is) over the `Var` state (decision 79).
    User,
    Regr,
    /// A collecting slot rendered by its `Collect` form (array / string / json / xml / range / any_value).
    Collect,
    /// A `Var` list sorted by the WITHIN GROUP keys, then the class's final (`percentile_*`, `mode`).
    Ordered,
    /// The hypothetical-set rank arithmetic over the sorted `Var` list and the direct arguments.
    Hypo,
    /// A `Var` identity set (the DISTINCT arguments) folded by the class at the emit (the non-pair DISTINCT forms).
    Distinct,
    /// A `Var` list sorted by the call's ORDER BY keys, then folded by the class (an order-sensitive class).
    OrderedFold,
    /// `AGGSPLITOP_SKIPFINAL`: the emit is the transition state itself (serialized under `AGGSPLITOP_SERIALIZE`).
    Partial,
    /// An `Ext` image slot rendered back to its type (`MinImage` / `MaxImage`).
    Image,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyDecl<'p> {
    pub image: KeyImage,
    /// The key's record words (1; 2 for `Fp128` and the 16-byte images).
    pub words: u8,
    /// The key's collation (`grpCollations` / `uniqCollations`; a DISTINCT argument's `inputcollid`).
    pub coll: u32,
    /// A sorted group's input direction on this key (the emit keeps it; `ORDER_KEY` ascending is the abi's only form: need).
    pub desc: bool,
    pub nulls_first: bool,
    pub word: u16,
    pub resno: i16,
    pub kind: KeyKind,
    pub attno: i16,
    pub expr: Stripped<'p>,
    /// A numeric key's mantissa scale (an `I64` word), `-1` otherwise.
    pub scale: i16,
    /// An `Fp128` key's representative: the `MinCode` slot holding `(part, code)` of one row of the
    /// group (the emit renders the group's text from it once, never per row); a float / narrow numeric
    /// key's (M6 L0-m): the slot keeping one member's datum; `NONE` for other kinds.
    pub rep: u16,
    /// A numeric key's digits (scale + integer digits; 0 = not a numeric or unknown): D197's word bound.
    pub precision: u8,
    pub tf: u32,
    /// An anonymous `record` key's columns: the ROW() every producer of its values agrees with
    /// (`facts::record::key_row`), `None` for any other key and for a record whose columns are known only per row.
    pub rec: Option<Node<'p>>,
    /// D2163: the plan's equality operator for the key is a user `=` over byteaeq (`Facts::key_eq` Bytes): the key
    /// keys by its payload bytes (typesem bytea's form), not its type's canonical image.
    pub bytes_eq: bool,
    /// D2719 / D2640 (group lane): the plan's equality operator for the key when it is a user operator over user code or
    /// a built-in other than the type's own `=` (`effects::user_group_eq`), 0 otherwise: the group compares the key
    /// through its procedure (lowerv4 ops/group/stream.rs), never by its image, so only a sorted group that streams
    /// (`Op::GroupRun`) takes it.
    pub user_eq: u32,
    /// A scan column key's average bytes a value, its header not counted, by the statistics (`Facts::att_value_bytes`), 0
    /// unknown or not a scan column: an estimate, which only picks the slower of two forms with the same answers (num.pair's
    /// key).
    pub bytes: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct AggDecl<'p> {
    pub aggno: i16,
    pub fnoid: u32,
    pub reg: Register,
    pub slot: u16,
    pub count_slot: u16,
    pub final_: FinalForm,
    pub node: Node<'p>,
    /// The fixed scale of a numeric register's mantissas (`num_scale`'s form); `-1` for every other register.
    pub scale: i16,
    /// The `FILTER (WHERE …)` predicate gating the fold, evaluated per input row.
    pub filter: Option<Node<'p>>,
    /// The catalog row's transition-slot class (`aggtab`), the slot kind the fold uses, and the call's modifiers as fields.
    pub class: Class,
    pub trans: Trans,
    pub mods: Mods,
    /// The input collation's facts (`Aggref.inputcollid` through `Facts::coll`, D247): a text min / max may take a code's
    /// in-part rank for its order only when `bytes_order`.
    pub coll: typefacts::KeyColl,
    /// D256: a scaled sum / avg over a CASE whose result arms differ in dscale (`scale` = the largest): the `MaxI64`
    /// companion slot holding the largest dscale of the values summed, numeric sum's result dscale; `NONE` otherwise.
    pub dscale_slot: u16,
    /// Each result arm's dscale in `CaseExpr` order (the WHENs', then the ELSE; `NULL_ARM` for a NULL arm, which adds
    /// nothing), one entry per arm; meaningful only with `dscale_slot` (empty otherwise).
    pub dscales: &'p [u8],
}

pub const NULL_ARM: u8 = u8::MAX;

#[derive(Clone, Copy, Debug)]
pub struct TwoPassDecl {
    pub pass1: u16,
    pub slot: u8,
    pub key: u16,
    pub floor_desc: bool,
    pub floor_nulls_first: bool,
}

pub struct GroupDecl<'p> {
    pub slot: u8,
    /// The life the group's records live in (its producer pipeline): consumers find the group by it, never by slot.
    pub producer: LifeId,
    pub node_id: i32,
    pub form: GroupForm,
    pub hints: [Option<Hint>; 2],
    pub layout: Layout<'p>,
    pub keys: ArenaVec<KeyDecl<'p>>,
    pub aggs: ArenaVec<AggDecl<'p>>,
    pub two_pass: Option<TwoPassDecl>,
    /// The planner's group estimate (`numGroups`; a set's plan rows): the distinct form's round-2 partitioning.
    pub groups: u64,
    /// D3784: the largest group's share of the input rows, in 2^-16 (0: unknown), from ANALYZE's statistics of the group's
    /// key columns (ops/agg `top_share`): the pair route's skew (lowerv4 count_cut.rs).
    pub top: u32,
    /// This group is one arm of a grouping-sets union (`map_sets`): the planner's rewrite, one aggregate per set.
    pub set: Option<SetDecl>,
    /// A grouped node whose every key the planner dropped (the user 09-23: R2): an empty input emits no row, and
    /// `pinned` (input columns read outside the aggregates, in facts::demand `agg_reads`' order under `Junk::Computing`: a
    /// bare resjunk column's not among them) take the group's first row, as nodeAgg does.
    pub empty_no_row: bool,
    pub pinned: ArenaVec<i16>,
    /// The planner's aggregate-algebra finals (plannodes `GravityFinal`, the mark lowerv4's finals.cse reads) and the
    /// address of the Agg's own target list, whose positions their resnos are (a projection of another list reads none).
    pub finals: Option<(usize, &'p [types_nodes::plannodes::GravityFinal])>,
}

/// One grouping set's arm: keys outside the arm's `keys` read NULL in the node's target list, `GroupingFunc`
/// reads the arm's key resnos over its `cols`; the arms feed one union slot (`AppendDesc::Union`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetDecl {
    pub arm: u16,
    pub arms: u16,
    /// PostgreSQL reads the node's output whole (`gsets::read_whole`), as the arms read their phase's run.
    pub whole: bool,
}

/// D4620 (road `agg.gsets.cascade`, on by default since its flip: read through `Facts::default_on_road_on`, in
/// `ROADS_DEFAULT_ON`, off under a plain name): the road under which form T's tagged group folds each input row once,
/// under its finest set, and every coarser set is folded from a finer set's owned records (`Cascade`).
pub const GSETS_CASCADE_ROAD: &str = "agg.gsets.cascade";

/// D4620: one candidate parent of a cascaded set: a group whose records hold a strict superset of the set's columns (its
/// slot, the life that fills it, the tag its records of that set carry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CascadeParent {
    pub slot: u8,
    pub life: LifeId,
    pub tag: i32,
}

/// D4620: a grouping set folded from a finer set's records (`gsets_tag::cascade`). Its group (`groups`, producer
/// `producer`) has the base group's keys, aggregates and layout: its records are the base's record rewritten (the tag
/// key := `tag`, each key of `nulls` NULL) and folded by the aggregates' merge. Its producer reads the parent with the
/// fewest records at run time (`parents`: all of them consumed, in this order) by owner-run claims; the base group's
/// emit reads its slot after the base's (the emit's consumed regions past its first).
pub struct Cascade {
    pub producer: LifeId,
    pub slot: u8,
    /// The base: form T's tagged group over the input (its slot and producer).
    pub base_slot: u8,
    pub base: LifeId,
    pub tag: i32,
    pub parents: ArenaVec<CascadeParent>,
    /// The key positions (`GroupDecl::keys`) the set does not group: NULL in its records.
    pub nulls: ArenaVec<u16>,
}

/// Through the relabels and one `OUTER_VAR` level into the child's projection (a deeper reference stays outer: a breaker source's row).
/// An expression with its relabels gone and an Agg-level `OUTER_VAR` read once through the child's last projection (the
/// D181 class, audit C4): what it names can never be stripped again. `agg`: its own vars are still the Agg's.
#[derive(Clone, Copy, Debug)]
pub struct Stripped<'p> {
    pub(in crate::ops) node: Node<'p>,
    pub(in crate::ops) agg: bool,
    /// D1309: a relabel to bpchar was stripped (`t::bpchar` over a text column): the key is typed bpchar, its own result
    /// type (trimmed equality, hash and order), never the stripped node's text.
    pub(in crate::ops) bpchar: bool,
    /// D2832: the type a stripped relabel gave the expression when it is not the stripped node's (`c::text` over a citext
    /// column, `t::bpchar` over a text one): PostgreSQL's operator compares that type, so a key whose base it changes is
    /// typed by it (ops/agg keys.rs `retyped`).
    pub(in crate::ops) retype: Option<u32>,
}

/// The fold of a class: its register (the per-row op), the slot kind it folds into and the final (decompose.md §2.6's
/// classification table). `code_arg`: the argument is a code lane; `scaled`: a numeric argument's scale is decided.
pub fn fold_of(class: Class, fnoid: u32, code_arg: bool, scaled: bool) -> (Register, Trans, FinalForm) {
    use SlotKind as S;
    let abi = Trans::Abi;
    match class {
        Class::CountStar => (Register::CountStar, abi(S::Count), FinalForm::Identity),
        Class::CountAny => (Register::CountAny, abi(S::Count), FinalForm::Identity),
        Class::CountPair => (Register::CountPair, abi(S::Count), FinalForm::Identity),
        Class::SumI64 => (Register::SumInt, abi(S::SumI64), FinalForm::Identity),
        // M6-FOLD R4: PG's cash_pl by fmgr (the hosted fold), its overflow raised as PG raises it
        Class::SumMoney => (Register::SumMoney, Trans::Var, FinalForm::Rendered),
        Class::SumI128 => (Register::SumInt128, abi(S::SumI128), FinalForm::Int128Sum),
        Class::SumNum if scaled => (Register::SumNum, abi(S::SumNum), FinalForm::Numeric),
        Class::SumNum => (Register::SumNumVar, Trans::Var, FinalForm::Numeric),
        Class::SumF32 => (Register::SumF32, abi(S::SumF32), FinalForm::F32),
        Class::SumF64 => (Register::SumF64, abi(S::SumF64), FinalForm::Identity),
        Class::SumInterval => (Register::SumInterval, abi(S::IntervalSum), FinalForm::Interval),
        Class::AvgI128 if crate::catalog::agg::avg_int8(fnoid) => {
            fires::fire("agg.avg_i128"); // shapecheck fire signal (REGISTRY OID-03): avg(int8) in the I128 average register
            (Register::AvgInt128, abi(S::AvgI128), FinalForm::Int128Avg)
        }
        Class::AvgI128 => (Register::AvgInt, abi(S::AvgI128), FinalForm::Int128Avg),
        Class::AvgNum if scaled => (Register::AvgNum, abi(S::AvgNum), FinalForm::NumericAvg),
        Class::AvgNum => (Register::AvgNumVar, Trans::Var, FinalForm::NumericAvg),
        Class::AvgF64 => (Register::AvgF64, abi(S::F64Accum), FinalForm::F64Avg),
        Class::AvgInterval => (Register::AvgInterval, abi(S::IntervalSum), FinalForm::IntervalAvg),
        Class::Min(Ext::Int) => (Register::MinInt, abi(S::MinI64), FinalForm::Identity),
        Class::Max(Ext::Int) => (Register::MaxInt, abi(S::MaxI64), FinalForm::Identity),
        Class::Min(Ext::U64) => (Register::MinImage, abi(S::MinI64), FinalForm::Image),
        Class::Max(Ext::U64) => (Register::MaxImage, abi(S::MaxI64), FinalForm::Image),
        Class::Min(Ext::F64) => (Register::MinF64, abi(S::MinI64), FinalForm::F64Ordered),
        Class::Max(Ext::F64) => (Register::MaxF64, abi(S::MaxI64), FinalForm::F64Ordered),
        Class::Min(Ext::Num) if scaled => (Register::MinNum, abi(S::MinI64), FinalForm::Numeric),
        Class::Max(Ext::Num) if scaled => (Register::MaxNum, abi(S::MaxI64), FinalForm::Numeric),
        Class::Min(Ext::Text) if code_arg => (Register::MinCode, abi(S::MinCode), FinalForm::Rendered),
        Class::Max(Ext::Text) if code_arg => (Register::MaxCode, abi(S::MaxCode), FinalForm::Rendered),
        Class::Min(Ext::Opaque) if crate::catalog::agg::interval_extreme(fnoid) => (Register::MinInterval, abi(S::IntervalMin), FinalForm::Interval),
        Class::Max(Ext::Opaque) if crate::catalog::agg::interval_extreme(fnoid) => (Register::MaxInterval, abi(S::IntervalMax), FinalForm::Interval),
        Class::Min(_) => (Register::MinVar, Trans::Var, FinalForm::Rendered),
        Class::Max(_) => (Register::MaxVar, Trans::Var, FinalForm::Rendered),
        Class::BoolAnd => (Register::BoolAnd, abi(S::MinI64), FinalForm::Bool),
        Class::BoolOr => (Register::BoolOr, abi(S::MaxI64), FinalForm::Bool),
        Class::BitI64(op) => (bit_register(op), abi(bit_slot(op)), FinalForm::Identity),
        Class::BitVar(op) => (bit_register(op), Trans::Var, FinalForm::Rendered),
        Class::Stat3F64(_) => (Register::YoungsCramer, Trans::Stat24, FinalForm::Stat),
        Class::Stat3I128(_) => (Register::ExactStat, Trans::Stat48, FinalForm::Stat),
        Class::Stat3Num(_) => (Register::NumStat, Trans::Var, FinalForm::Stat),
        Class::Stat6F64(_) => (Register::Regr, Trans::Stat48, FinalForm::Regr),
        Class::Collect(_) => (Register::Collect, Trans::Var, FinalForm::Collect),
        Class::OrderedSet(_) => (Register::OrderedSet, Trans::Var, FinalForm::Ordered),
        Class::Hypothetical(_) => (Register::Hypothetical, Trans::Var, FinalForm::Hypo),
        Class::User => (Register::User, Trans::Var, FinalForm::User),
    }
}

/// D4565: the road (lowerv4 ops/agg/trans/class_stats.rs declares it; on by default) under which a plain var / stddev
/// over int2 / int4 folds PostgreSQL's transition state in record slots (`stat_slot`): off when
/// `pgrust.gravity_road_off` names `agg.stat_reg` plainly (`Facts::default_on_road_on`), and then it folds the hosted
/// state.
pub const STAT_REG_ROAD: &str = "agg.stat_reg";

/// D4567: the road (lowerv4 ops/agg/numeric/strategies/num_window.rs declares it; on by default) under which a numeric
/// sum / avg argument gets a decided scale from a mixed-scale CASE / COALESCE beside a `+` / `-` side of a larger fixed
/// scale (decompose agg numeric.rs `form_at`): off when `pgrust.gravity_road_off` names `agg.num_scale` plainly
/// (`Facts::default_on_road_on`), and then those arguments fold hosted.
pub const NUM_SCALE_ROAD: &str = "agg.num_scale";

/// D4567 (a): the road (lowerv4 ops/agg/numeric/strategies/num_window.rs declares it; default off, lowerv4 road_off.rs
/// `DEFAULT_OFF`) under which a numeric sum / avg of `x / k` (k a positive constant whose reciprocal is a finite decimal)
/// folds in registers at a decided scale with its display scale in the dscale word (decompose agg numeric.rs `div_form`):
/// on under `+agg.num_div` (`Facts::road_on`, listed in `ROADS`); off, the argument folds hosted.
pub const NUM_DIV_ROAD: &str = "agg.num_div";
/// D4723: the road (lowerv4 num_window.rs declares it; default off, lowerv4 road_off.rs `DEFAULT_OFF`) under which a
/// min / max of a materialized CTE's numeric column whose every value has one display scale (decompose agg mod.rs
/// `cte_one_dscale`) folds the values' order keys (`MinNumDyn` / `MaxNumDyn`), not the hosted numeric_smaller /
/// numeric_larger: on under `+agg.num_cte` (`Facts::road_on`, listed in `ROADS`).
pub const NUM_CTE_ROAD: &str = "agg.num_cte";

/// D4565: the record slot a plain var / stddev call folds PostgreSQL's own transition state into: int2 / int4's
/// Int128AggState {sumX, N} in an `AvgI128` slot, its sumX2 in the `SumI128` companion right after it (each merged by
/// its exact add, as `numeric_poly_combine`, so a group's records merge to the serial state whatever the eviction).
/// `None` for every other register: int8 and numeric keep their NumericAggState hosted, and float keeps its hosted
/// state too, because `float8_combine` of a group's evicted records is not `float8_accum`'s serial fold bit for bit
/// (a W0 hashed group under a small grant: m6_v427's var_pop rows), while PostgreSQL's W0 HashAgg never combines.
pub const fn stat_slot(reg: Register) -> Option<SlotKind> {
    match reg {
        Register::ExactStat => Some(SlotKind::AvgI128),
        _ => None,
    }
}

/// The record slot of an integer `bit_and / bit_or / bit_xor` (D252).
const fn bit_slot(op: crate::ops::shared::agg_class::Bit) -> SlotKind {
    match op {
        crate::ops::shared::agg_class::Bit::And => SlotKind::BitAnd,
        crate::ops::shared::agg_class::Bit::Or => SlotKind::BitOr,
        crate::ops::shared::agg_class::Bit::Xor => SlotKind::BitXor,
    }
}

const fn bit_register(op: crate::ops::shared::agg_class::Bit) -> Register {
    match op {
        crate::ops::shared::agg_class::Bit::And => Register::BitAnd,
        crate::ops::shared::agg_class::Bit::Or => Register::BitOr,
        crate::ops::shared::agg_class::Bit::Xor => Register::BitXor,
    }
}

/// D3080 (H025): the integer column a group key casts to text: `x::text` / `x::varchar` (CoerceViaIO through int2out /
/// int4out / int8out) over a column `x` of an integer type (normalize `integer_type`). Those output functions are
/// injective and read no GUC (D3043), so two rows' texts are equal exactly when their integers are: a hashed group keys
/// by the integer's word (ops/agg keys.rs `int_text_words`) and the emit renders the text once per group. `None` for any
/// other expression.
pub fn int_text_arg(e: Node<'_>) -> Option<Node<'_>> {
    let c = e.as_coerce_via_io()?;
    if c.resulttype != crate::catalog::TEXTOID && c.resulttype != crate::catalog::VARCHAROID {
        return None;
    }
    let v = c.arg.as_var()?;
    crate::normalize::integer_type(v.vartype).then_some(c.arg)
}

impl KeyDecl<'_> {
    /// D3080: a group key keyed by its integer (`int_text_arg`): typed by the integer, its expression the cast.
    pub fn int_text(&self) -> bool {
        crate::normalize::integer_type(self.tf) && int_text_arg(self.expr.node()).is_some()
    }
}
