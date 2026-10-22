//! The front key (main 09-24, lever A; exec.md "Admission from the graph"): one walk of the PlannedStmt that keys what
//! `decompose_in` and `key_and_nodes` read, so a repeat shape (the simple protocol's literals differ, its plan does
//! not) takes its program and words without either. Equal front keys mean the same graph modulo words:
//! - every field of every node the walk knows is hashed, except the `location`s, `queryId` / `stmt_location` /
//!   `stmt_len` (they move with a literal's text) and a by-value Const's value (a word); a node the walk does not know
//!   makes no key, and the statement takes today's path;
//! - a by-value Const decompose reads as a value is hashed by where it sits (`Class`): always (`Key`: a LIMIT bound, a
//!   cast's typmod arguments, an ORDER BY operator's value, an index's own expressions, anything under a node that may
//!   compile to an interpreter island: gravity_expr `island_capable`, D337), or on a condition the fill
//!   decides from the facts (`Tlist`: projections matched against an expression index's fingerprints, `graph.rs`
//!   `Order::project`; `Zone`: a pgrc scan's zone predicates, `sources::zone`);
//! - the catalog decompose and the walk read (relcache: rel / att / index / index_am / indexes_of / index_ext /
//!   index_expr / att_generated / instead_of / partition_leaves; pg_proc: result_life; pg_amop: op_dir; pg_collation:
//!   coll; pg_type: type_row; pg_operator: plancache.c's set) is not keyed: C3 S4d: a variant holds what its admission
//!   read (gravity_deps, recorded from the walk to the fill), and a hit serves it only while those reads stand (and
//!   replays them into the admission's recording). The non-catalog inputs are keyed: the database and the road-off set
//!   (hosts residency.rs `tier_salt`, the program tiers' salt), the server's parallel width (hosts inputs.rs
//!   `parallel_width`, `Facts::width`: the participants decompose ops/group/absorb.rs `drain_set_rows` costs a draining
//!   join's DISTINCT set at) and the session's serial class. No other GUC is read; the stats (rel_rows / att_range /
//!   att_estimate) are the lowerers', on a miss.
//! - the planner's estimates are not hashed raw (D359: an MCV literal and each template's ANALYZE sample move them, the
//!   graph does not): the key carries what decompose derives from them at the nodes that read them (a range source's
//!   and a bitmap index's rows, a Hash's partition bits, a hash join's outer-first), and nothing for the rest.
//! `tests/front_census.rs` perturbs every by-value Const of the decompose corpus against this rule, and every node's
//! estimates against the last one; the shadow check
//! (`pgrust.gravity_front_check`, on in every harness) decomposes again on a hit and raises on any difference.

use core::cell::{Cell, RefCell};
use core::hash::{Hash, Hasher};
use std::collections::HashMap;

use gravity_abi::hash::Mix128;
use gravity_decompose::join::outer_first;
use gravity_decompose::walk::Builder;
use gravity_decompose::{Am, Facts};
use gravity_program::{ArenaRef, ArenaVec, ContentKey, PipeEst};
use types_nodes::bitmapset::Bitmapset;
use types_nodes::list::{IntList, NodeList, OidList};
use types_nodes::plannodes::{GravityRewrite, ModifyTable, NestLoop, Plan, PlannedStmt};
use types_nodes::primnodes::{CoercionForm, OUTER_VAR};
use types_nodes::{Node, NodeTag};

/// How a by-value Const's value is keyed where it sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Word,
    Key,
    Tlist,
    Zone,
}

/// Where the walk met a Const (the census test's listing): the plan node, its field, the parent expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    pub plan: NodeTag,
    pub field: &'static str,
    pub parent: NodeTag,
}

pub type Trace<'a, 'p> = &'a mut dyn FnMut(Node<'p>, Class, Site);

/// A statement's front key: `main` always, `tl` / `zone` the conditional classes' values; `nodes` every expression node
/// in walk order (a word's node is named by its position here).
pub struct Front<'p> {
    pub main: u128,
    pub tl: u128,
    pub zone: u128,
    pub nodes: ArenaVec<Node<'p>>,
    /// The admission's arena (`file`'s sorted positions, `Positions`).
    arena: ArenaRef,
    /// V4R-18: the positions of the non-null by-reference Consts outside the `Key` class, whose images the main key
    /// leaves out: a filed variant names those it baked (`fill`), a hit compares their images (`hit`).
    pub byref: ArenaVec<u32>,
    indexes: ArenaVec<u32>,
    zones: ArenaVec<u32>,
    orderby: bool,
    /// The admission's bake scopes (gravity_expr's IR reads, gravity_decompose's plan-tree reads), closed on drop.
    scopes: (gravity_expr::bake::Scope, gravity_decompose::bake::Scope),
    /// D4449: the positions of the ValuesScans' rows of Consts alone (lower key.rs `Runs`), which `alias` does not visit.
    tables: gravity_lower::inputs::key::Runs,
    /// PS1-05: the planner charges a hit decides a filed program's cost guards on (lower inputs/key.rs `costs_hold`): each
    /// range scan's `scan_charge` by plan node id and each InitPlan's `InitCharge`, read off the plan, outside the key.
    pub scans: ArenaVec<(i32, f64)>,
    pub inits: ArenaVec<gravity_decompose::graph::InitCharge<'p>>,
    /// D3104: each index morsel site's plan node id and estimate (decompose closure.rs `index_morsel`), in walk order,
    /// outside the key (which holds the form's bit): a hit of a variant filed with `Sites` binds this statement's.
    pub morsel: ArenaVec<(i32, u64)>,
}

impl Front<'_> {
    /// D4449: the walk's literal-table runs (the ValuesScans' rows of Consts alone, `alias`).
    pub fn tables(&self) -> &[(u32, u32)] {
        self.tables.as_slice()
    }

    /// The images this admission's decompose has logged as read so far (gravity_decompose's scope; the census's check).
    pub fn plan_reads(&self) -> Vec<usize> {
        self.scopes.1.reads()
    }

    /// The images this admission's lowering has logged as read so far (gravity_expr's scope).
    pub fn ir_reads(&self) -> Vec<usize> {
        self.scopes.0.reads()
    }
}

/// One lane of the front key (main, tlist, zone) or the IR hash: the hash home's word-at-a-time content key hasher (abi
/// `hash::Mix128`, the code cache's), each started from its own seed. Perf: an integer field is one word, not eight
/// byte rounds; the walk hashes every field of every plan node on each re-planned execution. The key lives only in
/// this backend's memo, so no stored value depends on the hasher.
struct Lane(Mix128);

impl Hasher for Lane {
    fn finish(&self) -> u64 {
        self.0.finish()
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    fn write_u8(&mut self, v: u8) {
        self.0.word(v as u64)
    }
    fn write_u16(&mut self, v: u16) {
        self.0.word(v as u64)
    }
    fn write_u32(&mut self, v: u32) {
        self.0.word(v as u64)
    }
    fn write_u64(&mut self, v: u64) {
        self.0.word(v)
    }
    fn write_u128(&mut self, v: u128) {
        self.0.word(v as u64);
        self.0.word((v >> 64) as u64)
    }
    fn write_usize(&mut self, v: usize) {
        self.0.word(v as u64)
    }
    fn write_i8(&mut self, v: i8) {
        self.0.word(v as u64)
    }
    fn write_i16(&mut self, v: i16) {
        self.0.word(v as u64)
    }
    fn write_i32(&mut self, v: i32) {
        self.0.word(v as u64)
    }
    fn write_i64(&mut self, v: i64) {
        self.0.word(v as u64)
    }
    fn write_isize(&mut self, v: isize) {
        self.0.word(v as u64)
    }
}

impl Lane {
    fn new(seed: u64) -> Lane {
        let mut m = Mix128::new();
        m.word(0x6672_6f6e_7400_0001 ^ seed);
        Lane(m)
    }

