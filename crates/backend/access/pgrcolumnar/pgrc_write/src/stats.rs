//! Stats runs (P-1): a generation's planner facts and part-grain answers as a bounded set of immutable run files
//! (`pgrc_format::statsrun`) plus at most `LOOSE_MAX - 1` loose parts, so a publish writes stats bytes bounded by a
//! constant instead of by the table's part count.
//!
//! [`StatsState`] is a generation's stats in memory: its runs (shared by `Arc` across generations) and its loose
//! parts. [`plan_stats`] is the policy a publish applies to its base state: retire cuts, then installs (a bulk load's
//! parts go into runs of at most `RUN_PARTS_MAX`, a remainder under `LOOSE_MAX` goes loose), then a fold of
//! `LOOSE_MAX` loose parts into one small run, then a merge of `SMALL_RUNS_MAX` small runs into one big run (a held
//! small run stays out of it until more than `HELD_SMALL_MAX` are held), then the root. Each step writes at most a
//! constant number of parts' stats, and the publisher never reads its own stats back: the base state comes from the
//! catalog (or, without one, from the directory: [`StatsState::load`]).
//!
//! The law: `serve(state.image(m)) == serve(the fold of m.parts in manifest order)` with equal zone vectors, and every
//! part's answers equal its footer's. Accumulators merge (`FactsAcc::merge`); zones are re-assembled per part in
//! manifest order (`FactsAcc::assemble_zones`).

use std::collections::BTreeSet;
use std::sync::Arc;

use pgrc_format::answers::{encode_column, ColumnAnswers, DIGEST_LEN};
use pgrc_format::bankstats::PartIdent;
use pgrc_format::dirlayout::run_file_name;
use pgrc_format::facts::FactsImage;
use pgrc_format::manifest::{stats_ref, Manifest, PartRecord};
use pgrc_format::part::{SectionKind, SECTIONF_META_ZSTD};
use pgrc_format::statsrun::{encode_run, RunFile, RunImage, RunRef};
use pgrc_format::{FormatError, FormatResult};
use pgrc_read::{OpenPart, ReadError, ReadResult};

use crate::facts::{cut_bytes, part_facts_from_open, part_facts_from_sealed, ColSize, FactsAcc};
use crate::seal::SealedPart;
use crate::writer::PartCutPolicy;
use crate::wvfs::WriteVfs;
use crate::{WriteError, WriteResult};

/// Loose parts fold into a small run when this many are loose.
pub const LOOSE_MAX: usize = 16;
/// Small runs (at most `LOOSE_MAX` parts) merge into one big run when this many exist.
pub const SMALL_RUNS_MAX: usize = 16;
/// The hold bound (coordinator ruling 09-26, P-1 L2b): at most this many held small runs. A publish that would leave
/// more merges the oldest `SMALL_RUNS_MAX` small runs as if nothing were held, so a table's runs number at most its big
/// runs + `HELD_SMALL_MAX` held + `SMALL_RUNS_MAX - 1` others, compaction worker on or off.
pub const HELD_SMALL_MAX: usize = 16;
/// A bulk install's runs hold at most this many parts.
pub const RUN_PARTS_MAX: usize = 256;

/// One part's stats: from its seal (the publisher) or its footer (a load, a redo, a refold).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartStats {
    /// D4537 T18b: the bytes of this object once it is shared ([`PartStats::into_arc`]), declared FIRST (D4627).
    pub hold: Hold,
    pub ident: PartIdent,
    /// A one-part image (`header.parts == 1`).
    pub facts: FactsImage,
    /// `(attno, part-grain record, digest)`, attno ascending; the columns the part carries Stats for.
    pub answers: Vec<(u32, Vec<u8>, Option<[u8; DIGEST_LEN]>)>,
    /// PARTEXT records (K1).
    pub ext: Vec<(u16, Vec<u8>)>,
}

/// D4537 T18b: the charge of a shared stats object (a run or a loose part, one `Arc` across every generation that holds it)
/// on the part cache, opened where the object is shared and dropped with its last count. Never part of the object's
/// identity: it clones empty and compares equal.
#[derive(Debug, Default)]
pub struct Hold(Option<memledger::Charge>);

impl Clone for Hold {
    fn clone(&self) -> Hold {
        Hold(None)
    }
}

impl PartialEq for Hold {
    fn eq(&self, _: &Hold) -> bool {
        true
    }
}

impl Eq for Hold {}

impl Hold {
    /// `bytes` committed on the part cache.
    fn open(bytes: u64) -> Hold {
        const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
        let mut c = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
        c.commit(bytes);
        Hold(Some(c))
    }

    /// The bytes held (test witness).
    pub fn bytes(&self) -> u64 {
        self.0.as_ref().map_or(0, memledger::Charge::committed)
    }
}

const ARC: usize = 2 * core::mem::size_of::<usize>();

fn facts_heap(i: &FactsImage) -> usize {
    i.cols.capacity() * core::mem::size_of::<pgrc_format::facts::ColFacts>() + i.cols.iter().map(|c| c.ndv_body.capacity() + ARC + c.zones.bytes().len()).sum::<usize>()
}

impl PartStats {
    /// The object's heap bytes (it and its `Arc` block excluded).
    fn heap_bytes(&self) -> usize {
        facts_heap(&self.facts)
            + self.answers.capacity() * core::mem::size_of::<(u32, Vec<u8>, Option<[u8; DIGEST_LEN]>)>()
            + self.answers.iter().map(|a| a.1.capacity()).sum::<usize>()
            + self.ext.capacity() * core::mem::size_of::<(u16, Vec<u8>)>()
            + self.ext.iter().map(|e| e.1.capacity()).sum::<usize>()
    }

