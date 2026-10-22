//! The accessors (v3 design §3.2, kept with changes): a cell's value is read, keyed, ordered and retained only through its
//! Rep's arm. `rep_dispatch!` / `arm_kind` hold the only `match` on `Rep`: one arm per Rep × accessor (datum / hash / order),
//! each a kind of `face/kinds.rs` (`rep.word.datum` …) whose file a milestone lands and registers (`registry/m<k>.rs`). An
//! arm no appended table provides refuses `NotBuilt { hook: "rep", variant, milestone }` beside the code it protects.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::sort::KeyImage;
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::{ConvOp, IntBinOp, Type, Value};
use gravity_program::{Rep, Until};

use crate::cell::At;
use crate::face::kinds::KindId;
use crate::admit::refusal::Site;

pub use crate::lx::*;
pub mod code;
pub mod cstr;
pub(crate) mod expr_type;
pub mod fixed;
pub mod hosted;
pub(crate) mod keylane;
pub mod packednum;
pub(crate) mod storage;
pub(crate) mod varkey;
pub mod varlena;
pub mod word;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Accessor {
    /// `datum` (the value word + null flag), `retain` (the only way a value outlives its row), the H01 block form.
    Datum,
    /// The key / bucket image for hash tables and equality against a stored image.
    Hash,
    /// The order image (DESC / NULLS folded by the caller's key facts).
    Order,
}

impl Accessor {
    pub const ALL: [Accessor; 3] = [Accessor::Datum, Accessor::Hash, Accessor::Order];

    pub const fn name(self) -> &'static str {
        match self {
            Accessor::Datum => "datum",
            Accessor::Hash => "hash",
            Accessor::Order => "order",
        }
    }
}

/// An arm's datum accessors (`rep/<rep>/datum.rs` registers one for its `rep.<rep>.datum` kind).
pub struct DatumArm {
    pub datum: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts) -> (Value, Value),
    pub retain: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts, Until, RetainHome) -> Kept,
    /// The lane's H01 block form; `None` = the Rep has none (a mask road's predicate does not hold over it).
    pub block: Option<fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts, BlockRef) -> BlockLane>,
}

/// An arm's hash accessors (`rep/<rep>/hash.rs`).
pub struct HashArm {
    pub image: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts, Scope) -> Words,
    pub eq: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, StoredRef, &TypeFacts) -> Value,
}

/// An arm's order accessor (`rep/<rep>/order.rs`).
pub struct OrderArm {
    pub image: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts, KeyColl) -> Words,
    /// O3 (M3 decision 16): a `name` key's 64-byte order image as eight words under a collation that orders by bytes
    /// (rep.word.order's only; `None` for every other arm).
    pub name: Option<fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts, KeyColl) -> [Value; 8]>,
}