    fn get(&self) -> u128 {
        self.0.key128()
    }
}

struct W<'a, 'p> {
    h: Lane,
    tl: Lane,
    zone: Lane,
    f: Front<'p>,
    site: Site,
    trace: Option<Trace<'a, 'p>>,
    /// V4R-566 (`pin`): the plan nodes whose clauses key their Consts by value.
    pins: ArenaVec<usize>,
    /// PM3a: the first construct the walk does not key (`refusal`, the census of what makes no key).
    why: Option<Refusal>,
    stmt: &'p PlannedStmt<'p>,
    /// D3047: the session's serial class (`serial_session`), read once per walk: the index arms' morsel test and the key's
    /// salt read the same answer (no GUC moves during a walk).
    serial: Option<bool>,
}

/// Why a statement makes no front key: the plan or expression node and the field or construct the walk does not key.
pub type Refusal = (NodeTag, &'static str);

type R = Option<()>;

/// The statement's front key, or `None` when a node is outside the walk (today's path).
pub fn walk<'p>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, trace: Option<Trace<'_, 'p>>) -> Option<Front<'p>> {
    run(arena, stmt, trace).ok()
}

/// PM3a: the first construct that keeps `walk` from keying the statement, `None` when it keys.
pub fn refusal<'p>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>) -> Option<Refusal> {
    run(arena, stmt, None).err()
}

/// normalize `retype`'s session-zone casts (B2-31 / STEP-13): PG's own casts in this backend's TimeZone, read when each
/// execution converts its constant (the walk's `retype_class` never reads them), `None` where PG raises; the zone's
/// identity is its `pg_tz` (pgtz.c caches one per name), which `SET TimeZone` swaps. A named portal (DECLARE's cursor, a
/// PL/pgSQL cursor, a named protocol portal) takes its rows over several FETCHes, so its compares stay cross-type.
const ZONE: gravity_decompose::normalize::Zone = gravity_decompose::normalize::Zone {
    date_tstz: |d| {
        let mut e = gravity_fn::ErrSlot::NONE;
        zone_ok(gravity_fn::shims::byval::date_timestamptz(&mut e, i64::from(d)))
    },
    ts_tstz: |t| {
        let mut e = gravity_fn::ErrSlot::NONE;
        zone_ok(gravity_fn::shims::byval::timestamp_timestamptz(&mut e, t))
    },
    tstz_date: |t| {
        let mut e = gravity_fn::ErrSlot::NONE;
        zone_ok(gravity_fn::shims::byval::timestamptz_date(&mut e, t)).map(|d| d as i32)
    },
    spread: zone_spread,
    stamp: || pgtz::with_session_timezone(|z| z.map_or(0, |z| std::ptr::from_ref(z) as usize)),
    // B2-31: never for a reader whose rows come over several runs (a named cursor's portal, a counted or zone restart's start)
    active: || !crate::admit::entry::zone_off() && pquery::ActivePortal().is_none_or(|p| p.borrow().name.is_empty()),
};

/// SRC-07 (normalize `implied`): the least and greatest UTC offset (microseconds) the session zone takes at any instant
/// within `IMPLIED_WINDOW` of timestamptz `t`: the offset in effect at the window's start and each side of every DST
/// boundary in it (pgtz's `pg_next_dst_boundary`, as PG's `DetermineTimeZoneOffset` reads them). `None` past the zone
/// data or with no session zone.
fn zone_spread(t: i64) -> Option<(i64, i64)> {
    use localtime::NextDstBoundary;
    const UNIX_AT_PG_EPOCH: i64 = 946_684_800;
    const USECS: i64 = 1_000_000;
    let w = gravity_decompose::normalize::IMPLIED_WINDOW / USECS;
    let at = t.div_euclid(USECS).checked_add(UNIX_AT_PG_EPOCH)?;
    let (mut from, to) = (at.checked_sub(w)?, at.checked_add(w)?);
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    // the zone borrowed, not a handle (the shared Arc's count is every worker's cache line)
    pgtz::with_session_timezone(|tz| {
        let tz = tz?;
        // a zone changes offset a few times a year: 64 boundaries in six days is a zone this does not read
        for _ in 0..64 {
            match localtime::pg_next_dst_boundary(from, tz) {
                NextDstBoundary::Overflow => return None,
                NextDstBoundary::NoTransition { before_gmtoff, .. } => return Some((lo.min(before_gmtoff) * USECS, hi.max(before_gmtoff) * USECS)),
                NextDstBoundary::Boundary(b) => {
                    (lo, hi) = (lo.min(b.before_gmtoff), hi.max(b.before_gmtoff));
                    if b.boundary > to {
                        return Some((lo * USECS, hi * USECS));
                    }
                    (lo, hi) = (lo.min(b.after_gmtoff), hi.max(b.after_gmtoff));
                    from = b.boundary;
                }
            }
        }
        None
    })
}

fn zone_ok(r: gravity_fn::Ret) -> Option<i64> {
    (r.status == gravity_fn::OK).then_some(r.val)
}

/// The front walk's node list's first capacity: an RTT or point statement walks 6 to 30 nodes.
const NODES_AT_START: usize = 32;

fn run<'p>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, trace: Option<Trace<'_, 'p>>) -> Result<Front<'p>, Refusal> {
    gravity_decompose::normalize::set_zone(ZONE);
    let scopes = (gravity_expr::bake::Scope::open(), gravity_decompose::bake::Scope::open());
    // the walk's nodes start at a short statement's count: one block in place of a growth per doubling
    let f = Front { main: 0, tl: 0, zone: 0, nodes: ArenaVec::with_capacity_in(NODES_AT_START, arena), arena, byref: ArenaVec::new_in(arena), indexes: ArenaVec::new_in(arena), zones: ArenaVec::new_in(arena), orderby: false, scopes, scans: ArenaVec::new_in(arena), inits: ArenaVec::new_in(arena), morsel: ArenaVec::new_in(arena), tables: gravity_lower::inputs::key::Runs::NONE };
    let site = Site { plan: NodeTag::T_Invalid, field: "", parent: NodeTag::T_Invalid };
    // D3047: the session's serial class (`Facts::serial`, read by decompose's grouping-sets form pick)
    let serial = gravity_hosts::inputs::serial_session();
    let mut w = W { h: Lane::new(0), tl: Lane::new(1), zone: Lane::new(2), f, site, trace, pins: ArenaVec::new_in(arena), why: None, stmt, serial };
    if w.stmt(stmt).is_none() {
        // a node of an unexpected type where the walk expects another (a cast failed): named by the site the walk was at
        return Err(w.why.unwrap_or((w.site.plan, w.site.field)));
    }
    alias(w.f.arena, &w.f.nodes, w.f.tables.as_slice(), &mut w.h);
    let width = gravity_hosts::inputs::parallel_width();
    // C3 S4d: no catalog epoch: a variant serves while what its admission read stands (`Entry::deps`); the database and
    // the road-off set (D311) are the program tiers' salt
    (gravity_hosts::ops::scan::pgrc::residency::tier_salt(), width, serial).hash(&mut w.h);
    gravity_hosts::ops::scan::am_rows::hash_scoped(&mut w.h);
    (w.f.main, w.f.tl, w.f.zone) = (w.h.get(), w.tl.get(), w.zone.get());
    Ok(w.f)
}

/// A whole qual that is a boolean Const (`WHERE false`, `ON false`, a Result's one-time gate) decides whether its node
/// yields rows at all, and the lowering reads it (an empty input, a dropped gate): keyed by value, never a word.
/// DC-14: a constant-pattern match's class, the answer decompose's effects read (`effects::quiet_pattern`: the pattern is
/// read there without the bake log), with the server's own regex facts (`gravity_fn::collation::regex_compiles`, as
/// `RelcacheFacts` answers it). The collation's own answers (`coll != 0`, `bytes_eq`) are not read here: the caller keys
/// `inputcollid` (and the database), so they never split one key. A front entry over a good pattern is never hit by a
/// statement whose pattern raises, and LIKE over another good literal still hits it. None: no constant-pattern match.
fn pattern_class(funcid: u32, args: &NodeList<'_>, coll: u32) -> Option<bool> {
    use gravity_decompose::effects::{pattern_good, pattern_operand, Pattern};
    let (form, pat) = pattern_operand(funcid, args)?;
    Some(pattern_good(form, pat, coll, true, || gravity_fn::typesem::collation::regex_compiles(pat, form == Pattern::Regex(true), coll)))
}

/// D3788 (DC-14 over a list): `x op ANY / ALL (c)`'s class, whether every element of its constant pattern list is good
/// (decompose effects `pattern_list_good`, the answer `quiet_pattern_list` reads), with the server's own regex facts, as
/// `pattern_class`; lower key.rs holds the same. The list's values stay words (V4R-110). None: no constant pattern list.
fn pattern_list_class(funcid: u32, args: &NodeList<'_>, coll: u32) -> Option<bool> {
    gravity_decompose::effects::pattern_list_good(funcid, args, coll, true, &|pat, icase| gravity_fn::typesem::collation::regex_compiles(pat, icase, coll))
}

fn gate(n: Node<'_>, c: Class) -> Class {
    match n.as_const() {
        Some(k) if k.consttype == types_core::catalog::BOOLOID => Class::Key,
        _ => c,
    }
}

/// D325: a node the plan shares between two places (one equivalence class's Const under two quals when their constants
/// are equal, `a = 1 AND b = 1`) is another shape than two nodes of one value (`a = 1 AND b = 2`): decompose's key walk
/// takes a shared node as one word, and `fill` names each key-walk node by a position of it, so a hit of the other shape
/// bound one constant's word for both columns. The key carries the walk's aliasing: each repeat of a node (by identity)
/// with its previous position, in position order, then the number of repeats. D3642: the copies of one literal are one
/// node, as the content key holds them (gravity_lower key.rs `lit_share`), and their positions are put apart
/// (`alias_shares`): a hit names each key-walk node by identity, so one node twice and two copies are two shapes.
/// D4449: the repeats in one pass (`alias_repeats`: the pairs and order the identity sort gave), and `file` builds its
/// own lookup by address (`Positions`).
/// D4449: `tables`' positions (the ValuesScans' rows of Consts alone) are not visited (key.rs `alias_walk`: each such cell
/// is its list's own node, so what is put is unchanged).
/// D4600 (`admit::lean`): the walk's passes fused into one (key.rs `alias_walk_fused`, which puts what `alias_walk` puts).
fn alias(arena: ArenaRef, nodes: &[Node<'_>], tables: &[(u32, u32)], h: &mut Lane) {
    use gravity_lower::inputs::key::AliasPut;
    let walk = if crate::admit::lean::on() { gravity_lower::inputs::key::alias_walk_fused } else { gravity_lower::inputs::key::alias_walk };
    walk(arena, nodes, tables, &mut |p| match p {
        AliasPut::Pair(i, j) => (i, j).hash(h),
        AliasPut::Count(n) => n.hash(h),
        AliasPut::Share(v) => v.hash(h),
    });
}

/// The front walk's node positions by address, as `file` and `derived_at` look them up. D4449: a table where no
/// address repeats (the one position the binary search found), else the sorted (address, position) list (D3615 (a)) and
/// its binary search, so a node at two places resolves to the position it did before.
enum Positions {
    Table(gravity_lower::inputs::key::PosTable),
    Sorted(ArenaVec<(usize, u32)>),
}

impl Positions {
    fn of(nodes: &[Node<'_>], arena: ArenaRef) -> Positions {
        let mut t = gravity_lower::inputs::key::PosTable::with_capacity(nodes.len()); // alloc: Cold
        for (i, n) in nodes.iter().enumerate() {
            if t.put(n.as_raw().as_ptr() as usize as u64, i as u32).is_some() {
                let mut v = ArenaVec::with_capacity_in(nodes.len(), arena);
                v.extend(nodes.iter().enumerate().map(|(i, n)| (n.as_raw().as_ptr() as usize, i as u32)));
                v.sort_unstable();
                return Positions::Sorted(v);
            }
        }
        Positions::Table(t)
    }

    fn get(&self, a: usize) -> Option<u32> {
        match self {
            Positions::Table(t) => t.get(a as u64),
            Positions::Sorted(v) => v.binary_search_by_key(&a, |e| e.0).ok().map(|i| v[i].1),
        }
    }
}

impl<'p> W<'_, 'p> {
    /// A construct the walk does not key: the statement makes no key, and the census learns which.
    fn no(&mut self, tag: NodeTag, what: &'static str) -> R {
        self.why.get_or_insert((tag, what));
        None
    }

    fn put<T: Hash>(&mut self, v: T) {
        v.hash(&mut self.h);
    }

    /// The set's words as `Hash for [u64]` feeds them (its length, then each word: `u64::hash_slice` writes the words'
    /// bytes, which `Mix128::write` reads back a word at a time), without the byte pass.
    fn bms(&mut self, b: &Bitmapset<'_>) {
        let words = b.as_words();
        self.h.write_usize(words.len());
        words.iter().for_each(|&w| self.h.0.word(w));
    }

    fn ints(&mut self, l: &IntList<'_>) {
        self.put(l.len());
        l.iter().for_each(|i| i.hash(&mut self.h));
    }

    fn oids(&mut self, l: &OidList<'_>) {
        self.put(l.len());
        l.iter().for_each(|i| i.hash(&mut self.h));
    }

    fn stmt(&mut self, s: &'p PlannedStmt<'p>) -> R {
        let st = NodeTag::T_PlannedStmt;
        if s.utilityStmt.is_some() {
            return self.no(st, "utilityStmt");
        }
        // D487 (M3-MERGE): an inheritance / partition expansion's AppendRelInfos (carry.rs reads parent / child relid),
        // every field keyed
        self.put(s.appendRelations.len());
        for n in s.appendRelations.iter() {
            let a = n.as_append_rel_info()?;
            self.put((a.parent_relid, a.child_relid, a.parent_reltype, a.child_reltype, a.num_child_cols, a.parent_colnos, a.parent_reloid));
        }
        self.put((s.commandType as u32, s.planId, s.hasReturning, s.hasModifyingCTE, s.canSetTag, s.transientPlan, s.dependsOnRole, s.parallelModeNeeded, s.jitFlags));
        self.put(s.rtable.len());
        for n in s.rtable.iter() {
            let r = n.as_range_tbl_entry()?;
            // setrefs.c add_rte_to_flat_rtable zaps the sub-structure: decompose reads the scans' own fields
            let rte = NodeTag::T_RangeTblEntry;
            let fields = [(r.tablesample.is_some(), "tablesample"), (r.subquery.is_some(), "subquery"), (!r.functions.is_nil(), "functions"), (r.tablefunc.is_some(), "tablefunc"), (!r.values_lists.is_nil(), "values_lists"), (!r.joinaliasvars.is_nil(), "joinaliasvars"), (!r.groupexprs.is_nil(), "groupexprs"), (!r.securityQuals.is_nil(), "securityQuals")];
            if let Some(&(_, field)) = fields.iter().find(|f| f.0) {
                return self.no(rte, field);
            }
            self.put((r.rtekind as u32, r.relid, r.inh, r.relkind, r.rellockmode, r.perminfoindex, r.security_barrier, r.jointype as u32, r.joinmergedcols, r.funcordinality, r.ctelevelsup, r.self_reference));
            self.put((r.lateral, r.inFromCl, r.ctename, r.enrname)); // enrtuples: an estimate nothing reads (D359)
            self.ints(&r.joinleftcols);
            self.ints(&r.joinrightcols);
            // mergecmd record-row cache: a record row's column names are the RTE's eref (C ExecTypeSetColNames: decompose
            // subplan::RecordRow; the content key hashes them, key.rs `record_rows`), a relation's are its own (relcache)
            if !matches!(r.rtekind, types_nodes::parsenodes::RTEKind::RTE_RELATION | types_nodes::parsenodes::RTEKind::RTE_JOIN) {
                let names = r.eref.map(|e| &e.colnames);
                self.put(names.map_or(0, |n| n.len()));
                for c in names.iter().flat_map(|n| n.iter()) {
                    self.put(c.as_string().map(|x| x.sval));
                }
            }
            self.oids(&r.coltypes);
            self.ints(&r.coltypmods);
            self.oids(&r.colcollations);
        }
        self.bms(&s.unprunableRelids);
        self.put(s.permInfos.len());
        for n in s.permInfos.iter() {
            let p = n.as_rte_permission_info()?;
            self.put((p.relid, p.inh, p.requiredPerms, p.checkAsUser));
            [&p.selectedCols, &p.insertedCols, &p.updatedCols].into_iter().for_each(|b| self.bms(b));
        }
        self.ints(&s.resultRelations);
        self.bms(&s.rewindPlanIDs);
        self.marks(&s.rowMarks)?;
        self.oids(&s.relationOids);
        self.put(s.invalItems.len());
        for n in s.invalItems.iter() {
            let i = n.as_plan_inval_item()?;
            self.put((i.cacheId, i.hashValue));
        }
        self.oids(&s.paramExecTypes);
        // PM3a: runtime partition pruning (append.rs `map_prune` / `prune_of`): every field of every info and step, the
        // steps' expressions keyed by value
        self.put(s.partPruneInfos.len());
        for n in s.partPruneInfos.iter() {
            let info = n.as_partition_prune_info()?;
            self.bms(&info.relids);
            self.bms(&info.other_subplans);
            self.put(info.prune_infos.len());
            for l in info.prune_infos.iter() {
                let l = l.as_list()?;
                self.put(l.len());
                for r in l.iter() {
                    let r = r.as_partitioned_rel_prune_info()?;
                    self.put((r.rtindex, r.nparts, r.subplan_map, r.subpart_map, r.leafpart_rti_map, r.relid_map));
                    self.bms(&r.present_parts);
                    self.bms(&r.execparamids);
                    self.steps(&r.initial_pruning_steps)?;
                    self.steps(&r.exec_pruning_steps)?;
                }
            }
        }
        // PM3a: the SubPlans' trees, by plan_id (a SubPlan node names its tree by position here)
        self.put(s.subplans.len());
        for r in s.subplans.iter() {
            self.put(r.is_some());
            if let Some(r) = r {
                self.plan(r)?;
            }
        }
        self.plan(s.planTree?)
    }

    /// PM3a: a pgrust planner rewrite annotation (decompose executes it), every field.
    fn rewrite(&mut self, r: Option<GravityRewrite>) {
        self.put(r.map(|r| match r {
            GravityRewrite::ZoneTopK { attno, desc, nulls_first } => (0u8, attno, desc, nulls_first),
            GravityRewrite::ZoneWalk { attno, desc, nulls_first } => (1, attno, desc, nulls_first),
            GravityRewrite::TwoPass { attno, floor_desc, floor_nulls_first } => (2, attno, floor_desc, floor_nulls_first),
        }));
    }

    fn steps(&mut self, l: &NodeList<'p>) -> R {
        let tag = NodeTag::T_PartitionPruneInfo;
        self.put(l.len());
        for n in l.iter() {
            if let Some(o) = n.as_partition_prune_step_op() {
                self.put((o.step_id, o.opstrategy));
                self.oids(&o.cmpfns);
                self.bms(&o.nullkeys);
                self.exprs(tag, "pruning_steps", &o.exprs, Class::Key)?;
            } else {
                let c = n.as_partition_prune_step_combine()?;
                self.put((c.step_id, c.combineOp));
                self.ints(&c.source_stepids);
            }
        }
        Some(())
    }

    fn marks(&mut self, l: &NodeList<'p>) -> R {
        self.put(l.len());
        for n in l.iter() {
            let m = n.as_plan_row_mark()?;
            self.put((m.rti, m.prti, m.rowmarkId, m.markType as u32, m.allMarkTypes, m.strength as u32, m.waitPolicy as u32, m.isParent));
        }
        Some(())
    }

    fn plan(&mut self, n: Node<'p>) -> R {
        stack_depth_core::check_stack_depth_or_panic();
        use NodeTag::*;
        let (tag, p) = (n.node_tag(), n.as_plan()?);
        // no startup_cost / total_cost / plan_rows: the arms below key what decompose derives from them (D359)
        self.put((tag as u16, p.disabled_nodes, p.plan_width, p.parallel_aware, p.parallel_safe, p.async_capable, p.plan_node_id));
        self.bms(&p.extParam);
        self.bms(&p.allParam);
        // PM3a: an initPlan is a SubPlan node (its tree is the statement's `subplans` entry)
        self.exprs(tag, "initPlan", &p.initPlan, Class::Word)?;
        // PS1-05: its planner charge, which a cost guard decides on at a hit (`Front::inits`), outside the key
        for s in p.initPlan.iter().filter_map(|n| n.as_sub_plan()) {
            self.f.inits.push(gravity_decompose::graph::InitCharge::of(s));
        }
        // a WindowAgg's projections are its window functions' program: decompose keys their constants by value (window.rs)
        // grouping sets: decompose matches the projections and quals against the grouping columns by their images (agg/mod.rs)
        let gsets = n.as_agg().is_some_and(|a| !a.groupingSets.is_nil());
        self.exprs(tag, "targetlist", &p.targetlist, if tag == T_WindowAgg || gsets { Class::Key } else { Class::Tlist })?;
        let zoned = matches!(tag, T_SeqScan | T_BitmapHeapScan);
        let fill = n.as_cte_scan().is_some_and(|c| c.gravity_fill);
        let word = if !self.pins.is_empty() && self.pins.contains(&(p as *const Plan<'p> as usize)) { Class::Key } else if fill { Class::Zone } else { Class::Word };
        self.exprs(tag, "qual", &p.qual, if zoned { Class::Zone } else if gsets { Class::Key } else { word })?;
        match tag {
            T_Material => {}
            T_Result => self.opt(tag, "resconstantqual", n.as_result()?.resconstantqual, Class::Word)?,
            T_SeqScan => {
                let s = n.as_seq_scan()?;
                // the range source's predicted rows (sources.rs `map_seq`; an Agg's TwoPass pass 1, agg.rs `pass1`), exact: a front hit
                // binds without a graph, so it cannot re-run the program's estimate guards (lower inputs/key.rs `guards_hold`)
                self.put((s.scan.scanrelid, s.cb_scan_cols.is_some(), Builder::rows(p)));
                // sortmerge: a marked scan is a merged site on its mark (decompose `map_seq_scan`), another program than the
                // unmarked scan's; an unmarked scan's key bytes are as before
                if !s.pgrc_merge.is_empty() {
                    self.put(s.pgrc_merge);
                }
                s.cb_scan_cols.iter().for_each(|b| self.bms(b));
                self.f.zones.push(s.scan.scanrelid);
                // PS1-05: what a cost guard weighs an eager InitPlan against at a hit (`Front::scans`), outside the key
                self.f.scans.push((p.plan_node_id, gravity_decompose::graph::scan_charge(p)));
            }
            // PM3a: a FROM-clause function (srf.rs `map_function_scan`): each RangeTblFunction, every field
            T_FunctionScan => {
                let s = n.as_function_scan()?;
                self.put((s.scan.scanrelid, s.funcordinality, s.functions.len()));
                for f in s.functions.iter() {
                    let r = f.as_range_tbl_function()?;
                    self.put((r.funccolcount, r.funccolnames.len()));
                    for c in r.funccolnames.iter() {
                        self.put(c.as_string().map(|x| x.sval));
                    }
                    self.oids(&r.funccoltypes);
                    self.ints(&r.funccoltypmods);
                    self.oids(&r.funccolcollations);
                    self.bms(&r.funcparams);
                    self.opt(tag, "functions", r.funcexpr, word)?;
                }
            }
            T_IndexScan => {
                let s = n.as_index_scan()?;
                self.put((s.scan.scanrelid, s.indexid, s.indexorderdir));
                // an order-free reader's scan may take the bitmap morsel form (decompose closure.rs `index_morsel`): its
                // estimate test (the threshold of the session's class, as decompose's `index_morsel_min_rows`; the class is
                // in the key, D3047). D3104: past it, the eligible pipeline's predicted rows are not keyed but kept beside
                // the key (`Front::morsel`): a variant filed with `Sites` takes this statement's at a hit, else equal ones
                let wide = self.serial == Some(false);
                let min = if wide { gravity_program::cost::INDEX_MORSEL_MIN_ROWS_WIDE } else { gravity_program::cost::INDEX_MORSEL_MIN_ROWS };
                let morsel = p.plan_rows >= min;
                self.put(morsel);
                if morsel {
                    self.f.morsel.push((p.plan_node_id, Builder::rows(p)));
                }
                self.f.indexes.push(s.indexid);
                self.f.orderby |= !s.indexorderby.is_nil();
                self.exprs(tag, "indexqual", &s.indexqual, word)?;
                self.exprs(tag, "indexqualorig", &s.indexqualorig, word)?;
                self.exprs(tag, "indexorderby", &s.indexorderby, Class::Key)?;
                self.exprs(tag, "indexorderbyorig", &s.indexorderbyorig, Class::Key)?;
                self.oids(&s.indexorderbyops);
            }
            T_IndexOnlyScan => {
                let s = n.as_index_only_scan()?;
                self.put((s.scan.scanrelid, s.indexid, s.indexorderdir));
                // an order-free reader's index-only walk may be read as a range (decompose closure.rs `index_only_full`,
                // through `index_morsel`'s estimate test) or, in a session with workers, as the pool's leaf walk
                // (`index_only_leaves`, the wide class's test): the test of the session's class (the class is in the key,
                // D3047); past it, the predicted rows are kept beside the key (`Front::morsel`, D3104): a variant is hit only
                // by equal rows (neither form is the morsel's)
                let wide = self.serial == Some(false);
                let min = if wide { gravity_program::cost::INDEX_MORSEL_MIN_ROWS_WIDE } else { gravity_program::cost::INDEX_MORSEL_MIN_ROWS };
                let morsel = p.plan_rows >= min;
                self.put(morsel);
                if morsel {
                    self.f.morsel.push((p.plan_node_id, Builder::rows(p)));
                }
                self.f.indexes.push(s.indexid);
                self.f.orderby |= !s.indexorderby.is_nil();
                self.exprs(tag, "indexqual", &s.indexqual, word)?;
                self.exprs(tag, "recheckqual", &s.recheckqual, word)?;
                self.exprs(tag, "indexorderby", &s.indexorderby, Class::Key)?;
                self.exprs(tag, "indextlist", &s.indextlist, Class::Key)?;
            }
            T_BitmapIndexScan => {
                let s = n.as_bitmap_index_scan()?;
                self.put((s.scan.scanrelid, s.indexid, s.isshared, Builder::rows(p))); // BitmapPlan::Index rows (closure.rs)
                self.f.indexes.push(s.indexid);
                self.exprs(tag, "indexqual", &s.indexqual, Class::Word)?;
                self.exprs(tag, "indexqualorig", &s.indexqualorig, Class::Word)?;
            }
            T_BitmapHeapScan => {
                let s = n.as_bitmap_heap_scan()?;
                self.put(s.scan.scanrelid);
                // the morsel form's estimate test (decompose closure.rs `morsel_form`) and, past it, the eligible pipeline's
                // predicted rows, exact as a SeqScan's
                let morsel = p.plan_rows >= gravity_program::cost::BITMAP_MORSEL_MIN_ROWS;
                self.put((morsel, if morsel { Builder::rows(p) } else { 0 }));
                self.f.zones.push(s.scan.scanrelid);
                self.exprs(tag, "bitmapqualorig", &s.bitmapqualorig, Class::Zone)?;
            }
            T_BitmapAnd | T_BitmapOr => {
                let (plans, shared) = match n.as_bitmap_and() {
                    Some(a) => (&a.bitmapplans, false),
                    None => n.as_bitmap_or().map(|o| (&o.bitmapplans, o.isshared))?,
                };
                self.put((plans.len(), shared));
                for c in plans.iter() {
                    self.plan(c)?;
                }
            }
            // D487 (M3-MERGE): a MergeAppend keys its sort keys, its arms and its prune index (decompose's append merge)
            T_MergeAppend => {
                let m = n.as_merge_append()?;
                self.bms(&m.apprelids);
                self.put(m.part_prune_index);
                self.put((m.numCols, m.sortColIdx, m.sortOperators, m.collations, m.nullsFirst, m.mergeplans.len()));
                for c in m.mergeplans.iter() {
                    self.plan(c)?;
                }
            }
            // PM3a: an Append keys its arms, its ordered-Append mark and its prune index (decompose's append)
            T_Append => {
                let m = n.as_append()?;
                self.bms(&m.apprelids);
                self.put((m.part_prune_index, m.nasyncplans, m.first_partial_plan, m.gravity_sort_col_idx, m.gravity_sort_operators, m.gravity_nulls_first, m.appendplans.len()));
                for c in m.appendplans.iter() {
                    self.plan(c)?;
                }
            }
            // PM3a: a SubqueryScan keys its subquery's tree
            T_SubqueryScan => {
                let q = n.as_subquery_scan()?;
                self.put((q.scan.scanrelid, q.scanstatus, q.subplan.is_some()));
                if let Some(c) = q.subplan {
                    self.plan(c)?;
                }
            }
            // PM3a: a ValuesScan keys each row's expressions
            T_ValuesScan => {
                let v = n.as_values_scan()?;
                self.put((v.scan.scanrelid, v.values_lists.len()));
                let (start, mut consts) = (self.f.nodes.len() as u32, true);
                for row in v.values_lists.iter() {
                    let row = row.as_list()?;
                    consts &= row.iter().all(|c| c.as_const().is_some());
                    self.exprs(tag, "values_lists", row, word)?;
                }
                // D4449: rows of Consts alone: each cell a node of its own (`alias`)
                if consts {
                    self.f.tables.add(start, self.f.nodes.len() as u32, &v.values_lists as *const NodeList<'_> as usize);
                }
            }
            // PM3a: a WindowAgg, every field (window.rs; its frame offsets, run condition and projections are values decompose reads)
            T_WindowAgg => {
                let w = n.as_window_agg()?;
                self.put((w.winname, w.winref, w.partNumCols, w.partColIdx, w.partOperators, w.partCollations, w.ordNumCols, w.ordColIdx, w.ordOperators, w.ordCollations));
                self.put((w.frameOptions, w.startInRangeFunc, w.endInRangeFunc, w.inRangeColl, w.inRangeAsc, w.inRangeNullsFirst, w.topWindow));
                self.opt(tag, "startOffset", w.startOffset, Class::Key)?;
                self.opt(tag, "endOffset", w.endOffset, Class::Key)?;
                self.exprs(tag, "runCondition", &w.runCondition, Class::Key)?;
                self.exprs(tag, "runConditionOrig", &w.runConditionOrig, Class::Key)?;
            }
            // PM3a: a ProjectSet (no fields of its own: its target list's set-returning functions are walked)
            T_ProjectSet => {}
            // PM3a: a Unique (every field)
            T_Unique => {
                let u = n.as_unique()?;
                // its rows size the distinct record (agg/mod.rs `close`)
                self.put((u.numCols, u.uniqColIdx, u.uniqOperators, u.uniqCollations, Builder::rows(p)));
            }
            // PM3a: a Memoize (every field; its est_entries sizes decompose's memo, join.rs)
            T_Memoize => {
                let m = n.as_memoize()?;
                self.put((m.numKeys, m.hashOperators, m.collations, m.singlerow, m.binary_mode, m.est_entries));
                self.bms(&m.keyparamids);
                self.exprs(tag, "param_exprs", &m.param_exprs, Class::Word)?;
            }
            // PM3a: a SetOp (every field; the rows decompose sizes each side by, setop.rs)
            T_SetOp => {
                let o = n.as_set_op()?;
                let rows = |c: Option<Node<'p>>| c.and_then(|c| c.as_plan()).map_or(0, Builder::rows);
                self.put((o.cmd, o.strategy, o.numCols, o.cmpColIdx, o.cmpOperators, o.cmpCollations, o.cmpNullsFirst, o.numGroups, rows(p.lefttree), rows(p.righttree)));
            }
            // PM3a: an IncrementalSort (a Sort's fields and its presorted count)
            T_IncrementalSort => {
                let s = n.as_incremental_sort()?;
                self.rewrite(s.sort.gravity_rewrite);
                self.put((s.sort.numCols, s.sort.sortColIdx, s.sort.sortOperators, s.sort.collations, s.sort.nullsFirst, s.nPresortedCols));
            }
            // PM3a: a Group (every field)
            T_Group => {
                let g = n.as_group()?;
                // its rows size the group record (agg/mod.rs `close`)
                self.put((g.numCols, g.grpColIdx, g.grpOperators, g.grpCollations, Builder::rows(p)));
            }
            // PM3a: a TidScan and a TidRangeScan (their quals; the range scan's rows size its pipeline, closure.rs)
            T_TidScan => {
                let s = n.as_tid_scan()?;
                self.put(s.scan.scanrelid);
                self.exprs(tag, "tidquals", &s.tidquals, word)?;
            }
            T_TidRangeScan => {
                let s = n.as_tid_range_scan()?;
                self.put((s.scan.scanrelid, Builder::rows(p)));
                self.exprs(tag, "tidrangequals", &s.tidrangequals, word)?;
            }
            // PM3a: a SampleScan (its TABLESAMPLE method and arguments, keyed by value)
            T_SampleScan => {
                let s = n.as_sample_scan()?;
                self.put((s.scan.scanrelid, s.tablesample.is_some()));
                if let Some(t) = s.tablesample {
                    let t = t.as_table_sample_clause()?;
                    self.put(t.tsmhandler);
                    self.exprs(tag, "tablesample", &t.args, Class::Key)?;
                    self.opt(tag, "tablesample", t.repeatable, Class::Key)?;
                }
            }
            // PM3a: a CteScan, a WorkTableScan, a RecursiveUnion and a NamedTuplestoreScan (every field)
            T_CteScan => {
                let s = n.as_cte_scan()?;
                self.put((s.scan.scanrelid, s.ctePlanId, s.cteParam));
                if s.gravity_fill {
                    self.put(1u8);
                }
            }
            T_WorkTableScan => {
                let s = n.as_work_table_scan()?;
                self.put((s.scan.scanrelid, s.wtParam));
            }
            T_RecursiveUnion => {
                let r = n.as_recursive_union()?;
                self.put((r.wtParam, r.numCols, r.dupColIdx, r.dupOperators, r.dupCollations, r.numGroups));
            }
            T_NamedTuplestoreScan => {
                let s = n.as_named_tuplestore_scan()?;
                self.put((s.scan.scanrelid, s.enrname));
            }
            // PM3a: a TableFuncScan (every TableFunc field but the location; its expressions keyed by value like an island's)
            T_TableFuncScan => {
                let s = n.as_table_func_scan()?;
                self.put((s.scan.scanrelid, s.tablefunc.is_some()));
                if let Some(t) = s.tablefunc {
                    let t = t.as_table_func()?;
                    self.put((t.functype, t.ordinalitycol, t.ns_names.len(), t.colnames.len()));
                    for a in t.ns_names.iter() {
                        self.put(a.and_then(|a| a.as_string()).map(|s| s.sval));
                    }
                    for a in t.colnames.iter() {
                        self.put(a.as_string().map(|s| s.sval));
                    }
                    self.oids(&t.coltypes);
                    self.ints(&t.coltypmods);
                    self.oids(&t.colcollations);
                    self.bms(&t.notnulls);
                    self.exprs(tag, "tablefunc", &t.ns_uris, Class::Key)?;
                    self.opt(tag, "tablefunc", t.docexpr, Class::Key)?;
                    self.opt(tag, "tablefunc", t.rowexpr, Class::Key)?;
                    for l in [&t.colexprs, &t.coldefexprs, &t.colvalexprs] {
                        self.put(l.len());
                        for e in l.iter() {
                            self.opt(tag, "tablefunc", e, Class::Key)?;
                        }
                    }
                    self.exprs(tag, "tablefunc", &t.passingvalexprs, Class::Key)?;
                    self.opt(tag, "tablefunc", t.plan, Class::Key)?;
                }
            }
            T_Limit => {
                let l = n.as_limit()?;
                self.opt(tag, "limitOffset", l.limitOffset, Class::Key)?;
                self.opt(tag, "limitCount", l.limitCount, Class::Key)?;
                // D3005: a count over an extern parameter is this execution's constant (decompose `ops::bound`), which the
                // content key holds: so does this key
                let (c, o) = gravity_decompose::admit::limit_externs(l.limitCount, l.limitOffset);
                for v in [o, c].into_iter().flatten() {
                    (0xD3u8, v).hash(&mut self.h);
                }
                self.put((l.limitOption as u32, l.uniqNumCols, l.uniqColIdx, l.uniqOperators, l.uniqCollations));
            }
            T_Sort => {
                let s = n.as_sort()?;
                self.rewrite(s.gravity_rewrite);
                self.put((s.numCols, s.sortColIdx, s.sortOperators, s.collations, s.nullsFirst));
            }
            T_Agg => {
                let a = n.as_agg()?;
                self.rewrite(a.gravity_rewrite);
                // the aggregate algebra's finals (finals.cse reads them), every field
                self.put(a.gravity_finals.len());
                for f in a.gravity_finals {
                    self.put((f.resno, f.min, f.max, f.sum, f.count));
                }
                self.put((a.aggstrategy, a.aggsplit, a.numCols, a.grpColIdx, a.grpOperators, a.grpCollations, a.numGroups, a.transitionSpace, a.distinctHashed));
                self.bms(&a.aggParams);
                // PM3a: grouping sets: each set's columns and the chain's Aggs (agg/mod.rs walks both)
                self.put(a.groupingSets.len());
                for set in a.groupingSets.iter() {
                    match set.as_int_list() {
                        Some(l) => self.ints(l),
                        None => self.put(set.as_list().filter(|l| l.is_nil()).map(|_| usize::MAX)?), // the empty set ()
                    }
                }
                self.put(a.chain.len());
                for c in a.chain.iter() {
                    self.plan(c)?;
                }
            }
            T_ModifyTable => self.modify(n.as_modify_table()?)?,
            T_LockRows => {
                let l = n.as_lock_rows()?;
                self.put(l.epqParam);
                self.marks(&l.rowMarks)?;
            }
            T_NestLoop => {
                let j = n.as_nest_loop()?;
                // a catalog lookup's build rows (join.rs `map_catalog`: its inner's, V4R-481)
                let inner = p.righttree.and_then(|c| c.as_plan()).map_or(0, Builder::rows);
                self.put((j.join.jointype as u32, j.join.inner_unique, j.nestParams.len(), inner));
                self.exprs(tag, "joinqual", &j.join.joinqual, Class::Word)?;
                for x in j.nestParams.iter() {
                    let np = x.as_nest_loop_param()?;
                    self.put(np.paramno);
                    // the NestLoopParam itself: decompose's key walk visits each Nest param node (key.rs `JoinDesc::Nest`),
                    // so `file` places it by identity (without it every parameterized nest, a Memoize's too, filed nothing)
                    self.f.nodes.push(x);
                    self.site = Site { plan: tag, field: "nestParams", parent: tag };
                    self.expr(np.paramval, Class::Word, tag)?;
                }
                self.pin(j);
            }
            T_HashJoin => {
                let j = n.as_hash_join()?;
                // JoinDesc::Probe outer_first (join.rs), from the outer's startup and the Hash's total cost
                let cost = |c: Option<Node<'p>>, f: fn(&types_nodes::plannodes::Plan<'p>) -> f64| c.and_then(|c| c.as_plan()).map_or(0.0, f);
                let first = outer_first(j.join.jointype, p.parallel_aware, cost(p.lefttree, |o| o.startup_cost), cost(p.righttree, |h| h.total_cost));
                self.put((j.join.jointype as u32, j.join.inner_unique, first));
                self.exprs(tag, "joinqual", &j.join.joinqual, Class::Word)?;
                self.exprs(tag, "hashclauses", &j.hashclauses, word)?;
                self.oids(&j.hashoperators);
                self.oids(&j.hashcollations);
                self.exprs(tag, "hashkeys", &j.hashkeys, Class::Word)?;
            }
            // PM3a: a MergeJoin, every field (its merge clauses as a hash join's clauses)
            T_MergeJoin => {
                let j = n.as_merge_join()?;
                self.put((j.join.jointype as u32, j.join.inner_unique, j.skip_mark_restore, j.mergeFamilies, j.mergeCollations, j.mergeReversals, j.mergeNullsFirst));
                self.exprs(tag, "joinqual", &j.join.joinqual, Class::Word)?;
                self.exprs(tag, "mergeclauses", &j.mergeclauses, word)?;
            }
            T_Hash => {
                let x = n.as_hash()?;
                // the build's rows (join.rs `build_rows`, V4R-481; its partition bits derive from them); rows_total is an
                // estimate nothing reads
                self.put((x.skewTable, x.skewColumn, x.skewInherit, Builder::rows(p)));
                self.exprs(tag, "hashkeys", &x.hashkeys, Class::Word)?;
            }
            _ => return self.no(tag, "plan node"),
        }
        for c in [p.lefttree, p.righttree] {
            self.put(c.is_some());
            if let Some(c) = c {
                self.plan(c)?;
            }
        }
        Some(())
    }

    /// V4R-566 (decompose demand/readers.rs `uniq`): a loop over an Agg one of whose parameters reads an Agg entry that is a bare
    /// column outside its keys (a superset of the loops decompose rewrites) keys by value the Consts that rewrite compares:
    /// the inner index leaf's clauses and qual, and the Agg's hash join child's clauses or its nest child's index leaf's.
    fn pin(&mut self, j: &'p NestLoop<'p>) {
        let Some(a) = j.join.plan.lefttree.filter(|n| n.node_tag() == NodeTag::T_Agg).and_then(|n| n.as_agg()) else { return };
        let keyed = |c: i16| a.grpColIdx.iter().take(a.numCols.max(0) as usize).any(|&k| k == c);
        let passenger = |r: i16| a.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == r).and_then(|t| t.expr.as_var()).is_some_and(|v| v.varno == OUTER_VAR && !keyed(v.varattno));
        if !j.nestParams.iter().filter_map(|x| x.as_nest_loop_param()).any(|x| x.paramval.as_var().is_some_and(|v| v.varno == OUTER_VAR && passenger(v.varattno))) {
            return;
        }
        let leaf = |n: Option<Node<'p>>| n.filter(|n| matches!(n.node_tag(), NodeTag::T_IndexScan | NodeTag::T_IndexOnlyScan)).and_then(|n| n.as_plan()).map(|p| p as *const Plan<'p> as usize);
        let child = a.plan.lefttree.and_then(|c| match c.node_tag() {
            NodeTag::T_HashJoin => c.as_plan().map(|p| p as *const Plan<'p> as usize),
            NodeTag::T_NestLoop => leaf(c.as_plan()?.righttree),
            _ => None,
        });
        self.pins.extend(leaf(j.join.plan.righttree).into_iter().chain(child));
    }

    fn modify(&mut self, m: &'p ModifyTable<'p>) -> R {
        let tag = NodeTag::T_ModifyTable;
        // an FDW target's private list is opaque here: only the NIL entries of local targets
        if !m.fdwDirectModifyPlans.is_empty() || !m.fdwPrivLists.iter().all(|l| l.as_list().is_some_and(|l| l.is_nil())) {
            return self.no(tag, "fdwPrivLists");
        }
        self.put((m.operation as u32, m.canSetTag, m.nominalRelation, m.rootRelation, m.partColsUpdated, m.epqParam, m.onConflictAction, m.exclRelRTI, m.fdwPrivLists.len()));
        self.put((m.returningOldAlias, m.returningNewAlias));
        self.ints(&m.resultRelations);
        self.put(m.updateColnosLists.len());
        for l in m.updateColnosLists.iter() {
            self.ints(l.as_int_list()?);
        }
        self.put(m.returningLists.len());
        for l in m.returningLists.iter() {
            self.exprs(tag, "returningLists", l.as_list()?, Class::Tlist)?;
        }
        self.marks(&m.rowMarks)?;
        self.oids(&m.arbiterIndexes);
        self.exprs(tag, "onConflictSet", &m.onConflictSet, Class::Tlist)?;
        self.ints(&m.onConflictCols);
        self.opt(tag, "onConflictWhere", m.onConflictWhere, Class::Word)?;
        // PM3a: MERGE: each target's actions, every field, and its join condition
        self.put(m.mergeActionLists.len());
        for l in m.mergeActionLists.iter() {
            let l = l.as_list()?;
            self.put(l.len());
            for a in l.iter() {
                let a = a.as_merge_action()?;
                self.put((a.matchKind, a.commandType as u32, a.r#override));
                self.ints(&a.updateColnos);
                self.opt(tag, "mergeActionLists", a.qual, Class::Word)?;
                self.exprs(tag, "mergeActionLists", &a.targetList, Class::Tlist)?;
            }
        }
        self.exprs(tag, "mergeJoinConditions", &m.mergeJoinConditions, Class::Word)?;
        // PM3a: each target's check options, every field
        self.put(m.withCheckOptionLists.len());
        for l in m.withCheckOptionLists.iter() {
            let l = l.as_list()?;
            self.put(l.len());
            for w in l.iter() {
                let w = w.as_with_check_option()?;
                self.put((w.kind as u32, w.relname, w.polname, w.cascaded));
                self.opt(tag, "withCheckOptionLists", w.qual, Class::Word)?;
            }
        }
        self.exprs(tag, "exclRelTlist", &m.exclRelTlist, Class::Tlist)
    }

    fn exprs(&mut self, plan: NodeTag, field: &'static str, l: &NodeList<'p>, c: Class) -> R {
        self.site = Site { plan, field, parent: plan };
        self.put(l.len());
        l.iter().try_for_each(|n| self.expr(n, gate(n, c), plan))
    }

    fn opt(&mut self, plan: NodeTag, field: &'static str, n: Option<Node<'p>>, c: Class) -> R {
        self.site = Site { plan, field, parent: plan };
        self.put(n.is_some());
        n.map_or(Some(()), |n| self.expr(n, gate(n, c), plan))
    }

    fn args(&mut self, l: &NodeList<'p>, c: Class, parent: NodeTag) -> R {
        self.put(l.len());
        l.iter().try_for_each(|n| self.expr(n, c, parent))
    }

    fn arg(&mut self, n: Option<Node<'p>>, c: Class, parent: NodeTag) -> R {
        self.put(n.is_some());
        n.map_or(Some(()), |n| self.expr(n, c, parent))
    }

    fn expr(&mut self, n: Node<'p>, c: Class, parent: NodeTag) -> R {
        stack_depth_core::check_stack_depth_or_panic();
        use NodeTag::*;
        // an island's constants are its program's own, never words (D337: key.rs hashes them by the same predicate)
        let c = if gravity_expr::nodes::island_capable(n) { Class::Key } else { c };
        self.f.nodes.push(n);
        let tag = n.node_tag();
        self.put(tag as u16);
        match tag {
            T_Var => {
                let x = n.as_var()?;
                self.put((x.varno, x.varattno, x.vartype, x.vartypmod, x.varcollid, x.varlevelsup, x.varreturningtype as u32, x.varnosyn, x.varattnosyn));
                self.bms(&x.varnullingrels);
            }
            T_Const => {
                let k = n.as_const()?;
                self.put((k.consttype, k.consttypmod, k.constcollid, k.constlen, k.constisnull, k.constbyval));
                if k.constisnull {
                } else if !k.constbyval {
                    match super::raw::image(k) {
                        // V4R-18: the image is the variant's (`hit` compares the baked ones), not the shape's, unless
                        // the class keys it by value (an island's: the program owns it, D337)
                        Some(_) if c != Class::Key => {
                            self.f.byref.push(self.f.nodes.len() as u32 - 1);
                            // NEW-72 entry C: whether a varlena literal is `''`, which a lowering decides on unbaked (a code
                            // lane's `<> ''` forms) and the content key holds beside a bound text comparand's class. In the
                            // conditional `Zone` class: a code lane is a pgrc scan's, so only a statement with one reads it
                            // (`needs`); elsewhere `''` stays a word of the shape's one variant (V4R-18: a projected `|| ''`)
                            if let Some(empty) = gravity_decompose::expr::const_text_empty(k) {
                                (0xE0u8, empty).hash(&mut self.zone);
                            }
                        }
                        Some(b) => self.put(b),
                        None => self.put(k.constvalue.as_u64()),
                    }
                } else {
                    let v = k.constvalue.as_u64();
                    match c {
                        Class::Word => {}
                        Class::Key => v.hash(&mut self.h),
                        Class::Tlist => v.hash(&mut self.tl),
                        Class::Zone => v.hash(&mut self.zone),
                    }
                    let site = Site { parent, ..self.site };
                    if let Some(t) = self.trace.as_mut() {
                        t(n, c, site);
                    }
                }
            }
            T_Param => {
                let x = n.as_param()?;
                self.put((x.paramkind as u32, x.paramid, x.paramtype, x.paramtypmod, x.paramcollid));
            }
            T_OpExpr => {
                let o = n.as_op_expr()?;
                self.put((o.opno, o.opfuncid, o.opresulttype, o.opretset, o.opcollid, o.inputcollid));
                // D1103: an integer modulus's or division's divisor class, as the content key holds it (effects `divisor_class`)
                if let Some(q) = gravity_decompose::effects::divisor_class(o.opfuncid, &o.args, &gravity_decompose::effects::plan_word) {
                    self.put(q);
                }
                // E2: a strpos / position compare's class, as the content key holds it (percode `textpos_class`)
                if let Some(q) = gravity_expr::percode::textpos_class(o.opfuncid, &o.args) {
                    self.put(q);
                }
                // D3374: a size reader's int4 compare class (percode `sized_class`), as the content key holds it
                if let Some(q) = gravity_expr::percode::sized_class(o.opfuncid, &o.args) {
                    self.put(q);
                }
                // DC-14: a constant-pattern match's class (`pattern_class`), never its literal
                if let Some(q) = pattern_class(o.opfuncid, &o.args, o.inputcollid) {
                    self.put(q);
                }
                // normalize's classes of this compare, from one `commute` match (`op_classes`)
                let cls = gravity_decompose::normalize::op_classes(n);
                // `retype`'s class: whether the constant converts to the other side's type (a retyped constant is filed as
                // this constant's value converted per execution, `file`), never its literal
                if let Some(q) = cls.retype {
                    self.put(q);
                }
                // `detext`'s class (D3079): whether the text constant is an integer's canonical output text (the integer is
                // filed as this statement's constant parsed per execution, `file`), never its literal
                if let Some(q) = cls.detext {
                    self.put((0xde7eu16, q));
                }
                // `identity`'s class (D3078): whether `x op k`'s constant is the operator's unit (the rule drops it), never
                // its literal
                if let Some(q) = cls.identity {
                    self.put((0x1de7u16, q));
                }
                // `implied`'s class (SRC-07): whether a leading timestamptz compare gains its implied same-type bound (its
                // constant filed as this one's converted per execution), never its literal
                if let Some(q) = cls.implied {
                    self.put((0x1d5eu16, q));
                }
                // `like_twin`'s class: its by-value inputs (its pattern is filed as this statement's needle's, `file`; the
                // needle's image is a logged read, so the variant carries it)
                if let Some(q) = cls.twin {
                    self.put(q);
                }
                // `coalesce_filter`'s class: whether `COALESCE(x, c) op k`'s two constants let it drop the COALESCE (both
                // stay words), never their literals
                if let Some(q) = cls.coalesce {
                    self.put((0xc0a1u16, q));
                }
                // NEW-72: a SeqScan qual's top-level `col op c` of decompose's `bound_form` shape (lower key.rs
                // `zone_constants` skips just these; an OR's arms stay its zone hull's, baked): decompose reads no such
                // constant and lowerv4 binds it per execution, so it is a word, never the zone class's value
                let top = parent == T_SeqScan && self.site.field == "qual";
                let c = if c == Class::Zone && top && bound_word(n) { Class::Word } else { c };
                self.args(&o.args, c, tag)?;
            }
            T_DistinctExpr => {
                let o = n.as_distinct_expr()?;
                self.put((o.opno, o.opfuncid, o.opresulttype, o.opretset, o.opcollid, o.inputcollid));
                self.args(&o.args, c, tag)?;
            }
            T_NullIfExpr => {
                let o = n.as_null_if_expr()?;
                self.put((o.opno, o.opfuncid, o.opresulttype, o.opretset, o.opcollid, o.inputcollid));
                self.args(&o.args, c, tag)?;
            }
            T_ScalarArrayOpExpr => {
                let o = n.as_scalar_array_op_expr()?;
                self.put((o.opno, o.opfuncid, o.hashfuncid, o.negfuncid, o.useOr, o.inputcollid));
                // V4R-110: a constant list's class (gravity_expr konst.rs: its count to MAX_LIST, one class past), the
                // shape its words take, as lower/src/key.rs keys it; an image whose count it cannot read, by its bytes.
                // The image stays a by-reference word: a variant's only where a lowering logged it baked.
                if let Some(k) = o.args.nth(1).as_const().filter(|k| !k.constisnull && !k.constbyval) {
                    match super::raw::list_len(k) {
                        Some(len) => self.put(gravity_expr::nodes::konst::list_class(len)),
                        None => match super::raw::image(k) {
                            Some(i) => self.put(i),
                            None => return self.no(tag, "a by-reference Const of no image"),
                        },
                    }
                }
                // D3788: a constant pattern list's class (`pattern_list_class`), never its values
                if let Some(q) = pattern_list_class(o.opfuncid, &o.args, o.inputcollid) {
                    self.put((0xd378u16, q));
                }
                self.args(&o.args, c, tag)?;
            }
            T_FuncExpr => {
                let x = n.as_func_expr()?;
                self.put((x.funcid, x.funcresulttype, x.funcretset, x.funcvariadic, x.funcformat as u32, x.funccollid, x.inputcollid, x.args.len()));
                if let Some(q) = gravity_decompose::effects::divisor_class(x.funcid, &x.args, &gravity_decompose::effects::plan_word) {
                    self.put(q); // D1103, as T_OpExpr
                }
                if let Some(q) = gravity_expr::percode::textpos_class(x.funcid, &x.args) {
                    self.put(q); // E2, as T_OpExpr
                }
                if let Some(q) = gravity_lower::inputs::key::series_step(x.funcid, &x.args) {
                    self.put(q); // D1108 row 1: a generate_series step's class (lowerv4 spool_quiet.rs)
                }
                if let Some(q) = gravity_decompose::effects::text_arg_class(x.funcid, &x.args, &gravity_decompose::effects::plan_word) {
                    self.put(q); // substr's length / left, right's n class, as the content key holds it
                }
                if let Some(q) = gravity_decompose::effects::bit_index_class(x.funcid, &x.args, &gravity_decompose::effects::plan_word) {
                    self.put(q); // D3604: get_bit's index class, as the content key holds it
                }
                if let Some(q) = pattern_class(x.funcid, &x.args, x.inputcollid) {
                    self.put(q); // DC-14, as T_OpExpr
                }
                if let Some(q) = gravity_decompose::normalize::twin_class(n) {
                    self.put(q); // normalize `like_twin`'s class, as T_OpExpr
                }
                // a cast's trailing arguments are its typmod / explicitness: part of the type, never a literal's word; so is
                // round / trunc (numeric, int4)'s scale (PS4-13: decompose reads it as the result's dscale)
                let cast = matches!(x.funcformat, CoercionForm::COERCE_EXPLICIT_CAST | CoercionForm::COERCE_IMPLICIT_CAST) || gravity_decompose::catalog::casts::numeric_typmod_cast(x.funcid) || gravity_decompose::catalog::numeric::numeric_scale_arg(x.funcid);
                for (i, a) in x.args.iter().enumerate() {
                    self.expr(a, if cast && i > 0 { Class::Key } else { c }, tag)?;
                }
            }
            T_BoolExpr => {
                let x = n.as_bool_expr()?;
                self.put(x.boolop as u32);
                self.args(&x.args, c, tag)?;
            }
            T_NullTest => {
                let x = n.as_null_test()?;
                self.put((x.nulltesttype as u32, x.argisrow));
                self.arg(x.arg, c, tag)?;
            }
            T_BooleanTest => {
                let x = n.as_boolean_test()?;
                self.put(x.booltesttype as u32);
                self.arg(x.arg, c, tag)?;
            }
            T_RelabelType => {
                let x = n.as_relabel_type()?;
                self.put((x.resulttype, x.resulttypmod, x.resultcollid, x.relabelformat as u32));
                self.expr(x.arg, c, tag)?;
            }
            T_CoerceViaIO => {
                let x = n.as_coerce_via_io()?;
                self.put((x.resulttype, x.resultcollid, x.coerceformat as u32));
                self.expr(x.arg, c, tag)?;
            }
            T_TargetEntry => {
                let t = n.as_target_entry()?;
                self.put((t.resno, t.resname, t.ressortgroupref, t.resorigtbl, t.resorigcol, t.resjunk));
                self.expr(t.expr, c, tag)?;
            }
            T_CaseExpr => {
                let x = n.as_case_expr()?;
                self.put((x.casetype, x.casecollid));
                self.arg(x.arg, c, tag)?;
                self.args(&x.args, c, tag)?;
                self.arg(x.defresult, c, tag)?;
            }
            T_CaseWhen => {
                let x = n.as_case_when()?;
                self.arg(x.expr, c, tag)?;
                self.arg(x.result, c, tag)?;
            }
            T_CaseTestExpr => {
                let x = n.as_case_test_expr()?;
                self.put((x.typeId, x.typeMod, x.collation));
            }
            T_CoalesceExpr => {
                let x = n.as_coalesce_expr()?;
                self.put((x.coalescetype, x.coalescecollid));
                self.args(&x.args, c, tag)?;
            }
            T_MinMaxExpr => {
                let x = n.as_min_max_expr()?;
                self.put((x.minmaxtype, x.minmaxcollid, x.inputcollid, x.op as u32));
                self.args(&x.args, c, tag)?;
            }
            T_ArrayExpr => {
                let x = n.as_array_expr()?;
                self.put((x.array_typeid, x.array_collid, x.element_typeid, x.multidims));
                self.args(&x.elements, c, tag)?;
            }
            T_SubscriptingRef => {
                // every field by name: a field this arm does not read breaks the build (the key covers what the program
                // reads, D337 / D341). key.rs binds the container's and the element's facts and which bounds are there (an
                // open slice bound is no child); the container, the subscripts and an assigned value are the fetch site's
                // arguments, their constants words of the class the node sits in.
                let &types_nodes::primnodes::SubscriptingRef { refcontainertype, refelemtype, refrestype, reftypmod, refcollid, ref refupperindexpr, ref reflowerindexpr, refexpr, refassgnexpr } = n.as_subscripting_ref()?;
                self.put((refcontainertype, refelemtype, refrestype, reftypmod, refcollid));
                for l in [refupperindexpr, reflowerindexpr] {
                    self.put(l.len());
                    for e in l.iter() {
                        self.arg(e, c, tag)?;
                    }
                }
                self.arg(refexpr, c, tag)?;
                self.arg(refassgnexpr, c, tag)?;
            }
            T_SQLValueFunction => {
                let x = n.as_sql_value_function()?;
                self.put((x.op as u32, x.r#type, x.typmod));
            }
            T_Aggref => {
                let a = n.as_aggref()?;
                self.put((a.aggfnoid, a.aggtype, a.aggcollid, a.inputcollid, a.aggtranstype, a.aggstar, a.aggvariadic, a.aggkind, a.aggpresorted, a.agglevelsup, a.aggsplit, a.aggno));
                self.put(a.aggtransno);
                self.oids(&a.aggargtypes);
                self.args(&a.aggdirectargs, c, tag)?;
                self.args(&a.args, c, tag)?;
                self.args(&a.aggorder, c, tag)?;
                self.args(&a.aggdistinct, c, tag)?;
                self.arg(a.aggfilter, c, tag)?;
            }
            // PM3a: every field but the costs (an AlternativeSubPlan's choice reads those; it stays outside the walk)
            T_SubPlan => {
                let x = n.as_sub_plan()?;
                self.put((x.subLinkType as u32, x.plan_id, x.plan_name, x.firstColType, x.firstColTypmod, x.firstColCollation, x.useHashTable, x.unknownEqFalse, x.parallel_safe));
                self.ints(&x.paramIds);
                self.ints(&x.setParam);
                self.ints(&x.parParam);
                // a hashed SubPlan's build rows (subplan/mod.rs: its spill bits derive from them)
                if x.useHashTable {
                    let root = usize::try_from(x.plan_id - 1).ok().and_then(|i| (i < self.stmt.subplans.len()).then(|| self.stmt.subplans.nth(i)).flatten());
                    self.put(root.and_then(|r| r.as_plan()).map_or(0, Builder::rows));
                }
                self.arg(x.testexpr, c, tag)?;
                self.args(&x.args, c, tag)?;
            }
            T_WindowFunc => {
                let w = n.as_window_func()?;
                self.put((w.winfnoid, w.wintype, w.wincollid, w.inputcollid, w.winref, w.winstar, w.winagg));
                self.args(&w.args, c, tag)?;
                self.arg(w.aggfilter, c, tag)?;
                self.args(&w.runCondition, c, tag)?;
            }
            T_WindowFuncRunCondition => {
                let r = n.as_window_func_run_condition()?;
                self.put((r.opno, r.inputcollid, r.wfunc_left));
                self.expr(r.arg, c, tag)?;
            }
            // PM3a: an implicit-AND list in an expression's place (a MERGE join condition, a check option's planned qual, a
            // Result's one-time gate): each element is a whole qual
            T_List => {
                let l = n.as_list()?;
                self.put(l.len());
                l.iter().try_for_each(|x| self.expr(x, gate(x, c), tag))?;
            }
            // PM3a: a CoerceToDomain (every field but the location). D2754: then the domain's constraint set, as the content key
            // keys it (lower/src/inputs/key.rs, from the same type cache entry): the lowered program carries each NOT NULL
            // and each CHECK's expression, and ALTER DOMAIN changes them under an unchanged plan. The CHECK expressions are
            // walked as this statement's own nodes, their constants keyed by value (the type cache's, never a literal of the
            // statement), so `file` places the key walk's CHECK nodes and a changed constraint set is another front key
            // (C3 S4d: the walk's read of the type cache entry is the variant's dependency too). A type cache miss keys as no
            // constraints, as the content key does.
            T_CoerceToDomain => {
                let x = n.as_coerce_to_domain()?;
                self.put((x.resulttype, x.resulttypmod, x.resultcollid, x.coercionformat as u32));
                self.expr(x.arg, c, tag)?;
                // D3551: the names its errors carry, as the content key keys them
                self.put(gravity_expr::nodes::cast::domain_key_names(x.resulttype));
                let facts = gravity_expr::catalog::domain_facts(x.resulttype).ok();
                let cons = facts.as_ref().map_or(&[][..], |f| &f.constraints[..]);
                self.put(cons.len());
                for con in cons {
                    match con {
                        gravity_expr::catalog::DomainConstraint::NotNull => self.put(0u8),
                        gravity_expr::catalog::DomainConstraint::Check { name, expr } => {
                            self.put((1u8, *name));
                            self.expr(*expr, Class::Key, tag)?;
                        }
                    }
                }
            }
            // D2754: a domain CHECK's VALUE (`T_CoerceToDomain`'s constraint walk), as a CaseTestExpr
            T_CoerceToDomainValue => {
                let x = n.as_coerce_to_domain_value()?;
                self.put((x.typeId, x.typeMod, x.collation));
            }
            // PM3a: a RowExpr (every field but the location)
            T_RowExpr => {
                let x = n.as_row_expr()?;
                self.put((x.row_typeid, x.row_format as u32, x.colnames.len()));
                for c in x.colnames.iter() {
                    self.put(c.as_string().map(|s| s.sval));
                }
                self.args(&x.args, c, tag)?;
            }
            // PM3a: a FieldSelect (every field)
            T_FieldSelect => {
                let x = n.as_field_select()?;
                self.put((x.fieldnum, x.resulttype, x.resulttypmod, x.resultcollid));
                self.expr(x.arg, c, tag)?;
            }
            // PM3a: a RowCompareExpr (every field)
            T_RowCompareExpr => {
                let x = n.as_row_compare_expr()?;
                self.put(x.cmptype);
                self.oids(&x.opnos);
                self.oids(&x.opfamilies);
                self.oids(&x.inputcollids);
                self.args(&x.largs, c, tag)?;
                self.args(&x.rargs, c, tag)?;
            }
            // PM3a: an ArrayCoerceExpr (every field but the location)
            T_ArrayCoerceExpr => {
                let x = n.as_array_coerce_expr()?;
                self.put((x.resulttype, x.resulttypmod, x.resultcollid, x.coerceformat as u32));
                self.expr(x.arg, c, tag)?;
                self.arg(x.elemexpr, c, tag)?;
            }
            // PM3a: a ConvertRowtypeExpr (every field but the location)
            T_ConvertRowtypeExpr => {
                let x = n.as_convert_rowtype_expr()?;
                self.put((x.resulttype, x.convertformat as u32));
                self.expr(x.arg, c, tag)?;
            }
            // PM3a: a NextValueExpr (every field)
            T_NextValueExpr => {
                let x = n.as_variant::<types_nodes::primnodes::NextValueExpr>()?;
                self.put((x.seqid, x.typeId));
            }
            // PM3a: a FieldStore (every field)
            T_FieldStore => {
                let x = n.as_field_store()?;
                self.put(x.resulttype);
                self.ints(&x.fieldnums);
                self.expr(x.arg, c, tag)?;
                self.args(&x.newvals, c, tag)?;
            }
            // PM3a: a GroupingFunc (every field but the location)
            T_GroupingFunc => {
                let x = n.as_grouping_func()?;
                self.put(x.agglevelsup);
                self.ints(&x.refs);
                self.ints(&x.cols);
                self.args(&x.args, c, tag)?;
            }
            // PM3a: a MergeSupportFunc (every field but the location)
            T_MergeSupportFunc => {
                let x = n.as_merge_support_func()?;
                self.put((x.msftype, x.msfcollid));
            }
            // PM3a: a ReturningExpr (every field)
            T_ReturningExpr => {
                let x = n.as_returning_expr()?;
                self.put((x.retlevelsup, x.retold));
                self.expr(x.retexpr, c, tag)?;
            }
            // PM3a: an XmlExpr (every field but the location)
            T_XmlExpr => {
                let x = n.as_xml_expr()?;
                self.put((x.op, x.name, x.xmloption, x.indent, x.r#type, x.typmod, x.arg_names.len()));
                for a in x.arg_names.iter() {
                    self.put(a.as_string().map(|s| s.sval));
                }
                self.args(&x.named_args, c, tag)?;
                self.args(&x.args, c, tag)?;
            }
            // PM3a: the SQL/JSON expressions: JsonConstructorExpr, JsonIsPredicate, JsonValueExpr, JsonExpr and JsonBehavior (every field but the locations; a JsonValueExpr's raw form is the parser's, decompose reads the formatted one)
            T_JsonConstructorExpr => {
                let x = n.as_json_constructor_expr()?;
                self.put((x.r#type, x.absent_on_null, x.unique));
                self.put(x.returning.map(|r| (r.format.map(|f| (f.format_type, f.encoding)), r.typid, r.typmod)));
                self.args(&x.args, c, tag)?;
                self.arg(x.func, c, tag)?;
                self.arg(x.coercion, c, tag)?;
            }
            T_JsonIsPredicate => {
                let x = n.as_json_is_predicate()?;
                self.put((x.format.map(|f| (f.format_type, f.encoding)), x.item_type, x.unique_keys));
                self.arg(x.expr, c, tag)?;
            }
            T_JsonValueExpr => {
                let x = n.as_json_value_expr()?;
                self.put((x.format.map(|f| (f.format_type, f.encoding)), x.raw_expr.is_some()));
                self.arg(x.formatted_expr, c, tag)?;
            }
            T_JsonExpr => {
                let x = n.as_json_expr()?;
                self.put((x.op, x.column_name, x.format.map(|f| (f.format_type, f.encoding)), x.use_io_coercion, x.use_json_coercion, x.wrapper, x.omit_quotes, x.collation));
                self.put(x.returning.map(|r| (r.format.map(|f| (f.format_type, f.encoding)), r.typid, r.typmod)));
                self.put(x.passing_names.len());
                for a in x.passing_names.iter() {
                    self.put(a.as_string().map(|s| s.sval));
                }
                self.arg(x.formatted_expr, c, tag)?;
                self.arg(x.path_spec, c, tag)?;
                self.args(&x.passing_values, c, tag)?;
                self.arg(x.on_empty, c, tag)?;
                self.arg(x.on_error, c, tag)?;
            }
            T_JsonBehavior => {
                let x = n.as_json_behavior()?;
                self.put((x.btype, x.coerce));
                self.arg(x.expr, c, tag)?;
            }
            // PM3a: JSON_TABLE's plan: JsonTablePathScan, JsonTableSiblingJoin and JsonTablePath (every field)
            T_JsonTablePathScan => {
                let x = n.as_json_table_path_scan()?;
                self.put((x.errorOnError, x.colMin, x.colMax));
                self.arg(x.path, c, tag)?;
                self.arg(x.child, c, tag)?;
            }
            T_JsonTableSiblingJoin => {
                let x = n.as_json_table_sibling_join()?;
                self.arg(x.lplan, c, tag)?;
                self.arg(x.rplan, c, tag)?;
            }
            T_JsonTablePath => {
                let x = n.as_json_table_path()?;
                self.put(x.name);
                self.arg(x.value, c, tag)?;
            }
            // PM3a: a CurrentOfExpr (WHERE CURRENT OF: every field)
            T_CurrentOfExpr => {
                let x = n.as_current_of_expr()?;
                self.put((x.cvarno, x.cursor_name, x.cursor_param));
            }
            T_SortGroupClause => {
                let s = n.as_sort_group_clause()?;
                self.put((s.tleSortGroupRef, s.eqop, s.sortop, s.reverse_sort, s.nulls_first, s.hashable));
            }
            _ => return self.no(tag, "expression node"),
        }
        Some(())
    }
}

/// NEW-72: `n` has decompose's `bound_form` shape over the column it compares (a scan Var, or one under a cast).
fn bound_word(n: Node<'_>) -> bool {
    let Some(o) = n.as_op_expr() else { return false };
    let var = |a: Node<'_>| a.as_var().or_else(|| a.as_func_expr().filter(|f| f.args.len() == 1).and_then(|f| f.args.nth(0).as_var())).map(|v| v.varno);
    o.args.iter().filter_map(var).any(|varno| gravity_decompose::ops::shared::scan::bound_form(n, varno, &|_| true).is_some())
}

/// Whether the conditional classes are read for this statement (the fill's facts): `Tlist` when a scanned index has
/// an expression column or an ORDER BY operator (the order carries expression fingerprints), `Zone` when a zoned
/// scan's relation is pgrc. Facts it cannot find read as yes.
pub fn needs(f: &Front<'_>, stmt: &PlannedStmt<'_>, facts: &dyn Facts) -> (bool, bool) {
    let tl = f.orderby || f.indexes.iter().any(|&i| facts.index(i).is_none_or(|x| x.keys().contains(&0)));
    let relid = |rti: u32| (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid);
    let zone = f.zones.iter().any(|&rti| relid(rti).and_then(|r| facts.rel(r)).is_none_or(|r| r.am == Am::Pgrcolumnar));
    (tl, zone)
}

/// A filed variant of a shape: its content key, each key-walk node's position in the front walk, the conditional
/// classes' values it was filed under (`None`: not read for it), and the by-reference words it baked with their images
/// (V4R-18: a hit needs this statement's images equal at those positions; the other words bind per execution).
struct Entry {
    /// The entry's heap bytes (the boxed slices below), on the filing backend's session: this memo is that thread's
    /// (`leader_tls`), bounded by `FRONT_ENTRIES` x `FRONT_VARIANTS`, not evictable. Declared FIRST (D4627): a drop on
    /// another lone thread takes the bytes back before the slices free.
    _charge: memledger::Charge,
    key: ContentKey,
    pos: Box<[u32]>, // alloc: Cold charged the session (Entry::_charge)
    tl: Option<u128>,
    zone: Option<u128>,
    baked: Box<[(u32, Box<[u8]>)]>, // alloc: Cold charged the session (Entry::_charge)
    /// D644: the key-walk positions (in `pos` order) of the nodes decompose synthesized (carry.rs `after`: a partial-width
    /// UPDATE's carried column, a Var and its resjunk TargetEntry), which no front walk holds: `pos` has `SYNTH` there. A
    /// hit hands a stand-in at each; the fill proved no word or check locator reads one (`file`).
    synth: Box<[u32]>, // alloc: Cold charged the session (Entry::_charge)
    /// The key-walk constants decompose built (`Built`, from the bake log's `Derived`): `pos` has `DERIVED` there, and a
    /// hit builds each from this statement's plan constant as decompose built it (or the fixed value).
    built: Box<[Built]>, // alloc: Cold charged the session (Entry::_charge)
    /// The lowered IR's hash when this admission lowered it (`fill_lowered`): the shadow check lowers a variant hit
    /// whose content key differs (an unbaked image) again and compares.
    ir: Option<u128>,
    /// KEY-PREDICTED-ROWS road (b): the statement's estimates where the program under `key` carries others (exec admit
    /// `estimates`: a hit binds without the graph, so it takes them from here). The front key holds the plan's rows
    /// exactly, so every statement this variant serves has them.
    est: Option<Box<[PipeEst]>>, // alloc: Cold charged the session (Entry::_charge)
    /// D3104: the filed statement's index morsel sites (`Front::morsel`), and `Sites` when a hit may bind other rows there.
    morsel: Box<[(i32, u64)]>, // alloc: Cold charged the session (Entry::_charge)
    sites: Option<Sites>,
    /// C3 S4d: what the admission that filed it read from the catalog (the walk, decompose, the key walk; gravity_deps): a
    /// hit serves it only while they stand, and replays them into the admission's recording.
    held: gravity_deps::Held,
}

/// D3104: where a variant's hit may bind its index morsel sites' rows from the plan (`morsel_sites`): each site's graph
/// pipeline, in `Front::morsel` order, and each pipeline an end-rate guard of the program decides on with the plan nodes
/// its rate is read over (decompose ops/agg `end_rate_over`).
pub struct Sites {
    pipes: Box<[u16]>, // alloc: Cold charged the session (Entry::_charge)
    rates: Box<[(u16, Box<[i32]>)]>, // alloc: Cold charged the session (Entry::_charge)
}

/// D3104: a hit's index morsel rows where they differ from its variant's (graph pipeline, this statement's rows), and the
/// end-rate guards' pipelines with their plan nodes (`Sites::rates`): `moved_hold` decides the program's guards on them.
pub struct Moved {
    pub rows: Box<[(u16, u64)]>,
    rates: Box<[(u16, Box<[i32]>)]>,
}

/// D3104: `Sites` for a variant of `f` whose program (`guards`) was admitted through `graph`, or `None` (a hit then needs
/// the filed rows). Each site is the source of exactly one graph pipeline whose predicted rows are the site's, and no other
/// pipeline's estimate equals them (a copy: an arm, a continuation); each end-rate guard's rate is read off the plan again
/// as decompose read it (its pipeline's node ops but its groups' Agg nodes; the value must come out as filed). Rows and
/// build guards decide on estimates the key holds exactly but the sites', which `moved_hold` decides on again.
/// Fire record (PGRUST_GRAVITY_REPORT): `front.morsel_rows.unfiled.<why>` when a shape with sites is filed without them.
pub fn morsel_sites(f: &Front<'_>, graph: &gravity_decompose::graph::PipelineGraph<'_>, guards: &[gravity_lower::inputs::key::EstGuard], stmt: &PlannedStmt<'_>) -> Option<Sites> {
    if f.morsel.is_empty() {
        return None;
    }
    sites_of(f, graph, guards, stmt).map_err(fires::fire).ok()
}

fn sites_of(f: &Front<'_>, graph: &gravity_decompose::graph::PipelineGraph<'_>, guards: &[gravity_lower::inputs::key::EstGuard], stmt: &PlannedStmt<'_>) -> Result<Sites, &'static str> {
    use gravity_decompose::graph::Source;
    use gravity_lower::inputs::key::{Est, NOT_LIT};
    let mut pipes = Vec::with_capacity(f.morsel.len()); // alloc: Cold charged the session (Entry::_charge)
    for &(node, rows) in f.morsel.iter() {
        let mut of = graph.pipelines.iter().enumerate().filter(|(_, q)| q.node_id == node && matches!(q.source, Source::Bitmap { morsel: true, .. }));
        let (j, q) = of.next().ok_or("front.morsel_rows.unfiled.site")?;
        if of.next().is_some() || q.desc.predicted_rows != rows {
            return Err("front.morsel_rows.unfiled.site");
        }
        if graph.pipelines.iter().enumerate().any(|(i, o)| i != j && (o.desc.predicted_rows == rows || o.desc.build_rows == rows)) {
            return Err("front.morsel_rows.unfiled.copy");
        }
        pipes.push(u16::try_from(j).map_err(|_| "front.morsel_rows.unfiled.site")?);
    }
    let mut rates: Vec<(u16, Box<[i32]>)> = Vec::new(); // alloc: Cold charged the session (Entry::_charge)
    for g in guards.iter().filter(|g| g.lit == NOT_LIT && g.est == Est::EndRate) {
        if rates.iter().any(|(p, _)| *p == g.pipeline) {
            continue;
        }
        let q = graph.pipelines.get(g.pipeline as usize).ok_or("front.morsel_rows.unfiled.rate")?;
        let aggs: Vec<i32> = graph.groups.iter().filter(|d| d.producer.0 == g.pipeline).map(|d| d.node_id).collect(); // alloc: Cold
        if aggs.is_empty() {
            return Err("front.morsel_rows.unfiled.agg");
        }
        let ids: Box<[i32]> = q.node_ops.iter().map(|o| o.node_id).filter(|n| !aggs.contains(n)).collect(); // alloc: Cold charged the session (Entry::_charge)
        if gravity_decompose::ops::agg::end_rate_over(stmt, ids.iter().copied()) != q.desc.end_rate {
            return Err("front.morsel_rows.unfiled.rate");
        }
        rates.push((g.pipeline, ids)); // alloc: Cold charged the session (Entry::_charge)
    }
    Ok(Sites { pipes: pipes.into_boxed_slice(), rates: rates.into_boxed_slice() })
}

/// D3104: whether a program's estimate guards (`guards`) decide on a hit's moved rows (`m`, over this statement `stmt`) as
/// they did: a rows guard of a site's pipeline on its rows, an end-rate guard on its rate read off this plan. Every other
/// guard decides on an estimate the key holds exactly (`morsel_sites`).
pub fn moved_hold(guards: &[gravity_lower::inputs::key::EstGuard], m: &Moved, stmt: &PlannedStmt<'_>) -> bool {
    use gravity_lower::inputs::key::{Est, NOT_LIT};
    guards.iter().filter(|g| g.lit == NOT_LIT).all(|g| match g.est {
        Est::Rows => m.rows.iter().find(|(p, _)| *p == g.pipeline).is_none_or(|&(_, r)| (g.decide)(r, &g.args) == g.want),
        Est::Build => true,
        Est::EndRate => m.rates.iter().find(|(p, _)| *p == g.pipeline).is_some_and(|(_, ids)| (g.decide)(gravity_decompose::ops::agg::end_rate_over(stmt, ids.iter().copied()), &g.args) == g.want),
    })
}

/// The memo's bound: a full memo is cleared (a miss costs today's path and one fill).
/// ARBITRARY-SAFE (a per-session memory bound): a miss takes today's path (decompose and the key walk) and the program
/// cache, which is separate, still saves the compile; the value bounds memory and never changes an answer or a plan.
pub const FRONT_ENTRIES: usize = 1024;

/// A shape's variants, most recently hit first (V4R-18: literals a program baked make one per value, the rest one).
/// ARBITRARY-SAFE for the same reason as `FRONT_ENTRIES`: a shape cycling more baked values than this misses here and
/// takes today's path, the program cache unchanged.
pub const FRONT_VARIANTS: usize = 4;

leader_tls::leader_tls! {
    thread_local! {
        /// D4600: hashed by `admit::lean::KeyFold` (a front key is a 128-bit hash already; `forget`'s retain and the
        /// clear when full are the only passes over it, so no order is observable).
        static MEMO: RefCell<HashMap<u128, Vec<Entry>, crate::admit::lean::KeyFold>> = RefCell::new(HashMap::with_hasher(crate::admit::lean::KeyFold::new()));
        static CHECK: Cell<Option<(u64, bool)>> = const { Cell::new(None) };
        /// `pgrust.gravity_front_check`'s registry slot (D2896: looked up once, then read by its index).
        static CHECK_SLOT: Cell<gravity_hosts::inputs::OptionSlot> = const { Cell::new(gravity_hosts::inputs::OptionSlot::Unknown) };
    }
}

/// This statement's image at front position `p` (a by-reference word's; `None` for any other node).
fn image_at<'a>(f: &Front<'a>, p: u32) -> Option<&'a [u8]> {
    let k = f.nodes.get(p as usize)?.as_const().filter(|k| !k.constisnull && !k.constbyval)?;
    super::raw::image(k)
}

/// The content key and this plan's key-walk nodes of a filed variant of the shape: its classes' values and its baked
/// images equal this statement's. A variant with a constant decompose built (`Entry::built`) needs `hit_est`'s allocator.
pub fn hit<'p>(f: &Front<'p>, arena: ArenaRef) -> Option<(ContentKey, ArenaVec<Node<'p>>)> {
    hit_est(f, arena, None).map(|(k, n, _, _)| (k, n))
}

/// A hit's variants and estimates: the content key, this plan's key-walk nodes, the variant's estimates, and (D3104) its
/// index morsel rows where they differ from the variant's.
pub type Hit<'p> = (ContentKey, ArenaVec<Node<'p>>, Option<Box<[PipeEst]>>, Option<Moved>);

/// `hit` with the variant's estimates (KEY-PREDICTED-ROWS road (b), `Entry::est`); `mcx` builds the constants decompose
/// built (`Entry::built`; `None`: a variant with one is no hit). D3104: a variant whose index morsel rows equal this
/// statement's first, else one filed with `Sites`, whose moved rows the caller decides the program's guards on (`moved_hold`).
pub fn hit_est<'p>(f: &Front<'p>, arena: ArenaRef, mcx: Option<mcx::Mcx<'p>>) -> Option<Hit<'p>> {
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        let v = m.get_mut(&f.main)?;
        // C3 S4d: a variant whose recorded reads moved is gone (the shape decomposes again and files anew)
        if !v.iter().all(|e| e.held.stands()) {
            v.retain(|e| e.held.stands());
            if v.is_empty() {
                m.remove(&f.main);
                return None;
            }
        }
        let v = m.get_mut(&f.main)?;
        let alike = |e: &Entry| e.tl.is_none_or(|t| t == f.tl) && e.zone.is_none_or(|z| z == f.zone) && e.baked.iter().all(|(p, img)| image_at(f, *p) == Some(&img[..]));
        let sites = |e: &Entry| e.morsel.len() == f.morsel.len() && e.morsel.iter().zip(f.morsel.iter()).all(|(a, b)| a.0 == b.0);
        let i = v.iter().position(|e| alike(e) && e.morsel[..] == f.morsel[..]).or_else(|| v.iter().position(|e| alike(e) && e.sites.is_some() && sites(e)))?;
        v[..=i].rotate_right(1);
        let e = &v[0];
        e.held.replay();
        let moved = match &e.sites {
            Some(s) if e.morsel[..] != f.morsel[..] => {
                let rows = e.morsel.iter().zip(f.morsel.iter()).zip(s.pipes.iter()).filter(|((a, b), _)| a.1 != b.1).map(|((_, b), &j)| (j, b.1)).collect(); // alloc: Cold
                Some(Moved { rows, rates: s.rates.clone() }) // alloc: Cold
            }
            _ => None,
        };
        let mut nodes = ArenaVec::with_capacity_in(e.pos.len(), arena);
        // D644: a synthesized node's stand-in is the walk's first node, never read (the fill's locator proof)
        for &p in e.pos.iter() {
            nodes.push(*f.nodes.get(if p == SYNTH || p == DERIVED { 0 } else { p as usize })?);
        }
        for b in e.built.iter() {
            *nodes.get_mut(b.at as usize)? = b.node(f, mcx?)?;
        }
        Some((e.key, nodes, e.est.clone(), moved)) // alloc: Cold
    })
}

/// Files a variant of the shape when every key-walk node is a node of the front walk (by identity); `needs` names the
/// classes read. Every by-reference word is taken as baked (a hit needs all their images equal: today's key). A shape with
/// a synthesized node is not filed here (no locator proof: `fill_reading`, which also says why a shape was not filed).
pub fn fill(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool)) {
    let _ = file(f, key, nodes, needs, &f.byref, None, None, None, None, &gravity_deps::Deps::default());
}

/// V4R-18: `fill` after this admission lowered the program (`ir`: its `ir_hash`): the by-reference words baked are
/// those whose images the admission's bake scopes logged (gravity_expr's IR reads, gravity_decompose's plan-tree
/// reads); the others bind per execution. After a content-key hit nothing lowered here, so `fill` takes them all.
pub fn fill_lowered(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool), ir: u128) {
    let _ = fill_lowered_reading(f, key, nodes, needs, ir, None);
}

/// D644: `fill` with `reads`, every key-walk position the program's template binds from (its word and check locators,
/// admit.rs `template_reads`): a synthesized key-walk node (`synthesized`) at none of them is filed as `SYNTH`.
pub fn fill_reading(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool), reads: &[u32]) -> Result<(), &'static str> {
    fill_reading_est(f, key, nodes, needs, reads, None, None, &gravity_deps::Deps::default())
}

/// `fill_reading` with the statement's estimates (KEY-PREDICTED-ROWS road (b), `Entry::est`), (D3104) its index morsel
/// sites' `Sites` (`morsel_sites`; `None`: a hit needs this statement's rows) and (C3 S4d) what the admission read from the
/// catalog from the walk on (`Entry::deps`; the `fill*` forms without it file none: a test's memo).
#[allow(clippy::too_many_arguments)]
pub fn fill_reading_est(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool), reads: &[u32], est: Option<&[PipeEst]>, sites: Option<Sites>, deps: &gravity_deps::Deps) -> Result<(), &'static str> {
    file(f, key, nodes, needs, &f.byref, None, Some(reads), est, sites, deps)
}