    /// This object shared: its bytes and its `Arc` block on the part cache.
    pub fn into_arc(self) -> Arc<PartStats> {
        let bytes = (ARC + core::mem::size_of::<PartStats>() + self.heap_bytes()) as u64;
        let mut a = Arc::new(self); // alloc: Cold charged the part cache (PartStats::hold)
        if let Some(p) = Arc::get_mut(&mut a) {
            p.hold = Hold::open(bytes);
        }
        a
    }
}

impl Run {
    /// The object's heap bytes (it and its `Arc` block excluded).
    fn heap_bytes(&self) -> usize {
        self.parts.capacity() * core::mem::size_of::<PartIdent>()
            + facts_heap(&self.facts)
            + self.answers.capacity() * core::mem::size_of::<(u32, Vec<u8>)>()
            + self.answers.iter().map(|a| a.1.capacity()).sum::<usize>()
            + self.ext.capacity() * core::mem::size_of::<(u32, u16, Vec<u8>)>()
            + self.ext.iter().map(|e| e.2.capacity()).sum::<usize>()
    }

    /// This object shared: its bytes and its `Arc` block on the part cache.
    pub fn into_arc(self) -> Arc<Run> {
        let bytes = (ARC + core::mem::size_of::<Run>() + self.heap_bytes()) as u64;
        let mut a = Arc::new(self); // alloc: Cold charged the part cache (Run::hold)
        if let Some(r) = Arc::get_mut(&mut a) {
            r.hold = Hold::open(bytes);
        }
        a
    }
}

/// One run in memory: what its file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// D4537 T18b: the bytes of this object once it is shared ([`Run::into_arc`]), declared FIRST (D4627).
    pub hold: Hold,
    pub rref: RunRef,
    /// part_no ascending.
    pub parts: Vec<PartIdent>,
    pub facts: FactsImage,
    /// `(attno, answers::encode_column payload over parts)`, attno ascending.
    pub answers: Vec<(u32, Vec<u8>)>,
    pub ext: Vec<(u32, u16, Vec<u8>)>,
    /// The run's hold verdict ([`FullTest::run_holds`]), computed once per run per process.
    pub held: HeldCell,
}

/// A cached verdict about an immutable run (0 = not asked, 1 = no, 2 = yes). Never part of the run's identity: two
/// runs compare equal whatever their cells hold.
#[derive(Debug, Default)]
pub struct HeldCell(std::sync::atomic::AtomicU8);

impl HeldCell {
    fn verdict(&self, f: impl FnOnce() -> bool) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        match self.0.load(Relaxed) {
            0 => {
                let v = f();
                self.0.store(1 + u8::from(v), Relaxed);
                v
            }
            k => k == 2,
        }
    }
}

impl Clone for HeldCell {
    fn clone(&self) -> HeldCell {
        HeldCell(std::sync::atomic::AtomicU8::new(self.0.load(std::sync::atomic::Ordering::Relaxed)))
    }
}

impl PartialEq for HeldCell {
    fn eq(&self, _: &HeldCell) -> bool {
        true
    }
}
impl Eq for HeldCell {}

/// K1 L7's classification of a part as the compaction policy sees it (`pgrc_compact::PartView::full` under
/// `Policy::default()`, whose target is `PartCutPolicy::default()`): full at or above the bulk cut in rows or in
/// buffer bytes, the bytes estimated by [`cut_bytes`] from the part's own part-grain Stats records (the records K1's
/// estimate reads from the part's sections). P-1 L2, taken over from K1 L6: `hold` = the run holds a part that is not
/// full. Such a run stays out of big-run merges (up to `HELD_SMALL_MAX` of them), so a compaction, whose inputs are
/// never full parts (only a DV rewrite takes one), cuts at most a small run.
#[derive(Debug, Clone)]
pub struct FullTest {
    /// The table's columns, attno ascending, with their Values class and fixed length (no statistics).
    cols: Vec<ColSize>,
    max_rows: u64,
    max_bytes: u64,
}

impl FullTest {
    pub fn new(cols: Vec<ColSize>) -> FullTest {
        let cut = PartCutPolicy::default();
        FullTest { cols, max_rows: cut.max_rows, max_bytes: cut.max_bytes }
    }

    /// A test's or a tool's cut (the server's is always `PartCutPolicy::default()`, K1's target).
    pub fn with_cut(cols: Vec<ColSize>, max_rows: u64, max_bytes: u64) -> FullTest {
        FullTest { cols, max_rows, max_bytes }
    }

    /// The columns of one of the table's parts (a directory's parts share its schema).
    pub fn from_part(part: &OpenPart) -> ReadResult<FullTest> {
        crate::facts::part_columns(part).map(FullTest::new)
    }

    /// A part of `rows` rows whose part-grain Stats record for a column is `record(attno)` (empty: none) is full.
    fn full<'r>(&self, rows: u64, record: &dyn Fn(u32) -> &'r [u8]) -> bool {
        if rows >= self.max_rows {
            return true;
        }
        let stats = |c: &ColSize| {
            let body = record(c.attno);
            let r = (c.heap() && !body.is_empty()).then(|| pgrc_format::meta::StatsRecord::decode(&mut pgrc_format::wire::Cur::new(body)).ok()).flatten();
            r.filter(|r| r.flags & pgrc_format::meta::STATSF_COMPUTED != 0)
        };
        let cols: Vec<ColSize> = self.cols.iter().map(|c| ColSize { stats: stats(c), ..*c }).collect();
        cut_bytes(rows, &cols) >= self.max_bytes
    }

    /// COMPACTP: the part at `at` in `state` ([`StatsState::index`], [`StatsState::locate`]), of `rows` rows, is full.
    pub fn full_in(&self, state: &StatsState, at: (u32, u32), rows: u64) -> bool {
        self.full(rows, &|a| state.part_answer(at, a).0)
    }

    /// Part `p` is full.
    pub fn part_full(&self, p: &PartStats) -> bool {
        let src = Src::Part(p);
        self.full(p.ident.rows, &|a| src.record(a, 0).0)
    }

    /// `run` holds a part that is not full (computed once per run; the verdict rides the run's `Arc`).
    pub fn run_holds(&self, run: &Run) -> bool {
        run.held.verdict(|| {
            let src = Src::Run(run);
            run.parts.iter().enumerate().any(|(i, p)| !self.full(p.rows, &|a| src.record(a, i).0))
        })
    }
}

