use mcx::Mcx;
use types_core::Oid;
use types_nodes::bitmapset::Bitmapset;
use types_nodes::jointype::JoinType;
use types_nodes::list::{IntList, NodeList, OidList, OptNodeList};
use types_nodes::nodes_enums::{CmdType, LimitOption, LockClauseStrength, LockWaitPolicy};
use types_nodes::plannodes::{Result as ResultPlan, *};
use types_nodes::{Node, NodeTag};

use gravity_program::limits;

use crate::catalog::{expr_fp, nth_index_expr, Am, AttFact, ExprFp, Facts, IndexExt, IndexFact, InsteadOf, LeafFact, RelFact};
use crate::effects::FuncProps;
use crate::ledger::Kind;
use crate::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};

pub struct Fixture<'m> {
    pub name: String,
    pub sql: String,
    pub stmt: &'m PlannedStmt<'m>,
    pub facts: FixtureFacts,
    /// The context the plan was read into: `decompose_in`'s node allocator (a MERGE's carried columns).
    pub mcx: Mcx<'m>,
}

#[derive(Default)]
pub struct FixtureFacts {
    pub rels: Vec<(RelFact, String)>,
    pub atts: Vec<(u32, i16, AttFact)>,
    pub idxs: Vec<IndexFact>,
    pub idx_ams: Vec<(u32, crate::catalog::IndexAm)>,
    /// Per index: the AM / constraint facts (`am=`, `deferrable=`, `excl=`, `primary=`, `nkeyatts=`).
    pub exts: Vec<(u32, IndexExt)>,
    /// `(relid, attno, attgenerated)` for the generated columns (`gen=` on an att line).
    pub gens: Vec<(u32, i16, u8)>,
    /// Per index with expression columns: the raw `pg_index.indexprs` text (`-- idxexprs` line) and, once
    /// `load` parsed it, the fingerprint per key position (`ExprFp::NONE` for a column).
    pub exprs: Vec<(u32, String, [ExprFp; gravity_program::limits::KEYS])>,
    /// Per sort operator outside the built-in direction table (`-- op` line): descending.
    pub ops: Vec<(u32, bool)>,
    /// `(opno, left, right)` per `-- imageeq` line: an equality whose equal values are one image of each side's type
    /// (`Facts::op_image_eq`; the server answers from the key home's list): the fixture's own lines first, then the
    /// shared bootstrap rows (`SHARED_IMAGEEQ_LINES`), so a fixture's own line for an operator wins.
    pub image_eqs: Vec<(u32, u32, u32)>,
    /// Per relation with INSTEAD OF row triggers (`-- trig` line): which commands they take.
    pub trigs: Vec<(u32, InsteadOf)>,
    /// Per writable relation (`-- trigext` line, printed also when it has none): its trigger kinds (`Facts::trig_ext`).
    pub trig_exts: Vec<(u32, TrigExt)>,
    /// `(parent, child, has_triggers, has_rules)` per `-- part` line: the partition tree under a routing root.
    pub parts: Vec<(u32, u32, bool, bool)>,
    /// `(relid, attno, lo, hi)` per `range=lo:hi` on an att line: the column's zone extrema (`Facts::att_range`).
    pub ranges: Vec<(u32, i16, i64, i64)>,
    /// `(relid, attno, key)` per `shred=key` on an att line: a root key the parts elected as a Text path (`Facts::shred_election`).
    pub shreds: Vec<(u32, i16, String)>,
    /// Per function past the builtin table that the plan calls (`-- func` line): its pg_proc row as `Facts::func` and the
    /// lowering's catalog read it.
    pub funcs: Vec<FuncLine>,
    /// D3681 (a test's switch, never a fixture line): two heap relations of one access method whose captured attributes
    /// are equal are heap and write twins (`Facts::heap_twin`, `Facts::write_twin`); off, none is (the capture holds no
    /// layout or constraint facts past the attributes).
    pub twins: bool,
    /// D4497 / D3227 (a test's switch, never a fixture line): the on-by-default roads (`crate::ROADS_DEFAULT_ON`) this
    /// test names off, as `SET pgrust.gravity_road_off` names them on a server (`Facts::default_on_road_on`); empty,
    /// every such road reads on, as off a backend.
    pub roads_off: Vec<&'static str>,
    /// D4620 (a test's switch, never a fixture line): the default-off roads (`crate::ROADS`) this test names on, as
    /// `SET pgrust.gravity_road_off = '+<road>'` does on a server (`Facts::road_on`); empty, every such road reads off,
    /// as off a backend.
    pub roads_on: Vec<&'static str>,
}

/// A `-- func <oid> vol=<i|s|v> strict=<t|f> leakproof=<t|f> parallel=<s|r|u> retset=<t|f> nargs=<n> lang=<oid>
/// nodearg=<t|f>` line: a function's pg_proc row (provolatile, proisstrict, proleakproof, proparallel, proretset, pronargs,
/// prolang) and whether an argument is polymorphic, `any`, `record` or `internal` (its body reads its calling node).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncLine {
    pub oid: u32,
    pub props: FuncProps,
    pub retset: bool,
    pub nargs: i16,
    pub lang: u32,
    pub node_arg: bool,
}

impl FixtureFacts {
    /// `twins`: `a` and `b` heap relations of one width whose captured attributes are equal.
    fn twin(&self, a: u32, b: u32) -> bool {
        let (Some(x), Some(y)) = (self.rel(a), self.rel(b)) else { return false };
        let atts = |r: u32| self.atts.iter().filter(move |(id, _, _)| *id == r).map(|(_, n, f)| (*n, *f));
        self.twins && x.am == Am::Heap && y.am == Am::Heap && x.relkind == b'r' && y.relkind == b'r' && x.natts == y.natts && atts(a).eq(atts(b))
    }
}

impl Facts for FixtureFacts {
    fn heap_twin(&self, a: u32, b: u32) -> bool {
        self.twin(a, b)
    }

    fn write_twin(&self, a: u32, b: u32) -> bool {
        self.twin(a, b)
    }

    fn rel(&self, relid: u32) -> Option<RelFact> {
        self.rels.iter().find(|(r, _)| r.relid == relid).map(|(r, _)| *r)
    }