/// D644: `fill_lowered` with `reads` (`fill_reading`).
pub fn fill_lowered_reading(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool), ir: u128, reads: Option<&[u32]>) -> Result<(), &'static str> {
    fill_lowered_reading_est(f, key, nodes, needs, ir, reads, None, None, &gravity_deps::Deps::default())
}

/// `fill_lowered_reading` with the statement's estimates (KEY-PREDICTED-ROWS road (b), `Entry::est`), `Sites` (D3104) and
/// the admission's catalog reads (C3 S4d, `fill_reading_est`).
#[allow(clippy::too_many_arguments)]
pub fn fill_lowered_reading_est(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool), ir: u128, reads: Option<&[u32]>, est: Option<&[PipeEst]>, sites: Option<Sites>, deps: &gravity_deps::Deps) -> Result<(), &'static str> {
    let (by_ir, by_plan) = (f.ir_reads(), f.plan_reads());
    let read = |p: &u32| f.nodes[*p as usize].as_const().is_some_and(|k| by_ir.binary_search(&k.constvalue.as_usize()).is_ok() || by_plan.binary_search(&k.constvalue.as_usize()).is_ok());
    let baked: Vec<u32> = f.byref.iter().copied().filter(read).collect(); // alloc: Cold
    file(f, key, nodes, needs, &baked, Some(ir), reads, est, sites, deps)
}

