#![allow(unsafe_code)]
use core::hash::{Hash, Hasher};
use std::hash::DefaultHasher;

use gravity_program::limits;
use types_nodes::{Node, NodeTag};
pub mod agg;
pub mod arrays;
pub mod casts;
pub mod expr_type;
pub mod hash_eq;
pub mod keys;
pub mod keysem;
pub mod monotone;
pub mod never_null;
pub mod numeric;
pub mod opdir;
pub mod rows;
pub mod scan;
pub mod srf;
pub mod typefacts;
pub mod word_order;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Am {
    Heap,
    Pgrcolumnar,
    Other,
}

impl Am {
    pub fn of_name(name: &str) -> Am {
        match name {
            "heap" => Am::Heap,
            "pgrcolumnar" => Am::Pgrcolumnar,
            _ => Am::Other,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Am::Heap => "heap",
            Am::Pgrcolumnar => "pgrc",
            Am::Other => "other",
        }
    }
}

pub use keysem::image::KeyEq;

/// The index access method as a value (D3): the source's host is chosen by it, never by a code path per AM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IndexAm {
    Btree,
    Hash,
    Gist,
    Spgist,
    Gin,
    Brin,
    Other,
}

impl IndexAm {
    pub fn of_name(name: &str) -> IndexAm {
        IndexAm::ALL.iter().copied().find(|a| a.name() == name).unwrap_or(IndexAm::Other)
    }

    pub const ALL: [IndexAm; 6] = [IndexAm::Btree, IndexAm::Hash, IndexAm::Gist, IndexAm::Spgist, IndexAm::Gin, IndexAm::Brin];

