//! The shapes rig: the decomposer's fixtures lowered and run on the kernels' rig, checked against
//! the expression compiler's tier 0 over the same rows. The catalog answers from the reference
//! `.dat` files (no server), the rows are generated per site from the lanes' types.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use datum::{Datum, NullableDatum};
use gravity_decompose::{corpus, Bound, Facts, Op, PipelineGraph, PipelineNode, Source};
use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
use gravity_expr::{Catalog, Compiled, Expr, Region, Row};
use gravity_fn::{Shim, Tier};
use gravity_lower::{assemble, compile, lower_graph as lower, Backend, Lowered, Sequential, WordSrc};
use gravity_program::{Arena, LaneId, Program};
use mcx::MemoryContext;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::Node;
use types_portal::params::{ParamBind, ParamExternData};
use types_tuple::PgTypeShape;

/// memsig E-prog: this thread's test context for compiled programs' lists (never freed: a test compiles a few).
fn lists() -> mcx::Mcx<'static> {
    thread_local! { static CX: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("test program lists"))); } // alloc: Cold
    CX.with(|c| *c).mcx()
}

#[path = "../../../kernels/tests/common/mod.rs"]
pub mod krig;

pub const TEXTOID: Oid = 25;
const BOOLOID: Oid = 16;

fn dat_entries(path: &Path) -> Vec<HashMap<String, String>> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    // a brace inside a quoted value (`proargmodes => '{v}'`) is not an entry boundary
    let mut quoted = false;
    let mut prev = ' ';
    for ch in text.chars() {
        let escaped = prev == '\\';
        prev = ch;
        if ch == '\'' && depth > 0 && !escaped {
            quoted = !quoted;
        }
        match ch {
            _ if quoted => cur.push(ch),
            '{' => {
                depth += 1;
                cur.clear();
            }
            '}' if depth > 0 => {
                depth -= 1;
                let mut m = HashMap::new();
                let mut rest = cur.as_str();
                while let Some(i) = rest.find("=>") {
                    let key = rest[..i].trim().trim_start_matches(',').trim().to_owned();
                    let after = &rest[i + 2..];
                    let Some(q) = after.find('\'') else { break };
                    let after = &after[q + 1..];
                    // the closing quote: a `\'` inside the value is an escaped quote (numeric's descr)
                    let mut e = 0;
                    loop {
                        let Some(i) = after[e..].find('\'') else { break };
                        e += i;
                        if e > 0 && after.as_bytes()[e - 1] == b'\\' {
                            e += 1;
                            continue;
                        }
                        break;
                    }
                    if e >= after.len() {
                        break;
                    }
                    m.insert(key, after[..e].to_owned());
                    rest = &after[e + 1..];
                }
                out.push(m);
            }
            _ if depth > 0 => cur.push(ch),
            _ => {}
        }
    }
    out
}

struct TypeRow {
    name: String,
    len: i16,
    byval: bool,
    align: i8,
    typtype: i8,
    collation: Oid,
    elem: Oid,
}

struct ProcRow {
    strict: bool,
    retset: bool,
    nargs: i16,
    volatility: u8,
    parallel: u8,
    node_arg: bool,
}

/// The catalog the fixtures compile against: pg_type / pg_proc / pg_operator from the reference
/// tree; the function and shim tiers from `gravity_fn`.
pub struct DatCatalog {
    types: HashMap<Oid, TypeRow>,
    procs: HashMap<Oid, ProcRow>,
    ops: HashMap<Oid, (Oid, Oid, Oid)>,
    proc_names: HashMap<String, Oid>,
}

/// Whether the descriptors carry a hosted owner (a peek: `Descs::owner` is a `Cell`, shared with every assembled program).
pub fn has_owner(owner: &gravity_lower::Owner) -> bool {
    let o = owner.take();
    let some = o.is_some();
    owner.set(o);
    some
}

fn catalog_dir() -> PathBuf {
    manifest_dir().join("../../postgres-18.6-reference/src/include/catalog")
}

