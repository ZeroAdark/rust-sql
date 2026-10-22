//! The crate rig's write execution (M4-plan §8 "the crate rig cannot execute a write", risk 8; M4-RIG). A write fixture
//! (`tests/fixtures/m4/<name>.write`) names a census plan fixture, the tables it reads and writes (int, bool and text columns),
//! the rows before, and PostgreSQL 18.6's answer: the rows the statement returns, its completion count and every table
//! after. The statement's program runs through gravity_driver over in-memory tables: each source site serves its
//! relation's live rows (an index site: the rows whose key columns satisfy the fixture's `seek` operators against the
//! seek words), the writer applies INSERT / UPDATE / DELETE by position, the locker records its locks. An error or a
//! cancel rolls every table back (the statement's subtransaction, as PG), so the judgment is the table state as a
//! multiset, never the writer's calls; a position written twice in one statement is a rig failure (a stop that resumes
//! behind the write, plan §3.12), never PG's self-modified skip. Included by tests/m4.rs (`#[path]`): common/mod.rs is
//! closed at M1 and stays untouched.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};

use gravity_abi::ctx::lane;
use gravity_driver::{box_locker, box_source, box_writer, ArenaBox, ArenaRef, BindingRef, Cursor, Lane, Locker, ScanPos, SourceBox, Window, WindowBase, WriteOp, Writer};
use gravity_program::{LaneId, SiteDesc, WritePlan};

use super::common::multiset::{Outcome, Row};
use super::common::{cancel_error, Mode, RigCatalog};

type Datum = u64;

/// A column's type: what the rig holds and renders as PG's text output (text by bytes: the C collation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Int2,
    Int4,
    Int8,
    Text,
}

/// A cell's value: a by-value word, or a varlena image (4-byte header) whose address is the datum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    W(Datum),
    T(Vec<u8>),
}

impl Val {
    fn datum(&self) -> Datum {
        match self {
            Val::W(d) => *d,
            Val::T(img) => img.as_ptr() as Datum,
        }
    }
}

fn image(payload: &[u8]) -> Vec<u8> {
    let mut v = (((payload.len() + 4) as u32) << 2).to_le_bytes().to_vec();
    v.extend_from_slice(payload);
    v
}

/// The payload of the varlena at `d` (1-byte or 4-byte header, never compressed or external).
fn payload(d: Datum) -> Result<Vec<u8>, String> {
    let p = d as *const u8;
    // SAFETY: a text column's datum addresses a varlena the program or the rig keeps alive for the row op.
    let (hdr, len) = unsafe {
        match *p {
            b if b == 1 => return Err("an external (TOAST) datum".into()),
            b if b & 1 == 1 => (1, (b >> 1) as usize),
            b if b & 3 == 0 => (4, ((p as *const u32).read_unaligned() >> 2) as usize),
            _ => return Err("a compressed datum".into()),
        }
    };
    // SAFETY: as above; the header gives the length, itself included.
    Ok(unsafe { std::slice::from_raw_parts(p.add(hdr), len.saturating_sub(hdr)) }.to_vec())
}

/// The non-NULL elements of the one-dimensional-or-more array image at `d`, as `ty`'s datums (PG's ArrayType: the 16-byte
/// header, dims and lower bounds, the null bitmap when dataoffset is set, then the elements at their alignment; a text
/// element's datum addresses its varlena, 1- or 4-byte header).
fn elements(ty: Ty, d: Datum) -> Vec<Datum> {
    let p = d as *const u8;
    // SAFETY: a search array's word addresses an untoasted array image the program or the fixture keeps alive for the seek.
    unsafe {
        let rd = |o: usize| (p.add(o) as *const i32).read_unaligned();
        let (ndim, dataoff) = (rd(4) as usize, rd(8) as usize);
        let n: usize = if ndim == 0 { 0 } else { (0..ndim).map(|i| rd(16 + 4 * i) as usize).product() };
        let (mut o, mut out) = (if dataoff != 0 { dataoff } else { (16 + 8 * ndim + 7) & !7 }, Vec::new());
        for i in 0..n {
            if dataoff != 0 && *p.add(16 + 8 * ndim + i / 8) & (1 << (i % 8)) == 0 {
                continue;
            }
            let (len, align) = match ty {
                Ty::Bool => (1, 1),
                Ty::Int2 => (2, 2),
                Ty::Int4 => (4, 4),
                Ty::Int8 => (8, 8),
                Ty::Text => (0, if *p.add(o) == 0 { 4 } else { 1 }),
            };
            o = (o + align - 1) & !(align - 1);
            let q = p.add(o);
            let (len, v) = match ty {
                Ty::Bool => (len, *q as Datum),
                Ty::Int2 => (len, (q as *const i16).read_unaligned() as i64 as Datum),
                Ty::Int4 => (len, (q as *const i32).read_unaligned() as i64 as Datum),
                Ty::Int8 => (len, (q as *const i64).read_unaligned() as Datum),
                Ty::Text if *q & 1 == 1 => ((*q >> 1) as usize, q as Datum),
                Ty::Text => (((q as *const u32).read_unaligned() >> 2) as usize, q as Datum),
            };
            out.push(v);
            o += len;
        }
        out
    }
}

impl Ty {
    fn of(s: &str) -> Result<Ty, String> {
        Ok(match s {
            "bool" => Ty::Bool,
            "int2" => Ty::Int2,
            "int4" => Ty::Int4,
            "int8" => Ty::Int8,
            "text" | "bpchar" | "varchar" => Ty::Text,
            t => return Err(format!("column type {t}: the write rig holds bool / int2 / int4 / int8 / text")),
        })
    }

    fn render(self, v: &Val) -> String {
        match (self, v) {
            (Ty::Text, Val::T(img)) => String::from_utf8_lossy(&img[4..]).into_owned(),
            (Ty::Bool, Val::W(d)) => (if d & 0xff != 0 { "t" } else { "f" }).to_owned(),
            (Ty::Int2, Val::W(d)) => (*d as i16).to_string(),
            (Ty::Int4, Val::W(d)) => (*d as i32).to_string(),
            (Ty::Int8, Val::W(d)) => (*d as i64).to_string(),
            (t, v) => format!("<{t:?} holding {v:?}>"),
        }
    }