    pub const fn name(self) -> &'static str {
        match self {
            IndexAm::Btree => "btree",
            IndexAm::Hash => "hash",
            IndexAm::Gist => "gist",
            IndexAm::Spgist => "spgist",
            IndexAm::Gin => "gin",
            IndexAm::Brin => "brin",
            IndexAm::Other => "other",
        }
    }

    /// The AM returns rows in its key columns' order (`amcanorder`): btree alone.
    pub const fn ordered(self) -> bool {
        matches!(self, IndexAm::Btree)
    }

    /// The AM serves `ORDER BY column <op> value` (`amcanorderbyop`): the nearest-neighbour walk.
    pub const fn order_by_op(self) -> bool {
        matches!(self, IndexAm::Gist | IndexAm::Spgist)
    }

    /// The AM has an ordinary scan (`amgettuple`); gin and brin answer through a bitmap only.
    pub const fn gettuple(self) -> bool {
        !matches!(self, IndexAm::Gin | IndexAm::Brin | IndexAm::Other)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelFact {
    pub relid: u32,
    pub am: Am,
    pub relkind: u8,
    pub natts: i16,
    /// The order the AM vouches for over the whole relation, ASC NULLS LAST lexicographic: a fact only captures made
    /// before D4469 carry (the declared `sorted_by` key of a table of at most one part, the header's `sorted=`). The live
    /// facts hold none: a scan the plan relies on for its order is marked (`SeqScan.pgrc_merge`) and merges its parts.
    pub sorted: [i16; limits::KEYS],
    pub nsorted: u8,
    /// PGRCSERVE G4: read through its AM's slot scan (the rows road): seq scans map as heap (`SiteDesc::rows_via_am`);
    /// `am` is `Other`, so every other site over it (index, bitmap, TID, sample, write) refuses as over any other AM.
    pub rows: bool,
}

impl RelFact {
    pub const fn plain(relid: u32, am: Am, relkind: u8, natts: i16) -> RelFact {
        RelFact { relid, am, relkind, natts, sorted: [0; limits::KEYS], nsorted: 0, rows: false }
    }

    pub fn sorted_by(&self) -> &[i16] {
        &self.sorted[..self.nsorted as usize]
    }

    pub const fn index_fetch(&self) -> bool {
        !matches!(self.am, Am::Other) || self.rows
    }

    pub const fn on_rows_road(mut self) -> RelFact {
        self.am = Am::Other;
        self.rows = true;
        self.nsorted = 0;
        self
    }

    /// The sorted key from an attno list; past `limits::KEYS` the prefix (still an order, never an over-claim).
    pub fn with_sorted(mut self, attnos: impl Iterator<Item = i16>) -> RelFact {
        self.nsorted = 0;
        for a in attnos.take(limits::KEYS) {
            self.sorted[self.nsorted as usize] = a;
            self.nsorted += 1;
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeRow {
    pub typlen: i16,
    pub byval: bool,
    pub typtype: u8,
    pub base: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttFact {
    pub typid: u32,
    pub typmod: i32,
    pub typlen: i16,
    pub byval: bool,
    pub notnull: bool,
    /// D2700: the column's base type and typmod (C `getBaseTypeAndTypmod`): a domain's base, its typmod the domain's
    /// (`numeric(10, 2)` lives in the domain's `typtypmod`, the column's `atttypmod` is -1); `typid` / `typmod` otherwise.
    /// A domain stores its base's bytes, so the text and packed-numeric keys read these; `typid` stays the column's own
    /// (its output and its Vars' type).
    pub base: u32,
    pub base_typmod: i32,
}

/// A dropped attribute's physical layout: `pg_attribute.attlen`, `attbyval` and `attalign` in bytes (1 / 2 / 4 / 8),
/// which DROP COLUMN keeps (tuples written before it still carry the column's bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttLayout {
    pub typlen: i16,
    pub byval: bool,
    pub align: u8,
}

/// `pg_index.indoption` per key column (D5 facts, decompose.md §7 k).
pub const INDOPTION_DESC: u8 = 1;
pub const INDOPTION_NULLS_FIRST: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexFact {
    pub indexid: u32,
    pub relid: u32,
    /// The key columns' attributes; 0 = an expression column (`index_expr` names it).
    pub keys: [i16; limits::KEYS],
    /// The key columns' options (`INDOPTION_*`), the same positions as `keys`.
    pub opts: [u8; limits::KEYS],
    pub nkeys: u8,
    pub unique: bool,
    /// `indimmediate`: a unique index checked at each insert, not deferred (a deferrable one may hold two equal keys
    /// inside a transaction).
    pub immediate: bool,
    pub btree: bool,
    pub partial: bool,
}

impl IndexFact {
    pub fn keys(&self) -> &[i16] {
        &self.keys[..self.nkeys as usize]
    }

    pub fn desc(&self, i: usize) -> bool {
        self.opts[i] & INDOPTION_DESC != 0
    }

    pub fn nulls_first(&self, i: usize) -> bool {
        self.opts[i] & INDOPTION_NULLS_FIRST != 0
    }
}

/// A structural fingerprint of an expression with its `Var`s' range-table index normalised: two 64-bit SipHash
/// words over the node kinds and the fields that fix a node's meaning (`expr_fp`). `NONE` is no expression: a
/// key that is a column. Equal fingerprints mean the same expression over the same relation's columns (an index's
/// stored `indexprs` against a target list's entry), whatever the range-table indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExprFp(pub u64, pub u64);

impl ExprFp {
    pub const NONE: ExprFp = ExprFp(0, 0);

    pub const fn is_some(self) -> bool {
        self.0 != 0 || self.1 != 0
    }
}

/// The fingerprint of `node` whose `Var`s name `varno` (any other range-table index, an outer level, a system
/// column, or a node kind outside the expression table: none — an unfingerprinted expression has no order).
/// `opfuncid` / `funcformat` / collations / locations are not part of it: the stored `indexprs` has no
/// `opfuncid` set and the planner's copy does. Constants hash their `constlen` bytes.
pub fn expr_fp(node: Node<'_>, varno: i32) -> Option<ExprFp> {
    let mut a = DefaultHasher::new();
    let mut b = DefaultHasher::new();
    1u8.hash(&mut a);
    2u8.hash(&mut b);
    fp_walk(node, varno, &mut a, &mut b)?;
    let fp = ExprFp(a.finish(), b.finish());
    Some(if fp.is_some() { fp } else { ExprFp(1, 0) })
}

/// The fingerprint of `column <op> value` as `expr_fp` would hash the `OpExpr` a target list carries for it: a
/// nearest-neighbour walk's order key (`KeyForm::OrderBy`), whose expression the source keeps only in parts.
pub fn op_expr_fp(opno: u32, varno: i32, attno: i16, typid: u32, typmod: i32, value: Node<'_>) -> Option<ExprFp> {
    let mut a = DefaultHasher::new();
    let mut b = DefaultHasher::new();
    1u8.hash(&mut a);
    2u8.hash(&mut b);
    for hs in [&mut a, &mut b] {
        (NodeTag::T_OpExpr as u32).hash(hs);
        opno.hash(hs);
        (NodeTag::T_Var as u32).hash(hs);
        (attno, typid, typmod).hash(hs);
        0u32.hash(hs);
    }
    fp_walk(value, varno, &mut a, &mut b)?;
    2u32.hash(&mut a);
    2u32.hash(&mut b);
    let fp = ExprFp(a.finish(), b.finish());
    Some(if fp.is_some() { fp } else { ExprFp(1, 0) })
}

fn fp_walk(node: Node<'_>, varno: i32, a: &mut DefaultHasher, b: &mut DefaultHasher) -> Option<()> {
    crate::walk::stack_check(); // D3748
    macro_rules! h {
        ($x:expr) => {{
            let x = $x;
            x.hash(a);
            x.hash(b);
        }};
    }
    let tag = node.node_tag();
    h!((tag as u32));
    match tag {
        NodeTag::T_Var => {
            let v = node.as_var()?;
            if v.varno != varno || v.varlevelsup != 0 || v.varattno <= 0 {
                return None;
            }
            h!((v.varattno, v.vartype, v.vartypmod));
        }
        NodeTag::T_Const => {
            let c = node.as_const()?;
            h!((c.consttype, c.consttypmod, c.constlen, c.constisnull, c.constbyval));
            if c.constisnull {
                return Some(());
            }
            if c.constbyval {
                let raw = c.constvalue.as_usize() as u64;
                let v = match c.constlen {
                    1 => raw & 0xff,
                    2 => raw & 0xffff,
                    4 => raw & 0xffff_ffff,
                    _ => raw,
                };
                h!(v);
            } else if c.constlen == -1 {
                h!(crate::expr::const_bytes(c)?);
            } else if c.constlen == -2 {
                // the fingerprint decides which index expression the graph reads: the image is baked (crate::bake)
                let p = crate::bake::read(c).as_usize() as *const u8;
                let mut n = 0usize;
                while unsafe { *p.add(n) } != 0 {
                    n += 1;
                }
                h!(unsafe { core::slice::from_raw_parts(p, n) });
            } else if c.constlen > 0 {
                let p = crate::bake::read(c).as_usize() as *const u8;
                h!(unsafe { core::slice::from_raw_parts(p, c.constlen as usize) });
            } else {
                return None;
            }
        }
        NodeTag::T_OpExpr => h!(node.as_op_expr()?.opno),
        NodeTag::T_DistinctExpr => h!(node.as_distinct_expr()?.opno),
        NodeTag::T_NullIfExpr => h!(node.as_null_if_expr()?.opno),
        NodeTag::T_FuncExpr => {
            let f = node.as_func_expr()?;
            h!((f.funcid, f.funcresulttype, f.funcvariadic));
        }
        NodeTag::T_BoolExpr => h!((node.as_bool_expr()?.boolop as u32)),
        NodeTag::T_ScalarArrayOpExpr => {
            let s = node.as_scalar_array_op_expr()?;
            h!((s.opno, s.useOr));
        }
        NodeTag::T_CoalesceExpr => h!(node.as_coalesce_expr()?.coalescetype),
        NodeTag::T_MinMaxExpr => {
            let m = node.as_min_max_expr()?;
            h!((m.minmaxtype, m.op as u32));
        }
        NodeTag::T_ArrayExpr => {
            let x = node.as_array_expr()?;
            h!((x.array_typeid, x.element_typeid, x.multidims));
        }
        NodeTag::T_RowExpr => h!(node.as_row_expr()?.row_typeid),
        NodeTag::T_RelabelType => {
            let r = node.as_relabel_type()?;
            h!((r.resulttype, r.resulttypmod));
        }
        NodeTag::T_CoerceViaIO => h!(node.as_coerce_via_io()?.resulttype),
        NodeTag::T_CoerceToDomain => {
            let c = node.as_coerce_to_domain()?;
            h!((c.resulttype, c.resulttypmod));
        }
        NodeTag::T_ArrayCoerceExpr => {
            let c = node.as_array_coerce_expr()?;
            h!((c.resulttype, c.resulttypmod));
        }
        NodeTag::T_ConvertRowtypeExpr => h!(node.as_convert_rowtype_expr()?.resulttype),
        NodeTag::T_FieldSelect => {
            let f = node.as_field_select()?;
            h!((f.fieldnum, f.resulttype, f.resulttypmod));
        }
        NodeTag::T_NullTest => {
            let t = node.as_null_test()?;
            h!((t.nulltesttype as u32, t.argisrow));
        }
        NodeTag::T_BooleanTest => h!((node.as_boolean_test()?.booltesttype as u32)),
        NodeTag::T_CaseExpr => h!(node.as_case_expr()?.casetype),
        NodeTag::T_CaseWhen => {}
        NodeTag::T_CaseTestExpr => {
            let t = node.as_case_test_expr()?;
            h!((t.typeId, t.typeMod));
        }
        NodeTag::T_CollateExpr => h!(node.as_collate_expr()?.collOid),
        NodeTag::T_SQLValueFunction => {
            let s = node.as_sql_value_function()?;
            h!((s.op as u32, s.r#type, s.typmod));
        }
        _ => return None,
    }
    let mut ok = Some(());
    let mut n = 0u32;
    let known = crate::expr::kids(node, &mut |k| {
        n += 1;
        if ok.is_some() {
            ok = fp_walk(k, varno, a, b);
        }
    });
    if !known {
        return None;
    }
    n.hash(a);
    n.hash(b);
    ok
}

/// The write family's index facts past `IndexFact` (D1 writes, decompose.md §2.12): the
/// constraint forms (`indimmediate`, `indisexclusion`, `indisprimary`) and the key-column count
/// (`indnkeyatts`; INCLUDE columns follow). `known` is false when the facts source has none
/// (the relcache facts until they carry them: §7 need): the classification then says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexExt {
    pub deferrable: bool,
    pub valid: bool,
    pub ready: bool,
    pub exclusion: bool,
    pub primary: bool,
    pub nkeyatts: u8,
    pub known: bool,
}

/// One `column = value` conjunct over key column `col` (0-based) of an index, as `Facts::unique_bound` reads it: the
/// operator, its input collation, and whether its value may be NULL where the operator is called on it (a filter; an
/// index search key that is NULL matches nothing without a call).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyBind {
    pub col: u8,
    pub opno: u32,
    pub collid: u32,
    pub nullable: bool,
}

impl IndexExt {
    pub const UNKNOWN: IndexExt = IndexExt { deferrable: false, valid: true, ready: true, exclusion: false, primary: false, nkeyatts: 0, known: false };
}

/// D2063: per key column of an index, whether a scan keyed by initdb operators over initdb types reaches only initdb
/// functions through the column's operator class: its declared and stored types and its operator family are initdb
/// objects, and so is every member of the family over two initdb types (each pg_amproc support proc, the btree FUNCTION 1
/// `_bt_first` looks up per `(opcintype, subtype)` included, each pg_amop operator and sort family). A family OID below
/// FirstNormalObjectId alone is no proof: ALTER OPERATOR FAMILY adds members to an initdb family. `known` false (a source
/// without the catalog: a fixture, a capture) reads every column as a user's. A reader requires every column up to and
/// including its last keyed one (btree skip scan calls a key-less leading column's `=`, order and skip-support procs), and
/// tests its own key and ORDER BY operators; the fact says nothing about index expressions or a partial predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexOpc {
    /// Bit `i`: key column `i` passes (INCLUDE columns, past `nkeys`, have no operator class).
    pub builtin: u32,
    pub nkeys: u8,
    pub known: bool,
}

const _: () = assert!(limits::KEYS <= u32::BITS as usize);

impl IndexOpc {
    pub const UNKNOWN: IndexOpc = IndexOpc { builtin: 0, nkeys: 0, known: false };

    /// Key column `i` passes (false when unknown or past the key columns).
    pub const fn col(self, i: usize) -> bool {
        self.known && i < self.nkeys as usize && (self.builtin >> i) & 1 != 0
    }

    /// Key columns `0..=last` all pass.
    pub const fn through(self, last: usize) -> bool {
        self.known && last < self.nkeys as usize && self.builtin.trailing_ones() as usize > last
    }

    /// Every key column passes.
    pub const fn all(self) -> bool {
        self.nkeys > 0 && self.through(self.nkeys as usize - 1)
    }
}

/// A relation's INSTEAD OF row triggers (the write family's view target, decompose.md §2.12): which
/// commands a trigger takes the row for (`TriggerDesc.trig_{insert,update,delete}_instead_row`, enabled or
/// not, as the executor's `CheckValidResultRel` reads them). `known` is false when the facts source has
/// none (a fixture captured before the `trig` header line existed): the target then prints `instead=?`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct InsteadOf {
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub known: bool,
}

/// What a written relation carries beyond its shape (the write host's `writable` conditions, lower.md §8; the
/// ladder's rung 10): row triggers, rules, inheritance children, partition membership. `UNKNOWN` (`known` false)
/// when the source has none (the corpus capture); the server's relcache facts answer every relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelExt {
    pub has_triggers: bool,
    pub has_rules: bool,
    pub has_subclass: bool,
    pub is_partition: bool,
    pub known: bool,
    /// D1035 step 2 (V4R-444): `relpersistence = 't'` (a temporary relation: parallel-restricted, rescan.rs F5).
    pub temp: bool,
}

/// One leaf of a partitioned root as the routed write admits it (`route_leaves.rs`; the host re-reads every flag under
/// the leaf's lock at its first routed row): heap AM, the root's layout (no `ri_RootToPartitionMap`), plain btree indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafFact {
    pub relid: u32,
    pub natts: i16,
    pub heap: bool,
    /// A foreign table (relkind `f`): foreign tables are removed with the Foreign Scan (route_leaves `FOREIGN_LEAF`).
    pub foreign: bool,
    /// D3622: a pgrcolumnar table (relkind `r`): its writer is the load lane's (COPY); a routed INSERT's host raises the
    /// pgrc trickle gate's 0A000 at the first row routed into it (lowerv4 write.route `leaf`).
    pub pgrc: bool,
    pub same_layout: bool,
    pub has_triggers: bool,
    pub trig: crate::ops::modifytable::trig_facts::TrigExt,
    pub has_rules: bool,
    pub has_generated: bool,
    pub nindexes: u16,
    pub indexes_plain: bool,
}

