//! The keyed face (plan §3.1; H04 H08 H12 H13 H23 H25): a keyed breaker's table and what surrounds it. It covers
//! - how the table is sized, partitioned and bypassed (H13), and when a direct-indexed array replaces hashing (H23);
//! - prefetch ahead of an insert or probe (H04);
//! - what one pass hands the next (H08);
//! - owner tops (H12);
//! - the partition-parallel emit (H25).
//!
//! Every table keys by the one KeyHash over a keyimg.rs image. The route reads the hash's top bits, and a table's home
//! reads the bits below every route bit its rows consumed (`gravity_abi::hash::Depth`), at the loads `abi/src/hash.rs` names.
//! Keyed kinds (face Keyed) are made by `frame::EndMaker` (the table's end) and `frame::SourceMaker` (its emit).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::PgVec;
use gravity_program::LifeId;

use gravity_abi::breaker::RecordLayout;
use gravity_abi::hash::Depth;
use gravity_decompose::ops::shared::agg::GroupDecl;

use crate::cell::LaneId;
use crate::dict::face::CodeRep;
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::frame::stop::Exits;
use crate::admit::refusal::Variant;

crate::strategies! {
    /// Partition table for whole-partition frames. No mechanism yet: its readers take the straight form.
    WIN_PARTTABLE = 106, "win.parttable", Window;
}

crate::hook_variants! {
    /// The keyed-table policies (H13) and the dense-domain tables (H23).
    "H13 H23" pub enum TablePolicy {
        /// H13 a: observed-eviction bypass (a radix append plus an owner phase), for every fold.
        Bypass = Strategy(TABLE_BYPASS),
        /// H13 b: sized from a bound or an observation, grown in place, zeroed lazily.
        Sized = Strategy(TABLE_SIZED),
        /// H13 c: radix partitioning at every width, W0 included.
        Radix = Strategy(TABLE_RADIX),
        /// H13 d: an inserted bit and two counters per key (set ops, recursive dedup).
        Inserted = Kind(SetopSetop),
        /// H23 a: direct-indexed from the build's observed min / max (range <= 4x rows).
        DenseObserved = Strategy(JOIN_DENSE),
        /// H23 b: dense keys from value-range facts (`x % c`, series bounds, a domain <= 2^16).
        DenseRange = Strategy(DENSE_RANGE),
        /// H23 c: a serial small-domain dense word table instead of eviction.
        DenseSmall = Strategy(TABLE_SMALL),
        /// H23: a 16-bit tag in the directory head.
        BloomTag = Strategy(JOIN_BLOOMTAG),
        /// H23 (D3194): the build's Bloom words past the directory, tested by its probe ahead of the head.
        RowFilter = Strategy(JOIN_ROWFILTER),
        /// H23 (D3323): an armed filter's body ahead of a later probe's one-row body.
        RowFilterSingle = Strategy(JOIN_ROWFILTER_SINGLE),
        /// H23 (D3323): the filter's verdict from measured costs, sampled again through the probe's life.
        RowFilterCost = Strategy(JOIN_ROWFILTER_COST),
        /// H23 (D3168): a probe past L2 runs its rows in stages over a block (hash, heads, records, walk).
        ProbeStaged = Strategy(JOIN_PROBE_STAGED),
        /// H13 / H23 c: the partition table of whole-partition window frames.
        PartTable = Strategy(WIN_PARTTABLE),
    }
}

/// A keyed table as its breaker declares it:
/// - `image_words`: the image's width;
/// - `record_bytes`: the record after the image;
/// - `expect`: the rows it expects, from a bound or an observation (`None` when unknown);
/// - `depth`: the route bits its rows consumed;
/// - `life`: the life that owns it;
/// - `layout`: the record the owner phase merges (the key image words, the slots, the sentinel; slot-less = identity);
/// - `state_off`: the table's words at `env.state + state_off` (the end's own state after them);
/// - `key`: the key facts its policy is picked by;
/// - `facts`: the first of this table's `Facts` parameter words (`keyed_table_size::declare_facts`: DenseLo, DenseHi for a
///   bound domain, then TableSize, in `Fact` order), 0 = none;
/// - `copies`: D3312: the record slots holding a key copy's address (abi `IMG_SLOTS_KNOWN` and two bits a slot: hashagg
///   `table_decl`), set on the table at each open (kernels `agg_copies`); 0 = not named, its images never reclaimed.
#[derive(Clone, Copy, Debug)]
pub struct TableDecl {
    pub image_words: u16,
    pub record_bytes: u32,
    pub expect: Option<u64>,
    pub depth: Depth,
    pub life: LifeId,
    pub layout: RecordLayout,
    pub state_off: u32,
    pub key: KeyShape,
    pub facts: u16,
    pub copies: u64,
}