    fn parse(self, s: &str) -> Result<Val, String> {
        let bad = |e: std::num::ParseIntError| format!("{s:?}: {e}");
        Ok(Val::W(match self {
            Ty::Text => return Ok(Val::T(image(s.as_bytes()))),
            Ty::Bool => (s == "t") as Datum,
            Ty::Int2 => s.parse::<i16>().map_err(bad)? as i64 as Datum,
            Ty::Int4 => s.parse::<i32>().map_err(bad)? as i64 as Datum,
            Ty::Int8 => s.parse::<i64>().map_err(bad)? as Datum,
        }))
    }

    /// The row op's datum as the rig's value (a text column's varlena copied out).
    fn take(self, d: Datum) -> Result<Val, String> {
        Ok(if self == Ty::Text { Val::T(image(&payload(d)?)) } else { Val::W(d) })
    }

    fn cmp(self, a: &Val, key: Datum) -> std::cmp::Ordering {
        match (self, a) {
            (Ty::Text, Val::T(img)) => img[4..].cmp(&payload(key).unwrap_or_default()[..]),
            (Ty::Bool, Val::W(a)) => (a & 0xff != 0).cmp(&(key & 0xff != 0)),
            (Ty::Int2, Val::W(a)) => (*a as i16).cmp(&(key as i16)),
            (Ty::Int4, Val::W(a)) => (*a as i32).cmp(&(key as i32)),
            (_, Val::W(a)) => (*a as i64).cmp(&(key as i64)),
            _ => std::cmp::Ordering::Less,
        }
    }
}

/// One cell: `None` is NULL.
pub type Cell = Option<Val>;

/// A table: its columns, its unique column sets (index name, 0-based columns), and one slot per position (block = slot,
/// offset 1); a deleted or updated-away row's slot is `None`, an update appends the new version (a new position).
#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub cols: Vec<(String, Ty)>,
    pub unique: Vec<(String, Vec<usize>)>,
    pub slots: Vec<Option<Vec<Cell>>>,
    /// Per slot (D604, a concurrent version): 1 = superseded (the statement's snapshot sees it, the table after does
    /// not), 2 = its newer version (the table after and an EPQ recheck see it, the scan does not); 0 or absent: plain.
    pub ver: Vec<u8>,
}

impl Table {
    /// The live rows as PG's text, sorted (a multiset).
    pub fn digest(&self) -> Vec<Row> {
        let mut v: Vec<Row> = self.slots.iter().enumerate().filter(|(i, _)| self.ver.get(*i) != Some(&1)).filter_map(|(_, s)| s.as_ref()).map(|r| r.iter().zip(&self.cols).map(|(c, (_, t))| c.as_ref().map(|v| t.render(v))).collect()).collect();
        v.sort();
        v
    }

    fn conflict(&self, row: &[Cell], skip: Option<usize>) -> Option<&str> {
        self.unique.iter().find(|(_, cols)| {
            cols.iter().all(|&c| row[c].is_some())
                && self.slots.iter().enumerate().any(|(i, s)| Some(i) != skip && s.as_ref().is_some_and(|r| cols.iter().all(|&c| r[c] == row[c])))
        }).map(|(n, _)| n.as_str())
    }
}

/// A seek operator per index key, in `key_attnos` order (btree strategies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    /// `= ANY (array)`: the key word is a search array (an IN list's, literal-free); a row matches any non-NULL element.
    Any,
}

/// One parsed `.write` fixture.
#[derive(Clone, Debug)]
pub struct WriteCase {
    pub name: String,
    pub plan: String,
    pub tables: BTreeMap<u32, Table>,
    pub seeks: BTreeMap<u32, Vec<Op>>,
    pub pg: Outcome,
    pub tag: u64,
    pub after: BTreeMap<u32, Vec<Row>>,
    /// v4 refuses the statement today: the refusal's text contains this (`NotBuilt M7: ..`, `Shape: ..`); its builder deletes the line.
    pub refused: Option<String>,
    /// The lock host's answers, one per lock call in order (`lock:`), then `Done`.
    pub locks: Vec<LockAns>,
    /// `[locked]`: every (table oid, slot) the statement locked, a multiset; `None`: not judged.
    pub locked: Option<Vec<(u32, u32)>>,
}

/// One lock call's answer (M4-LOCK): locked, skipped (SKIP LOCKED / a deleted row), or a newer version now at slot `n`
/// (the EvalPlanQual recheck runs on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockAns {
    Done,
    Skip,
    Recheck(u32),
}

fn cells(t: &Table, line: &str) -> Result<Vec<Cell>, String> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != t.cols.len() {
        return Err(format!("{}: row {line:?} has {} cells, the table {}", t.name, f.len(), t.cols.len()));
    }
    f.iter().zip(&t.cols).map(|(s, (_, ty))| if *s == "\\N" { Ok(None) } else { ty.parse(s).map(Some) }).collect()
}