impl RelExt {
    pub const UNKNOWN: RelExt = RelExt { has_triggers: false, has_rules: false, has_subclass: false, is_partition: false, known: false, temp: false };
}

impl InsteadOf {
    pub const UNKNOWN: InsteadOf = InsteadOf { insert: false, update: false, delete: false, known: false };
    pub const NONE: InsteadOf = InsteadOf { insert: false, update: false, delete: false, known: true };

    pub const fn any(self) -> bool {
        self.insert || self.update || self.delete
    }

    /// The capture's / render's letters: `i`, `u`, `d` in that order (`-` for none).
    pub fn letters(self) -> [u8; 3] {
        let mut l = [0u8; 3];
        let mut n = 0;
        for (f, c) in [(self.insert, b'i'), (self.update, b'u'), (self.delete, b'd')] {
            if f {
                l[n] = c;
                n += 1;
            }
        }
        if n == 0 {
            l[0] = b'-';
        }
        l
    }

    pub fn of_letters(s: &str) -> InsteadOf {
        InsteadOf { insert: s.contains('i'), update: s.contains('u'), delete: s.contains('d'), known: true }
    }
}

impl core::fmt::Display for InsteadOf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use core::fmt::Write;
        if !self.known {
            return f.write_char('?');
        }
        self.letters().iter().filter(|c| **c != 0).try_for_each(|c| f.write_char(*c as char))
    }
}

/// An aggregate's support functions (`pg_aggregate`; 0 = none). CREATE AGGREGATE stores the aggregate's own pg_proc row as
/// IMMUTABLE ("not needed for agg", pg_aggregate.c AggregateCreate), so what calling it does is what these functions do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggSupport {
    pub trans: u32,
    pub final_: u32,
    pub combine: u32,
    pub serial: u32,
    pub deserial: u32,
    pub mtrans: u32,
    pub minv: u32,
    pub mfinal: u32,
}

impl AggSupport {
    /// The support functions the aggregate has.
    pub fn each(self) -> impl Iterator<Item = u32> {
        [self.trans, self.final_, self.combine, self.serial, self.deserial, self.mtrans, self.minv, self.mfinal].into_iter().filter(|&f| f != 0)
    }
}

/// KeySem slice 3 (PLAN-v5-keysem §4, §6): what a plan-time consumer asks of one key (`Facts::key`), the one
/// `KeyReq`: the same type `gravity_fn::typesem::resolve` reads (decompose reads no gravity_fn; the server's facts
/// answer it from the same catalog reads). Built only with its constructors.
pub use gravity_abi::key::{KeyReq, KeyUse};

/// KeySem slice 3: a sorted key's order as PostgreSQL's tuplesort resolves it (`get_ordering_op_properties`, the type
/// cache's default btree family, then the family's sortsupport / comparator procs at the sort's first call): the one
/// plan-time answer (`Facts::key`) the direction table (`dir`), another family's direction and family (an own-family
/// key's), the CompareFn comparator (`cmp`) and the missing-proc raise (`no_proc`) are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyOrder {
    /// Strategy 5 (`>`): the key sorts descending.
    pub desc: bool,
    /// The btree family the operator orders by.
    pub family: u32,
    /// The family's input type the operator is an ordering member for.
    pub opcintype: u32,
    /// Whether `family` is `opcintype`'s default btree family (the type cache's `btree_opf`): only then does a key's image
    /// keep its order (D1346); any other family orders by its own comparator (the CompareFn key kind).
    pub default_family: bool,
    /// The family's comparator (support function 1) for `(opcintype, opcintype)`; 0 when it has none or the source cannot
    /// read `pg_amproc`.
    pub cmp: u32,
    /// W8-EQUIVCLASS-SORTOP (M11): the family holds neither a sortsupport (2) nor a comparator (1) for
    /// `(opcintype, opcintype)`, so PostgreSQL's `FinishSortSupportFunction` raises "missing support function 1(T,T) in
    /// opfamily F" at the sort's first call. False when the source cannot read `pg_amproc`.
    pub no_proc: bool,
}