/// A key's domain as the dense tables (H23) know it: unknown, constant bounds, or bound at run time (the Facts protocol's
/// DenseLo / DenseHi words); `Div`, a key `x / c` (dense.range): the bound of `x`'s lane divided by `c > 0`, exact since
/// PostgreSQL's integer division truncates toward zero, which is non-decreasing in `x`; `Shift`, a key `x + c` (`x - c`,
/// `c + x`): the bound of `x`'s lane shifted by `c`, exact since integer addition is increasing in `x` and raises where it
/// would leave its type (a bound past i64 wraps below the other: no domain).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    Unknown,
    Const { lo: i64, hi: i64 },
    Bound,
    Div { c: i64 },
    Shift { c: i64 },
}

/// dense.range's key form of `range_key`: `x % c`, `x / c` or `x + c` (`x - c` as `x + (-c)`, `c + x`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeForm {
    Rem,
    Div,
    Shift,
}

impl RangeForm {
    /// Whether the key's domain is its column's run-time bound mapped through the form (`Div`, `Shift`), not a constant
    /// one (`Rem`).
    pub const fn bound(self) -> bool {
        !matches!(self, RangeForm::Rem)
    }
}

/// dense.range's key forms (M3; `None` while the road is off, so a road-off image is M1's): `(Rem, x, c)` for `x % c`
/// over any integer `x` (PostgreSQL's remainder takes the dividend's sign: `|x % c| < c`), `(Div, x, c)` for `x / c`, `c`
/// a positive constant; `(Shift, x, c)` for any key whose value is `x + c` (`shifted`): `x + c`, `c + x`, `x - c`, chains
/// of them, an int-to-int cast of one, a date offset. The constant is a non-null int2 / int4 / int8 (each operator is
/// strict: a non-null `x` never yields NULL). KEYED-23: `x` of a bound form is read through int-to-int casts (keysem
/// `dependent_form`: a cast keeps the value or raises), so `x::int8 / 10` is `Div` over `x`. The key is still computed per
/// row as before (an `x` that raises, raises): the form only picks the table, and a row outside the domain (a stale
/// bound, a date offset of an infinite date, which PostgreSQL leaves infinite) takes the table's overflow, never a wrong
/// row.
pub fn range_key<'a>(cat: &dyn gravity_expr::Catalog<'_>, e: types_nodes::Node<'a>) -> Option<(RangeForm, types_nodes::Node<'a>, i64)> {
    use gravity_fn::typesem::integral::int_divmod;
    if !TablePolicy::DenseRange.on() {
        return None;
    }
    if let Some(o) = e.as_op_expr().filter(|o| o.args.len() == 2) {
        let f = if o.opfuncid != 0 { o.opfuncid } else { cat.opcode(o.opno).ok()? };
        if let Some(dm) = int_divmod(f) {
            let (x, y) = (o.args.nth(0), o.args.nth(1));
            let c = int_const(y).filter(|&c| c > 0)?;
            if !dm.div {
                return Some((RangeForm::Rem, x, c));
            }
            let (x, 0) = shifted(cat, x)? else { return None };
            return Some((RangeForm::Div, x, c));
        }
    }
    // a bare column is the table's `Bound` domain, not a shift
    let (x, c) = shifted(cat, e).filter(|_| e.as_var().is_none())?;
    Some((RangeForm::Shift, x, c))
}

/// A non-null by-value int2 / int4 / int8 constant's value.
fn int_const(n: types_nodes::Node<'_>) -> Option<i64> {
    let c = n.as_const().filter(|c| !c.constisnull && c.constbyval)?;
    let d = gravity_decompose::bake::read(c);
    match c.constlen {
        2 => Some(d.as_i16() as i64),
        4 => Some(d.as_i32() as i64),
        8 => Some(d.as_i64()),
        _ => None,
    }
}