impl DatCatalog {
    pub fn load() -> DatCatalog {
        let d = catalog_dir();
        let mut names: HashMap<String, Oid> = HashMap::new();
        let mut types = HashMap::new();
        for e in dat_entries(&d.join("pg_type.dat")) {
            let (Some(oid), Some(name)) = (e.get("oid"), e.get("typname")) else { continue };
            let oid: Oid = oid.parse().unwrap();
            names.insert(name.clone(), oid);
            types.insert(oid, (e.clone(), name.clone()));
        }
        let mut tys = HashMap::new();
        // the array types genbki derives from `array_type_oid`: varlena, the element's alignment (at least int)
        let mut arrays: Vec<(Oid, Oid)> = Vec::new();
        for (oid, (e, _)) in &types {
            if let Some(a) = e.get("array_type_oid").and_then(|a| a.parse::<Oid>().ok()) {
                arrays.push((a, *oid));
            }
        }
        for (oid, (e, _)) in &types {
            let len: i16 = e.get("typlen").map_or(-1, |v| match v.as_str() {
                "NAMEDATALEN" => 64,
                "SIZEOF_POINTER" => 8,
                v => v.parse().unwrap(),
            });
            let byval = e.get("typbyval").is_some_and(|v| v != "f");
            let align = e.get("typalign").map_or(b'i', |v| v.as_bytes()[0]) as i8;
            let typtype = e.get("typtype").map_or(b'b', |v| v.as_bytes()[0]) as i8;
            let collation = match e.get("typcollation").map(|s| s.as_str()) {
                Some("default") => 100,
                Some("C") => 950,
                _ => 0,
            };
            let elem = e.get("typelem").and_then(|n| names.get(n)).copied().unwrap_or(0);
            tys.insert(*oid, TypeRow { name: types[oid].1.clone(), len, byval, align, typtype, collation, elem });
        }
        for (arr, base) in arrays {
            let Some(b) = tys.get(&base) else { continue };
            let align = if b.align == b'd' as i8 { b'd' as i8 } else { b'i' as i8 };
            let row = TypeRow { name: format!("_{}", b.name), len: -1, byval: false, align, typtype: b'b' as i8, collation: b.collation, elem: base };
            tys.insert(arr, row);
        }
        let mut procs = HashMap::new();
        let mut proc_names: HashMap<String, Oid> = HashMap::new();
        for e in dat_entries(&d.join("pg_proc.dat")) {
            let (Some(oid), Some(name)) = (e.get("oid"), e.get("proname")) else { continue };
            let oid: Oid = oid.parse().unwrap();
            proc_names.entry(name.clone()).or_insert(oid);
            let nargs = e.get("proargtypes").map_or(0, |a| a.split_whitespace().count()) as i16;
            let node_arg = e.get("proargtypes").is_some_and(|a| a.split_whitespace().any(|t| matches!(t, "any" | "anyarray" | "anyelement" | "anynonarray" | "anyenum" | "anyrange" | "anymultirange" | "anycompatible" | "anycompatiblearray" | "anycompatiblenonarray" | "anycompatiblerange" | "anycompatiblemultirange" | "internal" | "record")))
                || e.get("provariadic").is_some_and(|v| v == "any");
            procs.insert(oid, ProcRow { strict: e.get("proisstrict").is_none_or(|v| v == "t"), retset: e.get("proretset").is_some_and(|v| v == "t"), nargs, volatility: e.get("provolatile").map_or(b'i', |v| v.as_bytes()[0]), parallel: e.get("proparallel").map_or(b's', |v| v.as_bytes()[0]), node_arg });
        }
        let mut ops = HashMap::new();
        for e in dat_entries(&d.join("pg_operator.dat")) {
            let (Some(oid), Some(code)) = (e.get("oid"), e.get("oprcode")) else { continue };
            let oid: Oid = oid.parse().unwrap();
            let f = proc_names.get(code).copied().unwrap_or(0);
            let l = e.get("oprleft").and_then(|n| names.get(n)).copied().unwrap_or(0);
            let r = e.get("oprright").and_then(|n| names.get(n)).copied().unwrap_or(0);
            ops.insert(oid, (f, l, r));
        }
        DatCatalog { types: tys, procs, ops, proc_names }
    }
}