impl KeyOrder {
    /// The direction table's answer (`Some(true)` = descending): the direction of an ordering operator of its input type's
    /// default btree family (decision 49), whose order a key's image keeps; none for another family's.
    pub const fn dir(self) -> Option<bool> {
        if self.default_family { Some(self.desc) } else { None }
    }
}

/// KeySem (LANGUAGE internal; equivclass#53): the support functions a plan node naming an operator alone runs (gravity_fn
/// typesem `keysem::OpCalls`), raw procs the effects home reads each by `effects::body`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpCalls {
    /// `oprcode`.
    pub code: u32,
    /// An ordering operator's btree family sortsupport (2) and comparator (1) for its input type; `[0, 0]`: the sort raises
    /// "missing support function" at its first call. None: no ordering operator.
    pub order: Option<[u32; 2]>,
    /// A hashable equality's hash procs for each input.
    pub hash: Option<[u32; 2]>,
}

impl OpCalls {
    /// Every proc it names (none 0).
    pub fn each(&self) -> impl Iterator<Item = u32> {
        let (o, h) = (self.order.unwrap_or([0, 0]), self.hash.unwrap_or([0, 0]));
        [self.code, o[0], o[1], h[0], h[1]].into_iter().filter(|&p| p != 0)
    }

    /// An ordering operator whose family has neither a sortsupport nor a comparator for its type.
    pub fn no_order(&self) -> bool {
        self.order == Some([0, 0])
    }
}

