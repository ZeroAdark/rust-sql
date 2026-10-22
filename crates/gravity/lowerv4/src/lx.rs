//! The rep context (moved from rep/mod.rs, main 09-24): `Lx`, the lowering context an arm runs in, its `Frame` and
//! memo, the cell / stored / block references and words an accessor takes, and retain's homes; rep/mod.rs re-exports it.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFn, Value};
use gravity_program::{DictRef, LifeId, Rep};
use mcx::{GrowIn, Mcx, PgVec};

use crate::cell::{At, Cell, CellHome};
use crate::admit::refusal::{Reason, Site};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    SameDict(DictRef),
    Program,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainHome {
    Record(LifeId),
    Run(LifeId),
    Spool(LifeId),
    Frame,
}

/// A cell and the IR values that hold it in the current row.
#[derive(Clone, Copy, Debug)]
pub struct CellRef<'a> {
    pub cell: &'a Cell,
    pub val: Value,
    pub null: Option<Value>,
}

/// A stored key image a hit is compared against: its `Rep` and address.
#[derive(Clone, Copy, Debug)]
pub struct StoredRef {
    pub rep: Rep,
    pub ptr: Value,
    pub off: i32,
}

/// A key or bucket image of at most two words; `exact`: equal words decide a hit (`WordImage::equality`).
#[derive(Clone, Copy, Debug)]
pub struct Words {
    pub w: [Option<Value>; 2],
    pub exact: bool,
}

/// A 64-row block of a window (H01): the address of row `j0`'s element in the lane and the block's row count.
#[derive(Clone, Copy, Debug)]
pub struct BlockRef {
    pub elem: Value,
    pub rows: Value,
}

/// A lane's block form: its elements' base address and byte width, read by a mask road (`mask.word`, `mask.vec`).
#[derive(Clone, Copy, Debug)]
pub struct BlockLane {
    pub base: Value,
    pub bytes: u8,
}

/// The per-row memo of `datum`, keyed by the cell's storage (`At`, backing value, `Rep`) and its NULL value (D1189: a
/// drained row's NULL cell holds the constant 0, so `x + null` folds to `x`'s value): a step that re-numbers cells cannot
/// answer from another row's.
/// No `Default` (M6-REPS, batch B): a branch sets the memo aside only as a `SetAside`, which only `restore` takes back, so no
/// put-back can drop the bounded headers the branch recorded; `Env::lx` lends it whole and gets it back by assignment.
pub struct Memo<'e> {
    rows: PgVec<'e, ((At, Value, Rep, Option<Value>), Value, Value)>,
    /// The row's numeric mantissa words (fold/num_word.rs `word`): `(datum, NULL flag, scale, with dscale)`
    /// -> `(mantissa, dscale)`, so two aggregates of one column decode it once. Cleared and set aside with `rows`.
    nums: PgVec<'e, ((Value, Value, i16, bool), Value, Value)>,
    /// Road text.lean (rep/varlena/cmp_text.rs `text_memo`): a text datum's header this row, `(datum, NULL flag, trim)` ->
    /// `[plain, length, payload address, length without trailing blanks (= length without trim)]`, so every test of one
    /// column decodes it once. Cleared and set aside with `rows`.
    texts: PgVec<'e, ((Value, Value, bool), [Value; 4])>,
    /// Loop headers an accessor emitted that its data bounds (a key's bytes, M6), not a window: the cancel verifier accepts
    /// them (end/sink.rs `bounded_since`'s rule); kept across `clear`, handed to `Env::bounded` when the function is done.
    bounded: PgVec<'e, Block>,
}

/// A memo set aside around a branch (`Memo::set_aside`); `Memo::restore` is its only way back.
#[must_use = "a memo set aside goes back through Memo::restore"]
pub struct SetAside<'e>(Memo<'e>);

impl<'e> Memo<'e> {
    pub fn clear(&mut self) {
        self.rows.clear();
        self.nums.clear();
        self.texts.clear();
    }

    /// Every block created since `n0` (the helper that emitted the bounded loops ran in between).
    pub fn bounded_since(&mut self, b: &FunctionBuilder<'_>, n0: usize) {
        self.bounded.extend_in((n0..b.num_blocks()).map(|i| Block(i as u32)));
    }

    /// An empty memo: `Env::new`'s and `Lx::new`'s.
    pub fn root(mcx: Mcx<'e>) -> Memo<'e> {
        Memo { rows: PgVec::new_in(mcx), nums: PgVec::new_in(mcx), texts: PgVec::new_in(mcx), bounded: PgVec::new_in(mcx) }
    }

    /// The whole memo, bounded headers too, for `Env::lx`'s round trip (`env.memo = cx.memo` puts it back).
    pub fn lend(&mut self) -> Memo<'e> {
        let mcx = *self.bounded.allocator();
        core::mem::replace(self, Memo::root(mcx))
    }