fn unknown(what: &str, oid: Oid) -> Box<PgError> {
    Box::new(PgError::error(format!("fixture catalog: unknown {what} {oid}")))
}

impl<'mcx> Catalog<'mcx> for DatCatalog {
    fn func(&self, funcid: Oid) -> PgResult<FuncFacts> {
        let p = self.procs.get(&funcid).ok_or_else(|| unknown("function", funcid))?;
        Ok(FuncFacts { strict: p.strict, retset: p.retset, nargs: p.nargs, volatility: Some(p.volatility), parallel: Some(p.parallel), node_arg: p.node_arg, leakproof: None, prolang: None, body: 0 })
    }
    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        Ok(self.ops.get(&opno).map_or(0, |o| o.0))
    }
    fn op_input_types(&self, opno: Oid) -> PgResult<(Oid, Oid)> {
        let o = self.ops.get(&opno).ok_or_else(|| unknown("operator", opno))?;
        Ok((o.1, o.2))
    }
    fn btree_cmp_proc(&self, _opfamily: Oid, _l: Oid, _r: Oid) -> PgResult<Oid> {
        Ok(0)
    }
    fn opclass(&self, _opclass: Oid) -> PgResult<(Oid, Oid)> {
        Ok((0, 0))
    }
    fn type_facts(&self, typid: Oid) -> PgResult<TypeFacts> {
        let t = self.types.get(&typid).ok_or_else(|| unknown("type", typid))?;
        Ok(TypeFacts { len: t.len, byval: t.byval, align: t.align, typtype: t.typtype, collation: t.collation, elem: t.elem })
    }
    fn type_cmp_proc(&self, _typid: Oid) -> PgResult<Oid> {
        Ok(0)
    }
    fn base_type(&self, typid: Oid, _typmod: &mut i32) -> PgResult<Oid> {
        Ok(typid)
    }
    fn domain(&self, typid: Oid) -> PgResult<DomainFacts<'mcx>> {
        Err(unknown("domain", typid))
    }
    fn io(&self, typid: Oid) -> PgResult<IoFacts> {
        Err(unknown("io of", typid))
    }
    fn text(&self, _collation: Oid) -> PgResult<TextFacts> {
        Ok(TextFacts { deterministic: true, equality_is_bytes: true, order_is_bytes: true, ctype_is_c: true })
    }
    fn tier(&self, funcid: Oid) -> Tier {
        gravity_fn::tier_of(funcid)
    }
    fn shim(&self, funcid: Oid) -> Option<&'static Shim> {
        gravity_fn::shim_of(funcid)
    }
}

static SEAMS: Once = Once::new();
static CAT: std::sync::OnceLock<DatCatalog> = std::sync::OnceLock::new();

pub fn cat() -> &'static DatCatalog {
    CAT.get_or_init(DatCatalog::load)
}