/// The IR hash of a lowered program (`fill_lowered`, the shadow check): the IR and its functions, which bind every
/// word per execution, so two statements a variant serves lower to one hash unless a literal was baked unlogged.
/// Taken only under the shadow check (admit.rs `install`); never 0, which is `IR_UNHASHED`.
pub fn ir_hash(l: &gravity_lower::Lowered) -> u128 {
    let mut h = Lane::new(3);
    l.ir.hash(&mut h);
    l.funcs.iter().for_each(|f| f.0.hash(&mut h));
    h.get() | 1
}

/// The `ir` of a variant a lowering filed while `pgrust.gravity_front_check` was off: the hash only feeds that check, so
/// it is not taken (the shadow check turned on later lowers such a variant again but has nothing to compare).
pub const IR_UNHASHED: u128 = 0;

/// The shadow check's compare of a variant's filed IR hash with this statement's lowering (`hash`, taken only when needed):
/// an `IR_UNHASHED` variant (filed with the check off) is accepted, never reported as a mismatch.
pub fn variant_ir_ok(filed: u128, hash: impl FnOnce() -> u128) -> bool {
    filed == IR_UNHASHED || hash() == filed
}

/// KEY-PREDICTED-ROWS road (b): drops every variant filed under content key `key`. The program cache holds one program
/// per content key, a program with estimate guards serves only the estimates its guards decide alike (lower
/// inputs/key.rs `guards_hold`), which a hit here cannot re-run, and a variant's estimates are bound over its program's
/// pipelines: when a program is filed under `key` (admit.rs `file_local`), the variants filed for the one before it go.
pub fn forget(key: ContentKey) {
    MEMO.with(|m| m.borrow_mut().retain(|_, v| {
        v.retain(|e| e.key != key);
        !v.is_empty()
    }));
}