    fn att(&self, relid: u32, attno: i16) -> Option<AttFact> {
        self.atts.iter().find(|(r, a, _)| *r == relid && *a == attno).map(|(_, _, f)| *f)
    }

    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.ranges.iter().find(|(r, a, _, _)| *r == relid && *a == attno).map(|(_, _, lo, hi)| (*lo, *hi))
    }

    fn index(&self, indexid: u32) -> Option<IndexFact> {
        self.idxs.iter().find(|i| i.indexid == indexid).copied()
    }

    fn index_am(&self, indexid: u32) -> crate::catalog::IndexAm {
        self.idx_ams.iter().find(|(i, _)| *i == indexid).map_or(crate::catalog::IndexAm::Other, |(_, a)| *a)
    }

    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) {
        self.idxs.iter().filter(|i| i.relid == relid).for_each(|i| f(*i));
    }

    fn index_ext(&self, indexid: u32) -> IndexExt {
        self.exts.iter().find(|(i, _)| *i == indexid).map_or(IndexExt::UNKNOWN, |(_, e)| *e)
    }

    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.gens.iter().find(|(r, a, _)| *r == relid && *a == attno).map_or(0, |(_, _, g)| *g)
    }

    /// KeySem slice 3: a captured `-- op` line is a default family's ordering operator (the direction table's), its family,
    /// input type and comparator unknown to the capture (0); else the bootstrap rows' (`opdir::builtin_key`).
    fn key(&self, req: crate::catalog::KeyReq) -> Option<crate::catalog::KeyOrder> {
        let captured = self.ops.iter().find(|(o, _)| *o == req.op).map(|&(_, desc)| crate::catalog::KeyOrder { desc, family: 0, opcintype: 0, default_family: true, cmp: 0, no_proc: false });
        captured.or_else(|| crate::catalog::opdir::builtin_key(req.op))
    }

    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.image_eqs.iter().find(|e| e.0 == opno).map(|e| (e.1, e.2))
    }

    fn instead_of(&self, relid: u32) -> InsteadOf {
        self.trigs.iter().find(|(r, _)| *r == relid).map_or(InsteadOf::UNKNOWN, |(_, t)| *t)
    }

    /// The relation's `-- trigext` line; `UNKNOWN` without one (a capture older than the line).
    fn shred_election(&self, relid: u32, attno: i16, keys: &[&str]) -> Option<(u8, i32)> {
        self.shreds.iter().any(|(r, a, k)| *r == relid && *a == attno && keys == [k.as_str()]).then_some((0, 0))
    }

    fn trig_ext(&self, relid: u32) -> TrigExt {
        self.trig_exts.iter().find(|(r, _)| *r == relid).map_or(TrigExt::UNKNOWN, |(_, t)| *t)
    }

    /// The tree walk of `hosts/src/facts.rs::leaves_under` over the `-- part` lines (none under the root = unknown).
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(LeafFact)) -> bool {
        if !self.parts.iter().any(|p| p.0 == root) {
            return false;
        }
        self.leaves_under(root, root, f);
        true
    }

    fn index_expr(&self, indexid: u32, col: u8) -> Option<ExprFp> {
        self.exprs.iter().find(|(i, _, _)| *i == indexid).map(|(_, _, fps)| fps[col as usize]).filter(|fp| fp.is_some())
    }

    fn reads_hash_functions(&self) -> bool {
        false
    }

    /// The fixture's `-- func` line, else the builtin table (the default's answer).
    fn func(&self, funcid: u32) -> Option<FuncProps> {
        self.funcs.iter().find(|f| f.oid == funcid).map(|f| f.props).or_else(|| crate::effects::builtin_func(funcid))
    }

    /// On unless the test named the road off (`roads_off`).
    fn default_on_road_on(&self, road: &str) -> bool {
        !self.roads_off.iter().any(|r| *r == road)
    }

    /// Off unless the test named the road on (`roads_on`).
    fn road_on(&self, name: &str) -> bool {
        self.roads_on.iter().any(|r| *r == name)
    }
}

type R<T> = core::result::Result<T, String>;

fn kv<'a>(words: &[&'a str], key: &str) -> R<&'a str> {
    words.iter().find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('='))).ok_or_else(|| format!("fixture header: missing {key}=")) // alloc: Cold
}

fn num<T: core::str::FromStr>(s: &str, what: &str) -> R<T> {
    s.parse().map_err(|_| format!("fixture header: bad {what} {s:?}")) // alloc: Cold
}

impl FixtureFacts {
    fn leaves_under(&self, root: u32, parent: u32, f: &mut dyn FnMut(LeafFact)) {
        crate::walk::stack_check();
        for &(_, child, trig, rules) in self.parts.iter().filter(|p| p.0 == parent) {
            match self.rel(child) {
                Some(r) if r.relkind == b'p' => self.leaves_under(root, child, f),
                _ => f(self.leaf_fact(root, child, trig, rules)),
            }
        }
    }

    fn leaf_fact(&self, root: u32, leaf: u32, trig: bool, rules: bool) -> LeafFact {
        let atts = |relid: u32| self.atts.iter().filter(move |(r, _, _)| *r == relid);
        let rel = self.rel(leaf);
        let natts = rel.map_or(0, |r| r.natts);
        let same_layout = self.rel(root).is_some_and(|r| r.natts == natts) && atts(root).count() == atts(leaf).count() && atts(root).zip(atts(leaf)).all(|(a, b)| a.1 == b.1 && a.2.typid == b.2.typid && a.2.typmod == b.2.typmod);
        let (mut nindexes, mut plain) = (0u16, true);
        self.indexes_of(leaf, &mut |i| {
            nindexes += 1;
            let e = self.index_ext(i.indexid);
            plain &= i.btree && !i.partial && !e.deferrable && !e.exclusion && i.nkeys > 0 && !i.keys().iter().any(|&a| a <= 0) && (e.nkeyatts == 0 || e.nkeyatts == i.nkeys);
        });
        let trig_ext = if trig { self.trig_ext(leaf) } else { TrigExt::NONE };
        LeafFact { relid: leaf, natts, heap: rel.is_some_and(|r| r.am == Am::Heap && r.relkind == b'r'), foreign: rel.is_some_and(|r| r.relkind == b'f'), pgrc: rel.is_some_and(|r| r.am == Am::Pgrcolumnar && r.relkind == b'r'), same_layout, has_triggers: trig, trig: trig_ext, has_rules: rules, has_generated: self.gens.iter().any(|g| g.0 == leaf), nindexes, indexes_plain: plain }
    }

