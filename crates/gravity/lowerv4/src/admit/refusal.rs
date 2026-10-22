//! `refuse!`: v4's typed panic (plan §3.1, v3 design §3.5 kept): the payload, its reasons, the milestones that build what a
//! `NotBuilt` names, and the seam text. A kind or road no appended registry table provides is `NotBuilt` (the only refusal
//! besides `Grant`, `Interrupted` and the typed PostgreSQL-semantics reasons below).
use core::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use gravity_program::{LifeId, NumTypmod};
use pgsync::Mutex;

use crate::face::kinds::KindId;
use crate::face::strategy::Strategy;

/// The landing order of plan §5 (`tools/model.py` `MILESTONES`): a variant declared at `Mk` refuses until `Mk`'s registry
/// line lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Milestone {
    M0,
    M1,
    M2,
    M3,
    M4,
    M5,
    M6,
    M7,
    M8,
    M9,
    M10,
    M11,
}

impl Milestone {
    pub const COUNT: usize = 12;
    pub const ALL: [Milestone; Milestone::COUNT] = [Milestone::M0, Milestone::M1, Milestone::M2, Milestone::M3, Milestone::M4, Milestone::M5, Milestone::M6, Milestone::M7, Milestone::M8, Milestone::M9, Milestone::M10, Milestone::M11];

    pub const fn name(self) -> &'static str {
        match self {
            Milestone::M0 => "M0",
            Milestone::M1 => "M1",
            Milestone::M2 => "M2",
            Milestone::M3 => "M3",
            Milestone::M4 => "M4",
            Milestone::M5 => "M5",
            Milestone::M6 => "M6",
            Milestone::M7 => "M7",
            Milestone::M8 => "M8",
            Milestone::M9 => "M9",
            Milestone::M10 => "M10",
            Milestone::M11 => "M11",
        }
    }

    pub fn from_name(s: &str) -> Option<Milestone> {
        Milestone::ALL.into_iter().find(|m| m.name() == s)
    }
}

/// What a `NotBuilt` names: a kind of `face/kinds.rs` (Rep × accessor arms included) or a strategy (`face/strategy.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    Kind(KindId),
    Strategy(Strategy),
}