/// Parses `<name>.write`: header lines `plan: <census id>`, `refused: <text>`, `table: <oid> <name> <col>:<type> ..`, `unique: <oid>
/// <index name> <col>[,<col>..]`, `seek: <oid> <op> ..` (`<` `<=` `=` `>=` `>` `any`; default `=` per key), then the sections
/// `[before <oid>]`, `[pg]` (returned rows) or `[pg_error]` (SQLSTATE<TAB>message), `[tag]` (the count) and `[after <oid>]`.
/// A locking statement's `lock: <answer> ..` (`done`, `skip`, `recheck=<slot>`: the host's answer per lock call, in
/// order; `done` past the list) and `[locked]` (`<oid><TAB><slot>` per row the statement must lock: the slots are the
/// `[before]` rows' 0-based order, an update's new version the next slot).
pub fn parse_write(name: &str, text: &str) -> Result<WriteCase, String> {
    let (mut plan, mut tables, mut seeks, mut refused) = (None, BTreeMap::<u32, Table>::new(), BTreeMap::new(), None);
    let (mut locks, mut locked) = (Vec::new(), None::<Vec<(u32, u32)>>);
    let (mut sec, mut before, mut pg, mut err, mut tag, mut after) = (None::<(String, u32)>, BTreeMap::<u32, Vec<String>>::new(), None, None, None, BTreeMap::<u32, Vec<String>>::new());
    let oid = |s: &str| s.parse::<u32>().map_err(|e| format!("{name}: oid {s:?}: {e}"));
    for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let (w, o) = s.split_once(' ').unwrap_or((s, "0"));
            sec = Some((w.to_owned(), oid(o)?));
            match w {
                "before" => drop(before.entry(oid(o)?).or_default()),
                "after" => drop(after.entry(oid(o)?).or_default()),
                "pg" => pg = Some(Vec::new()),
                "locked" => locked = Some(Vec::new()),
                "pg_error" | "tag" => {}
                x => return Err(format!("{name}: section [{x}]")),
            }
            continue;
        }
        match &sec {
            Some((w, o)) => match w.as_str() {
                "before" => before.get_mut(o).unwrap().push(line.to_owned()),
                "after" => after.get_mut(o).unwrap().push(line.to_owned()),
                "pg" => pg.as_mut().unwrap().push(line.split('\t').map(|c| (c != "\\N").then(|| c.to_owned())).collect::<Row>()),
                "locked" => {
                    let (o, n) = line.split_once('\t').ok_or(format!("{name}: [locked] row {line:?}"))?;
                    locked.as_mut().unwrap().push((oid(o)?, n.trim().parse::<u32>().map_err(|e| format!("{name}: [locked] slot {n:?}: {e}"))?));
                }
                "pg_error" => err = line.split_once('\t').map(|(s, m)| Outcome::Error { sqlstate: s.to_owned(), message: m.to_owned() }),
                _ => tag = Some(line.trim().parse::<u64>().map_err(|e| format!("{name}: tag {line:?}: {e}"))?),
            },
            None => {
                let (k, v) = line.split_once(": ").ok_or(format!("{name}: line {line:?}"))?;
                let w: Vec<&str> = v.split_whitespace().collect();
                match k {
                    "plan" => plan = Some(v.trim().to_owned()),
                    "refused" => refused = Some(v.trim().to_owned()),
                    "lock" => locks = w.iter().map(|a| Ok(match *a {
                        "done" => LockAns::Done,
                        "skip" => LockAns::Skip,
                        x => LockAns::Recheck(x.strip_prefix("recheck=").and_then(|n| n.parse().ok()).ok_or(format!("{name}: lock answer {x}"))?),
                    })).collect::<Result<_, String>>()?,
                    "table" => {
                        let cols = w[2..].iter().map(|c| c.split_once(':').ok_or(format!("{name}: column {c:?}")).and_then(|(n, t)| Ok((n.to_owned(), Ty::of(t)?)))).collect::<Result<_, _>>()?;
                        tables.insert(oid(w[0])?, Table { name: w[1].to_owned(), cols, unique: Vec::new(), slots: Vec::new(), ver: Vec::new() });
                    }
                    "unique" => {
                        let t = tables.get_mut(&oid(w[0])?).ok_or(format!("{name}: unique before its table"))?;
                        let cols = w[2].split(',').map(|c| t.cols.iter().position(|x| x.0 == c).ok_or(format!("{name}: unique column {c}"))).collect::<Result<_, _>>()?;
                        t.unique.push((w[1].to_owned(), cols));
                    }
                    "seek" => drop(seeks.insert(oid(w[0])?, w[1..].iter().map(|o| Ok(match *o {
                        "<" => Op::Lt,
                        "<=" => Op::Le,
                        "=" => Op::Eq,
                        ">=" => Op::Ge,
                        ">" => Op::Gt,
                        "any" => Op::Any,
                        x => return Err(format!("{name}: seek op {x}")),
                    })).collect::<Result<Vec<_>, String>>()?)),
                    x => return Err(format!("{name}: header {x}")),
                }
            }
        }
    }
    for (o, rows) in before {
        let t = tables.get_mut(&o).ok_or(format!("{name}: [before {o}] names no table"))?;
        // `~<row>`: the newer version of the row above it, committed by another session after the statement's snapshot
        t.ver = rows.iter().map(|l| l.starts_with('~') as u8 * 2).collect();
        for i in 1..t.ver.len() {
            if t.ver[i] == 2 {
                t.ver[i - 1] = 1;
            }
        }
        t.slots = rows.iter().map(|l| cells(t, l.trim_start_matches('~')).map(Some)).collect::<Result<_, _>>()?;
    }
    let after = after.into_iter().map(|(o, rows)| {
        let t = tables.get(&o).ok_or(format!("{name}: [after {o}] names no table"))?;
        let mut v: Vec<Row> = rows.iter().map(|l| l.split('\t').map(|c| (c != "\\N").then(|| c.to_owned())).collect()).collect();
        v.sort();
        let _ = t;
        Ok((o, v))
    }).collect::<Result<_, String>>()?;
    Ok(WriteCase {
        name: name.to_owned(),
        plan: plan.ok_or(format!("{name}: no plan:"))?,
        tables,
        seeks,
        pg: err.or(pg.map(Outcome::Rows)).ok_or(format!("{name}: no [pg] / [pg_error]"))?,
        tag: tag.ok_or(format!("{name}: no [tag]"))?,
        after,
        refused,
        locks,
        locked: locked.map(|mut v| {
            v.sort();
            v
        }),
    })
}

/// A writer or locker fault the rig must catch (its selftest): each makes some mode's verdict fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    /// An error or cancel keeps the rows written before it (no rollback).
    NoRollback,
    /// Every DELETE / UPDATE reports `Done` and changes nothing.
    Drop,
}