pub fn init() {
    SEAMS.call_once(|| {
        fmgr_core::init_seams();
        syscache_seams::pg_type_element_shape::set(|typid| Ok(cat().types.get(&typid).filter(|t| t.elem != 0).map(|t| syscache_seams::PgTypeElementShape { typelem: t.elem, typsubscript: 0 })));
        syscache_seams::lookup_pg_type_shape::set(|typid| Ok(cat().types.get(&typid).map(|t| PgTypeShape { typlen: t.len, typbyval: t.byval, typalign: t.align, typstorage: if t.len == -1 { b'x' as i8 } else { b'p' as i8 }, typcollation: t.collation })));
        syscache_seams::lookup_pg_type_typcache_shape::set(|typid| {
            Ok(cat().types.get(&typid).map(|t| {
                let mut name = types_tuple::NameData::default();
                name.namestrcpy(&t.name);
                syscache_seams::PgTypeTypcacheShape { typname: name, typlen: t.len, typbyval: t.byval, typalign: t.align, typstorage: if t.len == -1 { b'x' as i8 } else { b'p' as i8 }, typtype: t.typtype, typisdefined: true, typrelid: 0, typsubscript: 0, typelem: t.elem, typarray: 0, typcollation: t.collation }
            }))
        });
        syscache_seams::pg_type_typtype::set(|typid| Ok(cat().types.get(&typid).map(|t| t.typtype)));
        // pg_aggregate from the decomposer's generated table (the transition function by name: the emit reads its strictness only)
        syscache_seams::lookup_pg_aggregate_shape::set(|fnoid| {
            let by_name = |n: &str| cat().proc_names.get(n).copied().unwrap_or(0);
            Ok(gravity_decompose::ops::agg::trans::table::row(fnoid).map(|r| syscache_seams::PgAggregateShape { aggkind: r.kind as i8, aggnumdirectargs: r.ndirect as i16, aggtransfn: by_name(r.transfn), aggfinalfn: by_name(r.finalfn), aggcombinefn: by_name(r.combinefn), aggserialfn: 0, aggdeserialfn: 0, aggmtransfn: 0, aggminvtransfn: 0, aggmfinalfn: 0, aggfinalextra: false, aggmfinalextra: false, aggfinalmodify: b'r' as i8, aggmfinalmodify: b'r' as i8, aggsortop: r.sortop, aggtranstype: r.transtype, aggtransspace: 0, aggmtranstype: 0 }))
        });
        syscache_seams::pg_type_base_shape::set(|typid| Ok(cat().types.get(&typid).map(|t| syscache_seams::PgTypeBaseShape { typtype: t.typtype, typbasetype: 0, typtypmod: -1, typelem: t.elem, typsubscript: 0 })));
        miscinit_seams::get_user_id::set(|| 10);
        aclchk_seams::object_aclcheck::set(|_classid, _objid, _roleid, _mode| Ok(0));
        syscache_seams::pg_proc_proname::set(|_funcid| Ok(None));
        namespace_seams::is_temp_namespace::set(|_| false);
        namespace_seams::type_is_visible::set(|_| Ok(true));
        postgres_seams::check_for_interrupts::set(|| Ok(()));
    });
}

pub fn plans_dir() -> PathBuf {
    manifest_dir().join("../decompose/tests/plans")
}

/// The fixtures whose name starts with `prefix`, sorted.
pub fn fixtures(prefix: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(plans_dir()).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "plan")).filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with(prefix)).map(|p| (p.file_stem().unwrap().to_str().unwrap().to_owned(), std::fs::read_to_string(&p).unwrap())).collect();
    v.sort();
    v
}

pub struct Fx<'m> {
    pub fx: corpus::Fixture<'m>,
}

pub fn load<'m>(ctx: &'m MemoryContext, text: &str) -> corpus::Fixture<'m> {
    corpus::load(ctx.mcx(), text).unwrap()
}

/// A varlena image of `bytes` (a 4-byte header), kept alive by the returned box.
pub fn varlena(bytes: &[u8]) -> Box<[u8]> {
    let len = (bytes.len() + 4) as u32;
    let mut v = vec![0u8; len as usize];
    v[..4].copy_from_slice(&(len << 2).to_le_bytes());
    v[4..].copy_from_slice(bytes);
    v.into_boxed_slice()
}

/// The rig's dictionary: 64 entries in byte-rank order (the format's contract, `pgrc_format::dict.rs`:
/// code order is byte order), code 0 the empty string.
pub fn dict() -> Vec<Vec<u8>> {
    let mut d: Vec<Vec<u8>> = (0..64).map(|c| if c == 0 { Vec::new() } else if c % 7 == 3 { format!("www.garden.com/{c}").into_bytes() } else { format!("entry-{c}").into_bytes() }).collect();
    d.sort();
    d
}

thread_local! {
    static ROWS: RefCell<Vec<krig::Row>> = const { RefCell::new(Vec::new()) };
    // the fixture's NOT NULL attributes `(relid, attno)` (`decompose_fx` records them): a generated row never carries a NULL there —
    // the lowerers trust the fact and read no bitmap on such a lane (PD-OWNER, D048), so a NULL fed there is not a row the server can see
    static NOTNULL: RefCell<Vec<(u32, i16)>> = const { RefCell::new(Vec::new()) };
}