/// KEYED-23: `e` as `x + c` of one column `x` (a Var), through the order images keysem names (`dependent_form`): an
/// int-to-int cast (the value kept, or a raise), an integer `+` / `-` with a constant (typesem `int_arith`: exact, or a
/// raise), a date `+` / `-` an int4 constant (days: a date's word is its day number); relabels are read through. `None`
/// for anything else (a product, a negation: the image is not a shift) or a shift past i64.
fn shifted<'a>(cat: &dyn gravity_expr::Catalog<'_>, e: types_nodes::Node<'a>) -> Option<(types_nodes::Node<'a>, i64)> {
    crate::stack_check();
    use gravity_fn::typesem::integral::{int_arith, IntOp};
    use gravity_fn::typesem::keysem::image::{dependent_form, Form};
    if let Some(r) = e.as_relabel_type() {
        return shifted(cat, r.arg);
    }
    if e.as_var().is_some() {
        return Some((e, 0));
    }
    if let Some(f) = e.as_func_expr().filter(|f| f.args.len() == 1) {
        return matches!(dependent_form(f.funcid), Some(Form::IntCast { .. })).then(|| shifted(cat, f.args.nth(0)))?;
    }
    let o = e.as_op_expr().filter(|o| o.args.len() == 2)?;
    let f = if o.opfuncid != 0 { o.opfuncid } else { cat.opcode(o.opno).ok()? };
    let (x, y) = (o.args.nth(0), o.args.nth(1));
    let (inner, c) = match (int_arith(f).map(|a| a.op), dependent_form(f)) {
        (Some(IntOp::Add), _) => match (int_const(y), int_const(x)) {
            (Some(c), _) => (x, c),
            (None, Some(c)) => (y, c),
            (None, None) => return None,
        },
        (Some(IntOp::Sub), _) => (x, int_const(y)?.checked_neg()?),
        (None, Some(Form::DateOffset)) => {
            let c = int_const(y)?;
            (x, if date_minus(f) { c.checked_neg()? } else { c })
        }
        _ => return None,
    };
    let (x, d) = shifted(cat, inner)?;
    Some((x, d.checked_add(c)?))
}

/// Date offset function `f` (keysem `DateOffset`) subtracts its days (`date_mii`), else adds them (`date_pli`).
fn date_minus(f: u32) -> bool {
    gravity_fn::row_of(f).is_some_and(|r| r.name == "date_mii")
}

/// The column a dense.range key narrows to one image word (keyed_emit_group `key_nullable`): `x / c` or `x + c` (its span
/// is the run time's), or `x % c` whose constant span could fit a full T1 of the smallest record (16 bytes); a wider `x % c` keeps
/// the flags word, as with the road off (it opens hashed either way, and a one-word image hashes worse there: twin cells).
pub fn range_x<'a>(cat: &dyn gravity_expr::Catalog<'_>, e: types_nodes::Node<'a>) -> Option<types_nodes::Node<'a>> {
    let (form, x, c) = range_key(cat, e)?;
    (form.bound() || (2 * c as i128 - 1) < (gravity_abi::breaker::t1_default_bytes() / 16) as i128).then_some(x)
}

/// A record's key domain (keyed_emit_group `key_shape`): a single one-word key `x % c` is `Const [1 - c, c - 1]` (bounds in
/// i128, `Unknown` past i64), `x / c` over a bound lane `Div`, `x + c` over one `Shift`, a bound lane's own column `Bound`,
/// else `Unknown`.
pub fn key_domain(cat: &dyn gravity_expr::Catalog<'_>, keys: &[gravity_decompose::ops::shared::agg::KeyDecl<'_>], nkeys: usize, bound: bool) -> Domain {
    let range = match (keys, nkeys) {
        ([k], 1) => range_key(cat, k.expr.node()),
        _ => None,
    };
    match (range, bound) {
        (Some((RangeForm::Rem, _, c)), _) => match (i64::try_from(1 - c as i128), i64::try_from(c as i128 - 1)) {
            (Ok(lo), Ok(hi)) => Domain::Const { lo, hi },
            _ => Domain::Unknown,
        },
        (Some((RangeForm::Div, _, c)), true) => Domain::Div { c },
        (Some((RangeForm::Shift, _, c)), true) => Domain::Shift { c },
        (_, true) => Domain::Bound,
        _ => Domain::Unknown,
    }
}