/// What the tables and the writer share for one execution.
struct State {
    tables: BTreeMap<u32, Table>,
    seeks: BTreeMap<u32, Vec<Op>>,
    written: BTreeMap<(u32, u32), u32>,
    fault: Fault,
    err: Option<String>,
    /// The abort byte, raised at the n-th row op (a write without RETURNING pushes nothing: the cancel lands on the ops).
    cancel_at: Option<(u32, *const AtomicU8)>,
    ops: u32,
    /// The lock host's script (`lock:`), the answers given, every position locked, the last recheck's newer version.
    locks: Vec<LockAns>,
    locked: Vec<(u32, u32)>,
    recheck: ScanPos,
    /// Every version a DELETE took, to the statement's end (a text cell's datum is its image), and the one the last row op
    /// took: RETURNING of a DELETE projects over it (`written`, as the hosts' delta iii fills the deleted version).
    deleted: Vec<Vec<Cell>>,
    taken: Option<usize>,
}

type Shared = std::rc::Rc<std::cell::RefCell<State>>;

struct TableSource {
    st: Shared,
    rel: u32,
    ids: Vec<LaneId>,
    keys: Vec<i16>,
    range: bool,
    rows: Vec<(u32, Vec<Cell>)>,
    at: usize,
    row: Vec<Cell>,
    lanes: Vec<Datum>,
    valid: Vec<u64>,
    offs: [u16; 1],
    cur: Cursor,
    /// EvalPlanQual (M4-LOCK): the one slot this site yields in the recheck node (`substitute`), until its next seek.
    only: Option<u32>,
}

impl gravity_driver::Source for TableSource {
    fn bind(&mut self, _b: &BindingRef<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }

    fn seek(&mut self, keys: &[Datum], keys_null: &[u8]) -> gravity_driver::Result<()> {
        let st = self.st.borrow();
        let t = st.tables.get(&self.rel).ok_or_else(|| gravity_driver::Error::host(format!("the write rig has no table {}", self.rel)))?;
        let ops = st.seeks.get(&self.rel).cloned().unwrap_or_default();
        let keycols: Vec<usize> = self.keys.iter().map(|&a| a as usize - 1).collect();
        let only = self.only.take();
        self.rows = t.slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|r| (i as u32, r.clone()))).filter(|(i, r)| {
            if t.ver.get(*i as usize) == Some(&2) && only != Some(*i) {
                return false;   // a newer version: the recheck's alone
            }
            if let Some(o) = only {
                // the newer version alone; an index site rechecks its key conditions on it (PG's indexqualorig)
                if *i != o {
                    return false;
                }
                if self.range {
                    return true;
                }
            }
            if self.range {
                return (*i as u64) >= keys[0] && (*i as u64) < keys[1];
            }
            keycols.iter().enumerate().all(|(k, &c)| keys_null.get(k) == Some(&0) && r[c].as_ref().is_some_and(|v| {
                if ops.get(k) == Some(&Op::Any) {
                    return elements(t.cols[c].1, keys[k]).into_iter().any(|e| t.cols[c].1.cmp(v, e).is_eq());
                }
                let o = t.cols[c].1.cmp(v, keys[k]);
                match ops.get(k).copied().unwrap_or(Op::Eq) {
                    Op::Lt => o.is_lt(),
                    Op::Le => o.is_le(),
                    Op::Eq => o.is_eq(),
                    Op::Ge => o.is_ge(),
                    Op::Gt => o.is_gt(),
                    Op::Any => false,
                }
            }))
        }).collect();
        self.at = 0;
        self.cur.clear();
        Ok(())
    }

    /// One row per window, a lane per the site's lanes: attributes (by reference for text), the RowId lane (an index
    /// site: the row's tid word, as hosts v4/m2_index.rs) or the offset lane (a range site); the row is block `slot`, offset 1.
    fn window(&mut self, w: &mut Window<'_>) -> gravity_driver::Result<u32> {
        if let Some(o) = self.only.take() {
            // substituted with no seek after it: that slot's live row
            let st = self.st.borrow();
            self.rows = st.tables.get(&self.rel).and_then(|t| t.slots.get(o as usize).cloned().flatten()).map(|r| (o, r)).into_iter().collect();
            self.at = 0;
        }
        let Some((slot, row)) = self.rows.get(self.at).cloned() else { return Ok(0) };
        self.row = row;
        for (k, id) in self.ids.iter().enumerate() {
            let (c, byref) = match *id {
                LaneId::Attr(a) if a >= 1 => {
                    // D617: the datum addresses self.row's cell (alive to the next window, as a heap page's pin), never a clone
                    let v = self.row.get(a as usize - 1).and_then(Option::as_ref);
                    (v.map(Val::datum), matches!(v, Some(Val::T(_))))
                }
                LaneId::RowId => (Some(gravity_kernels::ops::scan::entries::tid_word(slot, 1)), false),
                LaneId::Offset => (Some(1), false),
                other => return Err(gravity_driver::Error::host(format!("the write rig publishes no {other:?} lane"))),
            };
            (self.lanes[k], self.valid[k]) = (c.unwrap_or(0), c.is_some() as u64);
            let valid = if self.valid[k] == 0 { &self.valid[k] as *const u64 } else { core::ptr::null() };
            let mut l = if *id == LaneId::Offset {
                Lane::byval(self.offs.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, id.encode())
            } else {
                Lane::byval(&self.lanes[k] as *const Datum as *const u8, valid, 8, lane::REGION_ARENA, id.encode())
            };
            if byref {
                l.kind = lane::KIND_BYREF;
            }
            w.set(k, l);
        }
        w.rows = 1;
        w.set_base(WindowBase::at(if self.range { ScanPos::new(self.rel, slot, 0) } else { ScanPos::row(self.rel, slot, 1) }));
        self.at += 1;
        Ok(1)
    }

    fn lanes_hint(&self) -> usize {
        self.ids.len()
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn refetch(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> gravity_driver::Result<bool> {
        Err(gravity_driver::Error::host("the write rig runs no recheck"))
    }

    fn substitute(&mut self, pos: ScanPos) {
        self.only = Some(pos.block);
    }

    fn unbind(&mut self) {
        self.only = None;
        self.rows.clear();
        self.cur.clear();
    }
}

fn host(sqlstate: &str, msg: String) -> gravity_driver::Error {
    gravity_driver::Error::host(format!("{sqlstate}\t{msg}"))
}

struct TableWriter {
    st: Shared,
    rel: u32,
    /// D655 Row C: the newer version `epq_row` handed the write site, kept to the next fetch (the hosts' epq slot).
    epq: Vec<Cell>,
}

impl TableWriter {
    fn apply(&self, st: &mut State, op: WriteOp, pos: ScanPos, values: &[Datum], isnull: &[u8]) -> gravity_driver::Result<gravity_driver::Outcome> {
        let name = st.tables.get(&self.rel).map(|t| t.name.clone()).unwrap_or_default();
        if op != WriteOp::Insert && st.tables.get(&self.rel).is_some_and(|t| t.ver.get(pos.block as usize) == Some(&1)) {
            // D655 Row C: a version another session superseded (`~`, the row after it): TM_Updated, the newer one locked
            st.recheck = ScanPos::row(pos.rel, pos.block + 1, 1);
            return Ok(gravity_driver::Outcome::Recheck);
        }
        let t = st.tables.get_mut(&self.rel).ok_or_else(|| host("XX000", format!("the write rig has no table {}", self.rel)))?;
        let row: Vec<Cell> = if op == WriteOp::Delete { Vec::new() } else {
            if values.len() < t.cols.len() {
                return Err(host("XX000", format!("the write rig: a row op on {name} with {} cells, the table {}", values.len(), t.cols.len())));
            }
            t.cols.iter().enumerate().map(|(k, (_, ty))| if isnull[k] != 0 { Ok(None) } else { ty.take(values[k]).map(Some) }).collect::<Result<_, _>>().map_err(|e| host("XX000", e))?
        };
        if op != WriteOp::Insert {
            let n = st.written.entry((pos.rel, pos.block)).or_insert(0);
            *n += 1;
            if *n > 1 {
                return Err(host("XX000", format!("the write rig: position {} of {name} written twice in one statement", pos.block)));
            }
            if st.fault == Fault::Drop {
                return Ok(gravity_driver::Outcome::Done);
            }
        }
        let slot = pos.block as usize;
        match op {
            WriteOp::Insert => {
                if let Some(ix) = t.conflict(&row, None) {
                    return Err(host("23505", format!("duplicate key value violates unique constraint \"{ix}\"")));
                }
                t.slots.push(Some(row));
            }
            WriteOp::Update => {
                if t.slots.get(slot).and_then(|s| s.as_ref()).is_none() {
                    return Ok(gravity_driver::Outcome::Skipped);
                }
                if let Some(ix) = t.conflict(&row, Some(slot)) {
                    return Err(host("23505", format!("duplicate key value violates unique constraint \"{ix}\"")));
                }
                t.slots[slot] = None;
                t.slots.push(Some(row));
            }
            WriteOp::Delete => {
                let Some(gone) = t.slots.get_mut(slot).and_then(Option::take) else { return Ok(gravity_driver::Outcome::Skipped) };
                st.deleted.push(gone);
                st.taken = Some(st.deleted.len() - 1);
            }
        }
        Ok(gravity_driver::Outcome::Done)
    }
}

impl Writer for TableWriter {
    fn bind(&mut self, _b: &BindingRef<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }

    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[Datum], isnull: &[u8]) -> gravity_driver::Result<gravity_driver::Outcome> {
        let mut st = self.st.borrow_mut();
        st.taken = None;
        let r = self.apply(&mut st, op, pos, values, isnull);
        st.ops += 1;
        if let Some((n, f)) = st.cancel_at {
            if st.ops >= n {
                // SAFETY: the flag outlives the execution (`run_write` owns it across entry.start .. entry.end).
                unsafe { (*f).store(1, Ordering::Release) };
            }
        }
        if let Err(gravity_driver::Error::Host(m)) = &r {
            st.err.get_or_insert_with(|| m.to_string());
        }
        r
    }

    /// A Done DELETE whose write site declares cells: the version it took (a missing cell NULL); INSERT and UPDATE keep
    /// the kernel's cells (the rig runs no BEFORE ROW trigger).
    fn written(&mut self, values: &mut [Datum], isnull: &mut [u8]) -> bool {
        let st = self.st.borrow();
        let Some(row) = st.taken.filter(|_| !values.is_empty()).map(|i| &st.deleted[i]) else { return false };
        for k in 0..values.len() {
            (values[k], isnull[k]) = row.get(k).and_then(Option::as_ref).map_or((0, 1), |v| (v.datum(), 0));
        }
        true
    }

    fn recheck_pos(&self) -> ScanPos {
        self.st.borrow().recheck
    }

    /// D655 Row C: the newer version the last Recheck named, its cells into the write site (a copy kept to the next call).
    fn epq_row(&mut self, values: &mut [Datum], isnull: &mut [u8]) -> gravity_driver::Result<ScanPos> {
        let pos = std::mem::replace(&mut self.st.borrow_mut().recheck, ScanPos::NONE);
        let st = self.st.borrow();
        let row = st.tables.get(&self.rel).and_then(|t| t.slots.get(pos.block as usize).cloned().flatten()).filter(|_| pos.is_valid());
        self.epq = row.ok_or_else(|| gravity_driver::Error::host("the write rig: an EvalPlanQual row without a newer version"))?;
        for k in 0..values.len() {
            (values[k], isnull[k]) = self.epq.get(k).and_then(Option::as_ref).map_or((0, 1), |v| (v.datum(), 0));
        }
        Ok(pos)
    }

    fn unbind(&mut self) {}
}