/// The IR hash of the variant `hit` last answered for `f`'s shape (`None`: filed without a lowering).
pub fn filed_ir(f: &Front<'_>) -> Option<u128> {
    MEMO.with(|m| m.borrow().get(&f.main).and_then(|v| v.first()).and_then(|e| e.ir))
}

/// C3 S4d: what the admission that filed the variant `hit` last answered for `f`'s shape read (the shadow check's).
pub fn filed_deps(f: &Front<'_>) -> Option<gravity_deps::Deps> {
    MEMO.with(|m| m.borrow().get(&f.main).and_then(|v| v.first()).map(|e| e.held.deps().clone())) // alloc: Cold
}

/// D644: a filed position with no front node: a node decompose synthesized (`synthesized`).
const SYNTH: u32 = u32::MAX;

/// A filed position with no front node: a constant decompose built (`Built`).
const DERIVED: u32 = u32::MAX - 1;

/// A key-walk constant decompose built (gravity_decompose bake `Derived`), as a hit builds it again at key-walk position
/// `at`: from the plan constant at front position `src` (`How::Fixed`: none), of type `ty` (typmod, collation, length and
/// by-value as the filed one).
struct Built {
    at: u32,
    src: u32,
    ty: (u32, i32, u32, i32, bool),
    how: How,
}