/// The key facts a table picks its policy by: a single code key `(lane, site)` for the code front (code.front_part), the
/// dense front's lanes (code.front_dense), the domain for the dense table.
#[derive(Clone, Copy, Debug)]
pub struct KeyShape {
    pub code: Option<(LaneId, i16)>,
    pub dense: Option<DenseFront>,
    pub domain: Domain,
}

/// The keys code.front_dense indexes by (KEYED's table_decl sets it for these shapes only): `n` >= 2 code keys (record
/// `c0 + n0 * (c1 + n1 * ..)` over the window's dictionary sizes, program cost/dict_front.rs; each key's code and lane
/// are the image's `FrontIndex::Codes`), or one IMMUTABLE / STABLE key derived from one code lane (`n` 1, `derived`),
/// indexed by the source code, its record keyed by the row's image. `groups`: the planner's group estimate, which
/// `dict_front::takes` weighs against the front's capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DenseFront {
    pub n: u8,
    pub derived: bool,
    pub groups: u64,
}

/// A table's emitted operations under one policy (registered as `TableImpl` by the policy roads). `h` is the image's
/// KeyHash word (`emit_key_hash`). A hit compares the whole image, never a tag.
pub trait TableMech: Sync {
    /// H17 a: the table handle's per-open words.
    fn open_words(&self, _t: &TableDecl, _w: &mut OpenWords) {}
    /// The record of `img`, inserted zeroed when absent: `(record, fresh)`.
    fn upsert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> (Value, Value);
    /// `upsert` whose appended record carries the implicit count `1 + n` already written (V4R-201, count-at-append)
    /// (`counted` true there: the caller skips its bump); the default, and `n` None, count nothing.
    fn upsert_n(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value, _n: Option<Value>) -> (Value, Value, Value) {
        let (r, f) = self.upsert(b, env, t, img, h);
        (r, f, b.bconst(false))
    }
    /// D3224 A4: a front's probe of the record it holds open, over an image of codes alone (keyimg.rs `codes`, no image
    /// words): `Some((l_hot, l_cold))`, the builder left in no block, where `l_hot(record, window)` takes the open record
    /// (its key image written when it opened; `window`: `Env::rec_window`'s, false for code.front_part's run cache, a
    /// table record) and `l_cold` every other row (a fresh record, a code past the front), where the caller images the
    /// row and calls `upsert`. `None` (the default): no probe, the caller images every row.
    fn probe(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _t: &TableDecl, _codes: &ImageWords) -> Option<(gravity_ir::Block, gravity_ir::Block)> {
        None
    }
    /// The record of `img`, or a null pointer.
    fn find(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> Value;
    fn state_bytes(&self, _t: &TableDecl) -> u32 {
        0
    }
    /// At the claim's open: the table laid out or resumed (sized / radix facts), a front bound.
    fn open(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _t: &TableDecl) {}
    /// At a window's end: a part-bound front flushes when the next window changes part.
    fn window_end(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _t: &TableDecl) {}
    /// The producer's end (section end / serial finish): the table drained into its partitions.
    fn finish(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _t: &TableDecl, _x: &Exits) {}
    /// Whether this policy takes `t` by its key facts (one code key, a small constant domain, a slot-less record); the
    /// hashed table takes every table.
    fn takes(&self, _t: &TableDecl) -> bool {
        true
    }
    /// D2916: the hashed table (`table.bypass`) itself, by type. Every policy's static is zero-sized, and a zero-sized
    /// static has no address of its own (code.front_dense's sat at the hashed table's in one link): never `addr_eq`.
    fn hashed(&self) -> bool {
        false
    }
}

pub struct TableImpl(pub &'static dyn TableMech);

/// Table `t`'s mechanism (plan §3.1, H13 / H23): the first road on whose mechanism takes `t`, in order: the code fronts
/// (`code.front_dense`, `code.front_part`) for a one-code key, the serial small-domain word table (`table.small`), the
/// dense table over value-range facts (`dense.range`, M3; M3 L0-e), else the hashed table (`table.bypass`, a slot-less
/// DISTINCT record's too; `table.sized` / `table.radix` shape its open). Admission refused every applicable unbuilt road;
/// a road switched off (twin A/Bs) is passed over.
pub fn table(cx: &impl crate::admit::refusal::Site, t: &TableDecl) -> &'static dyn TableMech {
    let order = [CodeRep::FrontDense.variant(), CodeRep::FrontPart.variant(), TablePolicy::DenseSmall.variant(), TablePolicy::DenseRange.variant(), TablePolicy::Bypass.variant()];
    for v in order {
        if let Variant::Strategy(r) = v {
            if crate::face::source::on(v) {
                let m = crate::frame::iface::need_strategy::<TableImpl>(cx, r).0;
                if m.takes(t) {
                    return m;
                }
            }
        }
    }
    crate::not_built!(cx, TablePolicy::Bypass.variant())
}

/// code.front_light (H23, V4R-184; the ladder's rung-3 text front): a group of one code key whose every slot is a row count
/// (`count(*)`, `count(key)`) or the key's representative counts each row as `cnt[code] += 1` over the part the claim reads,
/// with no key image and no hash, and converts the part's counts into its hashed table once per part. KEYED asks
/// `count_front` for a table it lays over the hashed one; registered as `CountFrontImpl` under `CODE_FRONT_LIGHT`.
pub trait CountFrontMech: Sync {
    /// The front's shape for table `t` of group `g` keyed by `keys` (every group key, in key order), or `None` (a slot that
    /// is not a row count or a key's representative, a record too wide).
    fn plan<'g>(&self, t: &TableDecl, g: &GroupDecl<'_>, keys: &[CountKey], mcx: mcx::Mcx<'g>) -> Option<CountShape<'g>>;
    /// The front's words, after the table's in the end's state (KEYED passes their address `w`, and the table's `st`).
    fn state_bytes(&self, c: &CountShape) -> u32;
    /// At the claim's open: no part bound.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: Value, c: &CountShape);
    /// Per window, before its rows (`EndMech::whole`): a window of another part binds it.
    fn bind(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value, w: Value, c: &CountShape);
    /// The row's key cells `keys` (their codes; a NULL row's is all ones) counted when the bound part's front takes them,
    /// then the returned block; the builder is left in the table's arm, which the caller ends there. `words` derives the
    /// front's words pointer from the end's state, so a block past the list's compare derives its own and keeps nothing
    /// of the row's live across it (D3159: a value live past that compare is spilled on every row of the filter's
    /// bit-scan loop). `at`: the row's index in its window (D3159: it picks the count's way; only a shape whose `ways` is
    /// set tests the ways).
    fn row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, words: &dyn Fn(&mut FunctionBuilder<'_>, &Env) -> Value, c: &CountShape, keys: &[crate::rep::CellRef<'_>], at: Value) -> gravity_ir::Block;
    /// At the claim's end, before the table's: the bound part's counts converted.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value, w: Value, c: &CountShape, x: &Exits);
}