    pub fn line(&mut self, line: &str) -> R<bool> {
        let w: Vec<&str> = line.split_whitespace().collect(); // alloc: Cold
        match w.first().copied() {
            Some("rel") => {
                let relid = num(w[1], "relid")?;
                let name = w[2].to_string(); // alloc: Cold
                let am = Am::of_name(kv(&w, "am")?);
                let relkind = kv(&w, "kind")?.bytes().next().unwrap_or(b'r');
                let natts = num(kv(&w, "natts")?, "natts")?;
                let mut f = RelFact::plain(relid, am, relkind, natts);
                if kv(&w, "rows").is_ok_and(|v| v == "t") {
                    f = f.on_rows_road();
                }
                if let Ok(s) = kv(&w, "sorted") {
                    let mut attnos = [0i16; limits::KEYS];
                    let mut n = 0;
                    for a in s.split(',') {
                        if n == limits::KEYS {
                            return Err(format!("fixture header: rel {relid}: more than {} sorted columns", limits::KEYS)); // alloc: Cold
                        }
                        attnos[n] = num(a, "sorted")?;
                        n += 1;
                    }
                    f = f.with_sorted(attnos[..n].iter().copied());
                }
                self.rels.push((f, name));
            }
            Some("att") => {
                let relid = num(w[1], "relid")?;
                let attno = num(w[2], "attno")?;
                let mut f = AttFact::plain(num(kv(&w, "typid")?, "typid")?, num(kv(&w, "typmod")?, "typmod")?, num(kv(&w, "len")?, "len")?, kv(&w, "byval")? == "t", kv(&w, "notnull")? == "t");
                // D2700: a domain column's base type and typmod (absent: the column's own)
                if let Ok(b) = kv(&w, "base") {
                    let (t, m) = b.split_once(':').ok_or_else(|| format!("fixture header: bad base {b:?}"))?; // alloc: Cold
                    (f.base, f.base_typmod) = (num(t, "base")?, num(m, "base typmod")?);
                }
                self.atts.push((relid, attno, f));
                if let Ok(g) = kv(&w, "gen") {
                    if let Some(g) = g.bytes().next().filter(|g| *g != b'-') {
                        self.gens.push((relid, attno, g));
                    }
                }
                if let Ok(r) = kv(&w, "range") {
                    let (lo, hi) = r.split_once(':').ok_or_else(|| format!("fixture header: bad range {r:?}"))?; // alloc: Cold
                    self.ranges.push((relid, attno, num(lo, "range")?, num(hi, "range")?));
                }
                if let Ok(k) = kv(&w, "shred") {
                    self.shreds.push((relid, attno, k.to_string())); // alloc: Cold
                }
            }
            Some("idx") => {
                let indexid = num(w[1], "indexid")?;
                let mut keys = [0i16; gravity_program::limits::KEYS];
                let mut nkeys = 0u8;
                for k in kv(&w, "keys")?.split(',').filter(|k| !k.is_empty()) {
                    if nkeys as usize == keys.len() {
                        return Err(format!("index {indexid}: more than {} key columns (limits::KEYS = INDEX_MAX_KEYS)", keys.len())); // alloc: Cold
                    }
                    keys[nkeys as usize] = num(k, "key")?;
                    nkeys += 1;
                }
                let am = kv(&w, "am")?;
                self.idx_ams.push((indexid, crate::catalog::IndexAm::of_name(am)));
                // the D5 facts are required: a fixture without the key options fails to load
                let mut opts = [0u8; gravity_program::limits::KEYS];
                for (i, o) in kv(&w, "opts")?.split(',').filter(|o| !o.is_empty()).enumerate() {
                    if i >= nkeys as usize {
                        return Err(format!("index {indexid}: more options than key columns")); // alloc: Cold
                    }
                    opts[i] = num(o, "opt")?;
                }
                // the D1 index facts are required: a fixture whose index does not classify fails to load
                let flag = |key: &str| kv(&w, key).map(|v| v == "t");
                let deferrable = flag("deferrable")?;
                self.idxs.push(IndexFact { indexid, relid: num(kv(&w, "rel")?, "rel")?, keys, opts, nkeys, unique: kv(&w, "unique")? == "t", immediate: !deferrable, btree: am == "btree", partial: kv(&w, "partial").is_ok_and(|p| p == "t") });
                self.exts.push((indexid, IndexExt { deferrable, valid: flag("valid").unwrap_or(true), ready: flag("ready").unwrap_or(true), exclusion: flag("excl")?, primary: flag("primary")?, nkeyatts: num(kv(&w, "nkeyatts")?, "nkeyatts")?, known: true }));
            }
            Some("op") => {
                // `op <opno> family=<opfamily> strat=<1|5>`: the btree opfamily membership of a user operator
                let opno = num(w[1], "opno")?;
                let strat: u8 = num(kv(&w, "strat")?, "strat")?;
                self.ops.push((opno, strat == 5)); // alloc: Cold
            }
            Some("imageeq") => {
                // `imageeq <opno> left=<typid> right=<typid>`: an image equality the plan reads (`Facts::op_image_eq`)
                let opno = num(w[1], "opno")?;
                self.image_eqs.push((opno, num(kv(&w, "left")?, "left")?, num(kv(&w, "right")?, "right")?)); // alloc: Cold
            }
            Some("trig") => {
                // `trig <relid> instead=<i?u?d?>`: the relation's INSTEAD OF row triggers (a view target)
                let relid = num(w[1], "relid")?;
                self.trigs.push((relid, InsteadOf::of_letters(kv(&w, "instead")?))); // alloc: Cold
            }
            Some("trigext") => {
                // `trigext <relid> before=<iud|-> after= instead= stmt= trans= internal= defer= rules=<t|f>`: the relation's
                // trigger kinds as the relcache's trigger descriptor holds them (gravity_capture.trigext); `rules=` is RelExt's;
                let bits = |key: &str| -> R<u8> {
                    let v = kv(&w, key)?;
                    v.chars().filter(|&c| c != '-').try_fold(0u8, |b, c| match c {
                        'i' => Ok(b | TRIG_INSERT),
                        'u' => Ok(b | TRIG_UPDATE),
                        'd' => Ok(b | TRIG_DELETE),
                        _ => Err(format!("fixture header: bad trigext {key}={v:?}")), // alloc: Cold
                    })
                };
                let t = TrigExt { before_row: bits("before")?, after_row: bits("after")?, instead_row: bits("instead")?, statement: bits("stmt")?, transition: bits("trans")?, internal: bits("internal")?, deferrable: bits("defer")?, known: true };
                self.trig_exts.push((num(w[1], "relid")?, t)); // alloc: Cold
            }
            Some("part") => {
                // `part <parent> <child> trig=<t|f> rules=<t|f>`: one pg_inherits edge of a partition tree
                let flag = |key: &str| kv(&w, key).map(|v| v == "t");
                self.parts.push((num(w[1], "parent")?, num(w[2], "child")?, flag("trig")?, flag("rules")?)); // alloc: Cold
            }
            Some("func") => {
                let flag = |key: &str| -> R<bool> {
                    match kv(&w, key)? {
                        "t" => Ok(true),
                        "f" => Ok(false),
                        v => Err(format!("fixture header: bad func {key}={v:?}")), // alloc: Cold
                    }
                };
                let (vol, par) = (kv(&w, "vol")?, kv(&w, "parallel")?);
                let props = FuncProps::of_letters(vol, flag("strict")?, flag("leakproof")?, par).ok_or_else(|| format!("fixture header: bad func vol={vol:?} parallel={par:?}"))?; // alloc: Cold
                let f = FuncLine { oid: num(w[1], "func oid")?, props, retset: flag("retset")?, nargs: num(kv(&w, "nargs")?, "nargs")?, lang: num(kv(&w, "lang")?, "lang")?, node_arg: flag("nodearg")? };
                self.funcs.push(f); // alloc: Cold
            }
            Some("idxexprs") => {
                let indexid = num(w[1], "indexid")?;
                let text = line.splitn(3, char::is_whitespace).nth(2).ok_or("fixture header: idxexprs without a list")?.trim();
                self.exprs.push((indexid, text.to_string(), [ExprFp::NONE; gravity_program::limits::KEYS])); // alloc: Cold
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// The bootstrap catalog's image equalities every fixture reads (corpus/imageeq.facts: test data, the same rows as the
/// key home's gravity_fn typesem `keysem::image`, which the server answers from; lowerv4 tests/keysem_image_fixture.rs
/// checks the two equal).
pub const SHARED_IMAGEEQ_LINES: &str = include_str!("corpus/imageeq.facts");

pub fn load<'m>(mcx: Mcx<'m>, text: &str) -> R<Fixture<'m>> {
    let mut name = String::new(); // alloc: Cold
    let mut sql = String::new(); // alloc: Cold
    let mut facts = FixtureFacts::default();
    let mut plan = String::new(); // alloc: Cold
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("-- ") {
            if let Some(n) = h.strip_prefix("name: ") {
                name = n.trim().to_string(); // alloc: Cold
            } else if let Some(s) = h.strip_prefix("sql: ") {
                sql = s.trim().to_string(); // alloc: Cold
            } else {
                facts.line(h)?;
            }
        } else if !line.trim().is_empty() {
            if !plan.is_empty() {
                plan.push(' ');
            }
            plan.push_str(line.trim());
        }
    }
    for line in SHARED_IMAGEEQ_LINES.lines().filter(|l| l.starts_with("-- imageeq ")) {
        facts.line(&line[3..])?;
    }
    let rd = Rd { mcx };
    for i in 0..facts.exprs.len() {
        let (indexid, text) = (facts.exprs[i].0, facts.exprs[i].1.clone()); // alloc: Cold
        let idx = facts.index(indexid).ok_or_else(|| format!("fixture: idxexprs for an unknown index {indexid}"))?; // alloc: Cold
        let list = rd.list(&text)?;
        for col in 0..idx.nkeys {
            if let Some(e) = nth_index_expr(&idx, &list, col) {
                facts.exprs[i].2[col as usize] = expr_fp(e, 1).unwrap_or(ExprFp::NONE);
            }
        }
    }
    let node = rd.node(&plan)?.ok_or_else(|| "fixture: no plan".to_string())?; // alloc: Cold
    let stmt = node.as_planned_stmt().ok_or_else(|| "fixture: not a PLANNEDSTMT".to_string())?; // alloc: Cold
    Ok(Fixture { name, sql, stmt, facts, mcx })
}

struct Rd<'m> {
    mcx: Mcx<'m>,
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\n' | b'\t' | b'\r')
}

fn span_end(s: &[u8], start: usize) -> usize {
    let mut i = start;
    if s[i..].starts_with(b"<>") {
        return i + 2;
    }
    let mut depth = 0usize;
    loop {
        if i >= s.len() {
            return i;
        }
        match s[i] {
            b'\\' => i += 2,
            b'{' | b'(' => {
                depth += 1;
                i += 1;
            }
            b'}' | b')' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            c if is_ws(c) && depth == 0 => return i,
            _ => i += 1,
        }
    }
}