/// A generation's stats: the root carrier's reference, the runs holding parts (the carrier too when it holds any),
/// and the loose parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatsState {
    pub root: Option<RunRef>,
    pub runs: Vec<Arc<Run>>,
    pub loose: Vec<Arc<PartStats>>,
}

/// A manifest record's identity as the stats see it.
pub fn ident_of(p: &PartRecord) -> PartIdent {
    PartIdent { part_no: p.part_no, granule_count: p.granule_count, band_count: p.band_count, rows: p.rows, footer_off: p.footer_off }
}

fn corrupt(at: &'static str) -> WriteError {
    WriteError::Format(FormatError::Corrupt { at })
}

/// A part read's error as the writer reports it.
pub fn read_error(e: ReadError) -> WriteError {
    match e {
        ReadError::Format(f) => WriteError::Format(f),
        ReadError::Io { at, errno } => WriteError::Io { op: at, path: String::new(), errno },
        _ => corrupt("stats: part open refused"),
    }
}

// ---------------------------------------------------------------------------
// Sources: a run or a loose part, as the assembly reads them
// ---------------------------------------------------------------------------

enum Src<'a> {
    Run(&'a Run),
    Part(&'a PartStats),
}

impl<'a> Src<'a> {
    fn parts(&self) -> &'a [PartIdent] {
        match self {
            Src::Run(r) => &r.parts,
            Src::Part(p) => std::slice::from_ref(&p.ident),
        }
    }

    fn facts(&self) -> &'a FactsImage {
        match self {
            Src::Run(r) => &r.facts,
            Src::Part(p) => &p.facts,
        }
    }

    fn attnos(&self) -> Vec<u32> {
        match self {
            Src::Run(r) => r.answers.iter().map(|a| a.0).collect(),
            Src::Part(p) => p.answers.iter().map(|a| a.0).collect(),
        }
    }

    /// Part `i`'s record and digest for `attno` (empty / `None` when it carries none).
    fn record(&self, attno: u32, i: usize) -> (&'a [u8], Option<[u8; DIGEST_LEN]>) {
        match self {
            Src::Run(r) => match r.answers.binary_search_by_key(&attno, |a| a.0) {
                Ok(k) => {
                    let col = ColumnAnswers::validated(&r.answers[k].1, r.parts.len());
                    (col.stats_body(i).unwrap_or(&[]), col.digest(i).and_then(|d| <[u8; DIGEST_LEN]>::try_from(d).ok()))
                }
                Err(_) => (&[], None),
            },
            Src::Part(p) => match p.answers.binary_search_by_key(&attno, |a| a.0) {
                Ok(k) => (&p.answers[k].1, p.answers[k].2),
                Err(_) => (&[], None),
            },
        }
    }

    fn ext(&self, i: usize) -> Vec<(u32, u16, Vec<u8>)> {
        match self {
            Src::Run(r) => {
                let pn = r.parts[i].part_no;
                r.ext.iter().filter(|e| e.0 == pn).cloned().collect()
            }
            Src::Part(p) => p.ext.iter().map(|(t, b)| (p.ident.part_no, *t, b.clone())).collect(),
        }
    }
}