pub struct CountFrontImpl(pub &'static dyn CountFrontMech);

/// The code key KEYED hands `CountFrontMech::plan`: its column in the producer's row, its code lane and that lane's Fp128
/// table.
#[derive(Clone, Copy, Debug)]
pub struct CountKey {
    pub col: i16,
    pub lane: u8,
    pub fp_table: u8,
}

/// A count front's shape: the count slot, the image's words and each key's (in key order).
#[derive(Clone, Debug)]
pub struct CountShape<'g> {
    pub count: u8,
    pub nkeys: u8,
    pub keys: PgVec<'g, CountKeyShape>,
    /// Two or more keys: the byte offset of the frame word that keeps the front's decline across the statement's claims
    /// (KEYED's end takes it; `None` from `plan` and for one key).
    pub declined: Option<i32>,
    /// D3159: one key whose planner group estimate fits the ways' codes (`LIGHT_WAYS_MAX_CODES`): its row loop tests the
    /// ways on the rows the list misses and its bind may count a part in ways. A key estimated past it keeps trunk's row
    /// loop (no test: the rows the list misses go straight to the hashed table) and never counts in ways.
    pub ways: bool,
}

/// A count front key's code lane, its Fp128 table and its representative slot.
#[derive(Clone, Copy, Debug)]
pub struct CountKeyShape {
    pub lane: u8,
    pub fp_table: u8,
    pub rep: Option<u8>,
}