/// The generated rows with every NOT NULL attribute of `p`'s relation made non-NULL (the value word kept).
pub fn honor_notnull(p: &PipelineNode<'_>, mut rows: Vec<krig::Row>) -> Vec<krig::Row> {
    let Some((_, relid, _)) = p.source.relation() else { return rows };
    let lanes = site_lanes(p);
    let nn: Vec<usize> = NOTNULL.with(|n| lanes.iter().enumerate().filter(|(_, l)| n.borrow().contains(&(relid, l.0))).map(|(k, _)| k).collect());
    for (_, nulls) in rows.iter_mut() {
        for &k in nn.iter() {
            if let Some(b) = nulls.get_mut(k) {
                *b = 0;
            }
        }
    }
    rows
}

pub fn set_rows(rows: Vec<krig::Row>) {
    ROWS.with(|r| *r.borrow_mut() = rows);
}

pub fn rows_from_tl(keys: &[u64], _: &[u8]) -> Vec<krig::Row> {
    // a finishing pipeline's section-end claim (`MorselRange::SECTION_END`, lo = u64::MAX) serves no rows, as the heap source's seek
    if keys.first().is_some_and(|&lo| lo == u64::MAX) {
        return Vec::new();
    }
    ROWS.with(|r| r.borrow().clone())
}

/// The lanes of the pipeline's site in lane order: `(attno, kind, width)`.
pub fn site_lanes(p: &PipelineNode<'_>) -> Vec<(i16, u8, u8)> {
    let site = match &p.source {
        Source::Range { site, .. } | Source::Index { site, .. } | Source::Bitmap { site, .. } => site,
        _ => return Vec::new(),
    };
    site.lanes().iter().filter_map(|l| if let LaneId::Attr(a) = l.id { Some((a, l.kind, l.width)) } else { None }).collect()
}

/// Rows for the site: `n` rows, the value of lane `k` from a small pattern around the zone
/// constants (so verdicts and filters split the rows), a NULL every 11th row, codes below 64.
pub fn gen_rows(p: &PipelineNode<'_>, n: usize, zone_values: &[i64]) -> Vec<krig::Row> {
    let lanes = site_lanes(p);
    let rows = (0..n)
        .map(|r| {
            let mut v = Vec::new();
            let mut nulls = Vec::new();
            for (k, &(attno, kind, _)) in lanes.iter().enumerate() {
                let base = ((r as i64 * 7919 + attno as i64 * 13) % 97) - 40;
                let val = if kind == gravity_abi::ctx::lane::KIND_CODES {
                    (r as i64 * 5 + k as i64) % 64
                } else if !zone_values.is_empty() && r % 5 == k % 5 {
                    zone_values[r % zone_values.len()]
                } else {
                    base
                };
                v.push(val as u64);
                nulls.push(u8::from(r % 11 == 7 && k == r % 3));
            }
            (v, nulls)
        })
        .collect();
    honor_notnull(p, rows)
}

pub struct Oracle<'a, 'm> {
    pub cat: &'a DatCatalog,
    pub fx: &'a corpus::Fixture<'m>,
    pub params: &'a [ParamExternData],
    pub dict: Vec<Box<[u8]>>,
}