/// O3: the eight-word order image of a `name` key under a collation that orders by bytes (C / POSIX), through
/// rep.word.order: each word signed-comparable, compared in order (DESC: each word's bitwise NOT). `None` for any other key,
/// or while rep.word.order files no name image: the caller takes its arm's order image, which refuses by name.
pub fn name_order(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Option<[Value; 8]> {
    let f = (tf.typid == types_core::catalog::NAMEOID && coll.bytes_order).then(|| arm::<OrderArm>(&*cx, KindId::RepWordOrder).name).flatten()?;
    Some(f(cx, c, tf, coll))
}

/// The one `match` on `Rep`: the kind that serves `acc` over a cell of `rep` and type `tf`. A by-reference type with no
/// native image (`WordImage::Canonical`: arrays, records, ranges, user types) keys and orders through the hosted arms.
pub const fn arm_kind(rep: Rep, acc: Accessor, tf: &TypeFacts) -> KindId {
    use Accessor::{Datum, Hash, Order};
    let native_varlena = tf.image.is_bytes() || tf.image.is_num_form();
    let native_fixed = tf.image.fixed_norm().is_some();
    match (rep, acc) {
        (_, Datum) => datum_kind(rep),
        (Rep::Word { .. }, Hash) => KindId::RepWordHash,
        // an enum orders by pg_enum.enumsortorder (C enum_cmp), never by its oid word (V4R-84): the hosted comparator over a
        // word, which has no var copy for the run's tail (rep.hosted.order.fixed)
        (Rep::Word { .. }, Order) if tf.image.is_enum() => KindId::RepHostedOrderFixed,
        (Rep::Word { .. }, Order) => KindId::RepWordOrder,
        (Rep::Code { .. }, Hash) => KindId::RepCodeHash,
        (Rep::Code { .. }, Order) => KindId::RepCodeOrder,
        (Rep::PackedNum { .. }, Hash) => KindId::RepPackedHash,
        // D2426: a packed lane orders by its mantissa word only where every mantissa fits one (p <= 18, NumTypmod::narrow);
        // a wider column's verbatim value has no such word, so it orders as an unpacked numeric does (hosted, by its datum)
        (Rep::PackedNum { facts, .. }, Order) if facts.narrow() => KindId::RepPackedOrder,
        (Rep::PackedNum { .. }, Order) => KindId::RepHostedOrder,
        // N14: a dynamic-scale packed lane has no word that keys or orders it alone (its scale is the window's); decompose
        // gives it no such reader, and the hosted arms would read its image (rep.packed.datum)
        (Rep::PackedNumDyn, Hash) => KindId::RepHostedHash,
        (Rep::PackedNumDyn, Order) => KindId::RepHostedOrder,
        // hj-0310: a key with a user key (`TypeFacts::user`: a hashed group's under a user `=`) is its class's image, rep.hosted.hash's `user_words`
        (Rep::VarlenaPtr | Rep::CstrPtr | Rep::FixedPtr { .. }, Hash) if tf.user != 0 => KindId::RepHostedHash,
        (Rep::VarlenaPtr, Hash) if native_varlena => KindId::RepVarlenaHash,
        // a numeric, inet or cidr does not order by its bytes: the hosted comparator's (D771, `bytes_order`)
        (Rep::VarlenaPtr, Order) if bytes_order(tf) => KindId::RepVarlenaOrder,
        (Rep::FixedPtr { .. }, Hash) if native_fixed => KindId::RepFixedHash,
        (Rep::FixedPtr { .. }, Order) if native_fixed => KindId::RepFixedOrder,
        (Rep::VarlenaPtr | Rep::CstrPtr | Rep::FixedPtr { .. }, Hash) => KindId::RepHostedHash,
        // a fixed by-reference key with no native image (macaddr, point, ...) has no var copy either (rep.hosted.order.fixed)
        (Rep::FixedPtr { .. }, Order) => KindId::RepHostedOrderFixed,
        (Rep::VarlenaPtr | Rep::CstrPtr, Order) => KindId::RepHostedOrder,
    }
}

/// Whether a by-reference key orders by its payload bytes (rep.varlena.order's images): KeySem's answer for the type row's
/// image and default btree comparator (`keysem::orders_by_bytes`: a bytes image whose comparator is a bytes compare), so
/// inet / cidr, whose network_cmp compares family, masklen and bits, take rep.hosted.order's (D387's class; props and the
/// arm read this one rule). A domain orders as its base type (D2347: its row carries the base's comparator).
pub const fn bytes_order(tf: &TypeFacts) -> bool {
    gravity_fn::typesem::keysem::orders_by_bytes(tf.image.is_bytes(), tf.cmp)
}

/// The datum arm a cell of `rep` is read through (a cstring's datum is its pointer, as a varlena's).
pub const fn datum_kind(rep: Rep) -> KindId {
    match rep {
        Rep::Word { .. } => KindId::RepWordDatum,
        Rep::Code { .. } => KindId::RepCodeDatum,
        Rep::PackedNum { .. } | Rep::PackedNumDyn => KindId::RepPackedDatum,
        Rep::FixedPtr { .. } => KindId::RepFixedDatum,
        Rep::VarlenaPtr | Rep::CstrPtr => KindId::RepVarlenaDatum,
    }
}

/// The arm a key of `kind` / `image` keys or orders through, as admission sees it before any cell exists: a code key is
/// the code arm, a numeric image the packed arm over packed lanes (else the varlena one), a text / bytes fingerprint the
/// varlena arm, uuid / interval / timetz the fixed arm, any other fingerprint (or none) the hosted arm, a word the word arm.
pub const fn key_kind(kind: KeyKind, image: Option<KeyImage>, packed: bool, acc: Accessor) -> KindId {
    // the family is the image's (decompose keysem `arm_family`: D2426 / D771 send a wide packed or unpacked numeric's
    // order to the hosted arm)
    let family = gravity_decompose::catalog::keysem::image::arm_family(kind, image, packed, matches!(acc, Accessor::Order));
    ARMS[family as usize][acc as usize]
}

/// Datum / hash / order kinds per arm family: word, code, packed, varlena, fixed, hosted (a hosted datum is the varlena's).
const ARMS: [[KindId; 3]; 6] = [
    [KindId::RepWordDatum, KindId::RepWordHash, KindId::RepWordOrder],
    [KindId::RepCodeDatum, KindId::RepCodeHash, KindId::RepCodeOrder],
    [KindId::RepPackedDatum, KindId::RepPackedHash, KindId::RepPackedOrder],
    [KindId::RepVarlenaDatum, KindId::RepVarlenaHash, KindId::RepVarlenaOrder],
    [KindId::RepFixedDatum, KindId::RepFixedHash, KindId::RepFixedOrder],
    [KindId::RepVarlenaDatum, KindId::RepHostedHash, KindId::RepHostedOrder],
];

/// The registered arm of kind `k`, or `NotBuilt` for it (a registered value of the wrong type is a Bug).
pub fn arm<T: 'static>(cx: &impl Site, k: KindId) -> &'static T {
    crate::frame::iface::rep_arm::<T>(cx, k)
}

/// `rep_dispatch!(cx, rep, tf, datum.datum(args…))`: the arm `arm_kind` names for (rep, accessor), called; resolved while
/// lowering, never per row.
#[macro_export]
macro_rules! rep_dispatch {
    ($cx:expr, $rep:expr, $tf:expr, datum.$m:ident($($a:expr),*)) => {
        ($crate::rep::arm::<$crate::rep::DatumArm>(&$cx, $crate::rep::arm_kind($rep, $crate::rep::Accessor::Datum, $tf)).$m)($($a),*)
    };
    ($cx:expr, $rep:expr, $tf:expr, hash.$m:ident($($a:expr),*)) => {
        ($crate::rep::arm::<$crate::rep::HashArm>(&$cx, $crate::rep::arm_kind($rep, $crate::rep::Accessor::Hash, $tf)).$m)($($a),*)
    };
    ($cx:expr, $rep:expr, $tf:expr, order.$m:ident($($a:expr),*)) => {
        ($crate::rep::arm::<$crate::rep::OrderArm>(&$cx, $crate::rep::arm_kind($rep, $crate::rep::Accessor::Order, $tf)).$m)($($a),*)
    };
}

/// `datum`, memoised per (row, cell): the second read of a cell in a row reuses the first's values.
pub fn datum(cx: &mut Lx, c: CellRef<'_>, tf: &TypeFacts) -> (Value, Value) {
    if let Some(v) = cx.memo.get(c) {
        return v;
    }
    let f = arm::<DatumArm>(cx, arm_kind(c.cell.rep, Accessor::Datum, tf)).datum;
    let v = f(cx, c, tf);
    cx.memo.put(c, v);
    v
}

/// The only way a value outlives its row: nothing is emitted when the cell's life already reaches `until`. A copying arm
/// takes its bytes from `cx.space`.
pub fn retain(cx: &mut Lx, c: CellRef<'_>, tf: &TypeFacts, until: Until, home: RetainHome) -> Kept {
    let reaches = match home {
        RetainHome::Record(l) | RetainHome::Run(l) | RetainHome::Spool(l) => c.cell.life.reaches_into(until, l),
        RetainHome::Frame => c.cell.life.reaches(until),
    };
    if reaches {
        return Kept { cell: *c.cell, val: c.val, null: c.null };
    }
    let f = arm::<DatumArm>(cx, arm_kind(c.cell.rep, Accessor::Datum, tf)).retain;
    f(cx, c, tf, until, home)
}

/// D2854: `retain` that copies whatever the cell's life. Every arm of a build's Arms run keeps a by-reference payload
/// column as an image in the build's region (ops/hashjoin/build.rs), so no arm keeps one by pointer where another copies.
pub fn retain_copy(cx: &mut Lx, c: CellRef<'_>, tf: &TypeFacts, until: Until, home: RetainHome) -> Kept {
    let f = arm::<DatumArm>(cx, arm_kind(c.cell.rep, Accessor::Datum, tf)).retain;
    f(cx, c, tf, until, home)
}

/// The lane's block form, if its Rep has one.
pub fn block(cx: &mut Lx, c: CellRef<'_>, tf: &TypeFacts, at: BlockRef) -> Option<BlockLane> {
    let f = arm::<DatumArm>(cx, arm_kind(c.cell.rep, Accessor::Datum, tf)).block?;
    Some(f(cx, c, tf, at))
}

pub fn hash_image(cx: &mut Lx, c: CellRef<'_>, tf: &TypeFacts, scope: Scope) -> Words {
    let f = arm::<HashArm>(cx, arm_kind(c.cell.rep, Accessor::Hash, tf)).image;
    f(cx, c, tf, scope)
}

/// A `Rep::Code` cell's code in the current row (`I64`, the zero-extended u32 its Fp128 load indexes by): a lane element
/// is loaded, a record or kept word (`part << 32 | code`) is masked. The one reader of a code word (rep/code/datum.rs too).
pub fn code_word(cx: &mut Lx, c: CellRef<'_>) -> Value {
    match c.cell.at {
        At::Lane(_) => cx.b.load_zext(Type::I32, c.val, 0),
        _ => {
            let m = cx.b.iconst(Type::I64, u32::MAX as i64);
            cx.b.int_bin(IntBinOp::And, c.val, m)
        }
    }
}

pub fn eq(cx: &mut Lx, c: CellRef<'_>, stored: StoredRef, tf: &TypeFacts) -> Value {
    let f = arm::<HashArm>(cx, arm_kind(c.cell.rep, Accessor::Hash, tf)).eq;
    f(cx, c, stored, tf)
}

/// The order image words cell `c` takes under its type `tf` and collation `coll`, from the order arm of its rep and type.
pub fn order_words(cx: &mut Lx, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Words {
    let f = arm::<OrderArm>(cx, arm_kind(c.cell.rep, Accessor::Order, tf)).image;
    f(cx, c, tf, coll)
}

/// A packed numeric's typmod facts; `NONE` for every other representation.
pub const fn num_of(rep: Rep) -> gravity_program::NumTypmod {
    match rep {
        Rep::PackedNum { facts, .. } => facts,
        _ => gravity_program::NumTypmod::NONE,
    }
}

/// `internal`: a polymorphic call's argument no hosted site can take.
pub use types_core::catalog::INTERNALOID;

/// The datum of a cell of `bytes`-byte words and its null flag: a lane cell's `val` is its element's address (the source's
/// row), loaded by `gravity_expr`'s lane rule (8 as is, 4 / 2 sign-extended, 1 zero-extended); any other cell's `val` is the word.
pub fn datum_word(cx: &mut Lx, c: CellRef<'_>, bytes: u8) -> (Value, Value) {
    let null = match c.null {
        Some(n) => n,
        None => cx.b.bconst(false),
    };
    let v = match (c.cell.at, bytes) {
        (At::Lane(_), 8) => cx.b.load(Type::I64, c.val, 0),
        (At::Lane(_), 4 | 2) => {
            let w = cx.b.load(if bytes == 4 { Type::I32 } else { Type::I16 }, c.val, 0);
            cx.b.conv(ConvOp::Sext, Type::I64, w)
        }
        (At::Lane(_), _) => cx.b.load_zext(Type::I8, c.val, 0),
        _ => c.val,
    };
    (v, null)
}

/// A sorted group key the record keeps as an image whose order is its type's comparator (rep.hosted.order, M6-REPS HO: a
/// sort's tail reads a var copy; a group record's sort words read none): a shape the group sort does not take, named by
/// its shape (D1165 is closed; the refusal text keeps its words).
pub(crate) const SORTED_BY_COMPARATOR: &str = "D1165: a sorted group key ordered by its type's comparator (the group record keeps its image, no copy the comparator reads)";

/// A sorted group key's order arm `arm` that the group sort cannot take (keyed/emit_key.rs, runs/topk_groups.rs): by D1165
/// once the arm is built, else by its name.
pub(crate) fn hosted_sorted(node: i32, arm: KindId) -> ! {
    if crate::ops::built_kinds().has(arm) {
        crate::refuse!(node, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(SORTED_BY_COMPARATOR) });
    }
    crate::not_built!(node, crate::admit::refusal::Variant::Kind(arm))
}

/// The storage alignment in bytes of a value of type `t` (its `typalign`, which `pg_attribute.attalign` copies: 'c' / 's' /
/// 'i' / 'd' -> 1 / 2 / 4 / 8), `None` where the catalog does not answer (heap.pages: the walk's step over a by-reference
/// column, `att_align_nominal` / `att_align_pointer`).
pub(crate) fn storage_align<'m>(catalog: &dyn gravity_expr::Catalog<'m>, t: u32) -> Option<u8> {
    match catalog.type_facts(t).ok()?.align as u8 {
        b'c' => Some(1),
        b's' => Some(2),
        b'i' => Some(4),
        b'd' => Some(8),
        _ => None,
    }
}

pub(crate) use crate::effects::visible::rep::user_reach;