/// The lock host: records every locked position and answers from the fixture's `lock:` script, in order, then `Done`;
/// a `recheck=<slot>` answer names the newer version the recheck node substitutes (`recheck_pos`).
struct TableLocker {
    st: Shared,
    epq: bool,
}

impl Locker for TableLocker {
    fn bind(&mut self, _b: &BindingRef<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }

    fn lock(&mut self, pos: ScanPos) -> gravity_driver::Result<gravity_driver::Outcome> {
        if self.epq {
            // RH6 / D604: the recheck's own lock node holds the row already (the hosts LockHost answers Done, no lock)
            return Ok(gravity_driver::Outcome::Done);
        }
        let mut st = self.st.borrow_mut();
        let k = st.locked.len();
        st.locked.push((pos.rel, pos.block));
        st.ops += 1;
        if let Some((n, f)) = st.cancel_at {
            if st.ops >= n {
                // SAFETY: as TableWriter::write's.
                unsafe { (*f).store(1, Ordering::Release) };
            }
        }
        Ok(match st.locks.get(k).copied().unwrap_or(LockAns::Done) {
            LockAns::Done => gravity_driver::Outcome::Done,
            LockAns::Skip => gravity_driver::Outcome::Skipped,
            LockAns::Recheck(n) => {
                st.recheck = ScanPos::row(pos.rel, n, 1);
                gravity_driver::Outcome::Recheck
            }
        })
    }

    fn epq(&mut self, on: bool) {
        self.epq = on;
    }

    fn recheck_pos(&self) -> ScanPos {
        self.st.borrow().recheck
    }

    fn unbind(&mut self) {}
}

struct TableDerive(Shared);

impl gravity_driver::Derive for TableDerive {
    fn size_hint(&self, _: &SiteDesc) -> usize {
        core::mem::size_of::<TableSource>()
    }

    fn derive_source(&mut self, d: &SiteDesc, a: ArenaRef) -> SourceBox {
        let ids: Vec<LaneId> = d.lanes.iter().map(|l| l.id).collect();
        let n = ids.len();
        box_source(TableSource { st: self.0.clone(), rel: d.relation, ids, keys: d.key_attnos.iter().copied().collect(), range: d.is_range(), rows: Vec::new(), at: 0, row: Vec::new(), lanes: vec![0; n], valid: vec![0; n], offs: [1], cur: Cursor::new(), only: None }, a)
    }

    fn writer_size_hint(&self) -> usize {
        core::mem::size_of::<TableWriter>()
    }