/// code.front_light's mechanism while the road is on (built, not dropped, not switched off).
pub fn count_front(cx: &impl crate::admit::refusal::Site) -> Option<&'static dyn CountFrontMech> {
    CodeRep::FrontLight.on().then(|| crate::frame::iface::need_strategy::<CountFrontImpl>(cx, crate::face::strategy::CODE_FRONT_LIGHT).0)
}

/// A keyed end's accumulators as a run carry drives them (M1-KEYED implements it).
pub trait RunFold {
    fn init<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value>;
    fn step<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, accs: &[Value]) -> PgVec<'e, Value>;
    fn close(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords, h: Value, accs: &[Value]);
    fn row_into(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, img: &ImageWords, h: Value);
    /// D3326: the run carry tests each run's key once, at its open (`admit`), and steps only an admitted run's rows (a
    /// two-pass form's pass 2, join.domain); `close` then folds the run with no test of its own.
    fn admits_runs(&self, _env: &Env) -> bool {
        false
    }
    /// Under `admits_runs`: whether the run of image `img` is admitted (`Bool`).
    fn admit(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _img: &ImageWords) -> Value {
        b.bconst(true)
    }
}

/// The open run a keyed end carries across rows (H06 / H22 run.collapse, H21 hash.runopen), registered as `RunCarryImpl`
/// under `RUN_COLLAPSE`. The carry is the run's words (image words, hash, open flag), then `fold.init()`'s.
pub trait RunCarry: Sync {
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, t: &TableDecl, fold: &dyn RunFold) -> PgVec<'e, Value>;
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, t: &TableDecl, win: &Win, row: &RowVals, img: &ImageWords, carry: &[Value], fold: &dyn RunFold) -> PgVec<'e, Value>;
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, carry: &[Value], fold: &dyn RunFold);
}

pub struct RunCarryImpl(pub &'static dyn RunCarry);

crate::hook_variants! {
    /// In-memory prefetch K rows ahead of the probe (H04), gated on the table's runtime size exceeding L2.
    "H04" pub enum Prefetch {
        /// Home slot / probe bucket prefetch.
        Table = Strategy(PREFETCH_TABLE),
        /// Fingerprint-entry prefetch when a text-key run closes.
        Fp = Strategy(PREFETCH_FP),
    }
}

crate::strategies! {
    /// A finished build's key set cuts later claims. No mechanism yet: its readers take the straight form.
    SEMIJOIN_MARKS = 90, "semijoin.marks", SemiJoin;
    /// Second pass probes one owned-result view. No mechanism yet: its readers take the straight form.
    WIN_OWNEDVIEW = 107, "win.ownedview", Window;
}

crate::hook_variants! {
    /// Cross-pass products (H08): what one pass hands a later one.
    "H08" pub enum CrossPass {
        /// The admitted key set: HAVING over a keyed fold (pass 1 counts, pass 2 folds the admitted groups).
        Admitted = Strategy(TWOPASS_HAVING),
        /// A count top's admitted key set: pass 1 counts, pass 2 folds the groups ranking with its n-th row (D-06c).
        CountTop = Strategy(TWOPASS_COUNT),
        /// One owned-result view over a breaker's records that every worker of a later pipeline probes. It is sized from the
        /// memo's group count, and refused at admission when it cannot fit.
        OwnedView = Strategy(WIN_OWNEDVIEW),
        /// A finished build's key set cuts later claims.
        SemiMarks = Strategy(SEMIJOIN_MARKS),
        /// A finished hash build's key set is the admitted set of the keyed fold its probe reads (D3148): the fold folds only
        /// the rows whose group key the build holds (ops/hashjoin/strategies/domain.rs).
        Domain = Strategy(JOIN_DOMAIN),
        /// A grouping set's group folded from a finer set's owned records, not from the input (D4620,
        /// ops/agg/cascade.rs).
        Cascade = Strategy(GSETS_CASCADE),
    }
}

crate::strategies! {
    /// Count over a set op from the table. No mechanism yet: its readers take the straight form.
    SETOP_COUNT = 104, "setop.count", Setop;
}

crate::hook_variants! {
    /// Owner tops (H12): count-ordered and final-ordered, with a threshold gate, lazy render and a tie fingerprint.
    "H12" pub enum OwnerTop {
        /// Count-ordered.
        Count = Strategy(OWNER_COUNT),
        /// Final-ordered: a top-k over group finals.
        Final = Kind(RunsTopkGroups),
        /// A one-round plain distinct total.
        DistinctTotal = Strategy(DISTINCT_TOTAL),
        /// A count over a set op, taken from its table.
        SetopCount = Strategy(SETOP_COUNT),
    }
}

/// H12 a (owner.count, M3 `runs/owner_count.rs`; M3 L0-d): the count-ordered owner top. When a group's top orders first by a
/// count final (`applies::CountTopk`) with a constant bound `n` (count + offset), the group's hash end declares this owner
/// descriptor in place of the generic one, and the emit gathers each owner's kept records (host_abi `FORM_COUNT_TOPK`) in
/// place of every record before `agg_sort`.
pub trait OwnerTopMech: Sync {
    /// The owner descriptor of pipeline `k`'s group `g`, whose table is `t`, under a count-ordered top of `n` rows; declared
    /// at the hash end's build, so the mechanism may claim words (V4R-113).
    fn owner<'p>(&self, px: &mut Px<'_, 'p>, k: usize, g: &GroupDecl<'p>, t: &TableDecl, n: u32) -> gravity_program::OwnerDesc;
    /// Whether the emit reads the owners' kept records (true) or gathers every record (false).
    fn kept(&self) -> bool {
        true
    }
}