pub trait Facts {
    fn rel(&self, relid: u32) -> Option<RelFact>;
    fn att(&self, relid: u32, attno: i16) -> Option<AttFact>;
    /// Attribute `attno`'s layout when it is a dropped column (`att` answers none for one), for a tuple walk that steps
    /// over its bytes; none for a live column, one past the last, or a source without the catalog (the default).
    fn dropped_layout(&self, _relid: u32, _attno: i16) -> Option<AttLayout> {
        None
    }
    /// A column's extrema over the relation's resident zone summary (`shapes.rs::derived_in_range`'s source; never optimistic): none without one. `att_estimate` is its ESTIMATE (`pg_statistic` on heap; M3-L6J): only a per-row guarded consumer (the packed front) may read that one.
    fn att_range(&self, _relid: u32, _attno: i16) -> Option<(i64, i64)> {
        None
    }
    fn att_estimate(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.att_range(relid, attno)
    }
    /// The relation's rows as its resident parts hold them (pgrc's latest generation), none on any other source:
    /// join.buildside's size fact (H18c), where a stale count only builds the slower side.
    fn rel_rows(&self, _relid: u32) -> Option<u64> {
        None
    }
    /// An upper bound on a heap relation's rows that no statistic can make stale: its main fork's current pages x the
    /// most tuples a heap page holds (MaxHeapTuplesPerPage), none on any other source: join.buildside's size fact for a
    /// heap side against an exact one (PS1-21 / PS1-26 step 1b), so the swap builds a heap side only when it is provably
    /// the smaller one (a stale or never-analyzed reltuples can not mislead it; a bloated table only keeps the plan's side).
    fn rel_rows_bound(&self, _relid: u32) -> Option<u64> {
        None
    }
    /// Column `attno`'s per-code entries over the relation's parts (pgrc: a dictionary-coded column's dictionary entries,
    /// a verbatim column's distinct values, which its codes rank), none on any other source: the per-code program's datum
    /// guard (lowerv4 dict/percode.rs), where a stale count only picks the slower of two forms with the same answers.
    fn att_entries(&self, _relid: u32, _attno: i16) -> Option<u64> {
        None
    }
    /// Column `attno`'s average datum width in bytes (pgrc: the footer's planner-convention width, else ANALYZE's
    /// `stawidth`), none on any other source or without either: what a by-reference lane's arena holds a granule row
    /// (decompose ops/scan `datum_text_lanes`), where a stale width only picks the slower of two forms with the same answers.
    fn att_width(&self, _relid: u32, _attno: i16) -> Option<u32> {
        None
    }
    /// Column `attno`'s average bytes a non-NULL value, its header not counted (pgrc: the footer's width less its 4-byte
    /// header, else ANALYZE's `stawidth` less a short value's 1-byte one; another table: the latter), none on any other
    /// source or without either: whether a text key's values fit num.pair's word (ops/agg `KeyDecl.bytes`), where a stale
    /// count only picks the slower of two forms with the same answers.
    fn att_value_bytes(&self, _relid: u32, _attno: i16) -> Option<u32> {
        None
    }
    /// The planner's distinct values of column `attno` over the whole relation and the rows they were counted against, as
    /// `get_variable_numdistinct` reads them: ANALYZE's `stadistinct`, a negative one (a fraction of the rows) times the
    /// relation's rows (pgrc's resident rows, heap's `reltuples`); none without statistics or rows, or off a backend. An
    /// estimate: a stale one only picks the slower of two forms with the same answers (ops/agg `multi_counts`' text).
    fn att_ndistinct(&self, _relid: u32, _attno: i16) -> Option<(u64, u64)> {
        None
    }
    /// D4497: whether default-off road `name` (a decompose rule's switch, lowerv4 road_off.rs `DEFAULT_OFF`) is on: this
    /// session's `pgrust.gravity_road_off` names it `+<name>` and does not name it off plainly (the option's text keys the
    /// front key and the program tiers, D311); false off a backend.
    fn road_on(&self, _name: &str) -> bool {
        false
    }
    /// D3784: the share of `rows` rows read from relation `relid` (filtered: the planner's estimate) that column
    /// `attno`'s most common value holds there (NULL a value, as a group key reads it), from ANALYZE's statistics; none
    /// without statistics or off a backend. An estimate: a wrong one only picks the slower of two forms with the same
    /// answers (lowerv4 count_cut.rs `pair_partition`'s route).
    fn att_top_share(&self, _relid: u32, _attno: i16, _rows: u64) -> Option<f64> {
        None
    }
    /// The widest width a parallel statement of this server runs at (`max_parallel_workers` within the driver's
    /// `MAX_WIDTH`; 0 the leader alone), none off a backend: a lowering cost rule's participants (KEYED-31's key form,
    /// lowerv4 dict/strategies/percode_guard.rs) and a decompose one's (ops/group/absorb.rs `drain_set_rows`; the front
    /// key, exec inputs/front.rs, carries it). The server's bound, not a session's `max_parallel_workers_per_gather`:
    /// the program a lowering makes serves every session through the process-wide program tier, which keys by the graph.
    /// Read on a miss as the stats are, so a stale width only picks the slower of two forms with the same answers.
    fn width(&self) -> Option<u16> {
        None
    }
    /// D3047: whether THIS session runs its statements on the leader alone (`max_parallel_workers_per_gather` 0, or a
    /// server bound of 0); none off a backend. Unlike `width`, a session's fact: exec inputs/front.rs keys it, so a rule
    /// reading it (ops/agg/gsets_tag.rs, the one-table form vs the spool) yields one graph per class, each its own program.
    fn serial(&self) -> Option<bool> {
        None
    }
    /// D4568: whether road `scan.mergefree` is on (on by default, D4585: off when `pgrust.gravity_road_off` names it
    /// plainly, lowerv4 road_off.rs `DEFAULT_OFF`): a merge mark whose order no reader above reads is dropped
    /// (ops/scan `map_seq_scan`) where the session runs wide (`serial` Some(false)). A lowering input no content key carries; the program tiers' key and the front key hold the
    /// road text (hosts residency.rs `tier_salt`). False off a backend.
    fn scan_mergefree(&self) -> bool {
        false
    }
    /// D4585: whether road `scan.mergefree.build` is on (default off: on only when `pgrust.gravity_road_off` names it
    /// `+scan.mergefree.build`): a hash build input's unread merge mark is dropped too (facts/order.rs `merge_free_under`).
    /// The road text keys the program tiers as `scan_mergefree`'s does. False off a backend.
    fn scan_mergefree_build(&self) -> bool {
        false
    }
    /// D4565 / D4567 / D4497 / D3227 K3 / D3227 (b) / D4620: whether on-by-default road `road` (one of
    /// `ROADS_DEFAULT_ON`: agg.stat_reg, agg.num_scale, the four nest.keep.*, nest.key.eq and
    /// agg.gsets.cascade; none on lowerv4 road_off.rs `DEFAULT_OFF`) is on: on unless this session's
    /// `pgrust.gravity_road_off` names it off plainly (`+<road>` changes nothing). Apart from `road_on`, whose default-off
    /// roads flip on their own entries. The road text keys the front key and the program tiers (D311). True off a
    /// backend, as on a server with the option unset.
    fn default_on_road_on(&self, _road: &str) -> bool {
        true
    }
    /// M6 D2 (L0-g): the `(kind, scale)` (`gravity_program::shred_kind`) a shredded path lane of jsonb column `attno` at
    /// the path of keys `keys` (outermost first) compiles at, from the relation's resident parts; none on any other source
    /// or when no part elected it (the value form). Never a correctness fact: a part that did not elect it at that kind
    /// and scale publishes the lane all-invalid, and those rows walk their image.
    fn shred_election(&self, _relid: u32, _attno: i16, _keys: &[&str]) -> Option<(u8, i32)> {
        None
    }
    /// SRC-30: whether every resident part that elected text path `keys` of jsonb column `attno` sealed it as the
    /// writer's executable dictionary, so the path's code lane opens that dictionary once a part; false where a part
    /// stored the path verbatim (its code lane is the part's byte ranks, about an entry a value, ranked at each open),
    /// when none elected it, and on any other source. lowerv4 jsonb.codes reads a compare beyond `=` / `<>` by code only
    /// then. Never a correctness fact: it picks one of two forms with the same answers.
    fn shred_dict(&self, _relid: u32, _attno: i16, _keys: &[&str]) -> bool {
        false
    }
    /// D3038: whether relation `relid`'s resident parts code column `attno` by dictionary where its rows are (pgrc's
    /// footer: codec `CODEC_DICT` in every part, or the parts holding nearly all its rows), so a per-code table over its
    /// code lane holds one entry a dictionary value there; false where its rows are mostly in verbatim parts (a code lane
    /// that ranks each part's values, about an entry a row), before a publish, and on any other source. lowerv4
    /// strategies/octet_lane.rs keeps such a column's `octet_length` on the per-code table. Never a correctness fact: it
    /// picks one of two forms with the same answers.
    fn att_dict(&self, _relid: u32, _attno: i16) -> bool {
        false
    }
    /// jsonops (3): whether every resident part that elected text path `keys` of jsonb column `attno` sealed it
    /// `STREAMF_SHRED_OBJECT_ROOTS` (each non-NULL document of the part an object, so the lane's validity is the key's
    /// presence), and one did; false on any other source. lowerv4 jsonb.lanes reads a `?` test from presence lanes only
    /// then. Never a correctness fact: an unflagged part publishes the presence lane all-invalid and its rows take
    /// PostgreSQL's test.
    fn shred_object_roots(&self, _relid: u32, _attno: i16, _keys: &[&str]) -> bool {
        false
    }
    /// sortpath: whether the relation declares jsonb path `keys` of column `attno` a `sorted_by` key (pgrc's declared
    /// key: its parts sealed in that order, so the path's zone records prune); false on any other source. lowerv4
    /// jsonb.lanes then declares a chain compare's path lane though the document is read past the filters (its zones
    /// skip granules a read in place cannot). Never a correctness fact: either form gives PostgreSQL's rows.
    fn shred_sorted(&self, _relid: u32, _attno: i16, _keys: &[&str]) -> bool {
        false
    }
    fn index(&self, indexid: u32) -> Option<IndexFact>;
    /// The index's access method; a provider that knows only `IndexFact.btree` answers `Other` for the rest (decompose.md §7 S1).
    fn index_am(&self, indexid: u32) -> IndexAm {
        match self.index(indexid) {
            Some(i) if i.btree => IndexAm::Btree,
            _ => IndexAm::Other,
        }
    }
    /// D3279: whether a lowered write may maintain the index through the generic `index_insert` (hosts write_index_am
    /// `insertable`: the core AMs and the extension AMs the server implements, by handler); a provider without the
    /// handler answers for the core AMs alone.
    fn index_insertable(&self, indexid: u32) -> bool {
        !matches!(self.index_am(indexid), IndexAm::Other)
    }
    /// Every index of a relation, in `indexid` order (the write plan's index list; D4).
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(IndexFact));
    /// `indexes_of` for a write's target (D582): false when some index's facts could not be read now (another session
    /// holds a lock on it that conflicts with reading them), and the write refuses: an index left out of the list would
    /// be one the write never maintains.
    fn indexes_all(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) -> bool {
        self.indexes_of(relid, f);
        true
    }
    /// The index's constraint and AM facts (D1 writes); `UNKNOWN` when the source has none.
    fn index_ext(&self, _indexid: u32) -> IndexExt {
        IndexExt::UNKNOWN
    }
    /// AGB-28 (`rows`): whether `binds` hold at most one row of index `indexid`'s relation: the index is UNIQUE, IMMEDIATE
    /// and VALID (partial only when `partial_ok`: a scan of the index reads only its own rows), and each of its key
    /// columns has a bind whose operator is the column's btree family's equality, under the column's collation (or none),
    /// and strict when the bind's value may be NULL. False on a source without the catalog (a fixture).
    fn unique_bound(&self, _indexid: u32, _binds: &[KeyBind], _partial_ok: bool) -> bool {
        false
    }
    /// D2063: the key columns' operator class facts (`IndexOpc`); `UNKNOWN` when the source has no catalog.
    fn index_opc(&self, _indexid: u32) -> IndexOpc {
        IndexOpc::UNKNOWN
    }
    /// `pg_attribute.attgenerated` (`s` stored, `v` virtual, 0 none); 0 when the source has none.
    fn att_generated(&self, _relid: u32, _attno: i16) -> u8 {
        0
    }
    /// The relation's INSTEAD OF row triggers (a view target, decompose.md §2.12); `UNKNOWN` when the source has none.
    /// The relation's triggers, rules, children and partition membership (`RelExt`); `UNKNOWN` when the source has none.
    fn rel_ext(&self, _relid: u32) -> RelExt {
        RelExt::UNKNOWN
    }
    fn partition_leaves(&self, _root: u32, _f: &mut dyn FnMut(LeafFact)) -> bool {
        false
    }
    /// D3585: heap relations `a` and `b` are read by one scan's code: both plain permanent heap tables of one physical
    /// layout (every attribute's type, typmod, length, alignment, storage, collation, nullability and generation the
    /// same, dropped ones included), no attribute with a missing value. A relation-set site (decompose ops/append.rs
    /// `group_twins`) reads only twins of its first relation. False when the source cannot say.
    fn heap_twin(&self, _a: u32, _b: u32) -> bool {
        false
    }
    /// D3681: heap relations `a` and `b` are written alike by one UPDATE / DELETE: `heap_twin`, neither with row security,
    /// the same CHECK constraints (name, expression, enforcement, validity, inheritance), the same column defaults and the
    /// same indexes in index-list order (access method, columns, operator families, collations, options, uniqueness,
    /// constraint forms, expressions and predicate). A write over a relation set (decompose ops/append.rs `relsets`)
    /// writes only such twins of its first relation. False when the source cannot say.
    fn write_twin(&self, _a: u32, _b: u32) -> bool {
        false
    }

    fn instead_of(&self, _relid: u32) -> InsteadOf {
        InsteadOf::UNKNOWN
    }
    fn trig_ext(&self, _relid: u32) -> crate::ops::modifytable::trig_facts::TrigExt {
        crate::ops::modifytable::trig_facts::TrigExt::UNKNOWN
    }
    /// The fingerprint of the expression an index's key column `col` (0-based position, `keys[col] == 0`)
    /// computes (`pg_index.indexprs`, its `Var`s over the indexed relation as range-table index 1): none for a
    /// plain column, an unknown index or an expression `expr_fp` does not cover (D5 facts, §7 l).
    fn index_expr(&self, indexid: u32, col: u8) -> Option<ExprFp>;
    /// D2313: the btree opfamily (`rd_opfamily`) of an index's key column `col` (0-based) when it is not its type's
    /// default family (a `text_pattern_ops` column orders by bytes under any collation), whose order an index scan
    /// declares under that family (`Order::fams`); 0 for the default family's, a column past the keys, an unknown index,
    /// or a source without the catalog (the default: a fixture).
    fn index_family(&self, _indexid: u32, _col: u8) -> u32 {
        0
    }
    /// D3752: the collation of an index's key column `col` (0-based, `rd_indcollation`; 0 for a type that has none); none
    /// for a column past the keys, an unknown index, or a source without the catalog (the default: a fixture).
    fn index_coll(&self, _indexid: u32, _col: u8) -> Option<u32> {
        None
    }
    /// D3752: whether sequence `seqid` cycles (`pg_sequence.seqcycle`); none for no sequence or a source without the
    /// catalog (the default: a fixture). One that does not returns distinct values or raises at every `nextval` call.
    fn seq_cycles(&self, _seqid: u32) -> Option<bool> {
        None
    }
    fn type_row(&self, _typid: u32) -> Option<TypeRow> {
        None
    }
    /// D3796: the constraints a coercion to domain `typid` checks (C `DomainConstraintRef`'s list, load_domaintype_info:
    /// those of every domain in its chain), each handed to `f` while the facts hold it: a CHECK's expression as the
    /// executor compiles it (planned: eval_const_expressions + fix_opfuncids), None for a NOT NULL. False when the facts
    /// cannot read them (the default: a fixture carries none), and the reader fails closed (`Found::DOMAIN_CHECK`).
    fn domain_checks(&self, _typid: u32, _f: &mut dyn for<'n> FnMut(Option<Node<'n>>)) -> bool {
        false
    }
    /// A range type's subtype (`pg_range.rngsubtype`, C `get_range_subtype`): none for a type that is no range, or a
    /// source without the catalog (the default: fixtures carry none).
    fn range_subtype(&self, _typid: u32) -> Option<u32> {
        None
    }
    /// D693: attribute `attno` (1-based) of composite type `typid`, through a domain to its composite base, as
    /// `lookup_rowtype_tupdesc` reads it; none for a dropped one, one past the last, a type that is no composite, or a
    /// source without the catalog (the default: no fixture or capture carries row types, so none moves).
    fn row_att(&self, _typid: u32, _attno: i16) -> Option<AttFact> {
        None
    }
    /// AGC-20: each live column of composite type `typid` (through a domain to its composite base) in attribute order, a
    /// dropped one skipped, as `lookup_rowtype_tupdesc` reads them; false when the type is no composite or its descriptor
    /// cannot be read, and on a source without the catalog (the default: no fixture carries row types, so none moves).
    fn row_cols(&self, _typid: u32, _f: &mut dyn FnMut(AttFact)) -> bool {
        false
    }
    /// D1901, V4R-508's in-row refusal (join.rs `key_decls`, agg.rs `map`): whether a key of type `typid` keys by a
    /// canonical form (gravity_fn canon.rs) holding a multirange at any level (the type, a domain's base, an array's
    /// element, a composite's column, a range's subtype). The default (no catalog: a fixture) answers false.
    fn key_holds_multirange(&self, _typid: u32) -> bool {
        false
    }
    /// D2163: what a key of type `typid` compares by under equality operator `opno` (gravity_fn typesem `eq_form`: the
    /// operator's procedure as fmgr resolves it against the type's own `=`). The default (no catalog: a fixture) answers
    /// `User`, so a user operator keeps D1901's multirange refusal and every other key its form.
    fn key_eq(&self, _opno: u32, _typid: u32) -> KeyEq {
        keysem::image::UNKNOWN_EQ
    }
    /// hj-0310 (D2640 serve): whether a key of type `typid` hashed under user equality operator `opno` and collation
    /// `coll` has a user key over the operator's own hash class procs (gravity_fn typesem `op_user_key`: PostgreSQL's
    /// hashed Agg buckets by its hash support and confirms by its procedure, which `m6_user_fp` calls the same way). The
    /// default (no catalog: a fixture) answers false, so the key keeps its refusal.
    fn op_key_hash(&self, _opno: u32, _typid: u32, _coll: u32) -> bool {
        false
    }
    /// KeySem slice 4 (D345 / D434): an equality operator's (left, right) types when its equal values are one image of each
    /// side's type (gravity_fn typesem `keysem::image`: the own `=` of int2 / int4 / int8 / oid / date / bool / char, the
    /// integer `=` across widths); read through `catalog::keysem::image_eq` / `image_eq_same`. The default (no catalog)
    /// answers none; a corpus fixture answers from its `-- imageeq` lines and the shared corpus/imageeq.facts.
    fn op_image_eq(&self, _opno: u32) -> Option<(u32, u32)> {
        None
    }
    /// An anonymous `record` constant's columns (facts::record: the planner folds a constant ROW() to one): the row type
    /// its datum's blessed typmod names in this backend's record cache (C `lookup_rowtype_tupdesc(RECORDOID, typmod)`),
    /// each column's (type, collation); none for a typmod the cache does not hold, a dropped column, or no catalog (the
    /// default: a fixture).
    fn record_cols(&self, _typmod: i32) -> Option<Vec<(u32, u32)>> {
        None
    }
    /// D1073: column `attno` (1-based) of FROM call `call`'s row as C `ExecInitFunctionScan` types it through
    /// `get_expr_result_type` (a function's OUT parameters, a composite type through a domain): (type, typmod,
    /// collation); none for a scalar or bare `record` call, a dropped column, one past the last, or no catalog.
    fn call_col(&self, _call: Node<'_>, _attno: i16) -> Option<(u32, i32, u32)> {
        None
    }
    /// D3374 (producerchain_fn_copy): whether column `attno` (1-based) of FROM call `call`'s row as C
    /// `ExecInitFunctionScan` types it (`get_expr_result_type`'s descriptor) is a dropped attribute of its row type, which
    /// `call_col` answers none for; false for a live one, one past the last, a scalar call, or no catalog.
    fn call_col_dropped(&self, _call: Node<'_>, _attno: i16) -> bool {
        false
    }
    fn result_life(&self, _proc_oid: u32, ret_byval: bool) -> gravity_program::FnLife {
        if ret_byval { gravity_program::FnLife::Value } else { gravity_program::FnLife::Call }
    }
    /// KeySem slice 3: what key `req` compares by, as PostgreSQL resolves it. `KeyReq::sort`: the ordering operator's
    /// `KeyOrder` (`get_ordering_op_properties`, the type cache's default btree family, the family's comparator and
    /// sortsupport for its input type); none for an operator that is no btree ordering operator or one the source never
    /// saw. The one order method a source implements, so two facts of one key cannot disagree: the direction table
    /// (`KeyOrder::dir`), another family's direction and family, the CompareFn comparator and the missing-proc raise are
    /// all read from it. A source that reads no catalog answers the bootstrap rows' (`opdir::builtin_key`: every family,
    /// no `pg_amproc`, so `cmp` 0 and `no_proc` false).
    fn key(&self, req: KeyReq) -> Option<KeyOrder> {
        opdir::builtin_key(req.op)
    }
    /// OID-07: whether `opno` is a btree equality (strategy 3 of some btree opfamily, `get_mergejoin_opfamilies`): equal
    /// under it is equal under the family's order. A source that reads no catalog answers the bootstrap catalog's
    /// (`opdir::builtin_eq`).
    fn op_eq(&self, opno: u32) -> bool {
        opdir::builtin_eq(opno)
    }
    /// KeySem (LANGUAGE internal; equivclass#53): the built-in C function a call of `funcid` runs (gravity_fn typesem
    /// `keysem::body`): a built-in is itself; a user function `LANGUAGE internal` is the built-in its `prosrc` names when
    /// PostgreSQL calls it exactly as it calls the built-in (one kind, arity and storage of every argument and the result,
    /// no SECURITY DEFINER or SET clause); none for any other function. The effects home reads a call by it
    /// (`effects::body`, which takes a built-in as itself before asking). A source that reads no catalog answers none.
    fn builtin_body(&self, _funcid: u32) -> Option<u32> {
        None
    }
    /// D3082 (clauses.c `inline_function`, read for effects only): `judge`'s answer over the body PostgreSQL's planner
    /// would substitute for a call of user `LANGUAGE sql` function `funcid` (gravity_fn `keysem::sql_inlined`: the
    /// planner's own parse and gates; its parameters left as PARAM_EXTERN leaves). The call stays the SQL function's.
    /// False when it is not inlinable or the inline fails; a source that reads no catalog answers false.
    fn sql_inlined(&self, _funcid: u32, _judge: &mut dyn FnMut(Node<'_>) -> bool) -> bool {
        false
    }
    /// D3713: `sql_inlined` past the SECURITY DEFINER and SET clause gates, the body fmgr_sql runs for a call when the
    /// SET clause names no search_path (gravity_fn `keysem::sql_fmgr_body`). False as `sql_inlined`.
    fn sql_fmgr_body(&self, _funcid: u32, _judge: &mut dyn FnMut(Node<'_>) -> bool) -> bool {
        false
    }
    /// KeySem: what a plan node naming operator `opno` alone runs (`OpCalls`, gravity_fn `keysem::op_calls`), asked for an
    /// operator the builtin facts (`oper`) do not hold. None without a catalog (the operator stays unknown).
    fn op_calls(&self, _opno: u32) -> Option<OpCalls> {
        None
    }
    /// Record-image order: type `typid`'s default btree opfamily (the typcache's `btree_opf`): a merge clause of another
    /// family asks that family's order of its side. None without a catalog (the clause read as the default's, as before).
    fn default_family(&self, _typid: u32) -> Option<u32> {
        None
    }
    /// A key's collation facts (D247): bytes order values iff PG's `collate_is_c` for it (libc "C" / "POSIX", any builtin
    /// locale; never libc "C.UTF-8" or ICU), equal bytes are equal values iff it is deterministic. The database's default
    /// collation is whatever the database was created with, so only a source that reads the catalog (`pg_locale`) knows
    /// it; one that does not answers `KeyColl::of`, which never claims byte order for the default.
    fn coll(&self, oid: u32) -> crate::catalog::typefacts::KeyColl {
        crate::catalog::typefacts::KeyColl::of(oid)
    }
    /// Whether the database encoding is UTF-8, where a byte match of valid text starts on a character boundary (normalize's
    /// `like_twin`). A source that does not read the encoding answers false.
    fn database_utf8(&self) -> bool {
        false
    }
    /// DC-14 (the effects home's constant-pattern rule, `effects::quiet_pattern`): regular expression `pattern` (a Const's
    /// bytes) compiles under `collation` with `~`'s ARE flags (`icase`: `~*`'s), as the first row's match compiles it, so
    /// the match raises on no row. A source that cannot compile one (a fixture) answers false: the match may raise.
    fn regex_compiles(&self, _pattern: &[u8], _icase: bool, _collation: u32) -> bool {
        false
    }
    /// FACTS-1b: a function's `provolatile` / `proisstrict` / `proleakproof` (`fnprops.rs`); none = unknown, and every fact
    /// that reads it is false. The default answers the builtin catalog only, for a fixture and the server alike.
    fn func(&self, funcid: u32) -> Option<crate::effects::FuncProps> {
        crate::effects::builtin_func(funcid)
    }
    /// D1018 (create_function_sql#92): `f` over function `funcid`'s pg_proc row as text (its namespace's name, its name, its
    /// language oid, its body) when the catalog has one; false without the catalog (the default) or the row.
    fn proc_text(&self, _funcid: u32, _f: &mut dyn FnMut(&[u8], &[u8], u32, &[u8]) -> bool) -> bool {
        false
    }
    /// FACTS-1b: an operator's function and hash / merge properties (`fnprops.rs`); none = unknown (no equality conjunct).
    fn oper(&self, opno: u32) -> Option<crate::effects::OperProps> {
        crate::effects::builtin_oper(opno)
    }
    /// D2036: the hash function PostgreSQL's hash join calls on each input of `opno` under collation `coll` (typesem's
    /// key: `get_op_hash_functions`, the operator's first hash family's FUNCTION 1 at the left and the right input type);
    /// none = unknown, no hash support or an indeterminate collation. The default answers none, so a hash join under an
    /// operator that is no builtin hashable equality refuses.
    fn op_hash_functions(&self, _opno: u32, _coll: u32) -> Option<(u32, u32)> {
        None
    }
    /// D2036: whether this provider reads hash functions at all. A plan capture (`corpus::FixtureFacts`) records none and
    /// is never run, so its user-operator hash joins keep the graph they had before D2036 (key images and the clause's
    /// confirm). Every other provider answers `op_hash_functions` or refuses by name.
    fn reads_hash_functions(&self) -> bool {
        true
    }
    /// An aggregate's `pg_aggregate` support functions (`AggSupport`). None = no row: unknown for an aggregate (read as
    /// VOLATILE), and a window function that is not an aggregate. The default answers none: a builtin aggregate's own
    /// pg_proc row already states its volatility, and a fixture carries no user aggregate's row.
    fn agg_support(&self, _aggfnoid: u32) -> Option<AggSupport> {
        None
    }
    /// D1071: a type's `(typinput, typoutput)` as `getTypeInputInfo` / `getTypeOutputInfo` read them from pg_type (a
    /// domain's typinput is domain_in and its typoutput its base's; an enum's enum_in / enum_out; a user base type's own
    /// functions), the two a CoerceViaIO calls (`check_functions_in_node`); `func` then reads their provolatile. None =
    /// unknown (read as VOLATILE). The default answers the builtin catalog only, for a fixture and the server alike.
    fn type_io(&self, typid: u32) -> Option<(u32, u32)> {
        crate::effects::builtin_type_io(typid)
    }
    /// D1214 (V4R-393): the relation's PRIMARY KEY columns, as `get_primary_key_attnos(relid, deferrable_ok = false)`
    /// reads them for `check_functional_grouping` (pg_constraint contype 'p', its conkey; a deferrable key is none).
    /// `(attnos, n)`; none without a usable key. The default reads the key's index: `indisprimary`, `indimmediate` and
    /// its `indnkeyatts` key columns are the constraint's (a fixture carries them on its `-- idx` line).
    fn primary_key(&self, relid: u32) -> Option<([i16; limits::KEYS], u8)> {
        let mut pk = None;
        self.indexes_of(relid, &mut |i| {
            let e = self.index_ext(i.indexid);
            if pk.is_none() && e.known && e.primary && !e.deferrable {
                let n = if e.nkeyatts == 0 { i.nkeys } else { e.nkeyatts.min(i.nkeys) };
                pk = Some((i.keys, n));
            }
        });
        pk.filter(|(k, n)| *n > 0 && k[..*n as usize].iter().all(|&a| a > 0))
    }
}

/// Whether `typid` is a pseudo-type: the catalog row's typtype; a source without the catalog answers the bootstrap catalog's,
/// where the builtin type table's pseudo-types are record and anyarray.
pub fn pseudo_type(f: &dyn Facts, typid: u32) -> bool {
    use types_core::catalog::{ANYARRAYOID, RECORDOID};
    f.type_row(typid).map_or(matches!(typid, RECORDOID | ANYARRAYOID), |r| r.typtype == b'p')
}

/// Whether `typid` is `record`, the anonymous row type: a value of it carries its columns in its datum (the blessed typmod
/// of the ROW() that formed it), so the type id names none (facts::record resolves them from the plan).
pub fn anonymous_record(typid: u32) -> bool {
    typid == types_core::catalog::RECORDOID
}

/// The k-th expression of an index's `indexprs` list, `k` = the expression columns before `col`.
pub fn nth_index_expr<'p>(idx: &IndexFact, exprs: &types_nodes::list::NodeList<'p>, col: u8) -> Option<Node<'p>> {
    let col = col as usize;
    if col >= idx.nkeys as usize || idx.keys[col] != 0 {
        return None;
    }
    let k = idx.keys[..col].iter().filter(|&&a| a == 0).count();
    (k < exprs.len()).then(|| exprs.nth(k))
}