    fn derive_writer(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Writer> {
        box_writer(TableWriter { st: self.0.clone(), rel: plan.target_rel, epq: Vec::new() }, a)
    }

    fn locker_size_hint(&self) -> usize {
        core::mem::size_of::<TableLocker>()
    }

    fn derive_locker(&mut self, _plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Locker> {
        box_locker(TableLocker { st: self.0.clone(), epq: false }, a)
    }
}

/// One execution's answer: the outcome (rows or error), the completion count, every table after, every position locked.
#[derive(Debug, PartialEq)]
pub struct Ran {
    pub outcome: Outcome,
    pub tag: u64,
    pub after: BTreeMap<u32, Vec<Row>>,
    pub locked: Vec<(u32, u32)>,
}

/// One plan fixture through decompose with a node allocator (`decompose_in`, as exec admit.rs), then `f` over the graph;
/// `None` when decompose declines the statement.
pub fn declared<R>(text: &str, f: impl FnOnce(&gravity_program::Arena, &gravity_decompose::PipelineGraph<'_>, &dyn gravity_decompose::Facts) -> R) -> Option<R> {
    RigCatalog::install_seams();
    let ctx = mcx::MemoryContext::new("v4 rig fixture");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = gravity_program::Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    Some(f(&arena, &graph, &fx.facts))
}

/// v4's verdict on a plan fixture decomposed as `declared`: `Ok(ir)` (the program's text) or the refusal.
pub fn lower_declared(text: &str) -> Option<gravity_lower::Result<String>> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    declared(text, |a, graph, facts| {
        gravity_expr::nodes::func::acl_defer_begin();
        let l = gravity_lowerv4::lower_graph(a.handle(), rig.mcx(), graph, facts, RigCatalog::get(), &mut gravity_lower::Sequential::new(a.handle()), gravity_lower::LowerOpts::default());
        gravity_expr::nodes::func::acl_defer_end();
        l.map(|l| super::common::wide_named(gravity_ir::text::print_program(&l.ir), &l.descs))
    })
}

/// The census plan fixture `c.plan` lowered by v4 and run over `c`'s tables in `mode` with `fault` planted; `Err` is why
/// no outcome (`refused: ..` the lowering's, else a rig failure).
pub fn run_write(c: &WriteCase, mode: Mode, fault: Fault) -> Result<Ran, String> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    // a census plan, else a lane's own plan fixture in tests/fixtures/m4 (M4-RET's ret-*: no census plan is a trigger-free DELETE RETURNING)
    let path = [super::common::plans_dir(), super::common::milestone_dir(4)].map(|d| d.join(format!("{}.plan", c.plan))).into_iter().find(|p| p.exists()).unwrap_or_else(|| super::common::plans_dir().join(format!("{}.plan", c.plan))); // a census plan, else a lane's own (tests/fixtures/m4: M4-RET's ret-*)
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    RigCatalog::install_seams();
    let ctx = mcx::MemoryContext::new("v4 write rig");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).map_err(|e| format!("fixture: {e}"))?;
    let arena = gravity_program::Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map_err(|_| "decompose declines the plan".to_owned())?;
    let mut words = gravity_lower::Sequential::new(arena.handle());
    gravity_expr::nodes::func::acl_defer_begin();
    let l = gravity_lowerv4::lower_graph(arena.handle(), rig.mcx(), &graph, &fx.facts, RigCatalog::get(), &mut words, gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let l = l.map_err(|r| format!("refused: {r}"))?;
    let codes = gravity_lower::compile(&l, gravity_lower::Backend::Interp).map_err(|e| format!("backend: {}", e.message()))?;
    let program = std::sync::Arc::new(gravity_lower::assemble(&l, codes));
    let n = words.words.len().max(1);
    let (mut params, mut nulls) = (vec![0u64; n], vec![0u8; n]);
    for (i, w) in words.words.iter().enumerate() {
        match *w {
            gravity_lower::WordSrc::Const { value, .. } | gravity_lower::WordSrc::Elem { value, .. } => (params[i], nulls[i]) = (value.value.as_usize() as u64, value.isnull as u8),
            gravity_lower::WordSrc::Kernel => {}
            // an IN list's array (lists are literal-free: the program reads the array from its word, bound per execution);
            // the fixture's Const lives in the fixture's context for the run (M4-INSERT's arm)
            gravity_lower::WordSrc::Expr(n) if n.as_const().is_some() => {
                let k = n.as_const().expect("a Const");
                (params[i], nulls[i]) = (k.constvalue.as_usize() as u64, k.constisnull as u8);
            }
            other => return Err(format!("word {i} is {other:?}: the crate binds literals only")),
        }
    }
    // a program with M8 protos (D655 Row C's data-modifying CTE: its store and its pull) lays them as the read rig does
    let mut lives = super::common::on_demand::lay(&program, &mut params, &mut nulls)?;
    lives.extend(super::common::on_demand::lay_literals(&program, &graph, &mut params, &mut nulls)?);
    let flag = AtomicU8::new(0);
    let st: Shared = std::rc::Rc::new(std::cell::RefCell::new(State {
        tables: c.tables.clone(),
        seeks: c.seeks.clone(),
        written: BTreeMap::new(),
        fault,
        err: None,
        cancel_at: match mode { Mode::Cancel(k) if k > 0 => Some((k, &flag as *const AtomicU8)), _ => None },
        ops: 0,
        locks: c.locks.clone(),
        locked: Vec::new(),
        recheck: ScanPos::NONE,
        deleted: Vec::new(),
        taken: None,
    }));
    if mode == Mode::Cancel(0) {
        flag.store(1, Ordering::Release);
    }
    gravity_fn::result_life::scribble::force(mode == Mode::Scribble);
    let r = drive_write(&program, &params, &nulls, mode, &flag, &mut TableDerive(st.clone()), &lives);
    gravity_fn::result_life::scribble::force(false);
    let (res, tag) = r?;
    let mut s = st.borrow_mut();
    // as the read rig (drive.rs): a raise's stashed error taken once; a cancel inside a nested run (the CTE's pulled plan)
    // is the host's stashed 57014 behind the reader's raise
    let stashed = matches!(res, Err(gravity_driver::Error::Raised(_))).then(gravity_fn::take_error).flatten();
    let outcome = match res {
        Ok(rows) => rows,
        Err(gravity_driver::Error::Cancelled) => cancel_error(),
        Err(gravity_driver::Error::Raised(_)) if stashed.as_ref().is_some_and(|e| e.sqlstate() == types_error::ERRCODE_QUERY_CANCELED) => cancel_error(),
        Err(e) => match s.err.take().and_then(|m| m.split_once('\t').map(|(a, b)| (a.to_owned(), b.to_owned()))) {
            Some((sqlstate, message)) => Outcome::Error { sqlstate, message },
            None => Outcome::Error { sqlstate: "XX000".into(), message: format!("{e:?}") },
        },
    };
    let mut locked = std::mem::take(&mut s.locked);
    locked.sort();
    // an error or a cancel ends the statement's subtransaction: every row it wrote goes (unless the planted fault keeps them)
    let tables = if matches!(outcome, Outcome::Error { .. }) && fault != Fault::NoRollback { &c.tables } else { &s.tables };
    Ok(Ran { outcome, tag, after: tables.iter().map(|(o, t)| (*o, t.digest())).collect(), locked })
}

type DriveResult = Result<(Result<Outcome, gravity_driver::Error>, u64), String>;

fn drive_write(program: &std::sync::Arc<gravity_program::Program>, params: &[u64], nulls: &[u8], mode: Mode, flag: &AtomicU8, derive: &mut TableDerive, lives: &super::common::on_demand::Lives) -> DriveResult {
    use gravity_driver::{Binding, BindingParts, Call, CommandId, Entry, Grant, Identity, PerTuple, Pins, Reservation, Snapshot, Step};
    let mut sink = Rows { out: &program.out, rows: Vec::new(), err: None };
    let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params, param_nulls: nulls, abort_flag: flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
    let mut b = Binding::new(parts, &mut sink);
    let (mut entry, cap) = (Entry::new(1), if let Mode::Stop(n) = mode { n.max(1) } else { program.layout.out_rows.max(1) as u32 });
    let lease = std::rc::Rc::new(Reservation::unadmitted(Grant::of(64 << 20)));
    let r = match entry.start(None, program, derive, &mut b, lease) {
        Err(e) => Err(e),
        // its pipelines as the read rig runs them (on_demand.rs: the CTE's plan run on its reader's pull); the op rechecks in the IR
        Ok(mut exec) if !lives.is_empty() => {
            let r = super::common::on_demand::run(&mut entry, &mut exec, &mut b, cap, program, lives);
            entry.end(exec, matches!(r, Ok(Ok(()))));
            r?
        }
        Ok(mut exec) => {
            let (mut call, mut rechecked) = (Call::batch(cap), 0u64);
            let r = loop {
                match entry.run(&mut exec, &mut b, call) {
                    Ok(Step::Done) => break Ok(()),
                    Ok(Step::Suspended { .. }) => call = Call::resume(cap),
                    // EvalPlanQual as exec epq.rs: a second node over the newer version at `site`, the other sites held
                    // at the positions the failing node settled; its surviving row goes out; then the pipeline resumes
                    Ok(Step::Recheck { site }) => {
                        let (pos, positions) = (exec.recheck_pos(), exec.node().positions().to_vec());
                        match recheck(&mut entry, program, derive, &mut b, &positions, site as usize, pos) {
                            Ok(n) => rechecked += n,
                            Err(e) => break Err(e),
                        }
                        // SAFETY: the node's context block lives as long as the node (exec runner::execute::resumes).
                        if unsafe { (*exec.node().ctx()).resume } == gravity_abi::ctx::RESUME_NONE {
                            break Ok(());
                        }
                        call = Call::resume(cap);
                    }
                    Ok(Step::BuildWait { pipeline }) => break Err(gravity_driver::Error::BuildWait(pipeline)),
                    Err(e) => break Err(e),
                }
            };
            entry.end(exec, r.is_ok());
            b.rows_processed += rechecked;
            r
        }
    };
    let tag = b.rows_processed;
    drop(b);
    if let Some(e) = sink.err {
        return Err(e);
    }
    Ok((r.map(|()| Outcome::Rows(sink.rows)), tag))
}

/// exec epq.rs's recheck over the rig: the recheck node with `site` substituted by `pos` and every other site held at
/// `positions`; a surviving row is pushed to the statement's sink; the count it adds (a lock's row counts one).
fn recheck(entry: &mut gravity_driver::Entry, program: &std::sync::Arc<gravity_program::Program>, derive: &mut TableDerive, b: &mut gravity_driver::Binding<'_>, positions: &[ScanPos], site: usize, pos: ScanPos) -> Result<u64, gravity_driver::Error> {
    let natts = program.out.len();
    let (mut values, mut isnull, mut region) = (vec![0u64; natts], vec![0u8; natts], Vec::new());
    let byref: Vec<bool> = program.out.iter().map(|c| !c.typbyval).collect();
    let out = gravity_driver::RowBuf { values: &mut values, isnull: &mut isnull, region: &mut region, byref: &byref };
    let r = entry.recheck(None, program, derive, b, &mut |i, s| {
        if i == site {
            s.substitute(pos);
        } else if positions.get(i).is_some_and(|p| p.is_valid()) {
            s.substitute(positions[i]);
        }
    }, out, None, None, positions)?;
    match r {
        (gravity_driver::Recheck::Skip, _) => Ok(0),
        (gravity_driver::Recheck::Pass, rows) => {
            if natts > 0 {
                b.sink.push(gravity_driver::BorrowedBatch { natts, rows: 1, values: &values, isnull: &isnull, region: &region })?;
            }
            Ok(if program.write.cmd == gravity_program::Cmd::Select { rows.max(1) } else { rows })
        }
    }
}

/// The rows a write returns (RETURNING), rendered by the out columns' types.
struct Rows<'o> {
    out: &'o [gravity_program::OutCol],
    rows: Vec<Row>,
    err: Option<String>,
}