/// The facts image and (when `attnos` is given) the answers of the parts `order` names, in that order: `order[k] =
/// (source, index within the source)`. Every source must be wholly named (its accumulator is merged whole).
fn assemble(srcs: &[Src<'_>], order: &[(u32, u32)], key: (u64, u64, u64), attnos: Option<&[u32]>) -> FormatResult<(FactsImage, Vec<(u32, Vec<u8>)>)> {
    let mut acc = FactsAcc::new();
    for s in srcs {
        acc.merge(&FactsAcc::from_image(s.facts())?)?;
    }
    let offsets: Vec<Vec<u32>> = srcs
        .iter()
        .map(|s| -> FormatResult<Vec<u32>> {
            let mut off = pgrc_format::try_vec(s.parts().len(), "stats")?;
            let mut g = 0u32;
            for p in s.parts() {
                off.push(g);
                g += p.granule_count;
            }
            Ok(off)
        })
        .collect::<FormatResult<_>>()?;
    let slices: Vec<(u32, u32, u32)> = order.iter().map(|&(s, i)| (s, offsets[s as usize][i as usize], srcs[s as usize].parts()[i as usize].granule_count)).collect();
    let images: Vec<&FactsImage> = srcs.iter().map(|s| s.facts()).collect();
    acc.assemble_zones(&images, &slices)?;
    let image = acc.into_image(key.0, key.1, key.2)?;
    if image.header.parts != order.len() as u64 {
        return Err(FormatError::Corrupt { at: "stats: assembled part count" });
    }
    let mut answers = Vec::new();
    for &attno in attnos.unwrap_or(&[]) {
        let mut bodies: Vec<&[u8]> = pgrc_format::try_vec(order.len(), "stats")?;
        let mut digests: Vec<Option<[u8; DIGEST_LEN]>> = pgrc_format::try_vec(order.len(), "stats")?;
        for &(s, i) in order {
            let (b, d) = srcs[s as usize].record(attno, i as usize);
            bodies.push(b);
            digests.push(d);
        }
        answers.push((attno, encode_column(&bodies, &digests)?));
    }
    Ok((image, answers))
}

/// A run (not yet written: `rref` is set when it is encoded) over whole sources, parts in part_no order.
fn run_of_sources(srcs: &[Src<'_>]) -> FormatResult<Run> {
    let mut order: Vec<(u32, u32, u32)> = Vec::new();
    for (s, src) in srcs.iter().enumerate() {
        order.extend(src.parts().iter().enumerate().map(|(i, p)| (p.part_no, s as u32, i as u32)));
    }
    order.sort_unstable();
    if order.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(FormatError::Corrupt { at: "stats: a part in two sources" });
    }
    let mut attnos: Vec<u32> = srcs.iter().flat_map(|s| s.attnos()).collect();
    attnos.sort_unstable();
    attnos.dedup();
    let pairs: Vec<(u32, u32)> = order.iter().map(|o| (o.1, o.2)).collect();
    let (facts, answers) = assemble(srcs, &pairs, (0, 0, 0), Some(&attnos))?;
    let parts = order.iter().map(|o| srcs[o.1 as usize].parts()[o.2 as usize]).collect();
    let ext = order.iter().flat_map(|o| srcs[o.1 as usize].ext(o.2 as usize)).collect();
    Ok(Run { hold: Hold::default(), rref: RunRef { gen: 0, seq: 0, nparts: 0, len: 0, crc: 0 }, parts, facts, answers, ext, held: HeldCell::default() })
}

// ---------------------------------------------------------------------------
// StatsState
// ---------------------------------------------------------------------------

impl StatsState {
    fn sources(&self) -> Vec<Src<'_>> {
        self.runs.iter().map(|r| Src::Run(r)).chain(self.loose.iter().map(|p| Src::Part(p))).collect()
    }

    /// Manifest position → (source, index), or why the state is not this manifest's. One walk of the manifest merging
    /// the sources by their next `part_no` (P-1 L4a: O(parts + source switches × log sources), where a sort of every
    /// part was O(parts log parts) twice per publish); a state the walk does not match takes the sort, which decides.
    fn order(&self, m: &Manifest) -> Result<Vec<(u32, u32)>, OrderError> {
        let srcs = self.sources();
        if srcs.iter().map(|s| s.parts().len()).sum::<usize>() != m.parts.len() {
            return Err(OrderError::Mismatch("stats: runs and loose parts do not cover the manifest"));
        }
        match merge_order(&srcs, m).map_err(OrderError::Alloc)? {
            Some(out) => Ok(out),
            None => sorted_order(&srcs, m),
        }
    }

    /// Runs ∪ loose == `m.parts` (idents equal), runs disjoint.
    pub fn check(&self, m: &Manifest) -> Result<(), OrderError> {
        self.order(m).map(drop)
    }

    /// manifest position → (source, index): source < runs.len() is a run, else loose[source - runs.len()].
    pub fn index(&self, m: &Manifest) -> Vec<(u32, u32)> {
        self.order(m).unwrap_or_default()
    }

    /// [`Self::index`], or why the state is not `m`'s.
    pub fn try_index(&self, m: &Manifest) -> Result<Vec<(u32, u32)>, OrderError> {
        self.order(m)
    }
}

#[derive(Debug)]
pub enum OrderError {
    Mismatch(&'static str),
    Alloc(FormatError),
}

impl core::fmt::Display for OrderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OrderError::Mismatch(at) => f.write_str(at),
            OrderError::Alloc(e) => write!(f, "{e}"),
        }
    }
}

impl OrderError {
    pub fn or_corrupt(self) -> FormatError {
        match self {
            OrderError::Mismatch(at) => FormatError::Corrupt { at },
            OrderError::Alloc(e) => e,
        }
    }
}