/// The facts of a relation as the capture's header lines without the names (`rel`, `att`, `idx`, `idxexpr` with
/// the fingerprint): one rendering for every source, so a live server's facts and a fixture's compare as text.
pub fn render(facts: &dyn Facts, relid: u32, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    let Some(r) = facts.rel(relid) else { return writeln!(out, "rel {relid} none") };
    write!(out, "rel {relid} am={} kind={} natts={}", r.am.name(), r.relkind as char, r.natts)?;
    // the AM's whole-relation order rides only on a relation that has one (the capture prints it only then)
    for (i, a) in r.sorted_by().iter().enumerate() {
        write!(out, "{}{a}", if i == 0 { " sorted=" } else { "," })?;
    }
    if r.rows { // PGRCSERVE G4: on the rows road (`am=other` then)
        write!(out, " rows=t")?;
    }
    writeln!(out)?;
    for attno in 1..=r.natts {
        if let Some(a) = facts.att(relid, attno) {
            write!(out, "att {relid} {attno} typid={} typmod={} len={} byval={} notnull={}", a.typid, a.typmod, a.typlen, tf(a.byval), tf(a.notnull))?;
            // D2700: a domain's base rides only on a domain column (the capture prints it only then)
            if (a.base, a.base_typmod) != (a.typid, a.typmod) {
                write!(out, " base={}:{}", a.base, a.base_typmod)?;
            }
            // D1's `gen=` rides only on a generated column (the capture prints it only when set)
            match facts.att_generated(relid, attno) {
                0 => writeln!(out)?,
                g => writeln!(out, " gen={}", g as char)?,
            }
        }
    }
    // the INSTEAD OF row triggers ride only on a relation that has one (the capture prints the line only then)
    let t = facts.instead_of(relid);
    if t.known && t.any() {
        writeln!(out, "trig {relid} instead={t}")?;
    }
    let mut r = Ok(());
    facts.indexes_of(relid, &mut |i| {
        if r.is_err() {
            return;
        }
        r = (|| {
            write!(out, "idx {} rel={} keys=", i.indexid, i.relid)?;
            for (k, a) in i.keys().iter().enumerate() {
                write!(out, "{}{a}", if k > 0 { "," } else { "" })?;
            }
            write!(out, " opts=")?;
            for k in 0..i.nkeys as usize {
                write!(out, "{}{}", if k > 0 { "," } else { "" }, i.opts[k])?;
            }
            let e = facts.index_ext(i.indexid);
            writeln!(out, " unique={} am={} partial={} deferrable={} excl={} primary={} nkeyatts={}", tf(i.unique), facts.index_am(i.indexid).name(), tf(i.partial), tf(e.deferrable), tf(e.exclusion), tf(e.primary), e.nkeyatts)?;
            for k in 0..i.nkeys {
                if i.keys[k as usize] == 0 {
                    match facts.index_expr(i.indexid, k) {
                        Some(fp) => writeln!(out, "idxexpr {} {k} fp={:016x}{:016x}", i.indexid, fp.0, fp.1)?,
                        None => writeln!(out, "idxexpr {} {k} fp=none", i.indexid)?,
                    }
                }
            }
            Ok(())
        })();
    });
    r
}