fn items(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new(); // alloc: Cold
    if b.len() < 2 || b[0] != b'(' {
        return out;
    }
    let mut i = 1;
    while i < b.len() - 1 {
        if is_ws(b[i]) {
            i += 1;
            continue;
        }
        let e = span_end(b, i);
        out.push(&s[i..e]);
        i = e;
    }
    out
}

fn unescape(tok: &str) -> Option<String> {
    if tok == "<>" {
        return None;
    }
    let mut out = String::new(); // alloc: Cold
    let mut it = tok.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

struct Fields<'s>(Vec<(&'s str, &'s str)>);

impl<'s> Fields<'s> {
    fn raw(&self, label: &str) -> R<&'s str> {
        self.0.iter().find(|(l, _)| *l == label).map(|(_, v)| *v).ok_or_else(|| format!("fixture: missing field :{label}")) // alloc: Cold
    }

    fn opt(&self, label: &str) -> Option<&'s str> {
        self.0.iter().find(|(l, _)| *l == label).map(|(_, v)| *v)
    }

    fn i32(&self, l: &str) -> R<i32> {
        num(self.raw(l)?, l)
    }

    fn i64(&self, l: &str) -> R<i64> {
        num(self.raw(l)?, l)
    }

    fn u32(&self, l: &str) -> R<u32> {
        num(self.raw(l)?, l)
    }

    fn f64(&self, l: &str) -> R<f64> {
        num(self.raw(l)?, l)
    }

    fn bool(&self, l: &str) -> R<bool> {
        Ok(self.raw(l)? == "true")
    }

    fn i16s(&self, l: &str) -> R<Vec<i16>> {
        items(self.raw(l)?).iter().map(|t| num(t, l)).collect()
    }

    fn oids(&self, l: &str) -> R<Vec<Oid>> {
        items(self.raw(l)?).iter().map(|t| num(t, l)).collect()
    }

    fn i32s(&self, l: &str) -> R<Vec<i32>> {
        items(self.raw(l)?).iter().map(|t| num(t, l)).collect()
    }

    fn bools(&self, l: &str) -> R<Vec<bool>> {
        Ok(items(self.raw(l)?).iter().map(|t| *t == "true").collect())
    }
}

/// Plan-family nodes that are not plan nodes (`readfuncs` reads none of them): the append-rel map and the partition pruning descriptors.
const AUX_TAGS: &[&str] = &["APPENDRELINFO", "PARTITIONPRUNEINFO", "PARTITIONEDRELPRUNEINFO", "PARTITIONPRUNESTEPOP", "PARTITIONPRUNESTEPCOMBINE"];

const PLAN_TAGS: &[(&str, Kind)] = &[
    ("RESULT", Kind::Result),
    ("PROJECTSET", Kind::ProjectSet),
    ("MODIFYTABLE", Kind::ModifyTable),
    ("APPEND", Kind::Append),
    ("MERGEAPPEND", Kind::MergeAppend),
    ("RECURSIVEUNION", Kind::RecursiveUnion),
    ("BITMAPAND", Kind::BitmapAnd),
    ("BITMAPOR", Kind::BitmapOr),
    ("SEQSCAN", Kind::SeqScan),
    ("SAMPLESCAN", Kind::SampleScan),
    ("INDEXSCAN", Kind::IndexScan),
    ("INDEXONLYSCAN", Kind::IndexOnlyScan),
    ("BITMAPINDEXSCAN", Kind::BitmapIndexScan),
    ("BITMAPHEAPSCAN", Kind::BitmapHeapScan),
    ("TIDSCAN", Kind::TidScan),
    ("TIDRANGESCAN", Kind::TidRangeScan),
    ("SUBQUERYSCAN", Kind::SubqueryScan),
    ("FUNCTIONSCAN", Kind::FunctionScan),
    ("VALUESSCAN", Kind::ValuesScan),
    ("TABLEFUNCSCAN", Kind::TableFuncScan),
    ("CTESCAN", Kind::CteScan),
    ("NAMEDTUPLESTORESCAN", Kind::NamedTuplestoreScan),
    ("WORKTABLESCAN", Kind::WorkTableScan),
    ("FOREIGNSCAN", Kind::ForeignScan),
    ("NESTLOOP", Kind::NestLoop),
    ("NESTLOOPPARAM", Kind::NestLoopParam),
    ("MERGEJOIN", Kind::MergeJoin),
    ("HASHJOIN", Kind::HashJoin),
    ("MATERIAL", Kind::Material),
    ("MEMOIZE", Kind::Memoize),
    ("SORT", Kind::Sort),
    ("INCREMENTALSORT", Kind::IncrementalSort),
    ("GROUP", Kind::Group),
    ("AGG", Kind::Agg),
    ("WINDOWAGG", Kind::WindowAgg),
    ("UNIQUE", Kind::Unique),
    ("GATHER", Kind::Gather),
    ("GATHERMERGE", Kind::GatherMerge),
    ("HASH", Kind::Hash),
    ("SETOP", Kind::SetOp),
    ("LOCKROWS", Kind::LockRows),
    ("LIMIT", Kind::Limit),
    ("PLANROWMARK", Kind::PlanRowMark),
    ("PLANINVALITEM", Kind::PlanInvalItem),
];

fn cmd_type(v: u32) -> CmdType {
    match v {
        1 => CmdType::CMD_SELECT,
        2 => CmdType::CMD_UPDATE,
        3 => CmdType::CMD_INSERT,
        4 => CmdType::CMD_DELETE,
        5 => CmdType::CMD_MERGE,
        6 => CmdType::CMD_UTILITY,
        7 => CmdType::CMD_NOTHING,
        _ => CmdType::CMD_UNKNOWN,
    }
}

fn join_type(v: u32) -> JoinType {
    match v {
        1 => JoinType::JOIN_LEFT,
        2 => JoinType::JOIN_FULL,
        3 => JoinType::JOIN_RIGHT,
        4 => JoinType::JOIN_SEMI,
        5 => JoinType::JOIN_ANTI,
        6 => JoinType::JOIN_RIGHT_SEMI,
        7 => JoinType::JOIN_RIGHT_ANTI,
        8 => JoinType::JOIN_UNIQUE_OUTER,
        9 => JoinType::JOIN_UNIQUE_INNER,
        _ => JoinType::JOIN_INNER,
    }
}

/// An Agg's `gravity_finals`: (resno min max sum count) per final, flat (outfuncs `out_agg`).
fn finals(flat: &[i32]) -> R<Vec<GravityFinal>> {
    if flat.len() % 5 != 0 {
        return Err(format!("fixture: gravity_finals of {} numbers", flat.len())); // alloc: Cold
    }
    flat.chunks(5).map(|c| Ok(GravityFinal { resno: i16::try_from(c[0]).map_err(|_| format!("fixture: gravity_finals resno {}", c[0]))?, min: c[1], max: c[2], sum: c[3], count: c[4] })).collect() // alloc: Cold
}

fn rewrite(v: &str) -> R<Option<GravityRewrite>> {
    let nums: Vec<&str> = v.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').filter(|t| !t.is_empty()).collect(); // alloc: Cold
    let val = |key: &str| -> R<&str> { nums.iter().position(|t| *t == key).and_then(|p| nums.get(p + 1).copied()).ok_or_else(|| format!("fixture: gravity_rewrite without {key}")) }; // alloc: Cold
    let attno: i16 = num(val("attno")?, "attno")?;
    Ok(Some(match nums.first().copied() {
        Some("ZoneTopK") => GravityRewrite::ZoneTopK { attno, desc: val("desc")? == "true", nulls_first: val("nulls_first")? == "true" },
        Some("ZoneWalk") => GravityRewrite::ZoneWalk { attno, desc: val("desc")? == "true", nulls_first: val("nulls_first")? == "true" },
        Some("TwoPass") => GravityRewrite::TwoPass { attno, floor_desc: val("floor_desc")? == "true", floor_nulls_first: val("floor_nulls_first")? == "true" },
        _ => return Err(format!("fixture: unknown gravity_rewrite {v}")), // alloc: Cold
    }))
}