    /// The memo set aside for a branch (the branch starts empty): only `restore` takes the token back.
    pub fn set_aside(&mut self) -> SetAside<'e> {
        SetAside(self.lend())
    }

    /// D3553 (B): the memo set aside for a branch that starts with the outer rows (they dominate the branch's entry);
    /// `restore` drops what the branch adds.
    pub fn fork(&mut self) -> SetAside<'e> {
        let mcx = *self.rows.allocator();
        let (rows, nums, texts) = (mcx::to_vec_in(&self.rows[..], mcx), mcx::to_vec_in(&self.nums[..], mcx), mcx::to_vec_in(&self.texts[..], mcx));
        let outer = self.lend();
        (self.rows, self.nums, self.texts) = (rows, nums, texts);
        SetAside(outer)
    }

    /// The memo a branch set aside back in place: the branch's rows go (they do not dominate its join), its bounded loop
    /// headers stay (the cancel verifier's: a key hashed inside the branch, a FILTER's or a probe hit's hand-on, M6-REPS).
    pub fn restore(&mut self, outer: SetAside<'e>) {
        let mut outer = outer.0;
        outer.bounded.extend_from_slice_in(&self.bounded);
        *self = outer;
    }

    pub fn take_bounded(&mut self) -> PgVec<'e, Block> {
        let mcx = *self.bounded.allocator();
        core::mem::replace(&mut self.bounded, PgVec::new_in(mcx))
    }

    pub fn get(&self, c: CellRef<'_>) -> Option<(Value, Value)> {
        let k = (c.cell.at, c.val, c.cell.rep, c.null);
        self.rows.iter().find(|r| r.0 == k).map(|r| (r.1, r.2))
    }

    pub fn put(&mut self, c: CellRef<'_>, v: (Value, Value)) {
        self.rows.push_in(((c.cell.at, c.val, c.cell.rep, c.null), v.0, v.1));
    }

    /// A numeric datum's `(mantissa, dscale)` at `scale` this row already decoded (`k`: datum, NULL flag, scale, dscale).
    pub fn num(&self, k: (Value, Value, i16, bool)) -> Option<(Value, Value)> {
        self.nums.iter().find(|r| r.0 == k).map(|r| (r.1, r.2))
    }

    pub fn put_num(&mut self, k: (Value, Value, i16, bool), v: (Value, Value)) {
        self.nums.push_in((k, v.0, v.1));
    }

    /// A text datum's header words this row already decoded (`k`: datum, NULL flag, trim; road text.lean).
    pub fn text(&self, k: (Value, Value, bool)) -> Option<[Value; 4]> {
        self.texts.iter().find(|r| r.0 == k).map(|r| r.1)
    }

    pub fn put_text(&mut self, k: (Value, Value, bool), v: [Value; 4]) {
        self.texts.push_in((k, v));
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// The hosts an arm may call, taken from the frame's core hosts (`frame/env.rs` builds them).
#[derive(Clone, Copy, Debug)]
pub struct RepHosts {
    /// A dictionary code word `part << 32 | code` of a site's code lane rendered to its image (the `Code` datum).
    pub render: HostFn,
}

/// The function an accessor emits into: its context, the site its lanes are read from, the hosts.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub ctx: Value,
    pub site: Option<usize>,
    pub hosts: RepHosts,
    /// The window's bound per-code tables (`Env::tables`: table `t` at byte `24 t`, e.g. `DictRef.fp_table`).
    pub tables: Option<Value>,
}

/// The lowering context an accessor emits into: the function, the node its refusals name, the row's memo, the frame
/// (`None` in a unit test's bare function).
pub struct Lx<'a, 'f, 'e> {
    pub b: &'a mut FunctionBuilder<'f>,
    /// The lowering's planning context (`Env::mcx`; a rig's outside one): emission scratch.
    pub mcx: mcx::Mcx<'a>,
    pub node: i32,
    pub memo: Memo<'e>,
    pub at: Option<Frame>,
    /// Where a copying retain puts its bytes: the consumer's home region (`None`: a copy is a Bug).
    pub space: Option<&'a mut dyn KeepSpace>,
    /// Pipeline k's driving site (`Env::pipe_sites`): a record cell's dictionaries are its producer's site's.
    pub pipe_sites: Option<std::rc::Rc<[Option<u16>]>>,
    /// The function's raise block (`Env::l_raise`, its raise mask): a host that failed leaves through it at once, its
    /// stashed error the call's (D344; `None` in a unit test's bare function).
    pub raise: Option<Block>,
    /// The roads that apply to the statement (`Env::core.strategies`; empty in a unit test's bare function): an image road
    /// whose form must agree across every side of a table engages only where its statement-level predicate holds
    /// (key.tagged: `VarlenaKeyHeap`).
    pub strategies: crate::face::strategy::StrategySet,
    /// What a collated key's order word is for (road key.coll, rep/varlena/coll.rs); `Sort` everywhere but a bounded top.
    pub key_word: KeyWord,
    /// A1 (D3224): the window's packed numeric lane kind tests (`Env::num_kinds`) inside a row loop; `None` elsewhere.
    pub num_kinds: Option<std::rc::Rc<[(usize, u16, Value)]>>,
    /// join.payload_fp: the record code words a hash probe handed on with their Fp128 words (`Env::fps`); none elsewhere.
    pub fps: FpLinks,
}