/// How a hit builds a `Built` constant.
#[derive(Clone, Copy)]
enum How {
    /// normalize `retype`'s conversion under the btree strategy: `int2col = 5` over an int4 literal is `int2col =
    /// 5::int2`, `datecol >= timestamp '..'` a date; the front key holds whether it converts (`retype_class`).
    Retype(u8),
    /// The plan constant's own value (agg gsets.rs: a later arm's copy of a literal).
    Copy,
    /// normalize `like_twin`'s LIKE pattern of the plan text constant (`twin_const`): `like_twin` logs the needle's
    /// image, so the variant carries it, and the front key holds its by-value inputs (`twin_class`).
    Twin { head: bool, tail: bool },
    /// The filed by-value constant's value: fixed by plan fields the front key holds (agg gsets.rs's phase id, rolled
    /// flag and GROUPING mask).
    Fixed(u64),
    /// normalize `detext`'s integer (D3079): the plan text constant's text parsed as the built constant's integer type
    /// (`detext_value`); the front key holds that it parses (`detext_class`).
    Detext,
}

impl Built {
    /// This statement's constant; `None` when it cannot be built (a class the front key holds rules that out on a hit)
    /// or out of memory.
    fn node<'p>(&self, f: &Front<'p>, mcx: mcx::Mcx<'p>) -> Option<Node<'p>> {
        let (ty, typmod, coll, len, byval) = self.ty;
        if let How::Fixed(v) = self.how {
            return Node::mk_const(mcx, ty, typmod, coll, len, datum::Datum::from_u64(v), false, byval).ok();
        }
        let k = f.nodes.get(self.src as usize)?.as_const()?;
        // words bound per execution, as admit's bind reads one (no bake: exec tests/front_bake_lint.rs)
        match self.how {
            How::Retype(strategy) => {
                let v = if k.constisnull { datum::Datum::from_i64(0) } else { gravity_decompose::normalize::retype_value(k.consttype, ty, strategy, k.constvalue)? };
                Node::mk_const(mcx, ty, typmod, coll, len, v, k.constisnull, byval).ok()
            }
            How::Copy => Node::mk_const(mcx, ty, typmod, coll, len, k.constvalue, k.constisnull, byval).ok(),
            How::Twin { head, tail } => gravity_decompose::normalize::twin_const(mcx, k, head, tail, coll),
            How::Detext => {
                let v = gravity_decompose::normalize::detext_value(ty, gravity_decompose::expr::const_bytes_keyed(k)?)?;
                Node::mk_const(mcx, ty, typmod, coll, len, v, false, byval).ok()
            }
            How::Fixed(_) => None,
        }
    }
}