/// [`StatsState::order`]'s walk: every source's parts `part_no` ascending (a run's are; loose parts are one each), the
/// manifest's too. The source of each record is the current one while its next part is the record's, else the least
/// head of the others. `None` when a record's part is not the least head or its ident differs: the caller sorts. The
/// sources' part total equals the manifest's (checked), so a walk that matches every record used every part once.
fn merge_order(srcs: &[Src<'_>], m: &Manifest) -> FormatResult<Option<Vec<(u32, u32)>>> {
    use std::cmp::Reverse;
    let mut next = pgrc_format::try_filled(srcs.len(), 0usize, "stats order")?; // alloc: Cold
    let mut heads: std::collections::BinaryHeap<Reverse<(u32, usize)>> = srcs.iter().enumerate().filter_map(|(s, src)| src.parts().first().map(|p| Reverse((p.part_no, s)))).collect(); // alloc: Cold
    let mut cur: Option<usize> = None;
    let mut out = pgrc_format::try_vec(m.parts.len(), "stats order")?; // alloc: Cold
    for rec in &m.parts {
        let s = match cur {
            Some(c) if srcs[c].parts().get(next[c]).is_some_and(|p| p.part_no == rec.part_no) => c,
            _ => {
                if let Some(p) = cur.and_then(|c| srcs[c].parts().get(next[c]).map(|p| Reverse((p.part_no, c)))) {
                    heads.push(p);
                }
                let Some(Reverse((part_no, s))) = heads.pop() else { return Ok(None) };
                if part_no != rec.part_no {
                    return Ok(None);
                }
                cur = Some(s);
                s
            }
        };
        let i = next[s];
        if srcs[s].parts()[i] != ident_of(rec) {
            return Ok(None);
        }
        out.push((s as u32, i as u32));
        next[s] = i + 1;
    }
    Ok(Some(out))
}

/// [`StatsState::order`] by a sort of every part (the walk declined): decides the order or why there is none.
fn sorted_order(srcs: &[Src<'_>], m: &Manifest) -> Result<Vec<(u32, u32)>, OrderError> {
    let mut all: Vec<(u32, u32, u32)> = pgrc_format::try_vec(m.parts.len(), "stats order").map_err(OrderError::Alloc)?;
    for (s, src) in srcs.iter().enumerate() {
        all.extend(src.parts().iter().enumerate().map(|(i, p)| (p.part_no, s as u32, i as u32)));
    }
    all.sort_unstable();
    if all.len() != m.parts.len() {
        return Err(OrderError::Mismatch("stats: runs and loose parts do not cover the manifest"));
    }
    let mut out = pgrc_format::try_vec(all.len(), "stats order").map_err(OrderError::Alloc)?;
    for (rec, &(pn, s, i)) in m.parts.iter().zip(&all) {
        if pn != rec.part_no || srcs[s as usize].parts()[i as usize] != ident_of(rec) {
            return Err(OrderError::Mismatch("stats: a run or loose part differs from its manifest record"));
        }
        out.push((s, i));
    }
    Ok(out)
}

impl StatsState {
    /// The part at `(source, index)` ([`Self::index`]): its part-grain Stats record for `attno` (empty: none) and its
    /// PartDigest. No copy: a run's column is split in place.
    pub fn part_answer(&self, (s, i): (u32, u32), attno: u32) -> (&[u8], Option<&[u8]>) {
        let (s, i) = (s as usize, i as usize);
        match self.runs.get(s) {
            Some(r) => match r.answers.binary_search_by_key(&attno, |a| a.0) {
                Ok(k) => {
                    let col = ColumnAnswers::validated(&r.answers[k].1, r.parts.len());
                    (col.stats_body(i).unwrap_or(&[]), col.digest(i))
                }
                Err(_) => (&[], None),
            },
            None => {
                let p = &self.loose[s - self.runs.len()];
                match p.answers.binary_search_by_key(&attno, |a| a.0) {
                    Ok(k) => (&p.answers[k].1, p.answers[k].2.as_ref().map(|d| &d[..])),
                    Err(_) => (&[], None),
                }
            }
        }
    }

    /// The columns a generation's answers carry: those some part of it carries a record for. D4783 (FASTADD): a part
    /// written before an ADD COLUMN has no record of it and a later part has one; each part answers from its own.
    pub fn answer_attnos(&self, index: &[(u32, u32)]) -> Vec<u32> {
        let mut attnos: Vec<u32> = self.runs.iter().flat_map(|r| r.answers.iter().map(|a| a.0))
            .chain(self.loose.iter().flat_map(|p| p.answers.iter().map(|a| a.0))).collect();
        attnos.sort_unstable();
        attnos.dedup();
        attnos.into_iter().filter(|&a| index.iter().any(|&at| !self.part_answer(at, a).0.is_empty())).collect()
    }

    /// Bytes of part-grain answers held (runs' columns and loose records), for the catalog's census.
    pub fn answer_bytes(&self) -> u64 {
        let runs: usize = self.runs.iter().flat_map(|r| r.answers.iter().map(|a| a.1.len())).sum();
        let loose: usize = self.loose.iter().flat_map(|p| p.answers.iter().map(|a| a.1.len() + 24)).sum();
        (runs + loose) as u64
    }

    /// COMPACTP: `part_no`'s `(source, index)` ([`Self::index`]'s form) without the manifest walk: a loose part's, or
    /// the run's that holds it. O(runs + loose).
    pub fn locate(&self, part_no: u32) -> Option<(u32, u32)> {
        if let Some(k) = self.loose.iter().position(|p| p.ident.part_no == part_no) {
            return Some(((self.runs.len() + k) as u32, 0));
        }
        self.runs.iter().enumerate().find_map(|(s, r)| r.parts.binary_search_by_key(&part_no, |p| p.part_no).ok().map(|i| (s as u32, i as u32)))
    }

    pub fn run_of(&self, part_no: u32) -> Option<&Arc<Run>> {
        self.runs.iter().find(|r| r.parts.binary_search_by_key(&part_no, |p| p.part_no).is_ok())
    }

    /// The generation's image, `serve`-equal to the fold of `m.parts` in order; zones assembled in manifest order.
    pub fn image(&self, m: &Manifest) -> FormatResult<FactsImage> {
        self.generation(m, false).map(|(img, _)| img)
    }

    /// The image and the answers columns of `m` (the columns the first part carries, `build_from_parts`'s rule).
    pub fn generation(&self, m: &Manifest, with_answers: bool) -> FormatResult<(FactsImage, Vec<(u32, Vec<u8>)>)> {
        let order = self.order(m).map_err(OrderError::or_corrupt)?;
        let srcs = self.sources();
        let attnos: Option<Vec<u32>> = with_answers.then(|| match order.first() {
            None => Vec::new(),
            Some(&(s, i)) => {
                let src = &srcs[s as usize];
                src.attnos().into_iter().filter(|&a| !src.record(a, i as usize).0.is_empty()).collect()
            }
        });
        let h = m.header;
        assemble(&srcs, &order, (h.gen, h.publisher_fxid, h.schema_fingerprint), attnos.as_deref())
    }

    /// A generation's state from disk: the root carrier and the runs it lists (reusing `carried`'s `Arc<Run>` with an
    /// equal ref), loose parts from `carried`'s with an equal ident, else via `loose`. A run of another schema, a
    /// carrier without a list, or a state that does not cover `m` exactly is refused.
    pub fn load(
        vfs: &mut dyn WriteVfs,
        dir: &str,
        m: &Manifest,
        carried: Option<&StatsState>,
        loose: &mut dyn FnMut(&mut dyn WriteVfs, &PartRecord) -> WriteResult<PartStats>,
    ) -> WriteResult<StatsState> {
        // FASTADD: a run binds the schema of the generation that wrote it; one written before a metadata-only ALTER
        // binds the schema its parts keep, which the generation describes (EXT_SCHEMAS)
        let mut fps = vec![m.header.schema_fingerprint];
        if m.header.flags & pgrc_format::manifest::MANIFESTF_SCHEMAS != 0 {
            fps.extend(pgrc_format::manifest::schema_entries(&m.ext)?.iter().map(|e| e.fingerprint));
        }
        let fp = &fps[..];
        let mut state = StatsState { root: stats_ref(m)?, ..StatsState::default() };
        if let Some(c) = carried.filter(|c| state.root.is_some() && c.root == state.root) {
            // the same root names the same run set (a run's ref binds its bytes): no file is read
            state.runs = c.runs.clone();
        } else if let Some(root) = state.root {
            let (carrier, list) = read_run(vfs, dir, &root, fp, carried, true)?;
            let list = list.ok_or(corrupt("stats: the root carrier has no run list"))?;
            // the runs to read are hinted first, so a cold load reads them in parallel, not one round trip per run
            // (P-1 L4b: 70 ms of a 2,000-part table's cold load)
            for r in list.iter().filter(|r| !carried.is_some_and(|c| c.runs.iter().any(|x| x.rref == **r))) {
                vfs.willneed(&format!("{dir}/{}", run_file_name(r.gen, r.seq)), 0, r.len);
            }
            for r in &list {
                state.runs.push(read_run(vfs, dir, r, fp, carried, false)?.0);
            }
            if !carrier.parts.is_empty() {
                state.runs.push(carrier);
            }
        }
        let held: BTreeSet<u32> = state.runs.iter().flat_map(|r| r.parts.iter().map(|p| p.part_no)).collect();
        for rec in m.parts.iter().filter(|p| !held.contains(&p.part_no)) {
            let ident = ident_of(rec);
            let kept = carried.and_then(|c| c.loose.iter().find(|p| p.ident == ident));
            state.loose.push(match kept {
                Some(p) => Arc::clone(p),
                None => loose(vfs, rec)?.into_arc(),
            });
        }
        state.check(m).map_err(|e| match e { OrderError::Mismatch(at) => corrupt(at), OrderError::Alloc(e) => e.into() })?;
        Ok(state)
    }
}

/// One run by reference: the carried `Arc` when a carried state holds this ref and no list is wanted, else its file,
/// parsed and checked against the table's schema. Returns the run and, for `want_list`, its root list.
fn read_run(vfs: &mut dyn WriteVfs, dir: &str, r: &RunRef, fp: &[u64], carried: Option<&StatsState>, want_list: bool) -> WriteResult<(Arc<Run>, Option<Vec<RunRef>>)> {
    let held = carried.and_then(|c| c.runs.iter().find(|x| x.rref == *r));
    if let (Some(run), false) = (held, want_list) {
        return Ok((Arc::clone(run), None));
    }
    let f = RunFile::parse(vfs.read_full(&format!("{dir}/{}", run_file_name(r.gen, r.seq)))?, r)?;
    if !fp.iter().any(|&f2| f.binds_schema(f2)) {
        return Err(corrupt("stats: a run of another schema"));
    }
    let run = match held {
        Some(run) => Arc::clone(run),
        None => Run { hold: Hold::default(), rref: *r, parts: f.parts.clone(), facts: f.facts()?, answers: f.answers()?, ext: f.partext()?, held: HeldCell::default() }.into_arc(),
    };
    Ok((run, f.root))
}

// ---------------------------------------------------------------------------
// Per-part stats
// ---------------------------------------------------------------------------

fn one_part_image(acc: FactsAcc) -> FormatResult<FactsImage> {
    acc.into_image(0, 0, 0)
}

/// A sealed part's stats from its seal's captured slices (no file read). `Err` when the seal captured none.
pub fn part_stats_from_sealed(s: &SealedPart, part_no: u32) -> WriteResult<PartStats> {
    if s.facts_slices.is_empty() || s.plane_slices.is_empty() {
        return Err(WriteError::Contract { detail: "stats: the seal captured no facts slices" });
    }
    let mut acc = FactsAcc::new();
    acc.fold_part(&part_facts_from_sealed(s)?)?;
    let mut answers = pgrc_format::try_vec(s.plane_slices.len(), "stats")?;
    for p in &s.plane_slices {
        answers.push((p.attno, pgrc_format::try_to_vec(crate::bankplane::part_record(&p.stats_body), "stats")?, p.digest));
    }
    Ok(PartStats {
        hold: Hold::default(),
        ident: PartIdent { part_no, granule_count: s.granule_count, band_count: s.band_count, rows: s.rows, footer_off: s.footer_off },
        facts: one_part_image(acc)?,
        answers,
        ext: Vec::new(),
    })
}

fn meta_body(part: &OpenPart, idx: usize) -> ReadResult<Vec<u8>> {
    let flags = part.sections()[idx].flags;
    let raw = part.section_bytes(idx)?;
    if flags & SECTIONF_META_ZSTD != 0 {
        pgrc_codec::wrapper::meta_unwrap_body(raw.bytes()).map_err(ReadError::Format)
    } else {
        Ok(pgrc_format::try_to_vec(raw.bytes(), "stats")?)
    }
}

/// A part's stats from its footer sections: the walk's fold inputs (`part_facts_from_open`) folded into a one-part
/// image, and each Stats column's part-grain record with its PartDigest (`answers::build_from_parts`, one part).
pub fn part_stats_from_open(part: &OpenPart, rec: &PartRecord) -> ReadResult<PartStats> {
    let owned = part_facts_from_open(part, rec.file_len)?;
    let mut acc = FactsAcc::new();
    acc.fold_part(&owned.as_facts()).map_err(ReadError::Format)?;
    let mut answers = Vec::new();
    for c in owned.cols.iter().filter(|c| c.stats_body.is_some()) {
        let record = pgrc_format::try_to_vec(crate::bankplane::part_record(c.stats_body.as_deref().unwrap_or(&[])), "stats")?;
        let digest = match part.find_section(SectionKind::PartDigest, c.attno, 0) {
            Some(idx) => <[u8; DIGEST_LEN]>::try_from(meta_body(part, idx)?.as_slice()).ok(),
            None => None,
        };
        answers.push((c.attno, record, digest));
    }
    Ok(PartStats { hold: Hold::default(), ident: ident_of(rec), facts: one_part_image(acc)?, answers, ext: Vec::new() })
}

/// OC-5: one run of every base part from a root-less generation's in-memory image and answers (the catalog's copy,
/// or a completion's). Its image must be `m`'s and cover every part.
pub fn legacy_run(m: &Manifest, image: &FactsImage, answers: &[(u32, Vec<u8>)]) -> WriteResult<Run> {
    let h = image.header;
    if (h.gen, h.publisher_fxid, h.schema_fingerprint, h.parts) != (m.header.gen, m.header.publisher_fxid, m.header.schema_fingerprint, m.parts.len() as u64) {
        return Err(WriteError::Contract { detail: "stats: the legacy image is not the base generation's" });
    }
    for (_, payload) in answers {
        ColumnAnswers::parse(payload, m.parts.len())?;
    }
    let mut sorted = pgrc_format::try_to_vec(answers, "stats")?;
    sorted.sort_unstable_by_key(|a| a.0);
    Ok(Run { hold: Hold::default(), rref: RunRef { gen: 0, seq: 0, nparts: 0, len: 0, crc: 0 }, parts: m.parts.iter().map(ident_of).collect(), facts: image.clone(), answers: sorted, ext: Vec::new(), held: HeldCell::default() })
}

/// OC-5 without an image: one run of every part from the parts' own stats (a walk).
pub fn legacy_run_from_parts(parts: &[PartStats]) -> WriteResult<Run> {
    let srcs: Vec<Src<'_>> = parts.iter().map(Src::Part).collect();
    Ok(run_of_sources(&srcs)?)
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// The runs a publish stages (`(ref, file bytes)`, in write order: the last carries the root) and the new state.
#[derive(Debug, Clone, Default)]
pub struct StatsPlan {
    pub stage: Vec<(RunRef, Vec<u8>)>,
    pub state: StatsState,
    /// The root changed: the manifest's `EXT_STATS` record is `state.root`.
    pub root_changed: bool,
}

/// A run in the new state: carried (already written) or fresh (written by this publish).
enum Slot {
    Carried(Arc<Run>),
    Fresh(Run),
}

impl Slot {
    fn run(&self) -> &Run {
        match self {
            Slot::Carried(r) => r,
            Slot::Fresh(r) => r,
        }
    }
}

/// The policy (module doc) over `base`: `installs` are the new parts' stats, `retired` the part numbers leaving.
/// `refold` returns the footers' stats of a cut run's survivors; `hold` keeps a small run out of big-run merges (the
/// publisher's is [`FullTest::run_holds`]). `(gen, fxid, fp)` stamp the runs written.
#[allow(clippy::too_many_arguments)]
pub fn plan_stats(
    base: &StatsState,
    gen: u64,
    fxid: u64,
    fp: u64,
    installs: Vec<PartStats>,
    retired: &[u32],
    refold: &mut dyn FnMut(&[PartIdent]) -> WriteResult<Vec<PartStats>>,
    hold: &dyn Fn(&Run) -> bool,
) -> WriteResult<StatsPlan> {
    plan_with(base, None, gen, fxid, fp, installs, retired, refold, hold)
}

/// `plan_stats` over a root-less base whose parts are all in `legacy` (OC-5): the legacy run is written by this
/// publish, then the policy runs as usual.
#[allow(clippy::too_many_arguments)]
pub fn plan_stats_legacy(
    legacy: Run,
    gen: u64,
    fxid: u64,
    fp: u64,
    installs: Vec<PartStats>,
    retired: &[u32],
    refold: &mut dyn FnMut(&[PartIdent]) -> WriteResult<Vec<PartStats>>,
    hold: &dyn Fn(&Run) -> bool,
) -> WriteResult<StatsPlan> {
    plan_with(&StatsState::default(), Some(legacy), gen, fxid, fp, installs, retired, refold, hold)
}

#[allow(clippy::too_many_arguments)]
fn plan_with(
    base: &StatsState,
    legacy: Option<Run>,
    gen: u64,
    fxid: u64,
    fp: u64,
    mut installs: Vec<PartStats>,
    retired: &[u32],
    refold: &mut dyn FnMut(&[PartIdent]) -> WriteResult<Vec<PartStats>>,
    hold: &dyn Fn(&Run) -> bool,
) -> WriteResult<StatsPlan> {
    let gone: BTreeSet<u32> = retired.iter().copied().collect();
    let mut slots: Vec<Slot> = base.runs.iter().map(|r| Slot::Carried(Arc::clone(r))).collect();
    slots.extend(legacy.map(Slot::Fresh));
    let mut loose: Vec<Arc<PartStats>> = base.loose.iter().filter(|p| !gone.contains(&p.ident.part_no)).cloned().collect();

    // 1. retire cuts: a run losing every part goes; one losing some is rewritten from its survivors' footers
    let mut kept: Vec<Slot> = pgrc_format::try_vec(slots.len(), "stats")?;
    for slot in slots {
        let r = slot.run();
        let hit = r.parts.iter().filter(|p| gone.contains(&p.part_no)).count();
        if hit == 0 {
            kept.push(slot);
        } else if hit < r.parts.len() {
            let survivors: Vec<PartIdent> = r.parts.iter().filter(|p| !gone.contains(&p.part_no)).copied().collect();
            let mut stats = refold(&survivors)?;
            if stats.len() != survivors.len() || stats.iter().zip(&survivors).any(|(s, p)| s.ident != *p) {
                return Err(WriteError::Contract { detail: "stats: a refold returned other parts" });
            }
            for s in &mut stats {
                s.ext = r.ext.iter().filter(|e| e.0 == s.ident.part_no).map(|e| (e.1, e.2.clone())).collect();
            }
            let srcs: Vec<Src<'_>> = stats.iter().map(Src::Part).collect();
            kept.push(Slot::Fresh(run_of_sources(&srcs)?));
        }
    }
    let mut slots = kept;

    // 2. installs: a bulk install goes into runs; a remainder under LOOSE_MAX goes loose
    installs.sort_unstable_by_key(|p| p.ident.part_no);
    if installs.len() >= LOOSE_MAX {
        for chunk in installs.chunks(RUN_PARTS_MAX) {
            if chunk.len() >= LOOSE_MAX {
                let srcs: Vec<Src<'_>> = chunk.iter().map(Src::Part).collect();
                slots.push(Slot::Fresh(run_of_sources(&srcs)?));
            } else {
                loose.extend(chunk.iter().cloned().map(PartStats::into_arc));
            }
        }
    } else {
        loose.extend(installs.into_iter().map(PartStats::into_arc));
    }

    // 3. fold: LOOSE_MAX loose parts (the lowest part numbers) into one small run
    loose.sort_unstable_by_key(|p| p.ident.part_no);
    while loose.len() >= LOOSE_MAX {
        let fold: Vec<Arc<PartStats>> = loose.drain(..LOOSE_MAX).collect();
        let srcs: Vec<Src<'_>> = fold.iter().map(|p| Src::Part(p)).collect();
        slots.push(Slot::Fresh(run_of_sources(&srcs)?));
    }

    // 4. merge: SMALL_RUNS_MAX small runs (none held) into one big run; `hold` is asked only once enough small runs
    // exist for a merge. The hold bound: with more than HELD_SMALL_MAX held small runs, the oldest SMALL_RUNS_MAX small
    // runs (the first in the list) merge as if nothing were held.
    loop {
        let is_small = |s: &Slot| s.run().parts.len() <= LOOSE_MAX;
        let all_small: Vec<usize> = (0..slots.len()).filter(|&i| is_small(&slots[i])).collect();
        if all_small.len() < SMALL_RUNS_MAX {
            break;
        }
        let free: Vec<usize> = all_small.iter().copied().filter(|&i| !hold(slots[i].run())).collect();
        let small: Vec<usize> = if free.len() >= SMALL_RUNS_MAX {
            pgrc_format::try_to_vec(&free[..SMALL_RUNS_MAX], "stats")?
        } else if all_small.len() - free.len() > HELD_SMALL_MAX {
            pgrc_format::try_to_vec(&all_small[..SMALL_RUNS_MAX], "stats")?
        } else {
            break;
        };
        let merged = {
            let srcs: Vec<Src<'_>> = small.iter().map(|&i| Src::Run(slots[i].run())).collect();
            run_of_sources(&srcs)?
        };
        for &i in small.iter().rev() {
            slots.remove(i);
        }
        slots.push(Slot::Fresh(merged));
    }

    // 5. the root: unchanged when the run set is; else carried by the last run written (a zero-part carrier if none)
    let carried_refs: BTreeSet<(u64, u32)> = slots.iter().filter_map(|s| matches!(s, Slot::Carried(_)).then(|| s.run().rref.key())).collect();
    let base_refs: BTreeSet<(u64, u32)> = base.runs.iter().map(|r| r.rref.key()).collect();
    let fresh = slots.iter().filter(|s| matches!(s, Slot::Fresh(_))).count();
    let mut plan = StatsPlan::default();
    if fresh == 0 && carried_refs == base_refs {
        plan.state = StatsState { root: base.root, runs: slots.into_iter().map(|s| match s { Slot::Carried(r) => r, Slot::Fresh(_) => unreachable!() }).collect(), loose };
        return Ok(plan);
    }
    plan.root_changed = true;
    if slots.is_empty() {
        plan.state = StatsState { root: None, runs: Vec::new(), loose };
        return Ok(plan);
    }
    let mut runs: Vec<Arc<Run>> = pgrc_format::try_vec(slots.len(), "stats")?;
    let mut fresh_runs: Vec<Run> = pgrc_format::try_vec(fresh, "stats")?;
    for s in slots {
        match s {
            Slot::Carried(r) => runs.push(r),
            Slot::Fresh(r) => fresh_runs.push(r),
        }
    }
    let carrier_empty = fresh_runs.is_empty();
    if carrier_empty {
        fresh_runs.push(Run { hold: Hold::default(), rref: RunRef { gen: 0, seq: 0, nparts: 0, len: 0, crc: 0 }, parts: Vec::new(), facts: FactsImage::default(), answers: Vec::new(), ext: Vec::new(), held: HeldCell::default() });
    }
    let last = fresh_runs.len() - 1;
    for (seq, mut run) in fresh_runs.into_iter().enumerate() {
        let root = (seq == last).then(|| {
            let mut refs: Vec<RunRef> = runs.iter().map(|r| r.rref).collect();
            refs.sort_unstable_by_key(|r| r.key());
            refs
        });
        run.facts.header.gen = gen;
        run.facts.header.publisher_fxid = fxid;
        run.facts.header.schema_fingerprint = fp;
        let answers: Vec<(u32, &[u8])> = run.answers.iter().map(|(a, b)| (*a, b.as_slice())).collect();
        let ext: Vec<(u32, u16, &[u8])> = run.ext.iter().map(|(p, t, b)| (*p, *t, b.as_slice())).collect();
        let bytes = encode_run(&RunImage { gen, publisher_fxid: fxid, schema_fingerprint: fp, parts: &run.parts, facts: &run.facts, answers: &answers, root: root.as_deref(), partext: &ext }).map_err(WriteError::Format)?;
        run.rref = RunRef::of(gen, seq as u32, run.parts.len() as u32, &bytes);
        plan.stage.push((run.rref, bytes));
        if seq == last {
            plan.state.root = Some(run.rref);
        }
        if !run.parts.is_empty() {
            runs.push(run.into_arc());
        }
    }
    plan.state.runs = runs;
    plan.state.loose = loose;
    Ok(plan)
}