impl gravity_driver::Sink for Rows<'_> {
    fn push(&mut self, b: gravity_driver::BorrowedBatch<'_>) -> gravity_driver::Result<gravity_driver::Flow> {
        for i in 0..b.rows {
            let (v, n) = b.row(i);
            let r: Result<Row, String> = self.out.iter().enumerate().map(|(k, c)| {
                if n[k] != 0 {
                    return Ok(None);
                }
                Ok(Some(match c.typid {
                    16 => Ty::Bool.render(&Val::W(v[k])),
                    21 => Ty::Int2.render(&Val::W(v[k])),
                    23 => Ty::Int4.render(&Val::W(v[k])),
                    20 => Ty::Int8.render(&Val::W(v[k])),
                    25 | 1042 | 1043 => match payload(v[k]) {
                        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                        Err(e) => return Err(e),
                    },
                    t => return Err(format!("RETURNING type {t}: the write rig renders bool / int2 / int4 / int8 / text")),
                }))
            }).collect();
            match r {
                Ok(r) => self.rows.push(r),
                Err(e) => drop(self.err.get_or_insert(e)),
            }
        }
        Ok(gravity_driver::Flow::Continue)
    }
}

/// `ran` against PG's answer in `c` under `mode`: the rows returned (as a multiset), the count and every table; a cancel's
/// 57014 wants every table as before the statement. `Err` is the report.
pub fn judge(c: &WriteCase, mode: Mode, ran: &Ran) -> Result<(), String> {
    let before: BTreeMap<u32, Vec<Row>> = c.tables.iter().map(|(o, t)| (*o, t.digest())).collect();
    let bag = |o: &Outcome| match o {
        Outcome::Rows(r) => {
            let mut r = r.clone();
            r.sort();
            Outcome::Rows(r)
        }
        e => e.clone(),
    };
    let cancelled = matches!(mode, Mode::Cancel(_)) && ran.outcome == cancel_error();
    let (want, tables) = if cancelled { (cancel_error(), &before) } else { (c.pg.clone(), &c.after) };
    if bag(&ran.outcome) != bag(&want) {
        return Err(format!("{} {mode:?}: pg {want:?}, got {:?}", c.name, ran.outcome));
    }
    if !cancelled && matches!(c.pg, Outcome::Rows(_)) && ran.tag != c.tag {
        return Err(format!("{} {mode:?}: count pg {}, got {}", c.name, c.tag, ran.tag));
    }
    if let (Some(want), false) = (&c.locked, cancelled) {
        if &ran.locked != want {
            return Err(format!("{} {mode:?}: rows locked: pg {want:?}, got {:?} (a row locked and not returned, or one returned unlocked)", c.name, ran.locked));
        }
    }
    for (o, want) in tables.iter() {
        let got = ran.after.get(o).ok_or(format!("{}: table {o} gone", c.name))?;
        if got != want {
            return Err(format!("{} {mode:?}: table {o} after the statement: pg {want:?}, got {got:?}{}", c.name, if cancelled { " (a cancel leaves the table as before)" } else { "" }));
        }
    }
    Ok(())
}