impl<'m> Rd<'m> {
    fn err<T>(&self, e: Box<types_error::PgError>) -> R<T> {
        Err(format!("readfuncs: {}", e.message())) // alloc: Cold
    }

    fn node(&self, span: &str) -> R<Option<Node<'m>>> {
        crate::walk::stack_check();
        if span == "<>" || span.is_empty() {
            return Ok(None);
        }
        if span.starts_with('(') && !span.starts_with("(b") && !span.starts_with("(i") && !span.starts_with("(o") {
            return Node::mk_list(self.mcx, self.list(span)?).map(Some).or_else(|e| self.err(e));
        }
        if let Some(rest) = span.strip_prefix('{') {
            let tag = rest.split(|c: char| c.is_whitespace() || c == '}').next().unwrap_or("");
            if tag == "PLANNEDSTMT" {
                return self.planned_stmt(span).map(Some);
            }
            if let Some((_, k)) = PLAN_TAGS.iter().find(|(t, _)| *t == tag) {
                return self.plan_node(*k, span).map(Some);
            }
            if AUX_TAGS.contains(&tag) {
                return self.aux_node(tag, span).map(Some);
            }
        }
        match readfuncs::stringToNodeNullable(self.mcx, span) {
            Ok(n) => Ok(n),
            Err(e) => self.err(e),
        }
    }

    fn list(&self, span: &str) -> R<NodeList<'m>> {
        if span == "<>" {
            return Ok(NodeList::nil());
        }
        let mut v = Vec::new(); // alloc: Cold
        for it in items(span) {
            v.push(match self.node(it)? {
                Some(n) => n,
                None => Node::mk_list(self.mcx, NodeList::nil()).or_else(|e| self.err(e))?,
            });
        }
        NodeList::from_slice(self.mcx, &v).or_else(|e| self.err(e))
    }

    fn opt_list(&self, span: &str) -> R<OptNodeList<'m>> {
        if span == "<>" {
            return Ok(OptNodeList::nil());
        }
        let mut v = Vec::new(); // alloc: Cold
        for it in items(span) {
            v.push(self.node(it)?);
        }
        OptNodeList::from_slice(self.mcx, &v).or_else(|e| self.err(e))
    }

    fn bms(&self, span: &str) -> R<Bitmapset<'m>> {
        if span == "<>" || span == "(b)" {
            return Ok(Bitmapset::empty());
        }
        let n = self.node(span)?.ok_or_else(|| "fixture: empty bitmapset".to_string())?; // alloc: Cold
        n.as_bitmapset().ok_or_else(|| format!("fixture: not a bitmapset: {span}"))?.clone_in(self.mcx).or_else(|e| self.err(e)) // alloc: Cold
    }

    fn ints(&self, span: &str) -> R<IntList<'m>> {
        if span == "<>" {
            return Ok(IntList::nil());
        }
        let n = self.node(span)?.ok_or_else(|| "fixture: empty int list".to_string())?; // alloc: Cold
        n.as_int_list().ok_or_else(|| format!("fixture: not an int list: {span}"))?.clone_in(self.mcx).or_else(|e| self.err(e)) // alloc: Cold
    }

    fn oid_list(&self, span: &str) -> R<OidList<'m>> {
        if span == "<>" {
            return Ok(OidList::nil());
        }
        let n = self.node(span)?.ok_or_else(|| "fixture: empty oid list".to_string())?; // alloc: Cold
        n.as_oid_list().ok_or_else(|| format!("fixture: not an oid list: {span}"))?.clone_in(self.mcx).or_else(|e| self.err(e)) // alloc: Cold
    }

    fn slice<T: Clone>(&self, v: &[T]) -> R<&'m [T]> {
        mcx::slice_borrow_in(self.mcx, v).or_else(|e| self.err(e))
    }

    fn string(&self, tok: &str) -> R<Option<&'m str>> {
        match unescape(tok) {
            None => Ok(None),
            Some(s) => Ok(Some(mcx::slice_borrow_in(self.mcx, s.as_bytes()).or_else(|e| self.err(e)).map(|b| core::str::from_utf8(b).unwrap_or(""))?)),
        }
    }

    fn fields<'s>(&self, span: &'s str) -> R<(&'s str, Fields<'s>)> {
        let b = span.as_bytes();
        if b.first() != Some(&b'{') || b.last() != Some(&b'}') {
            return Err(format!("fixture: not a node: {span}")); // alloc: Cold
        }
        let mut i = 1;
        let tag_end = span_end(b, i);
        let tag = &span[i..tag_end];
        i = tag_end;
        let mut out = Vec::new(); // alloc: Cold
        let end = b.len() - 1;
        loop {
            while i < end && is_ws(b[i]) {
                i += 1;
            }
            if i >= end {
                break;
            }
            if b[i] != b':' {
                return Err(format!("fixture: expected a field label at {i} in {tag}")); // alloc: Cold
            }
            let le = span_end(b, i);
            let label = &span[i + 1..le];
            i = le;
            while i < end && is_ws(b[i]) {
                i += 1;
            }
            let mut ve = span_end(b, i);
            let mut value = &span[i..ve];
            if matches!(value, "ZoneTopK" | "ZoneWalk" | "TwoPass") {
                let mut j = ve;
                while j < end && is_ws(b[j]) {
                    j += 1;
                }
                ve = span_end(b, j);
                value = &span[i..ve];
            }
            out.push((label, value));
            i = ve;
        }
        Ok((tag, Fields(out)))
    }

    fn plan_base(&self, f: &Fields<'_>, pfx: &str) -> R<Plan<'m>> {
        let l = |s: &str| format!("{pfx}{s}"); // alloc: Cold
        Ok(Plan {
            disabled_nodes: f.i32(&l("disabled_nodes"))?,
            startup_cost: f.f64(&l("startup_cost"))?,
            total_cost: f.f64(&l("total_cost"))?,
            plan_rows: f.f64(&l("plan_rows"))?,
            plan_width: f.i32(&l("plan_width"))?,
            parallel_aware: f.bool(&l("parallel_aware"))?,
            parallel_safe: f.bool(&l("parallel_safe"))?,
            async_capable: f.bool(&l("async_capable"))?,
            plan_node_id: f.i32(&l("plan_node_id"))?,
            targetlist: self.list(f.raw(&l("targetlist"))?)?,
            qual: self.list(f.raw(&l("qual"))?)?,
            lefttree: self.node(f.raw(&l("lefttree"))?)?,
            righttree: self.node(f.raw(&l("righttree"))?)?,
            initPlan: self.list(f.raw(&l("initPlan"))?)?,
            extParam: self.bms(f.raw(&l("extParam"))?)?,
            allParam: self.bms(f.raw(&l("allParam"))?)?,
        })
    }

    fn scan_base(&self, f: &Fields<'_>) -> R<Scan<'m>> {
        Ok(Scan { plan: self.plan_base(f, "scan.plan.")?, scanrelid: f.u32("scan.scanrelid")? })
    }

    fn join_base(&self, f: &Fields<'_>) -> R<Join<'m>> {
        Ok(Join { plan: self.plan_base(f, "join.plan.")?, jointype: join_type(f.u32("join.jointype")?), inner_unique: f.bool("join.inner_unique")?, joinqual: self.list(f.raw("join.joinqual")?)? })
    }

    fn sort_fields(&self, f: &Fields<'_>, pfx: &str) -> R<Sort<'m>> {
        let l = |s: &str| format!("{pfx}{s}"); // alloc: Cold
        Ok(Sort {
            plan: self.plan_base(f, &l("plan."))?,
            numCols: f.i32(&l("numCols"))?,
            sortColIdx: self.slice(&f.i16s(&l("sortColIdx"))?)?,
            sortOperators: self.slice(&f.oids(&l("sortOperators"))?)?,
            collations: self.slice(&f.oids(&l("collations"))?)?,
            nullsFirst: self.slice(&f.bools(&l("nullsFirst"))?)?,
            gravity_rewrite: match f.opt(&l("gravity_rewrite")) {
                Some(v) => rewrite(v)?,
                None => None,
            },
        })
    }

    fn mk<T: types_nodes::node_tree::NodeVariant<'m>>(&self, v: T) -> R<Node<'m>> {
        Node::mk(self.mcx, v).or_else(|e| self.err(e))
    }

    fn planned_stmt(&self, span: &str) -> R<Node<'m>> {
        let (_, f) = self.fields(span)?;
        self.mk(PlannedStmt {
            commandType: cmd_type(f.u32("commandType")?),
            queryId: types_storage_cell(f.i64("queryId")?),
            planId: f.i64("planId")?,
            hasReturning: f.bool("hasReturning")?,
            hasModifyingCTE: f.bool("hasModifyingCTE")?,
            canSetTag: f.bool("canSetTag")?,
            transientPlan: f.bool("transientPlan")?,
            dependsOnRole: f.bool("dependsOnRole")?,
            parallelModeNeeded: f.bool("parallelModeNeeded")?,
            jitFlags: f.i32("jitFlags")?,
            planTree: self.node(f.raw("planTree")?)?,
            partPruneInfos: self.list(f.raw("partPruneInfos")?)?,
            rtable: self.list(f.raw("rtable")?)?,
            unprunableRelids: self.bms(f.raw("unprunableRelids")?)?,
            permInfos: self.list(f.raw("permInfos")?)?,
            resultRelations: self.ints(f.raw("resultRelations")?)?,
            appendRelations: self.list(f.raw("appendRelations")?)?,
            subplans: self.opt_list(f.raw("subplans")?)?,
            rewindPlanIDs: self.bms(f.raw("rewindPlanIDs")?)?,
            rowMarks: self.list(f.raw("rowMarks")?)?,
            relationOids: self.oid_list(f.raw("relationOids")?)?,
            invalItems: self.list(f.raw("invalItems")?)?,
            paramExecTypes: self.oid_list(f.raw("paramExecTypes")?)?,
            utilityStmt: self.node(f.raw("utilityStmt")?)?,
            stmt_location: f.i32("stmt_location")?,
            stmt_len: f.i32("stmt_len")?,
        })
    }

    fn aux_node(&self, tag: &str, span: &str) -> R<Node<'m>> {
        let (_, f) = self.fields(span)?;
        let f = &f;
        match tag {
            "APPENDRELINFO" => self.mk(AppendRelInfo {
                parent_relid: f.u32("parent_relid")?,
                child_relid: f.u32("child_relid")?,
                parent_reltype: f.u32("parent_reltype")?,
                child_reltype: f.u32("child_reltype")?,
                num_child_cols: f.i32("num_child_cols")?,
                parent_colnos: self.slice(&f.i16s("parent_colnos")?)?,
                parent_reloid: f.u32("parent_reloid")?,
            }),
            "PARTITIONPRUNEINFO" => self.mk(PartitionPruneInfo { relids: self.bms(f.raw("relids")?)?, prune_infos: self.list(f.raw("prune_infos")?)?, other_subplans: self.bms(f.raw("other_subplans")?)? }),
            "PARTITIONEDRELPRUNEINFO" => self.mk(PartitionedRelPruneInfo {
                rtindex: f.u32("rtindex")?,
                present_parts: self.bms(f.raw("present_parts")?)?,
                nparts: f.i32("nparts")?,
                subplan_map: self.slice(&f.i32s("subplan_map")?)?,
                subpart_map: self.slice(&f.i32s("subpart_map")?)?,
                leafpart_rti_map: self.slice(&f.i32s("leafpart_rti_map")?)?,
                relid_map: self.slice(&f.oids("relid_map")?)?,
                initial_pruning_steps: self.list(f.raw("initial_pruning_steps")?)?,
                exec_pruning_steps: self.list(f.raw("exec_pruning_steps")?)?,
                execparamids: self.bms(f.raw("execparamids")?)?,
            }),
            "PARTITIONPRUNESTEPOP" => self.mk(PartitionPruneStepOp { step_id: f.i32("step.step_id")?, opstrategy: num(f.raw("opstrategy")?, "opstrategy")?, exprs: self.list(f.raw("exprs")?)?, cmpfns: self.oid_list(f.raw("cmpfns")?)?, nullkeys: self.bms(f.raw("nullkeys")?)? }),
            _ => self.mk(PartitionPruneStepCombine { step_id: f.i32("step.step_id")?, combineOp: f.u32("combineOp")?, source_stepids: self.ints(f.raw("source_stepids")?)? }),
        }
    }

    fn plan_node(&self, k: Kind, span: &str) -> R<Node<'m>> {
        let (_, f) = self.fields(span)?;
        let f = &f;
        match k {
            Kind::Result => self.mk(ResultPlan { plan: self.plan_base(f, "plan.")?, resconstantqual: self.node(f.raw("resconstantqual")?)? }),
            Kind::ProjectSet => self.mk(ProjectSet { plan: self.plan_base(f, "plan.")? }),
            Kind::ModifyTable => self.mk(ModifyTable {
                plan: self.plan_base(f, "plan.")?,
                operation: cmd_type(f.u32("operation")?),
                canSetTag: f.bool("canSetTag")?,
                nominalRelation: f.u32("nominalRelation")?,
                rootRelation: f.u32("rootRelation")?,
                partColsUpdated: f.bool("partColsUpdated")?,
                resultRelations: self.ints(f.raw("resultRelations")?)?,
                updateColnosLists: self.list(f.raw("updateColnosLists")?)?,
                withCheckOptionLists: self.list(f.raw("withCheckOptionLists")?)?,
                returningOldAlias: self.string(f.raw("returningOldAlias")?)?,
                returningNewAlias: self.string(f.raw("returningNewAlias")?)?,
                returningLists: self.list(f.raw("returningLists")?)?,
                fdwPrivLists: self.list(f.raw("fdwPrivLists")?)?,
                fdwDirectModifyPlans: self.bms(f.raw("fdwDirectModifyPlans")?)?,
                rowMarks: self.list(f.raw("rowMarks")?)?,
                epqParam: f.i32("epqParam")?,
                onConflictAction: f.u32("onConflictAction")?,
                arbiterIndexes: self.oid_list(f.raw("arbiterIndexes")?)?,
                onConflictSet: self.list(f.raw("onConflictSet")?)?,
                onConflictCols: self.ints(f.raw("onConflictCols")?)?,
                onConflictWhere: self.node(f.raw("onConflictWhere")?)?,
                exclRelRTI: f.u32("exclRelRTI")?,
                exclRelTlist: self.list(f.raw("exclRelTlist")?)?,
                mergeActionLists: self.list(f.raw("mergeActionLists")?)?,
                mergeJoinConditions: self.list(f.raw("mergeJoinConditions")?)?,
            }),
            Kind::Append => self.mk(Append {
                plan: self.plan_base(f, "plan.")?,
                apprelids: self.bms(f.raw("apprelids")?)?,
                appendplans: self.list(f.raw("appendplans")?)?,
                nasyncplans: f.i32("nasyncplans")?,
                first_partial_plan: f.i32("first_partial_plan")?,
                part_prune_index: f.i32("part_prune_index")?,
                // the ordered-Append mark is printed only when the planner set it
                gravity_sort_col_idx: if f.opt("gravity_sort_col_idx").is_some() { self.slice(&f.i16s("gravity_sort_col_idx")?)? } else { &[] },
                gravity_sort_operators: if f.opt("gravity_sort_operators").is_some() { self.slice(&f.oids("gravity_sort_operators")?)? } else { &[] },
                gravity_nulls_first: if f.opt("gravity_nulls_first").is_some() { self.slice(&f.bools("gravity_nulls_first")?)? } else { &[] },
            }),
            Kind::MergeAppend => self.mk(MergeAppend {
                plan: self.plan_base(f, "plan.")?,
                apprelids: self.bms(f.raw("apprelids")?)?,
                mergeplans: self.list(f.raw("mergeplans")?)?,
                numCols: f.i32("numCols")?,
                sortColIdx: self.slice(&f.i16s("sortColIdx")?)?,
                sortOperators: self.slice(&f.oids("sortOperators")?)?,
                collations: self.slice(&f.oids("collations")?)?,
                nullsFirst: self.slice(&f.bools("nullsFirst")?)?,
                part_prune_index: f.i32("part_prune_index")?,
            }),
            Kind::RecursiveUnion => self.mk(RecursiveUnion {
                plan: self.plan_base(f, "plan.")?,
                wtParam: f.i32("wtParam")?,
                numCols: f.i32("numCols")?,
                dupColIdx: self.slice(&f.i16s("dupColIdx")?)?,
                dupOperators: self.slice(&f.oids("dupOperators")?)?,
                dupCollations: self.slice(&f.oids("dupCollations")?)?,
                numGroups: f.i64("numGroups")?,
            }),
            Kind::BitmapAnd => self.mk(BitmapAnd { plan: self.plan_base(f, "plan.")?, bitmapplans: self.list(f.raw("bitmapplans")?)? }),
            Kind::BitmapOr => self.mk(BitmapOr { plan: self.plan_base(f, "plan.")?, isshared: f.bool("isshared")?, bitmapplans: self.list(f.raw("bitmapplans")?)? }),
            Kind::SeqScan => self.mk(SeqScan {
                scan: self.scan_base(f)?,
                cb_scan_cols: match f.opt("cb_scan_cols") {
                    Some("<>") | None => None,
                    Some(v) => Some(self.bms(v)?),
                },
                cb_target_cols: match f.opt("cb_target_cols") {
                    Some("<>") | None => None,
                    Some(v) => Some(self.bms(v)?),
                },
                pgrc_merge: if f.opt("pgrc_merge").is_some() { self.slice(&f.i16s("pgrc_merge")?)? } else { &[] },
            }),
            Kind::SampleScan => self.mk(SampleScan { scan: self.scan_base(f)?, tablesample: self.node(f.raw("tablesample")?)? }),
            Kind::IndexScan => self.mk(IndexScan {
                scan: self.scan_base(f)?,
                indexid: f.u32("indexid")?,
                indexqual: self.list(f.raw("indexqual")?)?,
                indexqualorig: self.list(f.raw("indexqualorig")?)?,
                indexorderby: self.list(f.raw("indexorderby")?)?,
                indexorderbyorig: self.list(f.raw("indexorderbyorig")?)?,
                indexorderbyops: self.oid_list(f.raw("indexorderbyops")?)?,
                indexorderdir: f.i32("indexorderdir")?,
            }),
            Kind::IndexOnlyScan => self.mk(IndexOnlyScan {
                scan: self.scan_base(f)?,
                indexid: f.u32("indexid")?,
                indexqual: self.list(f.raw("indexqual")?)?,
                recheckqual: self.list(f.raw("recheckqual")?)?,
                indexorderby: self.list(f.raw("indexorderby")?)?,
                indextlist: self.list(f.raw("indextlist")?)?,
                indexorderdir: f.i32("indexorderdir")?,
            }),
            Kind::BitmapIndexScan => self.mk(BitmapIndexScan { scan: self.scan_base(f)?, indexid: f.u32("indexid")?, isshared: f.bool("isshared")?, indexqual: self.list(f.raw("indexqual")?)?, indexqualorig: self.list(f.raw("indexqualorig")?)? }),
            Kind::BitmapHeapScan => self.mk(BitmapHeapScan { scan: self.scan_base(f)?, bitmapqualorig: self.list(f.raw("bitmapqualorig")?)? }),
            Kind::TidScan => self.mk(TidScan { scan: self.scan_base(f)?, tidquals: self.list(f.raw("tidquals")?)? }),
            Kind::TidRangeScan => self.mk(TidRangeScan { scan: self.scan_base(f)?, tidrangequals: self.list(f.raw("tidrangequals")?)? }),
            Kind::SubqueryScan => self.mk(SubqueryScan { scan: self.scan_base(f)?, subplan: self.node(f.raw("subplan")?)?, scanstatus: f.u32("scanstatus")? }),
            Kind::FunctionScan => self.mk(FunctionScan { scan: self.scan_base(f)?, functions: self.list(f.raw("functions")?)?, funcordinality: f.bool("funcordinality")? }),
            Kind::ValuesScan => self.mk(ValuesScan { scan: self.scan_base(f)?, values_lists: self.list(f.raw("values_lists")?)? }),
            Kind::TableFuncScan => self.mk(TableFuncScan { scan: self.scan_base(f)?, tablefunc: self.node(f.raw("tablefunc")?)? }),
            Kind::CteScan => self.mk(CteScan { scan: self.scan_base(f)?, ctePlanId: f.i32("ctePlanId")?, cteParam: f.i32("cteParam")?, gravity_fill: f.opt("gravity_fill").is_some() && f.bool("gravity_fill")? }),
            Kind::NamedTuplestoreScan => self.mk(NamedTuplestoreScan { scan: self.scan_base(f)?, enrname: self.string(f.raw("enrname")?)? }),
            Kind::WorkTableScan => self.mk(WorkTableScan { scan: self.scan_base(f)?, wtParam: f.i32("wtParam")? }),
            Kind::NestLoop => self.mk(NestLoop { join: self.join_base(f)?, nestParams: self.list(f.raw("nestParams")?)? }),
            Kind::NestLoopParam => self.mk(NestLoopParam { paramno: f.i32("paramno")?, paramval: self.node(f.raw("paramval")?)?.ok_or_else(|| "fixture: NestLoopParam without paramval".to_string())? }), // alloc: Cold
            Kind::MergeJoin => self.mk(MergeJoin {
                join: self.join_base(f)?,
                skip_mark_restore: f.bool("skip_mark_restore")?,
                mergeclauses: self.list(f.raw("mergeclauses")?)?,
                mergeFamilies: self.slice(&f.oids("mergeFamilies")?)?,
                mergeCollations: self.slice(&f.oids("mergeCollations")?)?,
                mergeReversals: self.slice(&f.bools("mergeReversals")?)?,
                mergeNullsFirst: self.slice(&f.bools("mergeNullsFirst")?)?,
            }),
            Kind::HashJoin => self.mk(HashJoin { join: self.join_base(f)?, hashclauses: self.list(f.raw("hashclauses")?)?, hashoperators: self.oid_list(f.raw("hashoperators")?)?, hashcollations: self.oid_list(f.raw("hashcollations")?)?, hashkeys: self.list(f.raw("hashkeys")?)? }),
            Kind::Material => self.mk(Material { plan: self.plan_base(f, "plan.")? }),
            Kind::Memoize => self.mk(Memoize {
                plan: self.plan_base(f, "plan.")?,
                numKeys: f.i32("numKeys")?,
                hashOperators: self.slice(&f.oids("hashOperators")?)?,
                collations: self.slice(&f.oids("collations")?)?,
                param_exprs: self.list(f.raw("param_exprs")?)?,
                singlerow: f.bool("singlerow")?,
                binary_mode: f.bool("binary_mode")?,
                est_entries: f.u32("est_entries")?,
                keyparamids: self.bms(f.raw("keyparamids")?)?,
            }),
            Kind::Sort => self.mk(self.sort_fields(f, "")?),
            Kind::IncrementalSort => self.mk(IncrementalSort { sort: self.sort_fields(f, "sort.")?, nPresortedCols: f.i32("nPresortedCols")? }),
            Kind::Group => self.mk(Group { plan: self.plan_base(f, "plan.")?, numCols: f.i32("numCols")?, grpColIdx: self.slice(&f.i16s("grpColIdx")?)?, grpOperators: self.slice(&f.oids("grpOperators")?)?, grpCollations: self.slice(&f.oids("grpCollations")?)? }),
            Kind::Agg => self.mk(Agg {
                plan: self.plan_base(f, "plan.")?,
                aggstrategy: f.u32("aggstrategy")?,
                aggsplit: f.u32("aggsplit")?,
                numCols: f.i32("numCols")?,
                grpColIdx: self.slice(&f.i16s("grpColIdx")?)?,
                grpOperators: self.slice(&f.oids("grpOperators")?)?,
                grpCollations: self.slice(&f.oids("grpCollations")?)?,
                numGroups: f.i64("numGroups")?,
                transitionSpace: num(f.raw("transitionSpace")?, "transitionSpace")?,
                aggParams: self.bms(f.raw("aggParams")?)?,
                groupingSets: self.list(f.raw("groupingSets")?)?,
                chain: self.list(f.raw("chain")?)?,
                distinctHashed: f.opt("distinctHashed") == Some("true"),
                gravity_rewrite: match f.opt("gravity_rewrite") {
                    Some(v) => rewrite(v)?,
                    None => None,
                },
                gravity_finals: match f.opt("gravity_finals") {
                    Some(_) => self.slice(&finals(&f.i32s("gravity_finals")?)?)?,
                    None => &[],
                },
                gravity_groups_default: f.opt("gravity_groups_default") == Some("true"),
            }),
            Kind::WindowAgg => self.mk(WindowAgg {
                plan: self.plan_base(f, "plan.")?,
                winname: self.string(f.raw("winname")?)?,
                winref: f.u32("winref")?,
                partNumCols: f.i32("partNumCols")?,
                partColIdx: self.slice(&f.i16s("partColIdx")?)?,
                partOperators: self.slice(&f.oids("partOperators")?)?,
                partCollations: self.slice(&f.oids("partCollations")?)?,
                ordNumCols: f.i32("ordNumCols")?,
                ordColIdx: self.slice(&f.i16s("ordColIdx")?)?,
                ordOperators: self.slice(&f.oids("ordOperators")?)?,
                ordCollations: self.slice(&f.oids("ordCollations")?)?,
                frameOptions: f.i32("frameOptions")?,
                startOffset: self.node(f.raw("startOffset")?)?,
                endOffset: self.node(f.raw("endOffset")?)?,
                runCondition: self.list(f.raw("runCondition")?)?,
                runConditionOrig: self.list(f.raw("runConditionOrig")?)?,
                startInRangeFunc: f.u32("startInRangeFunc")?,
                endInRangeFunc: f.u32("endInRangeFunc")?,
                inRangeColl: f.u32("inRangeColl")?,
                inRangeAsc: f.bool("inRangeAsc")?,
                inRangeNullsFirst: f.bool("inRangeNullsFirst")?,
                topWindow: f.bool("topWindow")?,
            }),
            Kind::Unique => self.mk(Unique { plan: self.plan_base(f, "plan.")?, numCols: f.i32("numCols")?, uniqColIdx: self.slice(&f.i16s("uniqColIdx")?)?, uniqOperators: self.slice(&f.oids("uniqOperators")?)?, uniqCollations: self.slice(&f.oids("uniqCollations")?)? }),
            Kind::Gather => self.mk(Gather { plan: self.plan_base(f, "plan.")?, num_workers: f.i32("num_workers")?, rescan_param: f.i32("rescan_param")?, single_copy: f.bool("single_copy")?, invisible: f.bool("invisible")?, initParam: self.bms(f.raw("initParam")?)? }),
            Kind::GatherMerge => self.mk(GatherMerge {
                plan: self.plan_base(f, "plan.")?,
                num_workers: f.i32("num_workers")?,
                rescan_param: f.i32("rescan_param")?,
                numCols: f.i32("numCols")?,
                sortColIdx: self.slice(&f.i16s("sortColIdx")?)?,
                sortOperators: self.slice(&f.oids("sortOperators")?)?,
                collations: self.slice(&f.oids("collations")?)?,
                nullsFirst: self.slice(&f.bools("nullsFirst")?)?,
                initParam: self.bms(f.raw("initParam")?)?,
            }),
            Kind::Hash => self.mk(Hash { plan: self.plan_base(f, "plan.")?, hashkeys: self.list(f.raw("hashkeys")?)?, skewTable: f.u32("skewTable")?, skewColumn: num(f.raw("skewColumn")?, "skewColumn")?, skewInherit: f.bool("skewInherit")?, rows_total: f.f64("rows_total")? }),
            Kind::SetOp => self.mk(SetOp {
                plan: self.plan_base(f, "plan.")?,
                cmd: f.u32("cmd")?,
                strategy: f.u32("strategy")?,
                numCols: f.i32("numCols")?,
                cmpColIdx: self.slice(&f.i16s("cmpColIdx")?)?,
                cmpOperators: self.slice(&f.oids("cmpOperators")?)?,
                cmpCollations: self.slice(&f.oids("cmpCollations")?)?,
                cmpNullsFirst: self.slice(&f.bools("cmpNullsFirst")?)?,
                numGroups: f.i64("numGroups")?,
            }),
            Kind::LockRows => self.mk(LockRows { plan: self.plan_base(f, "plan.")?, rowMarks: self.list(f.raw("rowMarks")?)?, epqParam: f.i32("epqParam")? }),
            Kind::Limit => self.mk(Limit {
                plan: self.plan_base(f, "plan.")?,
                limitOffset: self.node(f.raw("limitOffset")?)?,
                limitCount: self.node(f.raw("limitCount")?)?,
                limitOption: if f.u32("limitOption")? == 1 { LimitOption::LIMIT_OPTION_WITH_TIES } else { LimitOption::LIMIT_OPTION_COUNT },
                uniqNumCols: f.i32("uniqNumCols")?,
                uniqColIdx: self.slice(&f.i16s("uniqColIdx")?)?,
                uniqOperators: self.slice(&f.oids("uniqOperators")?)?,
                uniqCollations: self.slice(&f.oids("uniqCollations")?)?,
            }),
            Kind::PlanRowMark => self.mk(PlanRowMark {
                rti: f.u32("rti")?,
                prti: f.u32("prti")?,
                rowmarkId: f.u32("rowmarkId")?,
                markType: match f.u32("markType")? {
                    1 => RowMarkType::ROW_MARK_NOKEYEXCLUSIVE,
                    2 => RowMarkType::ROW_MARK_SHARE,
                    3 => RowMarkType::ROW_MARK_KEYSHARE,
                    4 => RowMarkType::ROW_MARK_REFERENCE,
                    5 => RowMarkType::ROW_MARK_COPY,
                    _ => RowMarkType::ROW_MARK_EXCLUSIVE,
                },
                allMarkTypes: f.i32("allMarkTypes")?,
                strength: match f.u32("strength")? {
                    1 => LockClauseStrength::LCS_FORKEYSHARE,
                    2 => LockClauseStrength::LCS_FORSHARE,
                    3 => LockClauseStrength::LCS_FORNOKEYUPDATE,
                    4 => LockClauseStrength::LCS_FORUPDATE,
                    _ => LockClauseStrength::LCS_NONE,
                },
                waitPolicy: match f.u32("waitPolicy")? {
                    1 => LockWaitPolicy::LockWaitSkip,
                    2 => LockWaitPolicy::LockWaitError,
                    _ => LockWaitPolicy::LockWaitBlock,
                },
                isParent: f.bool("isParent")?,
            }),
            Kind::PlanInvalItem => self.mk(PlanInvalItem { cacheId: f.i32("cacheId")?, hashValue: f.u32("hashValue")? }),
            Kind::ForeignScan | Kind::CustomScan | Kind::PartitionPruneInfo | Kind::SubPlan | Kind::AlternativeSubPlan => Err(format!("fixture: {} is not read by the corpus loader", k.name())), // alloc: Cold
        }
    }
}

fn types_storage_cell(v: i64) -> types_nodes::SyncCell<i64> {
    types_nodes::SyncCell::new(v)
}