impl<'a: 'm, 'm> Oracle<'a, 'm> {
    pub fn new(cat: &'a DatCatalog, fx: &'a corpus::Fixture<'m>, params: &'a [ParamExternData]) -> Oracle<'a, 'm> {
        Oracle { cat, fx, params, dict: dict().iter().map(|e| varlena(e)).collect() }
    }

    pub fn compile(&self, node: Node<'m>) -> Compiled<'m> {
        let node = node.as_target_entry().map_or(node, |te| te.expr);
        let bind = ParamBind { extern_params: Some(self.params), exec_vals: None, n_exec: 0, param_callback: None, param_expanded: None, param_rw: None, param_ro_expanded: None, extern_fetch: None };
        Expr::compile(lists(), node, self.cat, bind).unwrap_or_else(|e| panic!("tier 0: {e} ({:?} in {})", node.node_tag(), self.fx.name))
    }

    /// `compile` of an aggregate argument through the child's projection (`outer.N` = entry `N` of `project`).
    pub fn compile_outer(&self, node: Node<'m>, project: &types_nodes::list::NodeList<'m>) -> Compiled<'m> {
        let node = node.as_target_entry().map_or(node, |te| te.expr);
        let bind = ParamBind { extern_params: Some(self.params), exec_vals: None, n_exec: 0, param_callback: None, param_expanded: None, param_rw: None, param_ro_expanded: None, extern_fetch: None };
        Expr::compile_outer(lists(), node, self.cat, bind, project).unwrap_or_else(|e| panic!("tier 0: {e} ({:?} in {})", node.node_tag(), self.fx.name))
    }

    pub fn eval(&self, c: &mut Compiled<'m>, values: &[Datum], nulls: &[bool]) -> NullableDatum {
        let mut buf = [0u8; 4096];
        let mut region = Region::over(&mut buf);
        let row = Row::scan(values, nulls);
        c.eval(&row, &mut region).unwrap_or_else(|e| panic!("tier 0 eval: {}", e.message()))
    }

    /// The expected output rows of pipeline `p` over `rows` (lane order): filters, projections
    /// (a Var's datum: the lane value, a code lane's the dictionary entry's text), LIMIT / OFFSET.
    pub fn expect(&self, p: &PipelineNode<'m>, rows: &[krig::Row]) -> Vec<Vec<u64>> {
        self.expect_junk(p, rows, false)
    }

    /// `expect` over the flat ops before a sort / join (their junk columns kept when `keep_junk`).
    pub fn expect_junk(&self, p: &PipelineNode<'m>, rows: &[krig::Row], keep_junk: bool) -> Vec<Vec<u64>> {
        let lanes = site_lanes(p);
        let mut out = Vec::new();
        let mut filters: Vec<Compiled<'m>> = Vec::new();
        let mut projects: Vec<Vec<Compiled<'m>>> = Vec::new();
        let (mut count, mut offset) = (usize::MAX, 0usize);
        let mut gates = Vec::new();
        for op in p.ops.iter() {
            match op {
                Op::Filter(n) => filters.push(self.compile(*n)),
                Op::Gate(n) => match n.as_list() {
                    Some(l) => gates.extend(l.iter().map(|q| self.compile(q))),
                    None => gates.push(self.compile(*n)),
                },
                Op::Project(t) => projects.push(t.iter().filter_map(|e| e.as_target_entry().filter(|te| keep_junk || !te.resjunk).map(|te| self.compile(te.expr))).collect()),
                Op::Limit { count: c, offset: o, .. } => {
                    count = match c {
                        Bound::All => usize::MAX,
                        Bound::Const(n) => *n as usize,
                        Bound::Expr(n) => self.eval(&mut self.compile(*n), &[], &[]).value.as_i64() as usize,
                    };
                    offset = match o {
                        Bound::All => 0,
                        Bound::Const(n) => *n as usize,
                        Bound::Expr(n) => self.eval(&mut self.compile(*n), &[], &[]).value.as_i64() as usize,
                    };
                }
                Op::Sort(_) | Op::Join(_) => break,
                // an identity set's marker changes no row of its pipeline (setops_shapes.rs models the set over the sides' rows)
                Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Identity { .. }) => {}
                Op::Window(_) | Op::SetOp(_) | Op::SubPlan(_) | Op::Append(_) | Op::Write(_) | Op::Lock(_) | Op::Srf(_) | Op::FirstOfRun(_) | Op::GroupRun(_) => return out,
            }
        }
        for g in gates.iter_mut() {
            let r = self.eval(g, &[], &[]);
            if r.isnull || r.value.as_u64() == 0 {
                return out;
            }
        }
        let mut src_rows: Vec<krig::Row> = if lanes.is_empty() { vec![(Vec::new(), Vec::new())] } else { rows.to_vec() };
        let mut vlanes: Vec<(i16, u8, u8)> = Vec::new();
        if let Source::Values { lists, .. } = &p.source {
            src_rows.clear();
            for l in lists.iter() {
                let (mut v, mut n) = (Vec::new(), Vec::new());
                for (k, e) in l.as_list().unwrap().iter().enumerate() {
                    let r = self.eval(&mut self.compile(e), &[], &[]);
                    v.push(r.value.as_u64());
                    n.push(u8::from(r.isnull));
                    if vlanes.len() <= k {
                        vlanes.push((k as i16 + 1, 0, 8));
                    }
                }
                src_rows.push((v, n));
            }
        }
        let lanes = if vlanes.is_empty() { lanes } else { vlanes };
        // a physical target list names attributes no lane carries (they are NULL: nothing downstream reads them)
        let varno = gravity_lower::varno_of(&p.source);
        let mut max_var = 0usize;
        for op in p.ops.iter() {
            if let Op::Project(t) = op {
                for e in t.iter() {
                    gravity_decompose::expr::walk(e, &mut |x| {
                        if let Some(v) = x.as_var() {
                            if v.varno == varno && v.varlevelsup == 0 && v.varattno > 0 {
                                max_var = max_var.max(v.varattno as usize);
                            }
                        }
                        true
                    });
                }
            }
        }
        // an index-only scan's `index.k` is the k-th index column: its lanes are read by position
        let by_position = matches!(p.source, Source::Index { only: true, .. });
        let natts = if by_position { lanes.len() } else { lanes.iter().map(|l| l.0 as usize).max().unwrap_or(0) }.max(max_var).max(1);
        let mut passed = 0usize;
        for (v, n) in &src_rows {
            let mut values = vec![Datum::from_i64(0); natts];
            let mut nulls = vec![true; natts];
            for (k, &(attno, kind, _)) in lanes.iter().enumerate() {
                let i = if by_position { k } else { attno as usize - 1 };
                nulls[i] = n[k] != 0;
                values[i] = if kind == gravity_abi::ctx::lane::KIND_CODES { Datum::from_usize(self.dict[v[k] as usize].as_ptr() as usize) } else { Datum::from_u64(v[k]) };
            }
            let mut ok = true;
            for f in filters.iter_mut() {
                let r = self.eval(f, &values, &nulls);
                if r.isnull || r.value.as_u64() == 0 {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            if passed < offset {
                passed += 1;
                continue;
            }
            if out.len() >= count {
                break;
            }
            passed += 1;
            let (mut cur_v, mut cur_n) = (values.clone(), nulls.clone());
            let mut row_out = Vec::new();
            for (si, stage) in projects.iter_mut().enumerate() {
                let outer_v = cur_v.clone();
                let outer_n = cur_n.clone();
                let mut nv = Vec::new();
                let mut nn = Vec::new();
                for c in stage.iter_mut() {
                    let (vals, nls) = if si == 0 { (&values, &nulls) } else { (&outer_v, &outer_n) };
                    let mut buf = [0u8; 4096];
                    let mut region = Region::over(&mut buf);
                    let row = if si == 0 { Row::scan(vals, nls) } else { Row { scan: gravity_expr::Cols::None, inner: gravity_expr::Cols::None, outer: gravity_expr::Cols::Slot { values: vals, nulls: nls, types: &[], tdtypeid: 0, desc: None, slot: None }, aggs: &[] } };
                    let r = c.eval(&row, &mut region).unwrap();
                    nv.push(r.value);
                    nn.push(r.isnull);
                }
                row_out = nv.iter().zip(&nn).map(|(v, n)| if *n { u64::MAX } else { v.as_u64() }).collect();
                cur_v = nv;
                cur_n = nn;
            }
            out.push(row_out);
        }
        out
    }
}