/// join.payload_fp (ops/hashjoin/strategies/payload_fp.rs): record code words and the Fp128 words their records carry,
/// the newest `FP_LINKS` kept in place (a ring: a probe's words are read in the body that lowers it, right after the
/// probe). A word whose link is gone keys by its render, as an unlinked one does.
#[derive(Clone, Copy, Debug)]
pub struct FpLinks {
    at: [(Value, [Value; 2]); FP_LINKS],
    n: usize,
}

/// The links an `FpLinks` keeps.
pub const FP_LINKS: usize = 16;

impl FpLinks {
    pub const NONE: FpLinks = FpLinks { at: [(Value(u32::MAX), [Value(u32::MAX); 2]); FP_LINKS], n: 0 };

    /// Code word `v` carries the Fp128 words `w`.
    pub fn add(&mut self, v: Value, w: [Value; 2]) {
        self.at[self.n % FP_LINKS] = (v, w);
        self.n += 1;
    }

    /// Every link of `o`, after this one's.
    pub fn merge(&mut self, o: &FpLinks) {
        for &(v, w) in o.links() {
            self.add(v, w);
        }
    }

    fn links(&self) -> &[(Value, [Value; 2])] {
        &self.at[..self.n.min(FP_LINKS)]
    }

    /// The Fp128 words code word `v` carries, if it is linked.
    pub fn get(&self, v: Value) -> Option<[Value; 2]> {
        self.links().iter().find(|e| e.0 == v).map(|e| e.1)
    }
}

/// What a collated key's order word serves (rep/varlena/coll.rs). `Sort`: the provider's sort-key prefix where it has one
/// (ICU: PostgreSQL's abbreviated key), else 0. `Plain`: 0, every compare the comparator's (a bounded sort takes no
/// abbreviated key, tuplesort.c `tuplesort_set_bound`). `Equal`: a word compared for equality only; under a collation whose
/// equal values are its equal bytes it is the payload's bytes prefix (rep.varlena.order's), PostgreSQL's presorted-key test
/// (nodeIncrementalSort.c `isCurrentGroup`, the type's equality: memcmp under such a collation), no provider call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWord {
    Sort,
    Plain,
    Equal,
}

/// A retain's destination, handed out by the consumer that owns the home (record, run, spool): `bytes` (8-aligned, 0 for a
/// NULL) in `home`'s region, the address returned.
pub trait KeepSpace {
    fn alloc(&mut self, b: &mut FunctionBuilder<'_>, home: RetainHome, bytes: Value) -> Value;
}

/// A retained cell and the values that now hold it, as `CellRef` holds a cell (`val` per its `At`).
#[derive(Clone, Copy, Debug)]
pub struct Kept {
    pub cell: Cell,
    pub val: Value,
    pub null: Option<Value>,
}

impl<'a, 'f, 'e> Lx<'a, 'f, 'e> {
    pub fn new(b: &'a mut FunctionBuilder<'f>, node: i32, mcx: Mcx<'e>) -> Lx<'a, 'f, 'e>
    where
        'e: 'a,
    {
        Lx { b, mcx, node, memo: Memo::root(mcx), at: None, space: None, pipe_sites: None, raise: None, strategies: crate::face::strategy::StrategySet::EMPTY, key_word: KeyWord::Sort, num_kinds: None, fps: FpLinks::NONE }
    }

    /// The site a cell's lanes and dictionaries belong to: a record cell's producing pipeline's, else the frame's.
    pub fn site_of(&self, c: &Cell) -> Option<usize> {
        let producer = |l: LifeId| self.pipe_sites.as_ref().and_then(|p| p.get(l.0 as usize).copied().flatten());
        match c.life.home {
            CellHome::Record(l) => producer(l).map(usize::from),
            _ => self.at.and_then(|f| f.site),
        }
    }

    /// The frame, or a Bug for an accessor that needs one in a bare function.
    pub fn frame(&self) -> Frame {
        match self.at {
            Some(f) => f,
            None => crate::refuse!(self.node, Reason::Bug { payload: "an accessor that reads the frame outside one" }),
        }
    }
}

impl Site for Lx<'_, '_, '_> {
    fn node(&self) -> i32 {
        self.node
    }
}