impl Variant {
    pub const fn name(self) -> &'static str {
        match self {
            Variant::Kind(k) => k.name(),
            Variant::Strategy(r) => r.name(),
        }
    }

    /// The hook the variant belongs to: a kind's face; `strategy` for a strategy (its declaration names no hook).
    pub const fn hook(self) -> &'static str {
        match self {
            Variant::Kind(k) => k.decl().face.name(),
            Variant::Strategy(_) => "strategy",
        }
    }

    /// A kind's milestone; a strategy declares none, and one no table provides refuses at the latest (no reader asks it:
    /// readers ask `on()`, and an unbuilt strategy never refuses admission).
    pub const fn milestone(self) -> Milestone {
        match self {
            Variant::Kind(k) => k.decl().milestone,
            Variant::Strategy(_) => Milestone::M11,
        }
    }

    /// The refusal of this variant unbuilt: hook and milestone from its declaration, never written at a call site.
    pub const fn not_built(self) -> Reason {
        Reason::NotBuilt { hook: self.hook(), variant: self, milestone: self.milestone() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumOp {
    Compare,
    Key,
    Order,
    Join,
    Sum,
    Avg,
    MinMax,
    Arith,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CollWhy {
    Missing,
    NonDeterministic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShapeWhy(pub &'static str);

/// A rechecking write over another pipeline's product (Gx::reads_product; D652): EvalPlanQual (execMain.c) must reuse the
/// leader's InitPlan words / rebuild a join's build. The text keeps its D-id: markers name it.
pub const READS_PRODUCT: ShapeWhy = ShapeWhy("a rechecking write that reads another pipeline's product (D652)");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// A kind or road the statement needs whose milestone has not landed (plan §3.1, R-A).
    NotBuilt { hook: &'static str, variant: Variant, milestone: Milestone },
    NumericTooWide { precision: u16, op: NumOp },
    NoOrder { typid: u32 },
    NoHash { typid: u32 },
    Collation { coll: u32, why: CollWhy },
    JoinScale { a: NumTypmod, b: NumTypmod },
    Shape { why: ShapeWhy },
    Width { needed: u32, have: u32 },
    Grant { life: LifeId, needed: u64, share: u64 },
    Stopgap { d: u16 },
    Bug { payload: &'static str },
    /// A pending interrupt stopped the lowering (D231, `cancelpoll::interrupts::pending`): the admission raises it.
    Interrupted,
    /// A scan of relation `relid` whose table AM is neither heap nor pgrc: v4's sources read only those two page formats
    /// (standing ruling: foreign AMs may refuse).
    UnsupportedAm { relid: u32 },
}

impl Reason {
    pub const VARIANTS: usize = 13;

    pub const fn variant(&self) -> &'static str {
        match self {
            Reason::NotBuilt { .. } => "NotBuilt",
            Reason::NumericTooWide { .. } => "NumericTooWide",
            Reason::NoOrder { .. } => "NoOrder",
            Reason::NoHash { .. } => "NoHash",
            Reason::Collation { .. } => "Collation",
            Reason::JoinScale { .. } => "JoinScale",
            Reason::Shape { .. } => "Shape",
            Reason::Width { .. } => "Width",
            Reason::Grant { .. } => "Grant",
            Reason::Stopgap { .. } => "Stopgap",
            Reason::Bug { .. } => "Bug",
            Reason::Interrupted => gravity_lower::INTERRUPTED,
            Reason::UnsupportedAm { .. } => "UnsupportedAm",
        }
    }

    /// The milestone a reason is burned down in: `NotBuilt`'s own; every other reason is the core's (M0) until one owns it.
    pub const fn milestone(&self) -> Milestone {
        match self {
            Reason::NotBuilt { milestone, .. } => *milestone,
            _ => Milestone::M0,
        }
    }

    /// The text built from compile-time facts only (variant, hook, milestone, static strings, enum fields): a finite set.
    fn static_text(&self) -> String {
        match self {
            Reason::NotBuilt { hook, variant, milestone } => format!("NotBuilt {}: {hook} {}", milestone.name(), variant.name()), // alloc: Cold
            Reason::NumericTooWide { op, .. } => format!("NumericTooWide: {op:?}"), // alloc: Cold
            Reason::Collation { why, .. } => format!("Collation: {why:?}"), // alloc: Cold
            Reason::Shape { why } => format!("Shape: {}", why.0), // alloc: Cold
            Reason::Bug { payload } => format!("Bug: {payload}"), // alloc: Cold
            r => r.variant().to_string(), // alloc: Cold
        }
    }

    /// The one value the seam's `oid` field carries (a type, a collation or a D-row); 0 = none.
    const fn oid(&self) -> u32 {
        match self {
            Reason::NoOrder { typid } | Reason::NoHash { typid } => *typid,
            Reason::Collation { coll, .. } => *coll,
            Reason::Stopgap { d } => *d as u32,
            Reason::UnsupportedAm { relid } => *relid,
            _ => 0,
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NotBuilt { hook, variant, milestone } => write!(f, "NotBuilt {{ hook: {hook}, variant: {}, milestone: {} }}", variant.name(), milestone.name()),
            r => write!(f, "{r:?}"),
        }
    }
}

/// The payload `refuse!` unwinds with: the reason and the plan node it names. PGRCSERVE M0 (D2): `native_rel`, the pgrc
/// relation whose native code lanes met this shape (`refuse_native`; its rows road serves it), else 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: Reason,
    pub node: i32,
    pub native_rel: u32,
}

/// The node a lowering context names in its refusals.
pub trait Site {
    fn node(&self) -> i32;
}

impl<T: Site + ?Sized> Site for &T {
    fn node(&self) -> i32 {
        (**self).node()
    }
}

impl<T: Site + ?Sized> Site for &mut T {
    fn node(&self) -> i32 {
        (**self).node()
    }
}

impl Site for i32 {
    fn node(&self) -> i32 {
        *self
    }
}

/// `refuse!(cx, Reason::…)`: unwind with the typed payload. Lowering code on the leader only (the §3.5 lint).
#[macro_export]
macro_rules! refuse {
    ($cx:expr, $reason:expr) => {
        $crate::admit::refusal::raise($crate::admit::refusal::Site::node(&$cx), $reason)
    };
}

/// `not_built!(cx, Variant::…)`: refuse the variant by name, its hook and milestone from the declaration.
#[macro_export]
macro_rules! not_built {
    ($cx:expr, $variant:expr) => {
        $crate::admit::refusal::raise($crate::admit::refusal::Site::node(&$cx), $crate::admit::refusal::Variant::not_built($variant))
    };
}

/// The lowering stops here when an interrupt is pending (D231, D2909, as C's CHECK_FOR_INTERRUPTS): at each pipeline, each
/// expression's compile and emission (step/expr.rs) and each VALUES row (ops/values/lower.rs), so a long lowering takes a
/// cancel, a statement_timeout or a terminate within one row's or one expression's work. The facade's `pending`
/// (cancelpoll::interrupts): one seam call, two thread-local loads, when none.
#[inline]
pub fn check_interrupts(node: i32) {
    if cancelpoll::interrupts::pending() {
        raise(node, Reason::Interrupted);
    }
}

/// `resume_unwind`, not `panic_any`: a refusal is a decision, so no panic hook runs and nothing is printed.
#[cold]
#[inline(never)]
pub fn raise(node: i32, reason: Reason) -> ! {
    std::panic::resume_unwind(Box::new(Refusal { reason, node, native_rel: 0 })) // alloc: Cold
}

/// PGRCSERVE M0 (D2): `raise` of a shape refusal only pgrc relation `relid`'s native code lane meets (D767, D1353).
#[cold]
#[inline(never)]
pub fn refuse_native(node: i32, relid: u32, why: ShapeWhy) -> ! {
    std::panic::resume_unwind(Box::new(Refusal { reason: Reason::Shape { why }, node, native_rel: relid })) // alloc: Cold
}

pub(crate) static BUGS: AtomicU64 = AtomicU64::new(0);

/// `lib::catch`'s `Reason::Bug` for a panic that is no refusal and no Postgres error: counted, a runtime message logged.
pub(crate) fn bug(node: i32, payload: Box<dyn core::any::Any + Send>) -> Refusal {
    let text = payload.downcast_ref::<&'static str>().copied();
    BUGS.fetch_add(1, Ordering::Relaxed);
    if text.is_none() {
        let msg = payload.downcast_ref::<String>().map_or("(non-string payload)", String::as_str);
        eprintln!("WARNING: gravity lowerv4: panic while lowering (Reason::Bug) node={node}: {msg}");
    }
    Refusal { reason: Reason::Bug { payload: text.unwrap_or("a panic with a runtime message (logged)") }, node, native_rel: 0 }
}

/// A facade refusal (`gravity_lower::admit::refuse`) as the seam's refusal, with the text of the `refuse!` it replaces.
pub(crate) fn of_facade(r: gravity_lower::admit::Refuse) -> Refusal {
    use gravity_lower::admit::Gap;
    let reason = match r.gap {
        Gap::Shape(why) | Gap::Bug(_, why) => Reason::Shape { why: ShapeWhy(why) },
        Gap::Am(relid) => Reason::UnsupportedAm { relid },
        Gap::Invariant(what) => return bug(r.node, Box::new(what)), // alloc: Cold
    };
    Refusal { reason, node: r.node, native_rel: 0 }
}

pub fn bugs() -> u64 {
    BUGS.load(Ordering::Relaxed)
}

static INTERNED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// The static text as a `&'static str` the seam's `Refusal` can hold: leaked once per distinct text, never per value.
pub fn intern(text: String) -> &'static str {
    let mut set = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = set.iter().find(|s| **s == text) {
        return s;
    }
    let s: &'static str = Box::leak(text.into_boxed_str()); // alloc: Cold
    set.push(s);
    s
}

pub fn interned() -> usize {
    INTERNED.lock().unwrap_or_else(|e| e.into_inner()).len()
}

pub const MODULE: &str = "lowerv4";

impl Refusal {
    /// The seam's refusal: the interned static text, the node, a type / collation / D-row in `oid`. Field values that are
    /// not compile-time facts (sizes, precisions, scales) are in the log line `lower_graph` writes, never in the text.
    pub fn to_seam(&self) -> gravity_lower::Refusal {
        let oid = self.reason.oid();
        let detail = match self.reason {
            Reason::Stopgap { .. } => "D-row",
            Reason::Collation { .. } => "collation",
            Reason::UnsupportedAm { .. } => "relation",
            _ if oid != 0 => "type",
            _ => "",
        };
        gravity_lower::Refusal { module: MODULE, what: intern(self.reason.static_text()), node: self.node, detail, oid, native_rel: self.native_rel }
    }
}