pub struct OwnerTopImpl(pub &'static dyn OwnerTopMech);

leader_tls::leader_tls! {
    thread_local! {
        static OWNER_STUB: core::cell::Cell<Option<&'static dyn OwnerTopMech>> = const { core::cell::Cell::new(None) };
    }
}

/// The count-ordered owner top's mechanism: road owner.count's when it is on (built, not dropped, not switched off), else
/// `None` (M1: owner.count is not built; the group sorts every record).
pub fn owner_top(cx: &impl crate::admit::refusal::Site) -> Option<&'static dyn OwnerTopMech> {
    OWNER_STUB.with(|s| s.get()).or_else(|| OwnerTop::Count.on().then(|| OwnerTop::Count.need::<OwnerTopImpl>(cx).0))
}

/// The L0-d rig only: `m` stands in for road owner.count's registered mechanism on this thread (`None` clears it). The
/// registry is static, so a test reaches the seam through this.
#[doc(hidden)]
pub fn stub_owner_top(m: Option<&'static dyn OwnerTopMech>) {
    OWNER_STUB.with(|s| s.set(m));
}

crate::strategies! {
    /// Partition-parallel drain, occupancy words, counters. No mechanism yet: its readers take the straight form. D3024
    /// (b): switched off (`gravity_road_off`), a hash join build drain keeps to the leader (drain/source.rs `Drain::wide`).
    DRAIN_PARTS = 88, "drain.parts", Drain;
    /// Right-family marks test before they set. No mechanism yet: its readers take the straight form.
    MARKS_TESTSET = 89, "marks.testset", RightJoin;
}

crate::hook_variants! {
    /// Partition-parallel breaker emits (H25). Every worker walks the merged table by slice, skips empty slots by occupancy
    /// word and answers counts from counters.
    "H25 -" pub enum PartEmit {
        /// A build's unmatched-row drain.
        Drain = Strategy(DRAIN_PARTS),
        /// A hash aggregate's group emit.
        Groups = Kind(KeyedEmitGroup),
        /// A set op's emit.
        Setop = Kind(SetopSetop),
        /// Right-family marks test before they set.
        MarksTest = Strategy(MARKS_TESTSET),
        /// A group emit into a draining probe's seeded end (D3024 (b) rest, emit_group.rs `Emit::marks`): each claim's
        /// probe marks the build and hands its rows to the drain's end.
        Marks = Strategy(EMIT_MARKS),
    }
}