/// D644: whether a key-walk node no front walk holds is one decompose synthesized that holds no literal and no
/// parameter value, so a hit hands a stand-in there (`file` proves no word or check reads it). Every node but a non-null
/// Const and a non-executor Param: carry.rs `after`'s carried Var and its resjunk TargetEntry; normalize's rewritten
/// operators, and every node above a rewritten one, which the expression mutator copies (`commute`'s OpExpr, the
/// BoolExpr over a `retype`d comparison: `x BETWEEN SYMMETRIC a AND b`, a row IN list); demand/readers.rs `rewrite`'s cells (a TargetEntry
/// over its new `min` / `any_value` Aggref, the Aggref, its argument's TargetEntry), `nulled`'s NULL, normalize
/// `distinct_count`'s `count(DISTINCT k)` (its Aggref, argument TargetEntry and SortGroupClause); V4R-566's rewritten
/// loop's NestLoopParam and PARAM_EXEC Param (an execution parameter's number, never its value). Each such node is a
/// function of plan nodes the walk keys (its kinds, operators, types and numbers are theirs or derived from them by a
/// rule whose value-dependent inputs the front key holds: `retype_class`, `pattern_class`, `coalesce_filter_class`), so
/// equal front keys give equal synthesized nodes. A literal decompose built (a non-null Const) is never a stand-in: the
/// bake log's `Derived` files it (`Built`) or the shape is not filed.
pub(crate) fn synthesized(n: Node<'_>) -> bool {
    match (n.as_const(), n.as_param()) {
        (Some(k), _) => k.constisnull,
        (_, Some(p)) => p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC,
        _ => true,
    }
}