const fn tf(b: bool) -> char {
    if b {
        't'
    } else {
        'f'
    }
}

pub struct NoFacts;

impl Facts for NoFacts {
    fn rel(&self, _: u32) -> Option<RelFact> {
        None
    }

    fn att(&self, _: u32, _: i16) -> Option<AttFact> {
        None
    }

    fn index(&self, _: u32) -> Option<IndexFact> {
        None
    }

    fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}

    fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
        None
    }
}

pub const TEXTOID: u32 = 25;
pub const VARCHAROID: u32 = 1043;
pub const BPCHAROID: u32 = 1042;
pub const NUMERICOID: u32 = 1700;

impl AttFact {
    /// A column that is no domain: its base is its own type.
    pub const fn plain(typid: u32, typmod: i32, typlen: i16, byval: bool, notnull: bool) -> AttFact {
        AttFact { typid, typmod, typlen, byval, notnull, base: typid, base_typmod: typmod }
    }

    /// D2700: text, varchar or bpchar, or a domain over one (its base's bytes).
    pub const fn is_text(&self) -> bool {
        matches!(self.base, TEXTOID | VARCHAROID | BPCHAROID)
    }

    /// D2700: a `numeric(p, s)` column's scale, a domain over one's included.
    pub const fn numeric_scale(&self) -> Option<u32> {
        if self.base == NUMERICOID && self.base_typmod >= 4 {
            Some(((self.base_typmod - 4) & 0xffff) as u32)
        } else {
            None
        }
    }

    /// D2700: a numeric column's facts from its typmod, a domain over `numeric(p, s)` by the domain's.
    pub const fn num_facts(&self) -> gravity_program::NumTypmod {
        if self.base == NUMERICOID { gravity_program::NumTypmod::of_typmod(self.base_typmod) } else { gravity_program::NumTypmod::NONE }
    }
}