#[cfg(test)]
mod keyed23_tests {
    use datum::Datum;
    use mcx::{Mcx, MemoryContext};
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{FuncExpr, OpExpr, Var};
    use types_nodes::Node;

    use super::{range_key, RangeForm};

    fn m() -> Mcx<'static> {
        Box::leak(Box::new(MemoryContext::new("keyed23"))).mcx() // alloc: Cold
    }
    fn col(m: Mcx<'static>, ty: u32) -> Node<'static> {
        Node::mk(m, Var { varno: 1, varattno: 2, vartype: ty, vartypmod: -1, ..Default::default() }).unwrap()
    }
    fn k(m: Mcx<'static>, ty: u32, len: i32, v: i64) -> Node<'static> {
        Node::mk_const(m, ty, -1, 0, len, Datum::from_i64(v), false, true).unwrap()
    }
    fn op(m: Mcx<'static>, f: u32, ty: u32, l: Node<'static>, r: Node<'static>) -> Node<'static> {
        Node::mk(m, OpExpr { opno: 1, opfuncid: f, opresulttype: ty, args: NodeList::from_slice(m, &[l, r]).unwrap(), ..Default::default() }).unwrap()
    }
    fn cast(m: Mcx<'static>, f: u32, ty: u32, x: Node<'static>) -> Node<'static> {
        Node::mk(m, FuncExpr { funcid: f, funcresulttype: ty, args: NodeList::from_slice(m, &[x]).unwrap(), ..Default::default() }).unwrap()
    }
    fn form(e: Node<'static>) -> Option<(RangeForm, bool, i64)> {
        range_key(&gravity_expr::catalog::PgCatalog, e).map(|(f, x, c)| (f, x.as_var().is_some(), c))
    }

    /// KEYED-23: a key whose value is `x + c` of one column through the order images keysem names is a Shift over `x`:
    /// a widening or narrowing int cast (`x::int8`, 481 int48; 314 i4toi2), chains of integer offsets (`(x + 3) - 1`), a
    /// date offset (1141 date_pli, 1142 date_mii); `x::int8 / 10` divides `x`'s bound. Never: a bare column (the Bound
    /// domain), a product (177 int4pl kept, 141 int4mul not), a shifted dividend (`(x + 1) / 10`), a non-constant offset.
    #[test]
    fn order_images_of_one_column_shift_it() {
        let m = m();
        let (x, d) = (col(m, 23), col(m, 1082));
        assert_eq!(form(cast(m, 481, 20, x)), Some((RangeForm::Shift, true, 0)), "x::int8");
        assert_eq!(form(cast(m, 314, 21, x)), Some((RangeForm::Shift, true, 0)), "x::int2");
        assert_eq!(form(op(m, 181, 23, op(m, 177, 23, x, k(m, 23, 4, 3)), k(m, 23, 4, 1))), Some((RangeForm::Shift, true, 2)), "(x + 3) - 1");
        assert_eq!(form(op(m, 463, 20, cast(m, 481, 20, x), k(m, 20, 8, -5))), Some((RangeForm::Shift, true, -5)), "x::int8 + -5");
        assert_eq!(form(op(m, 1141, 1082, d, k(m, 23, 4, 7))), Some((RangeForm::Shift, true, 7)), "d + 7");
        assert_eq!(form(op(m, 1142, 1082, d, k(m, 23, 4, 7))), Some((RangeForm::Shift, true, -7)), "d - 7");
        assert_eq!(form(op(m, 466, 20, cast(m, 481, 20, x), k(m, 20, 8, 10))), Some((RangeForm::Div, true, 10)), "x::int8 / 10");
        assert_eq!(form(x), None, "a bare column is the Bound domain");
        assert_eq!(form(op(m, 141, 23, x, k(m, 23, 4, 3))), None, "x * 3");
        assert_eq!(form(op(m, 154, 23, op(m, 177, 23, x, k(m, 23, 4, 1)), k(m, 23, 4, 10))), None, "(x + 1) / 10");
        assert_eq!(form(op(m, 177, 23, x, x)), None, "x + x");
    }
}