/// The sentinel positions of the variant `hit` last answered for `f`'s shape (the shadow check compares them by kind).
pub fn filed_synth(f: &Front<'_>) -> Vec<u32> {
    MEMO.with(|m| m.borrow().get(&f.main).and_then(|v| v.first()).map(|e| e.synth.to_vec()).unwrap_or_default()) // alloc: Cold
}

/// The key-walk positions of the constants decompose built of the variant `hit` last answered for `f`'s shape (the
/// shadow check compares them by value: `node_matches`).
pub fn filed_derived(f: &Front<'_>) -> Vec<u32> {
    MEMO.with(|m| m.borrow().get(&f.main).and_then(|v| v.first()).map(|e| e.built.iter().map(|b| b.at).collect()).unwrap_or_default()) // alloc: Cold
}

/// The shadow check's compare of a fresh decompose's key-walk node with a hit's at one position: a synthesized one by kind
/// (`synthesized`), a constant decompose built by its type and value (the hit built it, `Built::node`; a by-reference
/// one, always a varlena (`derived_at`), by its payload), any other by identity.
pub fn node_matches(fresh: Node<'_>, filed: Node<'_>, synth: bool, derived: bool) -> bool {
    match (synth, derived, fresh.as_const(), filed.as_const()) {
        (true, ..) => synthesized(fresh),
        (_, true, Some(a), Some(b)) => {
            let ty = |k: &types_nodes::primnodes::Const| (k.consttype, k.consttypmod, k.constcollid, k.constlen, k.constbyval, k.constisnull);
            let bytes = gravity_decompose::expr::const_bytes_keyed;
            ty(a) == ty(b) && (a.constisnull || if a.constbyval { a.constvalue.as_u64() == b.constvalue.as_u64() } else { bytes(a).is_some() && bytes(a) == bytes(b) })
        }
        (_, true, ..) => false,
        _ => fresh.ptr_eq(filed),
    }
}

/// Key-walk node `n` at position `k` of `nodes` as a constant decompose built (the bake log's `Derived`) with what a hit
/// needs to build it again (`by`: the front walk's node positions by address). `None` when the key walk
/// visits it twice (a hit builds one node per position), when it is built from a plan constant (not `Fixed`) but no word
/// or check reads it (`read`: the program would hold the filed value), when a by-reference one is no varlena or its
/// image was baked (`baked`: its value is the variant's, which a hit cannot compare), or when its source is no front node.
fn derived_at(n: &Node<'_>, k: u32, by: &Positions, nodes: &[Node<'_>], read: bool, baked: &dyn Fn(usize) -> bool) -> Option<Built> {
    use gravity_decompose::bake::Derived;
    let c = n.as_const().filter(|c| (c.constbyval || c.constlen == -1) && nodes.iter().filter(|m| m.ptr_eq(*n)).count() == 1)?;
    let image = (!c.constbyval && !c.constisnull).then(|| c.constvalue.as_usize());
    if image.is_some_and(baked) {
        return None;
    }
    let src = |a: usize| by.get(a);
    let (src, how) = match gravity_decompose::bake::derived(*n)? {
        Derived::Retype { src: a, strategy } if read && c.constbyval => (src(a)?, How::Retype(strategy)),
        Derived::Copy { src: a } if read => (src(a)?, How::Copy),
        Derived::Twin { src: a, head, tail } if read => (src(a)?, How::Twin { head, tail }),
        Derived::Detext { src: a } if read && c.constbyval => (src(a)?, How::Detext),
        Derived::Fixed if c.constbyval && !c.constisnull => (u32::MAX, How::Fixed(c.constvalue.as_u64())),
        _ => return None,
    };
    Some(Built { at: k, src, ty: (c.consttype, c.consttypmod, c.constcollid, c.constlen, c.constbyval), how })
}

#[allow(clippy::too_many_arguments)]
fn file(f: &Front<'_>, key: ContentKey, nodes: &[Node<'_>], needs: (bool, bool), baked: &[u32], ir: Option<u128>, reads: Option<&[u32]>, est: Option<&[PipeEst]>, sites: Option<Sites>, deps: &gravity_deps::Deps) -> Result<(), &'static str> {
    // C3 S4d: reads that may straddle a commit this backend has not processed (or another transaction's state: a parallel
    // worker, a historic snapshot) never hold; such a variant is not filed
    if deps.lagged() {
        return Err("the admission's catalog reads lagged (gravity_deps)");
    }
    // D3615 (a), D4449: the front nodes by address, in one pass (`Positions`)
    let by = Positions::of(&f.nodes, f.arena);
    let mut pos = Vec::with_capacity(nodes.len()); // alloc: Cold charged the session (Entry::_charge)
    let mut synth = Vec::new(); // alloc: Cold charged the session (Entry::_charge)
    let mut built = Vec::new(); // alloc: Cold charged the session (Entry::_charge)
    // V4R-18: the images the admission's bake scopes logged; without a lowering every image counts as baked
    let (by_ir, by_plan) = if ir.is_some() { (f.ir_reads(), f.plan_reads()) } else { (Vec::new(), Vec::new()) }; // alloc: Cold
    let image_baked = |a: usize| ir.is_none() || by_ir.binary_search(&a).is_ok() || by_plan.binary_search(&a).is_ok();
    // D3615 (a): the read nodes sorted once (each node searched the whole list: nodes x words, 16 M steps for a 4000-arm
    // filter, on every admission that files the front, a shared-tier hit's too)
    let reads_sorted = reads.map(|r| {
        let mut v = r.to_vec(); // alloc: Cold
        v.sort_unstable();
        v
    });
    for (k, n) in nodes.iter().enumerate() {
        let read = reads_sorted.as_ref().is_some_and(|r| r.binary_search(&(k as u32)).is_ok());
        match by.get(n.as_raw().as_ptr() as usize) {
            Some(p) => pos.push(p),
            None => match derived_at(n, k as u32, &by, nodes, read, &image_baked) {
                Some(b) => {
                    pos.push(DERIVED);
                    built.push(b); // alloc: Cold charged the session (Entry::_charge)
                }
                None if !synthesized(*n) => return Err("a key-walk node outside the plan"),
                None if reads.is_none() => return Err("a synthesized key-walk node without the template's locators"),
                None if read => return Err("a word or check at a synthesized node"),
                None => {
                    pos.push(SYNTH);
                    synth.push(k as u32);
                }
            },
        }
    }
    let mut images = Vec::with_capacity(baked.len()); // alloc: Cold charged the session (Entry::_charge)
    for &p in baked {
        let Some(img) = image_at(f, p) else { return Err("a baked by-reference word without a readable image") };
        images.push((p, Box::from(img))); // alloc: Cold charged the session (Entry::_charge)
    }
    let heap = pos.len() * core::mem::size_of::<u32>()
        + synth.len() * core::mem::size_of::<u32>()
        + built.len() * core::mem::size_of::<Built>()
        + images.iter().map(|(_, b): &(u32, Box<[u8]>)| core::mem::size_of::<(u32, Box<[u8]>)>() + b.len()).sum::<usize>()
        + est.map_or(0, |e| e.len() * core::mem::size_of::<PipeEst>())
        + f.morsel.len() * core::mem::size_of::<(i32, u64)>()
        + sites.as_ref().map_or(0, |t| t.pipes.len() * core::mem::size_of::<u16>() + t.rates.iter().map(|(_, r)| core::mem::size_of::<(u16, Box<[i32]>)>() + r.len() * core::mem::size_of::<i32>()).sum::<usize>());
    let charge = memledger::Charge::open(memledger::Owner::Session(memledger::current()), memledger::Release::Session, memledger::Kind::Region, memledger::charge::Door::Heap, heap as u64);
    let e = Entry { _charge: charge, key, pos: pos.into_boxed_slice(), tl: needs.0.then_some(f.tl), zone: needs.1.then_some(f.zone), baked: images.into_boxed_slice(), synth: synth.into_boxed_slice(), built: built.into_boxed_slice(), ir, est: est.map(Box::from), morsel: Box::from(&f.morsel[..]), sites, held: gravity_deps::Held::of(deps.clone()) }; // alloc: Cold charged the session (Entry::_charge)
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= FRONT_ENTRIES && !m.contains_key(&f.main) {
            m.clear();
        }
        let v = m.entry(f.main).or_default(); // alloc: Cold
        v.truncate(FRONT_VARIANTS - 1);
        v.insert(0, e); // alloc: Cold
    });
    Ok(())
}

/// Empties the memo: at session end (`invalidate::session_end`) and in tests.
pub fn clear() {
    MEMO.with(|m| m.borrow_mut().clear());
}

/// The memo's size (tests: the bound).
pub fn memo_len() -> usize {
    MEMO.with(|m| m.borrow().len())
}

/// `pgrust.gravity_front_check` (default off; on in every harness): a hit decomposes again and raises on any difference.
/// Read once per GUC change (`store_mutation_count`, grant.rs's snapshot).
pub fn check_on() -> bool {
    let at = guc::store::store_mutation_count();
    CHECK.with(|c| match c.get() {
        Some((seen, on)) if seen == at => on,
        _ => {
            let on = CHECK_SLOT.with(|slot| gravity_hosts::inputs::custom_guc_text_at(slot, "pgrust.gravity_front_check")).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes"));
            c.set(Some((at, on)));
            on
        }
    })
}

#[cfg(test)]
mod src07_spread {
    use super::zone_spread;

    const HOUR: i64 = 3_600_000_000;
    /// 2024-03-10 07:00:00+00 (New York's spring transition instant) as a timestamptz word.
    const SPRING: i64 = 763_369_200_000_000;

    /// SRC-07: the session zone's offsets within three days of an instant (normalize `implied`'s widening): one offset
    /// far from a transition, both across one, none without a session zone; SET TimeZone between two reads moves it.
    #[test]
    fn the_spread_is_the_zones_offsets_around_the_instant() {
        let system = std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists();
        if system {
            std::env::set_var("PGRUST_TZDIR", "/usr/share/zoneinfo");
        }
        pgtz::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        pgtz::set_session_timezone(None);
        assert_eq!(zone_spread(SPRING), None, "no session zone");
        let ny: &[u8] = if system { b"America/New_York" } else { b"EST5EDT,M3.2.0,M11.1.0" };
        pgtz::set_session_timezone(pgtz::pg_tzset(ny).unwrap());
        assert_eq!(zone_spread(SPRING), Some((-5 * HOUR, -4 * HOUR)), "the spring transition");
        assert_eq!(zone_spread(SPRING - 4 * 24 * HOUR), Some((-5 * HOUR, -5 * HOUR)), "four days before it: EST alone");
        assert_eq!(zone_spread(SPRING + 4 * 24 * HOUR), Some((-4 * HOUR, -4 * HOUR)), "four days after it: EDT alone");
        assert_eq!(zone_spread(SPRING + 70 * HOUR), Some((-5 * HOUR, -4 * HOUR)), "inside the window");
        pgtz::set_session_timezone(pgtz::pg_tzset(b"UTC").unwrap().or(pgtz::pg_tzset(b"GMT").unwrap()));
        assert_eq!(zone_spread(SPRING), Some((0, 0)), "SET TimeZone between two reads");
    }
}