/// The lowering of a fixture's graph under the sequential plan: the IR verified, the program
/// through `backend`; the words the binder fills, resolved for the rig (`params`).
pub struct Built<'p> {
    pub lowered: Lowered,
    pub program: Program,
    pub words: Vec<WordSrc<'p>>,
}

pub fn build<'p>(arena: &Arena, graph: &PipelineGraph<'p>, facts: &dyn Facts, cat: &DatCatalog, backend: Backend) -> Result<Built<'p>, gravity_lower::Refusal> {
    let mut plan = Sequential::new(arena.handle());
    let pctx = mcx::MemoryContext::new("GravityPlan");
    let lowered = lower(arena.handle(), pctx.mcx(), graph, facts, cat, &mut plan, gravity_lower::LowerOpts::default())?;
    gravity_ir::verify::verify(&lowered.ir).unwrap_or_else(|e| panic!("{e:?}\n{}", gravity_ir::text::print_program(&lowered.ir)));
    gravity_ir::verify::verify_dominance(&lowered.ir).unwrap_or_else(|e| panic!("{e:?}\n{}", gravity_ir::text::print_program(&lowered.ir)));
    let codes = compile(&lowered, backend).unwrap_or_else(|e| panic!("backend: {}", e.message()));
    let program = assemble(&lowered, codes);
    Ok(Built { lowered, program, words: plan.words.iter().copied().collect() })
}

/// The rig's parameter words from the plan's word sources; an `Expr` word is evaluated at tier 0.
pub fn rig_params(words: &[WordSrc<'_>], externs: &[ParamExternData], cat: &DatCatalog) -> Option<(Vec<u64>, Vec<u8>)> {
    let mut v = Vec::new();
    let mut n = Vec::new();
    for w in words {
        match *w {
            WordSrc::Const { value, .. } | WordSrc::Elem { value, .. } => {
                v.push(value.value.as_u64());
                n.push(u8::from(value.isnull));
            }
            WordSrc::Extern { id, .. } => {
                let p = *externs.get(id as usize - 1)?;
                v.push(p.value.as_u64());
                n.push(u8::from(p.isnull));
            }
            WordSrc::Kernel => {
                v.push(0);
                n.push(0);
            }
            // D422: the server binds the execution's CurrentOfExpr; the crate's rig has no cursor. D2834: nor a PL list
            WordSrc::Cursor(_) | WordSrc::Set { .. } | WordSrc::Deferred { .. } => return None,
            WordSrc::Exec { .. } => {
                v.push(0);
                n.push(1);
            }
            WordSrc::Expr(node) | WordSrc::Late(node) => {
                let bind = ParamBind { extern_params: Some(externs), exec_vals: None, n_exec: 0, param_callback: None, param_expanded: None, param_rw: None, param_ro_expanded: None, extern_fetch: None };
                let mut c = Expr::compile(lists(), node, cat, bind).ok()?;
                let mut buf = [0u8; 256];
                let mut region = Region::over(&mut buf);
                let r = c.eval(&Row::EMPTY, &mut region).ok()?;
                v.push(r.value.as_u64());
                n.push(u8::from(r.isnull));
            }
        }
    }
    if v.is_empty() {
        v.push(0);
        n.push(0);
    }
    Some((v, n))
}

pub fn count_insts(p: &gravity_ir::Program) -> (usize, usize) {
    let f = &p.funcs[0];
    (f.blocks.len(), f.blocks.iter().map(|b| b.insts.len() + 1).sum())
}

pub fn extern_params(vals: &[i64], types: &[Oid]) -> Vec<ParamExternData> {
    vals.iter().zip(types).map(|(&v, &t)| ParamExternData { value: Datum::from_i64(v), isnull: false, pflags: 0, ptype: t }).collect()
}

pub fn arc(p: Program) -> Arc<Program> {
    Arc::new(p)
}

pub fn decompose_fx<'p>(arena: &Arena, fx: &'p corpus::Fixture<'p>) -> Result<PipelineGraph<'p>, gravity_decompose::Refusal> {
    NOTNULL.with(|n| *n.borrow_mut() = fx.facts.atts.iter().filter(|(_, _, f)| f.notnull).map(|(r, a, _)| (*r, *a)).collect());
    gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx)
}

pub fn text_datum_bytes(d: u64) -> Vec<u8> {
    // SAFETY: the datum is a varlena the rig or the dictionary owns.
    unsafe { datum::varlena::VarlenaRef::from_ptr(d as *const u8) }.data().to_vec()
}

pub fn program_text(l: &Lowered) -> String {
    gravity_ir::text::print_program(&l.ir)
}

pub fn bool_type() -> Oid {
    BOOLOID
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
